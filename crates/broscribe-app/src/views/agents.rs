//! Agent access: the shared view of Settings' AI tools pane and the setup
//! assistant's AI tools step (design §4.1 step 11).
//!
//! Every action is a `broscribe` CLI child off the UI thread: `agents
//! status|install|uninstall` (the Terminal command link and the agent
//! skill) and `mcp enable|disable` (the MCP gate plus registration with the
//! detected agent CLIs). The MCP switch gates agent access through MCP only;
//! the installed CLI skill works without it. `mcp enable` always registers
//! again, so "Register again" runs it too. Controls are
//! disabled while a command runs; the view redraws only when an answer
//! arrives.
//!
//! Mounting: `cx.new(|_| AgentsView::new(state))`, then
//! [`AgentsView::reload`] whenever it is shown.

use std::ffi::OsString;

use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::switch::Switch;
use gpui::component::{ActiveTheme as _, Disableable as _, Sizable as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, ClickEvent, ClipboardItem, Context, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Role, StatefulInteractiveElement as _, Styled as _, Window, div,
};
use serde_json::Value;

use crate::jobs::{ChildKind, failure_text, sentence};
use crate::state::AppState;
use crate::ui::kit::{self, StatusKind};
use crate::ui::type_scale::{TextRole, TextRoleExt as _};
use crate::ui::{Spoken as _, text_line};
use crate::views::settings::{args, muted, reload_config, row};

/// An agent CLI `agents status` detected.
#[derive(Clone, Debug, PartialEq)]
pub struct DetectedAgent {
    pub name: String,
    pub path: String,
}

/// One agent-skill target (`agents`, `claude`) and its state (`missing`,
/// `current`, `outdated`, `foreign`).
#[derive(Clone, Debug, PartialEq)]
pub struct SkillTarget {
    pub target: String,
    pub path: String,
    pub state: String,
}

/// `broscribe agents status --json`.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentsStatus {
    /// The CLI the link, the skill and registrations point at.
    pub cli: String,
    pub link_path: String,
    /// `missing`, `ours`, `other` or `unsupported` (no link off macOS and
    /// Linux).
    pub link_state: String,
    pub agents: Vec<DetectedAgent>,
    pub skills: Vec<SkillTarget>,
    pub mcp_enabled: bool,
    /// The saved result of the last `mcp enable|disable` (`mcp.last`): what
    /// the vendor commands answered then, not live registration state.
    pub last_registration: Option<LastRegistration>,
}

/// `mcp.last` of `agents status`.
#[derive(Clone, Debug, PartialEq)]
pub struct LastRegistration {
    /// `enable` or `disable`.
    pub action: String,
    /// RFC 3339.
    pub at: String,
    pub agents: Vec<AgentResult>,
}

/// One item of `agents install|uninstall` (`link`, `skill:agents`,
/// `skill:claude`).
#[derive(Clone, Debug, PartialEq)]
pub struct ItemResult {
    pub item: String,
    pub path: String,
    pub error: Option<String>,
    pub error_code: Option<String>,
    /// Why an item that did not fail was left out (`unsupported`: this
    /// system has no such item); `None` when it was done.
    pub skipped: Option<String>,
}

/// One agent's `mcp enable|disable` outcome.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentResult {
    pub name: String,
    pub error: Option<String>,
}

/// The last `mcp enable|disable` answer.
#[derive(Clone, Debug, PartialEq)]
pub struct McpOutcome {
    pub enabled: bool,
    pub at: Option<String>,
    pub agents: Vec<AgentResult>,
}

/// A command the view runs; one at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentsAction {
    Install,
    Uninstall,
    EnableMcp,
    DisableMcp,
}

impl AgentsAction {
    fn args(self) -> Vec<OsString> {
        match self {
            Self::Install => args(["agents", "install"]),
            Self::Uninstall => args(["agents", "uninstall"]),
            Self::EnableMcp => args(["mcp", "enable"]),
            Self::DisableMcp => args(["mcp", "disable"]),
        }
    }
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or("").to_owned()
}

/// A row's failure; `None` when `ok` (done or `skipped`).
fn error_of(v: &Value) -> Option<String> {
    if v["ok"].as_bool() == Some(true) {
        return None;
    }
    // Without words of its own the cause is left out, never "failed".
    Some(v["error"].as_str().unwrap_or("").trim().to_owned())
}

/// The CLI's answer could not be read.
const UNEXPECTED_ANSWER: &str = "BroScribe got an answer it didn't expect.";

/// `agents status`; `None` when the answer is not one.
pub fn parse_status(v: &Value) -> Option<AgentsStatus> {
    let cli = v["cli"]["path"].as_str()?.to_owned();
    let link = &v["cli"]["link"];
    let list = |key: &str| v[key].as_array().cloned().unwrap_or_default();
    Some(AgentsStatus {
        cli,
        link_path: text(&link["path"]),
        link_state: text(&link["state"]),
        agents: list("agents")
            .iter()
            .filter_map(|a| {
                Some(DetectedAgent {
                    name: a["name"].as_str()?.to_owned(),
                    path: text(&a["path"]),
                })
            })
            .collect(),
        skills: list("skills")
            .iter()
            .filter_map(|s| {
                Some(SkillTarget {
                    target: s["target"].as_str()?.to_owned(),
                    path: text(&s["path"]),
                    state: text(&s["state"]),
                })
            })
            .collect(),
        mcp_enabled: v["mcp"]["enabled"].as_bool().unwrap_or(false),
        last_registration: parse_last(&v["mcp"]["last"]),
    })
}

/// `{action, at, agents}`; `None` when absent (`null`) or not one.
fn parse_last(v: &Value) -> Option<LastRegistration> {
    Some(LastRegistration {
        action: v["action"].as_str()?.to_owned(),
        at: text(&v["at"]),
        agents: agent_results(&v["agents"]),
    })
}

/// `[{name, ok, error}]` rows; rows without a name are skipped.
fn agent_results(v: &Value) -> Vec<AgentResult> {
    v.as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    Some(AgentResult {
                        name: r["name"].as_str()?.to_owned(),
                        error: error_of(r),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `agents install|uninstall`: per-item results and the status after them.
pub fn parse_items(v: &Value) -> (Vec<ItemResult>, Option<AgentsStatus>) {
    let items = v["results"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    Some(ItemResult {
                        item: r["item"].as_str()?.to_owned(),
                        path: text(&r["path"]),
                        error: error_of(r),
                        error_code: r["error_code"].as_str().map(str::to_owned),
                        skipped: r["skipped"].as_str().map(str::to_owned),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    (items, parse_status(&v["status"]))
}

/// `mcp enable|disable`; `None` when the answer has no `enabled` flag.
pub fn parse_mcp(v: &Value) -> Option<McpOutcome> {
    Some(McpOutcome {
        enabled: v["enabled"].as_bool()?,
        at: v["at"].as_str().map(str::to_owned),
        agents: agent_results(&v["agents"]),
    })
}

/// `'…'`-quotes `s` for a POSIX shell when needed.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@,".contains(c))
    {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn item_label(item: &str) -> &str {
    match item {
        "link" => "Terminal command",
        "skill:agents" => "BroScribe skill",
        "skill:claude" => "Claude Code skill",
        other => other,
    }
}

fn skill_label(target: &str) -> &str {
    match target {
        "agents" => "BroScribe skill",
        "claude" => "Skill for Claude Code",
        other => other,
    }
}

fn tool_name(id: &str) -> &str {
    match id {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "gemini" => "Gemini CLI",
        other => other,
    }
}

fn item_state(state: &str) -> (StatusKind, &'static str) {
    match state {
        "ours" | "current" => (StatusKind::Done, "Installed"),
        "outdated" => (StatusKind::Attention, "Out of date. Install updates it."),
        "other" | "foreign" => (
            StatusKind::Attention,
            "Another program uses this name; BroScribe leaves it alone.",
        ),
        _ => (StatusKind::Info, "Not installed"),
    }
}

fn installed_item(id: usize, title: &str, state: &str, path: &str, cx: &App) -> gpui::Div {
    let (kind, words) = item_state(state);
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(kit::status_line(
            ("agents-installed-state", id),
            kind,
            format!("{title} · {words}"),
            cx,
        ))
        .when(!path.is_empty(), |el| {
            el.child(
                div()
                    .id(("agents-installed-path", id))
                    .spoken(Role::Label, path.to_owned())
                    .text_role(TextRole::Mono)
                    .font_family(cx.theme().mono_font_family.clone())
                    .child(gpui::base::SelectableText::new(
                        ("agents-path-text", id),
                        path.to_owned(),
                    )),
            )
        })
}

const MCP_LABEL: &str = "Let AI tools connect (MCP)";
const MCP_SCOPE: &str = "Connected AI tools can read your meetings' transcripts, summaries, people and calendar details. What they send to their own AI service is up to each tool. The terminal command works without this.";

fn last_change(at: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(at)
        .ok()
        .map(|at| format!("Last change · {}", crate::ui::format::local_date_time(&at)))
}

fn mcp_json(cli: &str) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "mcpServers": {
            "broscribe": { "command": cli, "args": ["mcp"] }
        }
    }))
    .expect("MCP configuration contains only strings")
}

/// A line of the view: `(is_error, text)`.
pub type Line = (bool, String);

pub struct AgentsView {
    state: gpui::Entity<AppState>,
    status: Option<Result<AgentsStatus, String>>,
    in_setup: bool,
    /// Bumped by every status load and command; an older answer is dropped.
    generation: u64,
    running: Option<AgentsAction>,
    items: Option<Result<Vec<ItemResult>, String>>,
    mcp: Option<Result<McpOutcome, String>>,
    copied: bool,
}

impl AgentsView {
    pub fn new(state: gpui::Entity<AppState>) -> Self {
        Self {
            state,
            status: None,
            in_setup: false,
            generation: 0,
            running: None,
            items: None,
            mcp: None,
            copied: false,
        }
    }

    pub(crate) fn in_setup(mut self) -> Self {
        self.in_setup = true;
        self
    }

    pub fn status(&self) -> Option<&Result<AgentsStatus, String>> {
        self.status.as_ref()
    }

    /// The command running, if any.
    pub fn running(&self) -> Option<AgentsAction> {
        self.running
    }

    /// Whether the buttons and the switch accept a click.
    pub fn controls_enabled(&self) -> bool {
        self.running.is_none()
    }

    /// The MCP flag from the last answer (`None`: unknown).
    pub fn mcp_enabled(&self) -> Option<bool> {
        match &self.status {
            Some(Ok(s)) => Some(s.mcp_enabled),
            _ => None,
        }
    }

    fn mcp_switch(&self, cx: &Context<Self>) -> Switch {
        let enabled = self.mcp_enabled();
        Switch::new("mcp-switch")
            .accessibility_label(MCP_LABEL)
            .small()
            .checked(enabled.unwrap_or(false))
            .disabled(enabled.is_none() || !self.controls_enabled())
            .on_click(cx.listener(|this, on: &bool, _, cx| this.set_mcp(*on, cx)))
    }

    fn install_label(&self) -> &'static str {
        if self.running == Some(AgentsAction::Install) {
            "Installing…"
        } else if self.link_unsupported() {
            "Install skill"
        } else {
            "Install command and skill"
        }
    }

    fn link_unsupported(&self) -> bool {
        matches!(&self.status, Some(Ok(s)) if s.link_state == "unsupported")
    }

    fn can_uninstall(&self) -> bool {
        matches!(&self.status, Some(Ok(s)) if s.link_state == "ours" ||
            s.skills.iter().any(|skill| matches!(skill.state.as_str(), "current" | "outdated")))
    }

    /// Lists the link, skill, agent and MCP state again.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.running.is_some() {
            // The command's own answer brings the state.
            return;
        }
        self.copied = false;
        if matches!(&self.status, Some(Err(_))) {
            self.status = None;
            cx.notify();
        }
        self.generation += 1;
        let generation = self.generation;
        let task = self
            .state
            .read(cx)
            .run_cli(args(["agents", "status"]), ChildKind::Command, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.status = Some(match result {
                    Ok(v) => parse_status(&v).ok_or_else(|| UNEXPECTED_ANSWER.to_owned()),
                    Err(e) => Err(e.user_text()),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// `agents install`: the Terminal command link and the agent skill.
    pub fn install(&mut self, cx: &mut Context<Self>) {
        self.run(AgentsAction::Install, cx);
    }

    /// `agents uninstall`: removes only what BroScribe installed.
    pub fn uninstall(&mut self, cx: &mut Context<Self>) {
        if self.can_uninstall() {
            self.run(AgentsAction::Uninstall, cx);
        }
    }

    /// The MCP switch: `mcp enable` or `mcp disable`; nothing when the flag
    /// already reads `on`.
    pub fn set_mcp(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.mcp_enabled().is_none_or(|current| current == on) {
            return;
        }
        self.run(
            if on {
                AgentsAction::EnableMcp
            } else {
                AgentsAction::DisableMcp
            },
            cx,
        );
    }

    /// `mcp enable`, which (re)registers with every detected agent CLI.
    pub fn register(&mut self, cx: &mut Context<Self>) {
        if self.mcp_enabled() == Some(true) {
            self.run(AgentsAction::EnableMcp, cx);
        }
    }

    fn run(&mut self, action: AgentsAction, cx: &mut Context<Self>) {
        if self.running.is_some() {
            return;
        }
        self.running = Some(action);
        self.copied = false;
        self.generation += 1;
        cx.notify();
        let task = self
            .state
            .read(cx)
            .run_cli(action.args(), ChildKind::Command, cx);
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            if matches!(action, AgentsAction::EnableMcp | AgentsAction::DisableMcp) {
                // The child saved `[mcp] enabled`.
                cx.update(|cx| reload_config(&state, cx)).await;
            }
            this.update(cx, |this, cx| {
                this.running = None;
                let mut stale = false;
                match action {
                    AgentsAction::Install | AgentsAction::Uninstall => {
                        this.items = Some(match result {
                            Ok(v) => {
                                let (items, status) = parse_items(&v);
                                match status {
                                    Some(status) => this.status = Some(Ok(status)),
                                    None => stale = true,
                                }
                                Ok(items)
                            }
                            Err(e) => {
                                stale = true;
                                Err(failure_text(
                                    "Couldn't change agent access.",
                                    &e.user_text(),
                                    "",
                                ))
                            }
                        });
                    }
                    AgentsAction::EnableMcp | AgentsAction::DisableMcp => {
                        this.mcp = Some(match result.map(|v| parse_mcp(&v)) {
                            Ok(Some(outcome)) => {
                                match &mut this.status {
                                    Some(Ok(s)) => s.mcp_enabled = outcome.enabled,
                                    _ => stale = true,
                                }
                                Ok(outcome)
                            }
                            Ok(None) => {
                                stale = true;
                                Err(failure_text(
                                    "Couldn't change MCP access.",
                                    UNEXPECTED_ANSWER,
                                    "",
                                ))
                            }
                            Err(e) => {
                                stale = true;
                                Err(failure_text(
                                    "Couldn't change MCP access.",
                                    &e.user_text(),
                                    "",
                                ))
                            }
                        });
                    }
                }
                if stale {
                    this.reload(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The CLI path the status reported, else the one the app runs.
    fn cli_path(&self, cx: &App) -> String {
        match &self.status {
            Some(Ok(s)) if !s.cli.is_empty() => s.cli.clone(),
            _ => self.state.read(cx).cli().cli().display().to_string(),
        }
    }

    fn installed_items(&self, s: &AgentsStatus, cx: &App) -> gpui::Div {
        let installed_tools = if s.agents.is_empty() {
            "No AI tools installed. Install Claude Code or Codex, then open this section again."
                .into()
        } else {
            s.agents
                .iter()
                .map(|agent| tool_name(&agent.name))
                .collect::<Vec<_>>()
                .join(", ")
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .when(!self.link_unsupported(), |el| {
                el.child(installed_item(
                    0,
                    "Terminal command",
                    &s.link_state,
                    &s.link_path,
                    cx,
                ))
            })
            .children(s.skills.iter().enumerate().map(|(i, skill)| {
                installed_item(
                    i + 1,
                    skill_label(&skill.target),
                    &skill.state,
                    &skill.path,
                    cx,
                )
            }))
            .child(kit::fact_row(
                "agents-detected",
                "Installed AI tools",
                installed_tools,
                cx,
            ))
    }

    fn foreign_item(&self, item: &str) -> bool {
        let Some(Ok(status)) = &self.status else {
            return false;
        };
        match item {
            "link" => status.link_state == "other",
            skill => skill.strip_prefix("skill:").is_some_and(|target| {
                status
                    .skills
                    .iter()
                    .any(|skill| skill.target == target && skill.state == "foreign")
            }),
        }
    }

    /// The last install or uninstall, one line per item.
    pub fn item_lines(&self) -> Vec<Line> {
        match &self.items {
            None => Vec::new(),
            Some(Err(e)) => vec![(true, e.clone())],
            Some(Ok(items)) => items
                .iter()
                .filter(|item| {
                    item.skipped.as_deref() != Some("unsupported")
                        && !(item.error_code.as_deref() == Some("already_exists")
                            && self.foreign_item(&item.item))
                })
                .map(|i| match (&i.error, &i.skipped) {
                    (Some(cause), _) => (
                        true,
                        failure_text(
                            &format!("Couldn't change {}.", item_label(&i.item)),
                            &sentence(cause),
                            "",
                        ),
                    ),
                    (None, Some(_)) => (
                        false,
                        format!("{}: not available on this system", item_label(&i.item)),
                    ),
                    (None, None) => (false, format!("{}: {} done.", item_label(&i.item), i.path)),
                })
                .collect(),
        }
    }

    /// The `mcp enable|disable` run here, one line per agent; before one
    /// runs, the saved result of the last one (from `agents status`).
    pub fn mcp_lines(&self) -> Vec<Line> {
        match &self.mcp {
            None => self.last_registration_lines(),
            Some(Err(e)) => vec![(true, e.clone())],
            Some(Ok(outcome)) if outcome.agents.is_empty() => vec![(
                false,
                if outcome.enabled {
                    "MCP access is on. BroScribe found no AI tools to register.".into()
                } else {
                    "MCP access is off.".into()
                },
            )],
            Some(Ok(outcome)) => outcome
                .agents
                .iter()
                .map(|a| match &a.error {
                    None => (
                        false,
                        format!(
                            "{}: {}",
                            tool_name(&a.name),
                            if outcome.enabled {
                                "registered"
                            } else {
                                "registration removed"
                            }
                        ),
                    ),
                    Some(cause) => (
                        true,
                        failure_text(
                            &if outcome.enabled {
                                format!("Couldn't register MCP access with {}.", tool_name(&a.name))
                            } else {
                                format!(
                                    "Couldn't remove the registration from {}.",
                                    tool_name(&a.name)
                                )
                            },
                            &sentence(cause),
                            "",
                        ),
                    ),
                })
                .collect(),
        }
    }

    fn last_registration_lines(&self) -> Vec<Line> {
        let Some(Ok(AgentsStatus {
            last_registration: Some(last),
            ..
        })) = &self.status
        else {
            return Vec::new();
        };
        let enabled = last.action == "enable";
        let head = if enabled {
            "Last change (turned on)"
        } else {
            "Last change (turned off)"
        };
        if last.agents.is_empty() {
            return vec![(false, format!("{head}: BroScribe found no AI tools."))];
        }
        last.agents
            .iter()
            .map(|a| {
                let name = tool_name(&a.name);
                match &a.error {
                    None => {
                        let verb = if enabled {
                            "registered"
                        } else {
                            "registration removed"
                        };
                        (false, format!("{head}: {name} {verb}"))
                    }
                    Some(cause) => (
                        true,
                        failure_text(
                            &format!("{head}: {name} didn't finish."),
                            &sentence(cause),
                            "",
                        ),
                    ),
                }
            })
            .collect()
    }
}

/// `lines` as one named node each, identified as (`prefix`, index): an
/// error is a Failed status line (announced as an alert); the others are
/// status lines of kind `ok`, or labels read in place when `ok` is `None`.
fn lines_el(prefix: &'static str, lines: Vec<Line>, ok: Option<StatusKind>, cx: &App) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .children(
            lines
                .into_iter()
                .enumerate()
                .map(|(i, (error, t))| match (error, ok) {
                    (true, _) => kit::status_line((prefix, i), StatusKind::Failed, t, cx),
                    (false, Some(kind)) => kit::status_line((prefix, i), kind, t, cx),
                    (false, None) => muted((prefix, i), t, cx),
                }),
        )
}

/// The accessible name of the generic MCP server command: what it is, then
/// the exact command Copy puts on the clipboard.
fn mcp_command_name(command: &str) -> String {
    format!("MCP server command: {command}")
}

impl Render for AgentsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let idle = self.controls_enabled();
        let cli = self.cli_path(cx);
        let generic = format!("{} mcp", shell_quote(&cli));
        let copy = generic.clone();
        div()
            .id("agents")
            .role(Role::Group)
            .aria_label("AI tools access")
            .flex()
            .flex_col()
            .gap_2()
            // Before the first answer the one line is a wait; afterwards each
            // item's state is read in place, its failures as alerts.
            .map(|el| match &self.status {
                Some(Err(cause)) => el.child(kit::retry_line(
                    "agents-status-error",
                    failure_text("Couldn't check AI tools.", cause, ""),
                    cx,
                    |this, _, cx| this.reload(cx),
                )),
                None => el.child(kit::status_line(
                    "agents-status-line-0", StatusKind::Pending, "Checking AI tools…", cx
                )),
                Some(Ok(status)) => el.child(self.installed_items(status, cx)),
            })
            .child(muted(
                "agents-skill-note",
                "The BroScribe skill teaches AI tools to search your meetings with the terminal command.",
                cx,
            ))
            .child(
                row()
                    .debug_selector(|| "agents-actions".into())
                    .child(
                        Button::new("agents-install")
                            .label(self.install_label())
                            .when(self.in_setup, Button::outline)
                            .when(!self.in_setup, Button::primary)
                            .map(|button| {
                                crate::ui::kit::disable_with_reason(
                                    button,
                                    !idle || !matches!(&self.status, Some(Ok(_))),
                                    if idle { "Wait for BroScribe to check which AI tools are installed." }
                                    else { "The current AI tools action is still running." },
                                    cx,
                                )
                            })
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.install(cx))),
                    )
                    .child(
                        Button::new("agents-uninstall")
                            .label("Remove command and skill")
                            .when(self.link_unsupported(), |button| button.label("Remove skill"))
                            .outline()
                            .map(|button| {
                                crate::ui::kit::disable_with_reason(
                                    button,
                                    !idle || !self.can_uninstall(),
                                    if idle { "There is nothing BroScribe can remove." } else { "The current AI tools action is still running." },
                                    cx,
                                )
                            })
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.uninstall(cx)),
                            ),
                    ),
            )
            .child(lines_el(
                "agents-item-line",
                self.item_lines(),
                Some(StatusKind::Done),
                cx,
            ))
            .child(
                row()
                    .debug_selector(|| "mcp-actions".into())
                    .child(text_line("agents-mcp-title", Role::Label, MCP_LABEL))
                    .when(self.mcp_enabled().is_some(), |row| row.child(self.mcp_switch(cx))),
            )
            .child(muted("agents-mcp-note", MCP_SCOPE, cx))
            .when(self.mcp_enabled() == Some(true), |el| {
                el.child(
                    Button::new("mcp-register")
                        .label(if self.running == Some(AgentsAction::EnableMcp) {
                            "Registering…"
                        } else {
                            "Register again"
                        })
                        .ghost()
                        .map(|button| kit::disable_with_reason(
                            button, !idle, "The current AI tools action is still running.", cx,
                        ))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.register(cx))),
                )
            })
            .child(lines_el(
                "agents-mcp-line",
                self.mcp_lines(),
                Some(StatusKind::Done),
                cx,
            ))
            .when_some(
                match &self.mcp {
                    Some(Ok(outcome)) => outcome.at.as_deref(),
                    _ => self.status.as_ref().and_then(|status| status.as_ref().ok())
                        .and_then(|status| status.last_registration.as_ref())
                        .map(|last| last.at.as_str()),
                }.and_then(last_change),
                |el, when| el.child(muted("agents-mcp-last-change", when, cx)),
            )
            .child(text_line("agents-mcp-generic-note", Role::Label, "Other MCP clients"))
            .child(
                div()
                    .id("mcp-generic")
                    .spoken(Role::Code, mcp_command_name(&generic))
                    .px_2()
                    .py_1()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_role(TextRole::Mono)
                    .child(generic),
            )
            .child(
                row()
                    .child(
                        Button::new("mcp-generic-copy")
                            .debug_selector(|| "mcp-generic-copy".into())
                            .label("Copy")
                            .outline()
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()));
                                this.copied = true;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("mcp-json-copy")
                            .debug_selector(|| "mcp-json-copy".into())
                            .label("Copy JSON")
                            .outline()
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(mcp_json(&cli)));
                                this.copied = true;
                                cx.notify();
                            })),
                    )
                    .when(self.copied, |el| {
                        el.child(kit::status_line(
                            "mcp-generic-copied",
                            StatusKind::Done,
                            "Copied.",
                            cx,
                        ))
                    }),
            )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::{Path, PathBuf};

    use broscribe_core::Library;
    use broscribe_core::config::Config;
    use broscribe_platform::fakes::FakePaths;
    use broscribe_platform::resolve_paths;
    use gpui::{AppContext as _, Entity, TestAppContext};

    use super::*;

    /// A synthetic `broscribe` (argv logged to `args.log`) answering
    /// `agents status|install|uninstall`, `mcp enable|disable`, `app
    /// open-at-login on|off|status` and `models list` (`models.json` in
    /// `dir` when present) with invented paths. Nothing reaches an agent CLI.
    pub(crate) fn synthetic_agents_cli(dir: &Path) -> PathBuf {
        use crate::test_support::{ps_path, ps_str, write_cli};
        let status = serde_json::json!({
            "cli": {"path": "/synthetic/BroScribe.app/Contents/MacOS/broscribe",
                    "link": {"path": "/synthetic/home/.local/bin/broscribe", "state": "missing"}},
            "agents": [{"name": "claude", "path": "/synthetic/bin/claude"},
                       {"name": "codex", "path": "/synthetic/bin/codex"}],
            "skills": [{"target": "agents", "path": "/synthetic/home/.agents/skills/broscribe",
                        "state": "missing"},
                       {"target": "claude", "path": "/synthetic/home/.claude/skills/broscribe",
                        "state": "foreign"}],
            "mcp": {"enabled": false}
        });
        let mut installed = status.clone();
        installed["cli"]["link"]["state"] = "ours".into();
        installed["skills"][0]["state"] = "current".into();
        let install = serde_json::json!({
            "results": [
                {"item": "link", "path": "/synthetic/home/.local/bin/broscribe", "ok": true,
                 "error": null},
                {"item": "skill:agents", "path": "/synthetic/home/.agents/skills/broscribe",
                 "ok": true, "error": null},
                {"item": "skill:claude", "path": "/synthetic/home/.claude/skills/broscribe",
                 "ok": false, "error": "synthetic foreign skill"}
            ],
            "status": installed
        });
        let uninstall = serde_json::json!({
            "results": [{"item": "link", "path": "/synthetic/home/.local/bin/broscribe",
                         "ok": true, "error": null}],
            "status": status
        });
        let mcp = |enabled: bool| {
            serde_json::json!({"enabled": enabled, "at": "2026-11-16T12:00:00Z", "agents": [
                {"name": "claude", "ok": true, "error": null},
                {"name": "codex", "ok": false, "error": "synthetic vendor failure"}
            ]})
        };
        for (name, v) in [
            ("status.json", status.clone()),
            ("install.json", install),
            ("uninstall.json", uninstall),
            ("mcp-enable.json", mcp(true)),
            ("mcp-disable.json", mcp(false)),
        ] {
            std::fs::write(dir.join(name), v.to_string()).unwrap();
        }
        let d = dir.display();
        let file = |name: &str| ps_path(&dir.join(name));
        let open_at_login = |on: bool| ps_str(&format!("{{\"open_at_login\":{on}}}"));
        let ps = format!(
            "fx_log_args {log};$w=fx_words 4,5;\
             if($w -ceq 'agents status'){{fx_cat {status}}}\
             elseif($w -ceq 'agents install'){{fx_cat {install}}}\
             elseif($w -ceq 'agents uninstall'){{fx_cat {uninstall}}}\
             elseif($w -ceq 'mcp enable'){{fx_cat {enable}}}\
             elseif($w -ceq 'mcp disable'){{fx_cat {disable}}}\
             elseif($w -ceq 'app open-at-login'){{$s=fx_arg 6;\
             if($s -ceq 'on'){{fx_echo {on}}}elseif($s -ceq 'off'){{fx_echo {off}}}else{{fx_echo {on}}}}}\
             elseif($w -ceq 'models list'){{if(fx_exists {models}){{fx_cat {models}}}\
             else{{fx_echo {no_models}}}}}\
             else{{fx_echo '{{}}'}}",
            log = file("args.log"),
            status = file("status.json"),
            install = file("install.json"),
            uninstall = file("uninstall.json"),
            enable = file("mcp-enable.json"),
            disable = file("mcp-disable.json"),
            on = open_at_login(true),
            off = open_at_login(false),
            models = file("models.json"),
            no_models = ps_str(r#"{"models":[],"profiles":[]}"#),
        );
        write_cli(
            &dir.join("synthetic-cli"),
            &format!(
                "printf '%s\\n' \"$*\" >> '{d}/args.log'\n\
                 case \"$4 $5\" in\n\
                 'agents status') cat '{d}/status.json' ;;\n\
                 'agents install') cat '{d}/install.json' ;;\n\
                 'agents uninstall') cat '{d}/uninstall.json' ;;\n\
                 'mcp enable') cat '{d}/mcp-enable.json' ;;\n\
                 'mcp disable') cat '{d}/mcp-disable.json' ;;\n\
                 'app open-at-login')\n\
                   case \"$6\" in on) echo '{{\"open_at_login\":true}}' ;; \
                   off) echo '{{\"open_at_login\":false}}' ;; \
                   *) echo '{{\"open_at_login\":true}}' ;; esac ;;\n\
                 'models list') if [ -f '{d}/models.json' ]; then cat '{d}/models.json'; \
                   else echo '{{\"models\":[],\"profiles\":[]}}'; fi ;;\n\
                 *) echo '{{}}' ;;\n\
                 esac\n"
            ),
            &ps,
        )
    }

    pub(crate) fn args_log(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("args.log")).unwrap_or_default()
    }

    /// The commands the synthetic CLI received, without the global flags.
    pub(crate) fn commands(dir: &Path) -> Vec<String> {
        args_log(dir)
            .lines()
            .filter_map(|l| l.split_once(" --json ").map(|(_, c)| c.to_owned()))
            .collect()
    }

    pub(crate) fn agents_state(cx: &mut TestAppContext, dir: &Path) -> Entity<AppState> {
        let paths = resolve_paths(&FakePaths::new(dir)).unwrap();
        let cli = synthetic_agents_cli(dir);
        let library = Library::open(dir.join("lib")).unwrap();
        cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(paths, Config::default(), library, cli),
                cx,
            )
        })
    }

    #[test]
    fn answers_parse_leniently() {
        assert_eq!(parse_status(&serde_json::json!({"providers": []})), None);
        let s = parse_status(&serde_json::json!({
            "cli": {"path": "/x/broscribe", "link": {"path": "/l", "state": "ours"}},
            "agents": [{"name": "codex", "path": "/b/codex"}, {"path": "/no-name"}],
            "skills": [{"target": "agents", "path": "/s", "state": "outdated"}],
            "mcp": {"enabled": true}
        }))
        .unwrap();
        assert_eq!(s.link_state, "ours");
        assert_eq!(s.agents.len(), 1);
        assert_eq!(s.skills[0].state, "outdated");
        assert!(s.mcp_enabled);
        assert_eq!(s.last_registration, None, "no `mcp.last`");
        let s = parse_status(&serde_json::json!({
            "cli": {"path": "/x/broscribe", "link": {"path": "/l", "state": "ours"}},
            "mcp": {"enabled": true, "last": {"action": "disable", "at": "2026-11-15T12:00:00Z",
                    "agents": [{"name": "gemini", "ok": false, "error": "x"}, {"ok": true}]}}
        }))
        .unwrap();
        let last = s.last_registration.unwrap();
        assert_eq!(last.action, "disable");
        assert_eq!(last.at, "2026-11-15T12:00:00Z");
        assert_eq!(
            last.agents,
            [AgentResult {
                name: "gemini".into(),
                error: Some("x".into())
            }]
        );
        let (items, status) = parse_items(&serde_json::json!({"results": [
            {"item": "link", "path": "/l", "ok": true, "error": null},
            {"item": "skill:claude", "path": "/c", "ok": false, "error": "x"},
            {"item": "skill:agents", "path": "/a", "ok": false, "error": null},
            {"item": "link", "path": "/l", "ok": true, "skipped": "unsupported", "error": null}
        ]}));
        assert_eq!(status, None);
        assert_eq!(items[0].error, None);
        assert_eq!(items[0].skipped, None);
        assert_eq!(items[1].error.as_deref(), Some("x"));
        assert!(
            items[2].error.is_some(),
            "a failure without text is still one"
        );
        assert_eq!(items[3].error, None, "a skipped item is not a failure");
        assert_eq!(items[3].skipped.as_deref(), Some("unsupported"));
        assert_eq!(parse_mcp(&serde_json::json!({"agents": []})), None);
        let m = parse_mcp(&serde_json::json!({"enabled": false, "agents": [
            {"name": "gemini", "ok": true, "error": null}]}))
        .unwrap();
        assert!(!m.enabled && m.agents[0].error.is_none());
        assert_eq!(
            shell_quote("/Apps/BroScribe.app/x"),
            "/Apps/BroScribe.app/x"
        );
        assert_eq!(shell_quote("/a b/it's"), r"'/a b/it'\''s'");
    }

    fn agents_window<'a>(
        cx: &'a mut TestAppContext,
        dir: &Path,
    ) -> (Entity<AgentsView>, &'a mut gpui::VisualTestContext) {
        let state = agents_state(cx, dir);
        cx.update(gpui::init);
        let (view, cx) = cx.add_window_view(move |_, _| AgentsView::new(state));
        view.update(cx, |this, cx| this.reload(cx));
        cx.run_until_parked();
        (view, cx)
    }

    fn has_error(lines: &[Line], needle: &str) -> bool {
        lines.iter().any(|(e, t)| *e && t.contains(needle))
    }

    fn has_ok(lines: &[Line], needle: &str) -> bool {
        lines.iter().any(|(e, t)| !*e && t.contains(needle))
    }

    #[gpui::test]
    fn install_and_uninstall_run_the_cli_and_show_each_item(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = agents_window(cx, dir.path());
        assert_eq!(commands(dir.path()), ["agents status"]);
        view.read_with(cx, |this, _| {
            let s = this.status().unwrap().as_ref().unwrap();
            assert_eq!(s.link_state, "missing");
            assert_eq!(s.agents[0].name, "claude");
            assert_eq!(s.skills[1].state, "foreign");
            assert!(!this.can_uninstall());
        });
        // The shared controls are drawn.
        cx.simulate_resize(gpui::size(gpui::px(800.), gpui::px(600.)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(cx.debug_bounds("agents-actions").is_some());
        assert!(cx.debug_bounds("mcp-actions").is_some());

        // Children run on the background executor: until it is driven, the
        // install is still running.
        view.update(cx, |this, cx| {
            this.install(cx);
            assert_eq!(this.running(), Some(AgentsAction::Install));
            assert!(!this.controls_enabled());
            // Nothing else starts while it runs.
            this.uninstall(cx);
            this.set_mcp(true, cx);
            this.register(cx);
            this.reload(cx);
            assert_eq!(this.running(), Some(AgentsAction::Install));
        });
        cx.run_until_parked();
        assert_eq!(commands(dir.path()), ["agents status", "agents install"]);
        view.read_with(cx, |this, _| {
            assert!(this.controls_enabled());
            let s = this.status().unwrap().as_ref().unwrap();
            assert_eq!(s.link_state, "ours", "the answer's status is taken in");
            let items = this.item_lines();
            assert_eq!(items.len(), 3, "{items:?}");
            assert!(has_ok(&items, "/synthetic/home/.local/bin/broscribe"));
            assert!(has_ok(&items, "/synthetic/home/.agents/skills/broscribe"));
            // The CLI's words as a sentence after what failed (WRT-08).
            assert!(has_error(&items, ". Synthetic foreign skill."), "{items:?}");
        });
        view.update(cx, |this, cx| this.uninstall(cx));
        cx.run_until_parked();
        assert_eq!(
            commands(dir.path()),
            ["agents status", "agents install", "agents uninstall"]
        );
        view.read_with(cx, |this, _| {
            assert_eq!(
                this.status().unwrap().as_ref().unwrap().link_state,
                "missing"
            );
            assert_eq!(this.item_lines().len(), 1);
        });
    }

    #[gpui::test]
    fn mcp_switch_and_register_run_the_cli_and_show_each_agent(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = agents_window(cx, dir.path());
        view.update(cx, |this, cx| {
            assert_eq!(this.mcp_enabled(), Some(false));
            // Already off: nothing runs.
            this.set_mcp(false, cx);
            assert_eq!(this.running(), None);
            this.set_mcp(true, cx);
            assert_eq!(this.running(), Some(AgentsAction::EnableMcp));
            assert!(!this.controls_enabled());
        });
        cx.run_until_parked();
        view.read_with(cx, |this, _| {
            assert_eq!(this.mcp_enabled(), Some(true));
            let lines = this.mcp_lines();
            assert!(has_ok(&lines, "Claude Code"), "{lines:?}");
            assert!(has_error(&lines, "Synthetic vendor failure."), "{lines:?}");
        });
        // Registering again always runs `mcp enable`.
        view.update(cx, |this, cx| this.register(cx));
        cx.run_until_parked();
        view.update(cx, |this, cx| this.set_mcp(false, cx));
        cx.run_until_parked();
        assert_eq!(
            commands(dir.path()),
            ["agents status", "mcp enable", "mcp enable", "mcp disable"]
        );
        view.read_with(cx, |this, _| {
            assert_eq!(this.mcp_enabled(), Some(false));
            assert!(has_ok(&this.mcp_lines(), "Claude Code"));
            assert!(this.controls_enabled());
        });
    }

    /// SET-05 and §4.2 Agent access: every line of the section is a named
    /// node. The wait before the first answer is a status; each item's state
    /// is a label read in place (a foreign item a status); a finished action's
    /// lines are statuses and its failures alerts, one per item and agent.
    #[gpui::test]
    fn agent_access_lines_are_named_by_role(cx: &mut TestAppContext) {
        use crate::ui::spoken_log::{self, assert_distinct, node};

        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = agents_window(cx, dir.path());
        let named = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                view.update(cx, |this, cx| {
                    spoken_log::while_building(|| Render::render(this, window, cx)).1
                })
            })
        };

        view.update(cx, |this, _| this.status = None);
        let waiting = named(cx);
        assert_eq!(
            node(&waiting, "agents-status-line-0"),
            (Role::Status, "Checking AI tools…")
        );

        view.update(cx, |this, cx| this.reload(cx));
        cx.run_until_parked();
        let loaded = named(cx);
        assert_distinct(&loaded, &[]);
        assert_eq!(node(&loaded, "agents-mcp-title"), (Role::Label, MCP_LABEL));
        assert_eq!(
            node(&loaded, "agents-installed-state-0"),
            (Role::Status, "Terminal command · Not installed")
        );
        assert_eq!(
            node(&loaded, "agents-installed-path-0"),
            (Role::Label, "/synthetic/home/.local/bin/broscribe")
        );
        let state = loaded
            .iter()
            .position(|(id, ..)| id == "agents-installed-state-0")
            .unwrap();
        let path = loaded
            .iter()
            .position(|(id, ..)| id == "agents-installed-path-0")
            .unwrap();
        assert!(state < path);
        assert_eq!(
            node(&loaded, "agents-installed-state-2"),
            (
                Role::Status,
                "Skill for Claude Code · Another program uses this name; BroScribe leaves it alone."
            )
        );
        assert_eq!(
            node(&loaded, "agents-detected").1,
            "Installed AI tools: Claude Code, Codex"
        );
        view.update(cx, |this, cx| this.install(cx));
        cx.run_until_parked();
        let installed = named(cx);
        assert_distinct(&installed, &[]);
        assert_eq!(
            node(&installed, "agents-item-line-0"),
            (
                Role::Status,
                "Terminal command: /synthetic/home/.local/bin/broscribe done."
            )
        );
        assert_eq!(
            node(&installed, "agents-item-line-2"),
            (
                Role::Alert,
                "Couldn't change Claude Code skill. Synthetic foreign skill."
            )
        );

        view.update(cx, |this, cx| this.set_mcp(true, cx));
        cx.run_until_parked();
        let registered = named(cx);
        assert_eq!(
            node(&registered, "agents-mcp-line-0"),
            (Role::Status, "Claude Code: registered")
        );
        assert_eq!(
            node(&registered, "agents-mcp-line-1"),
            (
                Role::Alert,
                "Couldn't register MCP access with Codex. Synthetic vendor failure."
            )
        );
    }

    /// SET-05: the generic MCP server command is read by the exact text Copy
    /// puts on the clipboard, quoting included when the CLI's path has a space.
    #[gpui::test]
    fn leaving_a_foreign_skill_alone_uses_the_attention_row(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = agents_window(cx, dir.path());
        let reply = dir.path().join("install.json");
        let mut result: Value = serde_json::from_slice(&std::fs::read(&reply).unwrap()).unwrap();
        result["results"][2]["error_code"] = "already_exists".into();
        std::fs::write(reply, result.to_string()).unwrap();
        view.update(cx, |this, cx| this.install(cx));
        cx.run_until_parked();
        let tree = cx.update(crate::ui::a11y_tree::draw_and_read);
        let attention = &tree.nodes[tree.find("agents-installed-state-2")];
        assert_eq!(attention.role, "Status");
        assert!(attention.name.ends_with("BroScribe leaves it alone."));
        assert!(tree.nodes.iter().all(|node| node.role != "Alert"));
    }

    #[gpui::test]
    fn the_mcp_command_reads_as_what_copy_copies(cx: &mut TestAppContext) {
        use crate::ui::spoken_log::{self, assert_distinct, node};

        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = agents_window(cx, dir.path());
        let spaced = "/Applications/Synthetic Scribe.app/Contents/MacOS/broscribe";
        view.update(cx, |this, cx| {
            let Some(Ok(status)) = &mut this.status else {
                panic!("the synthetic status is read");
            };
            status.cli = spaced.into();
            cx.notify();
        });
        let named = cx.update(|window, cx| {
            view.update(cx, |this, cx| {
                spoken_log::while_building(|| Render::render(this, window, cx)).1
            })
        });
        assert_distinct(&named, &[]);
        let (role, name) = node(&named, "mcp-generic");
        assert_eq!(role, Role::Code);
        let name = name.to_owned();

        cx.simulate_resize(gpui::size(gpui::px(1200.), gpui::px(2400.)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let copy = cx.debug_bounds("mcp-generic-copy").unwrap();
        cx.simulate_click(copy.center(), gpui::Modifiers::none());
        let copied = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .expect("Copy wrote the command");
        assert_eq!(copied, format!("'{spaced}' mcp"));
        assert_eq!(name, format!("MCP server command: {copied}"));
        let json_button = cx.debug_bounds("mcp-json-copy").unwrap();
        cx.simulate_click(json_button.center(), gpui::Modifiers::none());
        let json = cx
            .read_from_clipboard()
            .and_then(|item| item.text())
            .unwrap();
        let decoded: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            decoded["mcpServers"]["broscribe"],
            serde_json::json!({"command": spaced, "args": ["mcp"]})
        );
    }

    #[gpui::test]
    fn mcp_switch_and_disclosure_are_in_the_accessibility_tree(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = agents_window(cx, dir.path());
        let read =
            |cx: &mut gpui::VisualTestContext| cx.update(crate::ui::a11y_tree::draw_and_read);
        let tree = read(cx);
        let switch = &tree.nodes[tree.find("mcp-switch")];
        assert_eq!(
            (switch.role.as_str(), switch.name.as_str()),
            ("Switch", MCP_LABEL)
        );
        assert!(
            tree.nodes[tree.find("agents-mcp-note")]
                .name
                .contains("transcripts, summaries, people and calendar details")
        );
        assert!(
            tree.nodes
                .iter()
                .all(|node| node.element_id.as_deref() != Some("mcp-register"))
        );
        view.update(cx, |this, cx| this.set_mcp(true, cx));
        cx.run_until_parked();
        let tree = read(cx);
        assert_eq!(tree.nodes[tree.find("mcp-register")].name, "Register again");
    }

    #[gpui::test]
    fn the_saved_last_registration_is_shown_until_one_runs(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = agents_window(cx, dir.path());
        view.read_with(cx, |this, _| assert!(this.mcp_lines().is_empty()));
        let mut status: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("status.json")).unwrap())
                .unwrap();
        status["mcp"]["last"] = serde_json::json!({
            "action": "enable", "at": "2026-11-15T12:00:00Z",
            "agents": [{"name": "claude", "ok": true, "error": null},
                       {"name": "codex", "ok": false, "error": "synthetic vendor outage"}]
        });
        std::fs::write(dir.path().join("status.json"), status.to_string()).unwrap();
        view.update(cx, |this, cx| this.reload(cx));
        cx.run_until_parked();
        view.read_with(cx, |this, _| {
            let lines = this.mcp_lines();
            assert!(
                has_ok(&lines, "Last change (turned on): Claude Code registered"),
                "{lines:?}"
            );
            assert!(
                has_error(
                    &lines,
                    "Last change (turned on): Codex didn't finish. Synthetic vendor outage."
                ),
                "{lines:?}"
            );
        });
        let tree = cx.update(crate::ui::a11y_tree::draw_and_read);
        assert_eq!(
            tree.nodes[tree.find("agents-mcp-last-change")].name,
            last_change("2026-11-15T12:00:00Z").unwrap()
        );
        // An explicit change here replaces the previous registration result.
        view.update(cx, |this, cx| this.set_mcp(true, cx));
        cx.run_until_parked();
        view.read_with(cx, |this, _| {
            let lines = this.mcp_lines();
            assert!(lines.iter().all(|(_, t)| !t.contains("Last change")));
            assert!(has_error(&lines, "Synthetic vendor failure."), "{lines:?}");
        });
    }

    #[gpui::test]
    fn a_failed_command_is_shown_and_the_status_reloaded(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = agents_window(cx, dir.path());
        // The CLI answers `mcp enable` without its flag.
        std::fs::write(dir.path().join("mcp-enable.json"), "{}").unwrap();
        view.update(cx, |this, cx| this.set_mcp(true, cx));
        cx.run_until_parked();
        assert_eq!(
            commands(dir.path()),
            ["agents status", "mcp enable", "agents status"]
        );
        view.read_with(cx, |this, _| {
            assert!(this.mcp_lines().iter().all(|(error, _)| *error));
            assert!(this.controls_enabled());
        });
    }

    /// Off macOS and Linux the CLI reports no link: status and install say
    /// the Terminal command is not available instead of a path or "done".
    #[gpui::test]
    fn an_unsupported_link_is_hidden_and_only_owned_items_enable_remove(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = agents_window(cx, dir.path());
        let read = |name: &str| -> Value {
            serde_json::from_slice(&std::fs::read(dir.path().join(name)).unwrap()).unwrap()
        };
        let mut status = read("status.json");
        status["cli"]["link"]["state"] = "unsupported".into();
        std::fs::write(dir.path().join("status.json"), status.to_string()).unwrap();
        let mut install = read("install.json");
        install["results"][0] = serde_json::json!({"item": "link",
            "path": "/synthetic/home/.local/bin/broscribe",
            "ok": true, "skipped": "unsupported", "error": null});
        install["status"] = status;
        std::fs::write(dir.path().join("install.json"), install.to_string()).unwrap();
        view.update(cx, |this, cx| this.reload(cx));
        cx.run_until_parked();

        let tree = cx.update(crate::ui::a11y_tree::draw_and_read);
        assert_eq!(
            tree.nodes[tree.find("agents-install")].name,
            "Install skill"
        );
        assert!(
            tree.nodes
                .iter()
                .all(|node| node.element_id.as_deref() != Some("agents-installed-state-0"))
        );
        view.update(cx, |this, cx| {
            assert!(!this.can_uninstall());
            this.uninstall(cx);
        });
        assert!(
            !commands(dir.path())
                .iter()
                .any(|command| command == "agents uninstall")
        );
        view.update(cx, |this, cx| this.install(cx));
        cx.run_until_parked();
        view.read_with(cx, |this, _| {
            let items = this.item_lines();
            assert!(
                items
                    .iter()
                    .all(|(_, line)| !line.contains("Terminal command")),
                "{items:?}"
            );
            assert!(has_ok(&items, "/synthetic/home/.agents/skills/broscribe"));
            assert!(!this.can_uninstall());
            assert_eq!(this.install_label(), "Install skill");
        });
    }
}
