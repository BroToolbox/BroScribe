//! Software updates (SH23): the signed, opt-in updater.
//!
//! Automatic part: [`start_automatic_checks`] runs `update check` shortly
//! after the app starts and then every [`CHECK_PERIOD`], through the app's
//! [`CliRunner`], whether or not this view is shown. Each cycle reads the
//! configuration again and starts nothing (no child, no network) unless
//! updates are on with a feed and trusted keys. A check only fetches and
//! verifies the signed envelope: it never downloads or installs. Its result
//! lives in the app-wide [`UpdateNotices`], which this view and the rest of
//! the app read.
//!
//! Manual part: downloading, installing, keeping, rolling back and
//! recovering happen only from this view (or the CLI), each as a
//! `broscribe --json update …` child on the background executor; no updater
//! code runs in the GUI process. Every child goes through the app's shared
//! [`CliRunner`] transport: stdout and stderr are drained at once and
//! bounded, so a large answer completes or fails as too large and never
//! leaves the view loading. Download progress comes from the CLI's
//! `{"progress":{…}}` stderr lines, coalesced to the newest value. Only
//! checking and downloading can be cancelled (nothing is replaced before an
//! install); install, rollback, keep and recover run to their own result.
//! Install and rollback need an
//! explicit confirmation naming the exact version, build, target and signer
//! the CLI reported; confirming sends exactly that update back
//! (`--expectation-stdin`), and the CLI refuses (`update_changed`) when the
//! pending update is no longer that one, so the view reloads and asks
//! again: there is no unbound `--yes`. The settings toggle saves the
//! `[updates]` preference through `update settings set --enabled --expect-revision`,
//! so a change made elsewhere is never overwritten. Feeds and keys are set
//! with the CLI (`update settings set`); the app never shows the feed URL or
//! key material. Every line, status, alert and the confirmation's
//! disclosure is an identified accessible node carrying its actual words.

use std::ffi::OsString;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use broscribe_core::config::{Config, UpdateSettings};
use broscribe_platform::HostPlatform;
use broscribe_platform::update::{PayloadIdentity, SignerIdentity};
use broscribe_platform_native::NativePlatform;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::component::button::Button;
use gpui::component::switch::Switch;
use gpui::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Div, Entity, Global, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Role, Stateful, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, WeakEntity, Window, div,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::jobs::{ChildKind, CliError, CliRunner, failure_text};
use crate::state::AppState;
use crate::ui::kit::{self, ConfirmSpec, StatusKind, confirm_line};
use crate::ui::type_scale::{TextRole, TextRoleExt as _};
use crate::ui::{text_line, title};

pub(crate) const RECORDING_HELD: &str =
    "Recording is off until you keep or roll back the BroScribe update.";
const ADMIN_COMMAND: &str = "broscribe update settings set --spec-stdin";
const AUTO_LABEL: &str = "Check for updates automatically";
const AUTO_HELP: &str = "Checks a minute after BroScribe starts and every 12 hours. Downloading and installing always wait for you.";

/// A release as the CLI reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub build: u64,
    pub target: String,
}

impl Release {
    fn from_value(value: &Value) -> Option<Self> {
        Some(Self {
            version: value["version"].as_str()?.to_owned(),
            build: value["build"].as_u64()?,
            target: value["target"].as_str().unwrap_or_default().to_owned(),
        })
    }

    pub fn label(&self) -> String {
        format!("{} (build {})", self.version, self.build)
    }
}

impl From<&PayloadIdentity> for Release {
    fn from(identity: &PayloadIdentity) -> Self {
        Self {
            version: identity.version.clone(),
            build: identity.build,
            target: identity.target.clone(),
        }
    }
}

/// One pending update exactly as `update status` reports it: the
/// installation it replaces, the update (its target included) and the
/// signer the update was verified against. A confirmation names it and
/// `--expectation-stdin` sends it back unchanged.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expectation {
    pub from: PayloadIdentity,
    pub to: PayloadIdentity,
    pub signer: SignerIdentity,
}

impl Expectation {
    fn from_update(update: &Value) -> Option<Self> {
        Some(Self {
            from: PayloadIdentity::deserialize(&update["from"]).ok()?,
            to: PayloadIdentity::deserialize(&update["to"]).ok()?,
            signer: SignerIdentity::deserialize(&update["signer"]).ok()?,
        })
    }

    fn signer_text(&self) -> String {
        match &self.signer.signer {
            Some(signer) => format!("Signed by {signer}"),
            None => "Verified against the signed release manifest".into(),
        }
    }
}

/// The updater's state from `update status`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateState {
    Off,
    Unavailable { code: String, message: String },
    Idle,
    Staged(Expectation),
    Interrupted { from: Release, to: Release },
    AwaitingConfirmation(Expectation),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateSnapshot {
    pub state: UpdateState,
    pub enabled: bool,
    /// A feed and at least one trusted key are set.
    pub configured: bool,
    pub signer_pin: Option<String>,
    /// The installation as the platform qualified it; `None` when it cannot
    /// be updated (development build, unsupported location).
    pub installed: Option<Release>,
    pub agent_running: bool,
    /// The agent holds an install admission: nothing can record.
    pub admission_held: bool,
    pub admission_in_progress: bool,
    /// A replacement step was handed to the update runner.
    pub handed_off: bool,
    /// The update runner's last result, in words.
    pub runner: Option<String>,
}

pub fn parse_status(value: &Value) -> UpdateSnapshot {
    let update = &value["update"];
    let release = |key: &str| Release::from_value(&update[key]);
    let state = match update["state"].as_str() {
        Some("idle") => UpdateState::Idle,
        // Never confirmable without its exact identities and signer.
        Some("staged") => {
            Expectation::from_update(update).map_or_else(unreadable, UpdateState::Staged)
        }
        Some("interrupted") => match (release("from"), release("to")) {
            (Some(from), Some(to)) => UpdateState::Interrupted { from, to },
            _ => unreadable(),
        },
        Some("awaiting_confirmation") => Expectation::from_update(update)
            .map_or_else(unreadable, UpdateState::AwaitingConfirmation),
        Some("unavailable") => UpdateState::Unavailable {
            code: update["code"].as_str().unwrap_or("internal").to_owned(),
            message: update["message"].as_str().unwrap_or_default().to_owned(),
        },
        Some("off") => UpdateState::Off,
        _ => unreadable(),
    };
    let settings = &value["settings"];
    let runner = value["runner"]["outcome"]
        .as_object()
        .map(|_| runner_text(&value["runner"]));
    UpdateSnapshot {
        state,
        enabled: settings["enabled"].as_bool().unwrap_or(false),
        configured: settings["feed_configured"].as_bool().unwrap_or(false)
            && settings["trusted_keys"].as_u64().unwrap_or(0) > 0,
        signer_pin: settings["signer"].as_str().map(str::to_owned),
        installed: Release::from_value(&value["installed"]),
        agent_running: value["agent"]["running"].as_bool().unwrap_or(false),
        admission_held: value["agent"]["install"].is_object(),
        admission_in_progress: value["agent"]["install"]["durable"].as_bool() == Some(false),
        handed_off: !value["handed_off"].is_null(),
        runner,
    }
}

fn unreadable() -> UpdateState {
    UpdateState::Unavailable {
        code: "bad_output".into(),
        message: "the updater's state couldn't be read".into(),
    }
}

fn runner_text(report: &Value) -> String {
    let outcome = &report["outcome"];
    let mut text = match outcome["state"].as_str() {
        Some("installed") => Release::from_value(&outcome["to"]).map_or_else(
            || "The update was installed.".to_owned(),
            |to| format!("BroScribe {} was installed.", to.label()),
        ),
        Some("rolled_back") => Release::from_value(&outcome["restored"]).map_or_else(
            || "The previous version was restored.".to_owned(),
            |restored| format!("BroScribe {} was restored.", restored.label()),
        ),
        Some("unchanged") => "Nothing needed replacing.".to_owned(),
        _ => format!(
            "The update did not finish: {}",
            outcome["message"].as_str().unwrap_or("unknown failure")
        ),
    };
    if report["agent_start_error"].is_string() {
        text.push_str(" BroScribe Agent didn't restart. Open BroScribe to start it.");
    }
    text
}

/// A newer release offered by `update check`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub release: Release,
    pub size: u64,
    pub release_notes: Option<String>,
    pub installed: Release,
    pub signer_pin: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckResult {
    UpToDate { installed: Release },
    NoVerifiedUpdate,
    Available(Offer),
}

pub fn parse_check(value: &Value) -> Option<CheckResult> {
    let installed = Release::from_value(&value["installed"])?;
    match value["state"].as_str()? {
        "up_to_date" => Some(CheckResult::UpToDate { installed }),
        "no_verified_update" => Some(CheckResult::NoVerifiedUpdate),
        "available" => Some(CheckResult::Available(Offer {
            release: Release::from_value(value)?,
            size: value["size"].as_u64()?,
            release_notes: value["release_notes"]
                .as_str()
                .filter(|notes| !notes.trim().is_empty())
                .map(str::to_owned),
            installed,
            signer_pin: value["signer_pin"].as_str().map(str::to_owned),
        })),
        _ => None,
    }
}

/// `{"progress":{"step":"download","bytes":N,"total":M}}`, the line
/// `update download --json` writes to stderr.
pub fn parse_progress(line: &str) -> Option<(u64, u64)> {
    let value: Value = serde_json::from_str(line).ok()?;
    let progress = value.get("progress")?;
    if progress["step"] != "download" {
        return None;
    }
    Some((progress["bytes"].as_u64()?, progress["total"].as_u64()?))
}

/// One `update` command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Status,
    Check,
    Download,
    Discard,
    Install,
    Confirm,
    Rollback,
    Recover,
    SettingsSet { enabled: bool, expect_revision: u64 },
}

impl Step {
    /// Arguments after `broscribe --json`.
    pub fn args(self) -> Vec<OsString> {
        let words: &[&str] = match self {
            Step::Status => &["update", "status"],
            Step::Check => &["update", "check"],
            Step::Download => &["update", "download"],
            Step::Discard => &["update", "discard"],
            Step::Install => &["update", "install", "--expectation-stdin"],
            Step::Confirm => &["update", "confirm"],
            Step::Rollback => &["update", "rollback", "--expectation-stdin"],
            Step::Recover => &["update", "recover"],
            Step::SettingsSet {
                enabled,
                expect_revision,
            } => {
                let mut args: Vec<OsString> = [
                    "update",
                    "settings",
                    "set",
                    "--enabled",
                    if enabled { "true" } else { "false" },
                    "--expect-revision",
                ]
                .map(OsString::from)
                .into();
                args.push(expect_revision.to_string().into());
                return args;
            }
        };
        words.iter().map(OsString::from).collect()
    }

    /// Only steps that replace nothing can be stopped midway.
    pub fn cancellable(self) -> bool {
        matches!(self, Step::Check | Step::Download)
    }

    fn running_text(self) -> &'static str {
        match self {
            Step::Status => "Reading the update state…",
            Step::Check => "Checking for updates…",
            Step::Download => "Downloading…",
            Step::Discard => "Removing the download…",
            Step::Install => {
                "Installing… Recording stays off until you keep this version or roll back."
            }
            Step::Confirm => "Keeping this version…",
            Step::Rollback => "Restoring the previous version…",
            Step::Recover => "Recovering…",
            Step::SettingsSet { .. } => "Saving…",
        }
    }
}

/// A replacement waiting for the user's explicit confirmation: exactly the
/// pending update shown, which confirming sends to the CLI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pending {
    Install(Expectation),
    Rollback(Expectation),
}

impl Pending {
    fn step(&self) -> (Step, &Expectation) {
        match self {
            Pending::Install(expected) => (Step::Install, expected),
            Pending::Rollback(expected) => (Step::Rollback, expected),
        }
    }
}

fn architecture(target: &str) -> Option<&'static str> {
    match target.split('-').next()? {
        "aarch64" if target.ends_with("-darwin") => Some("Apple silicon"),
        "x86_64" if target.ends_with("-darwin") => Some("Intel Macs"),
        "aarch64" => Some("ARM64"),
        "x86_64" => Some("64-bit Intel and AMD"),
        _ => None,
    }
}

fn replacement_spec(pending: &Pending) -> ConfirmSpec {
    let (update, install) = match pending {
        Pending::Install(update) => (update, true),
        Pending::Rollback(update) => (update, false),
    };
    let from = Release::from(&update.from);
    let to = Release::from(&update.to);
    let architecture_label = |release: &Release| {
        if from.target != to.target {
            architecture(&release.target).map_or(String::new(), |arch| format!(" for {arch}"))
        } else {
            String::new()
        }
    };
    let title = if install {
        let version = if from.version == to.version {
            to.label()
        } else {
            to.version.clone()
        };
        format!("Install BroScribe {version}{}?", architecture_label(&to))
    } else {
        format!(
            "Roll back to BroScribe {}{}?",
            from.label(),
            architecture_label(&from)
        )
    };
    let mut spec = ConfirmSpec::new(
        confirm_line("updates-confirm-title", title),
        "updates-confirm",
        "updates-confirm-actions",
        "updates-no",
        confirm_line(
            "updates-yes",
            if install {
                "Install and restart"
            } else {
                "Roll back"
            },
        ),
    );
    spec.primary_action = true;
    spec.irreversible = false;
    if install {
        spec.warning = Some(confirm_line(
            "updates-restart-warning",
            "BroScribe and BroScribe Agent restart. Recording stays off until you keep the new version or roll back.",
        ));
        spec.changes.push(confirm_line(
            "updates-replacement",
            format!(
                "Replaces {}{}. {}. The signature is checked again before anything is replaced.",
                from.label(),
                architecture_label(&from),
                update.signer_text()
            ),
        ));
    } else {
        spec.warning = Some(confirm_line(
            "updates-restart-warning",
            "BroScribe Agent restarts on the previous version.",
        ));
        spec.changes.push(confirm_line(
            "updates-replacement",
            format!(
                "Replaces {}{}. {}.",
                to.label(),
                architecture_label(&to),
                update.signer_text()
            ),
        ));
    }
    spec
}

/// Words for a refused or failed step: its own sentence for a known code,
/// otherwise "Couldn't continue the update. {Cause}." without the code.
pub fn refusal_text(error: &CliError) -> String {
    known_refusal(&error.code).map_or_else(
        || failure_text("Couldn't continue the update.", &error.user_text(), ""),
        str::to_owned,
    )
}

/// The sentence of a refusal whose code the update view knows.
fn update_location_instructions(host: HostPlatform) -> &'static str {
    match host {
        HostPlatform::MacOs => {
            "BroScribe can't update itself where it's installed. Move it to Applications, then try again."
        }
        HostPlatform::Windows | HostPlatform::Linux => {
            "BroScribe can't update itself where it's installed. Reinstall it in a folder you can write to, then try again."
        }
    }
}

fn known_refusal(code: &str) -> Option<&'static str> {
    Some(match code {
        "recording_active" => {
            "BroScribe is recording, or a recording isn't saved yet. Updates install after it is \
             saved."
        }
        "update_installing" | "busy" | "update_busy" => {
            "Another update step is in progress. Try again when it has finished."
        }
        "daemon_unavailable" => {
            "BroScribe Agent isn't running. Start it and try again: it \
                                 keeps recording off while BroScribe is replaced."
        }
        "helper_outdated" => {
            "BroScribe Agent and the app are different versions. Restart the installed \
                              BroScribe, then choose Finish recovery."
        }
        "update_agent_unproven" => {
            "BroScribe Agent hasn't confirmed the installed build. \
                                    Restart the installed BroScribe and choose Finish recovery. \
                                    Recording stays off until verification succeeds."
        }
        "not_configured" => "Automatic updates are off or not set up.",
        "update_signature_invalid" | "update_signer_mismatch" => {
            "The update isn't signed by the trusted publisher. Nothing was installed."
        }
        "update_location_unsupported" => {
            update_location_instructions(NativePlatform::host_platform())
        }
        "trust_store_unavailable" => {
            "The system's certificates couldn't be read, so updates can't be downloaded."
        }
        "config_stale" => "The update settings changed elsewhere; they were reloaded.",
        "update_changed" => {
            "The pending update changed since you confirmed it, so nothing was \
                             replaced. Check the details shown now and confirm again."
        }
        "update_foreign_stage" => {
            "Another BroScribe data folder has an update pending for this \
                                   installation. Finish or remove it there first."
        }
        "cancelled" => "Cancelled. Nothing was installed.",
        _ => return None,
    })
}

/// The automatic check's failure: a known refusal after "The last check
/// didn't finish.", otherwise "Couldn't check for updates. {Cause}.".
pub fn last_check_failure(error: &CliError) -> String {
    match known_refusal(&error.code) {
        Some(refusal) => format!("The last check didn't finish. {refusal}"),
        None => failure_text("Couldn't check for updates.", &error.user_text(), ""),
    }
}

// ---- automatic checks ----

/// Wait after the app starts before the first automatic check, so it never
/// competes with start-up.
pub const FIRST_CHECK_DELAY: Duration = Duration::from_secs(60);
/// Time between automatic checks.
pub const CHECK_PERIOD: Duration = Duration::from_secs(12 * 60 * 60);

/// The outcome of the latest check, automatic or manual.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AutoCheck {
    /// Updates are off or not set up: nothing was run.
    Off,
    Checked(CheckResult),
    /// The check failed: the CLI's answer, or the app's own failure around
    /// it ([`CliError::from_cli`] tells them apart), shown through
    /// [`last_check_failure`].
    Failed(CliError),
}

/// The app-wide result of update checks; it outlives every view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UpdateHold {
    Waiting(Release),
    Interrupted(Release),
    Held { handed_off: bool },
}

impl UpdateHold {
    fn of(snapshot: &UpdateSnapshot) -> Option<Self> {
        match &snapshot.state {
            UpdateState::AwaitingConfirmation(update) => {
                Some(Self::Waiting(Release::from(&update.to)))
            }
            UpdateState::Interrupted { from, .. } => Some(Self::Interrupted(from.clone())),
            _ if snapshot.admission_held => Some(Self::Held {
                handed_off: snapshot.handed_off || snapshot.admission_in_progress,
            }),
            _ => None,
        }
    }

    fn words(&self) -> String {
        match self {
            Self::Waiting(update) => format!(
                "BroScribe {} is installed. Recording stays off until you keep it or roll back.",
                update.version
            ),
            Self::Interrupted(from) => format!(
                "An update didn't finish. Recording stays off until BroScribe {} is restored.",
                from.label()
            ),
            Self::Held { handed_off: true } => {
                "An update is finishing. Recording stays off until it finishes.".into()
            }
            Self::Held { handed_off: false } => {
                "An update step didn't finish, so recording stays off.".into()
            }
        }
    }
}

/// The app-wide result of update checks; it outlives every view.
#[derive(Default)]
pub struct UpdateNotices {
    latest: Option<AutoCheck>,
    checked_at: Option<chrono::DateTime<chrono::FixedOffset>>,
    downloaded: Option<Offer>,
    hold: Option<UpdateHold>,
}

struct GlobalUpdateNotices(Entity<UpdateNotices>);

impl Global for GlobalUpdateNotices {}

impl UpdateNotices {
    pub(crate) fn current(cx: &App) -> Option<&Self> {
        Some(cx.try_global::<GlobalUpdateNotices>()?.0.read(cx))
    }

    /// The app-wide instance, created on first use.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalUpdateNotices>() {
            return global.0.clone();
        }
        let notices = cx.new(|_| Self::default());
        cx.set_global(GlobalUpdateNotices(notices.clone()));
        notices
    }

    pub fn latest(&self) -> Option<&AutoCheck> {
        self.latest.as_ref()
    }

    /// A newer release the latest check offered.
    pub fn offer(&self) -> Option<&Offer> {
        match &self.latest {
            Some(AutoCheck::Checked(CheckResult::Available(offer))) => Some(offer),
            _ => None,
        }
    }

    pub(crate) fn menu_state(&self, recording: bool) -> crate::menu::UpdateMenu {
        use crate::menu::UpdateMenu;
        if let Some(hold) = &self.hold {
            match hold {
                UpdateHold::Waiting(update) => UpdateMenu::Finish(update.version.clone()),
                _ => UpdateMenu::Recover,
            }
        } else if let Some(offer) = self.offer().filter(|_| !recording) {
            UpdateMenu::Available(offer.release.version.to_string())
        } else {
            UpdateMenu::Check
        }
    }

    pub(crate) fn set_update_state(&mut self, snapshot: &UpdateSnapshot, cx: &mut Context<Self>) {
        let hold = UpdateHold::of(snapshot);
        if self.hold != hold {
            self.hold = hold;
            cx.notify();
        }
    }

    pub(crate) fn holding(&self) -> bool {
        self.hold.is_some()
    }

    pub(crate) fn observe_hold<V: 'static>(cx: &mut Context<V>) -> Subscription {
        let notices = Self::global(cx);
        let holding = std::cell::Cell::new(notices.read(cx).holding());
        cx.observe(&notices, move |_, notices, cx| {
            let held = notices.read(cx).holding();
            if holding.replace(held) != held {
                cx.notify();
            }
        })
    }

    fn set(&mut self, latest: Option<AutoCheck>, cx: &mut Context<Self>) {
        if matches!(latest, Some(AutoCheck::Checked(_) | AutoCheck::Failed(_))) {
            self.checked_at = Some(chrono::Utc::now().fixed_offset());
        }
        if self.latest != latest
            || matches!(latest, Some(AutoCheck::Checked(_) | AutoCheck::Failed(_)))
        {
            self.latest = latest;
            cx.notify();
        }
    }
}

pub(crate) fn admission_changed(held: bool, cx: &mut App) {
    let Some(view) = cx
        .try_global::<SharedUpdateView>()
        .map(|shared| shared.0.clone())
    else {
        return;
    };
    if held {
        // While the updater's durable state loads, the agent's refusal is already authoritative.
        UpdateNotices::global(cx).update(cx, |notices, cx| {
            if notices.hold.is_none() {
                notices.hold = Some(UpdateHold::Held { handed_off: true });
                cx.notify();
            }
        });
    }
    view.update(cx, |this, cx| this.refresh(cx));
}

/// Whether `settings` let the updater use the network at all: on, with a
/// feed and at least one trusted key. Everything else is validated by the
/// CLI, which refuses a configuration it does not accept.
pub fn checks_allowed(settings: &UpdateSettings) -> bool {
    settings.enabled && settings.effective_keys().next().is_some()
}

/// One automatic check: reads the configuration now and runs `update check`
/// only when [`checks_allowed`]. Blocks; run it on the background executor.
pub fn automatic_check(data_dir: &Path, cli: &CliRunner) -> AutoCheck {
    let config = match Config::load(data_dir) {
        Ok(config) => config,
        Err(e) => {
            tracing::warn!(code = e.code(), "reading the config for an update check");
            return AutoCheck::Failed(CliError::from_core(&e));
        }
    };
    if !checks_allowed(&config.updates) {
        return AutoCheck::Off;
    }
    match cli.run_blocking(&Step::Check.args(), ChildKind::Command) {
        Ok(value) => parse_check(&value).map_or_else(
            || {
                AutoCheck::Failed(CliError::new(
                    "bad_output",
                    "the check's answer couldn't be read",
                ))
            },
            AutoCheck::Checked,
        ),
        Err(error) => AutoCheck::Failed(error),
    }
}

/// Starts the app's automatic update checks: the first after
/// [`FIRST_CHECK_DELAY`], then every [`CHECK_PERIOD`], each publishing to
/// [`UpdateNotices`]. Checks only; never downloads or installs. The loop
/// ends with the returned task or once the app state is gone.
pub fn start_automatic_checks(state: &Entity<AppState>, cx: &mut App) -> Task<()> {
    let notices = UpdateNotices::global(cx).downgrade();
    UpdateView::shared(state, cx);
    let state = state.downgrade();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(FIRST_CHECK_DELAY).await;
        loop {
            let Ok((data_dir, cli)) = state.read_with(cx, |state, _| {
                (state.paths().data_dir.clone(), state.cli().clone())
            }) else {
                return;
            };
            let outcome = cx
                .background_executor()
                .spawn(async move { automatic_check(&data_dir, &cli) })
                .await;
            if notices
                .update(cx, |notices, cx| notices.set(Some(outcome), cx))
                .is_err()
            {
                return;
            }
            cx.background_executor().timer(CHECK_PERIOD).await;
        }
    })
}

/// The newest download progress of the running step, coalesced: the stderr
/// reader overwrites one slot and wakes the view at most once until it
/// reads, so a flood of progress while the UI is parked holds one value and
/// one wake-up, never a queue.
struct DownloadProgress {
    latest: Mutex<Option<(u64, u64)>>,
    /// `None` once the step ended: no more wake-ups.
    wake: Mutex<Option<mpsc::Sender<()>>>,
}

impl DownloadProgress {
    fn new() -> (Arc<Self>, mpsc::Receiver<()>) {
        let (tx, wake) = mpsc::channel(0);
        let progress = Self {
            latest: Mutex::new(None),
            wake: Mutex::new(Some(tx)),
        };
        (Arc::new(progress), wake)
    }

    fn report(&self, bytes: u64, total: u64) {
        *lock(&self.latest) = Some((bytes, total));
        if let Some(wake) = lock(&self.wake).as_mut() {
            // Full: a wake-up is already pending and reads this value.
            let _ = wake.try_send(());
        }
    }

    /// The newest progress not read yet.
    fn take(&self) -> Option<(u64, u64)> {
        lock(&self.latest).take()
    }

    fn close(&self) {
        lock(&self.wake).take();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn settings_show_args() -> Vec<OsString> {
    ["update", "settings", "show"].map(OsString::from).into()
}

/// Runs one step's `broscribe --json update …` child through the app's
/// shared [`CliRunner`] transport, which drains stdout (up to its limit) and
/// stderr at once so neither pipe can block the child. Blocks.
///
/// Only [`Step::cancellable`] steps run as a contained tree that `cancel`
/// stops; a child that already exited returns its real answer. Every other
/// step is an ordinary command left to its own terminal result, also when
/// the view or the app goes away: it may already have committed. Download
/// progress (stderr lines, [`parse_progress`]) goes to `progress`; other
/// stderr lines are logged, over-long ones skipped whole by the transport.
fn run_step(
    cli: &CliRunner,
    step: Step,
    stdin: Option<Vec<u8>>,
    cancel: &Arc<AtomicBool>,
    progress: &Arc<DownloadProgress>,
) -> Result<Value, CliError> {
    let args = step.args();
    match (step, stdin) {
        (Step::Download, None) => {
            let reader = Arc::clone(progress);
            cli.run_blocking_cancellable_progress(
                &args,
                ChildKind::Command,
                Arc::clone(cancel),
                move |bytes| {
                    let line = String::from_utf8_lossy(bytes);
                    match parse_progress(&line) {
                        Some((bytes, total)) => reader.report(bytes, total),
                        None if !line.trim().is_empty() => tracing::info!("update: {line}"),
                        None => {}
                    }
                },
            )
        }
        (Step::Check, None) => {
            cli.run_blocking_cancellable(&args, ChildKind::Command, Arc::clone(cancel))
        }
        (_, Some(input)) => cli.run_blocking_stdin(&args, ChildKind::Command, input),
        (_, None) => cli.run_blocking(&args, ChildKind::Command),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Loading,
    Ready,
    Running(Step),
    Downloading {
        bytes: u64,
        total: u64,
    },
    /// The view's [`UpdateView::pending`] confirmation is shown.
    Confirming,
}

/// After an install in this session: what the app must do for the new
/// version to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restart {
    /// Restart this app on the replaced installation.
    Relaunch,
    /// The update runner replaces BroScribe once the app has quit.
    QuitForRunner,
}

struct SharedUpdateView(Entity<UpdateView>);

impl Global for SharedUpdateView {}

pub struct UpdateView {
    cli: CliRunner,
    state: Option<WeakEntity<AppState>>,
    admin_open: bool,
    admin_copied: bool,
    snapshot: Option<UpdateSnapshot>,
    /// `update settings show`: the revision and the `[updates]` table.
    settings_revision: Option<u64>,
    /// The app-wide check results (automatic and from this view).
    notices: Entity<UpdateNotices>,
    check_when_ready: bool,
    _notices: Subscription,
    _recording: Option<Subscription>,
    phase: Phase,
    /// Shown while [`Phase::Confirming`]: exactly the update named.
    pending: Option<Pending>,
    confirmation_window: Option<gpui::AnyWindowHandle>,
    install_focus: gpui::FocusHandle,
    rollback_focus: gpui::FocusHandle,
    banner_rollback_focus: gpui::FocusHandle,
    /// `(is_error, text)`.
    message: Option<(bool, String)>,
    kept: bool,
    restart: Option<Restart>,
    /// Stops the running step's child; set only for a
    /// [`Step::cancellable`] step.
    cancel: Arc<AtomicBool>,
    /// Stops the state read in flight once a newer one replaces it or the
    /// view goes away (it only reads).
    reading: Arc<AtomicBool>,
    generation: u64,
    _task: Option<Task<()>>,
    _progress: Option<Task<()>>,
}

impl UpdateView {
    /// `cli`: the app's shared runner, whose bounded child transport every
    /// `update` command uses.
    pub fn new(cli: CliRunner, cx: &mut Context<Self>) -> Self {
        let notices = UpdateNotices::global(cx);
        let observe = cx.observe(&notices, |_, _, cx| cx.notify());
        let mut this = Self {
            cli,
            state: None,
            admin_open: false,
            admin_copied: false,
            snapshot: None,
            settings_revision: None,
            notices,
            check_when_ready: false,
            _notices: observe,
            _recording: None,
            phase: Phase::Loading,
            pending: None,
            confirmation_window: None,
            install_focus: cx.focus_handle(),
            rollback_focus: cx.focus_handle(),
            banner_rollback_focus: cx.focus_handle(),
            message: None,
            kept: false,
            restart: None,
            cancel: Arc::default(),
            reading: Arc::default(),
            generation: 0,
            _task: None,
            _progress: None,
        };
        this.refresh(cx);
        this
    }

    pub(crate) fn shared(state: &Entity<AppState>, cx: &mut App) -> Entity<Self> {
        if let Some(shared) = cx.try_global::<SharedUpdateView>() {
            return shared.0.clone();
        }
        let cli = state.read(cx).cli().clone();
        let daemon = state.read(cx).daemon().cloned();
        let state = state.downgrade();
        let view = cx.new(|cx| {
            let mut view = Self::new(cli, cx);
            view.state = Some(state);
            view._recording = daemon.map(|daemon| {
                let recording = std::cell::Cell::new(view.recording(cx));
                cx.observe(&daemon, move |_, daemon, cx| {
                    let daemon = daemon.read(cx);
                    let current = daemon.record_start_pending() || daemon.panel.recording.is_some();
                    if recording.replace(current) != current {
                        cx.notify();
                    }
                })
            });
            view
        });
        cx.set_global(SharedUpdateView(view.clone()));
        view
    }

    fn recording(&self, cx: &App) -> bool {
        self.state
            .as_ref()
            .and_then(WeakEntity::upgrade)
            .and_then(|state| state.read(cx).daemon())
            .is_some_and(|daemon| {
                let daemon = daemon.read(cx);
                daemon.record_start_pending() || daemon.panel.recording.is_some()
            })
    }

    #[cfg(all(test, unix))]
    pub(crate) fn phase(&self) -> &Phase {
        &self.phase
    }

    #[cfg(all(test, unix))]
    pub(crate) fn snapshot(&self) -> Option<&UpdateSnapshot> {
        self.snapshot.as_ref()
    }

    #[cfg(all(test, unix))]
    pub(crate) fn message(&self) -> Option<&(bool, String)> {
        self.message.as_ref()
    }

    #[cfg(all(test, unix))]
    pub(crate) fn restart(&self) -> Option<Restart> {
        self.restart
    }

    fn busy(&self) -> bool {
        matches!(
            self.phase,
            Phase::Loading | Phase::Running(_) | Phase::Downloading { .. }
        )
    }

    /// Reads the status and the settings again. While a step runs this does
    /// nothing: the step keeps its phase until its own child answered, and
    /// its end reads the state again. A newer read replaces (and stops) one
    /// still in flight.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self
            .confirmation_window
            .is_some_and(|handle| !cx.windows().contains(&handle))
        {
            self.cancel_pending(cx);
        }
        if matches!(
            self.phase,
            Phase::Confirming | Phase::Running(_) | Phase::Downloading { .. }
        ) {
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        self.enter(Phase::Loading);
        self.reading.store(true, Ordering::SeqCst);
        self.reading = Arc::default();
        let reading = Arc::clone(&self.reading);
        let cli = self.cli.clone();
        let task = cx.background_executor().spawn(async move {
            let status = cli.run_blocking_cancellable(
                &Step::Status.args(),
                ChildKind::Command,
                Arc::clone(&reading),
            )?;
            let settings =
                cli.run_blocking_cancellable(&settings_show_args(), ChildKind::Command, reading);
            Ok::<_, CliError>((status, settings))
        });
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.enter(Phase::Ready);
                match result {
                    Ok((status, settings)) => {
                        let snapshot = parse_status(&status);
                        this.notices
                            .update(cx, |notices, cx| notices.set_update_state(&snapshot, cx));
                        this.snapshot = Some(snapshot);
                        this.settings_revision = match settings {
                            Ok(settings) => settings["revision"].as_u64(),
                            Err(error) => {
                                this.message = Some((true, refusal_text(&error)));
                                None
                            }
                        };
                    }
                    Err(e) => {
                        this.settings_revision = None;
                        this.message = Some((true, refusal_text(&e)));
                    }
                }
                if std::mem::take(&mut this.check_when_ready) && this.message.is_none() {
                    this.check_from_menu(cx);
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn run(&mut self, step: Step, stdin: Option<Vec<u8>>, cx: &mut Context<Self>) {
        if self.phase != Phase::Ready
            || (matches!(
                step,
                Step::Check | Step::Download | Step::Install | Step::Rollback
            ) && self.recording(cx))
        {
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        self.enter(Phase::Running(step));
        self.message = None;
        self.kept = false;
        self.cancel = Arc::default();
        let cancel = Arc::clone(&self.cancel);
        let cli = self.cli.clone();
        let (progress, mut wake) = DownloadProgress::new();
        let reader = Arc::clone(&progress);
        let task = cx.background_executor().spawn(async move {
            let result = run_step(&cli, step, stdin, &cancel, &reader);
            reader.close();
            result
        });
        self._progress = Some(cx.spawn(async move |this, cx| {
            while wake.next().await.is_some() {
                let Some((bytes, total)) = progress.take() else {
                    continue;
                };
                let updated = this.update(cx, |this, cx| {
                    if this.generation == generation
                        && matches!(
                            this.phase,
                            Phase::Running(Step::Download) | Phase::Downloading { .. }
                        )
                    {
                        this.enter(Phase::Downloading { bytes, total });
                        cx.notify();
                    }
                });
                if updated.is_err() {
                    break;
                }
            }
        }));
        self._task = Some(cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.enter(Phase::Ready);
                this.finished(step, result, cx);
            })
            .ok();
        }));
        cx.notify();
    }

    fn finished(&mut self, step: Step, result: Result<Value, CliError>, cx: &mut Context<Self>) {
        match result {
            Ok(value) => match step {
                Step::Check => {
                    let latest = match parse_check(&value) {
                        Some(result) => {
                            let message = match &result {
                                CheckResult::UpToDate { installed } => {
                                    Some(format!("BroScribe {} is up to date.", installed.label()))
                                }
                                CheckResult::NoVerifiedUpdate => {
                                    Some("No verified update available.".into())
                                }
                                CheckResult::Available(_) => None,
                            };
                            self.message = message.map(|text| (false, text));
                            AutoCheck::Checked(result)
                        }
                        None => AutoCheck::Failed(CliError::new(
                            "bad_output",
                            "the check's answer couldn't be read",
                        )),
                    };
                    self.notices
                        .update(cx, |notices, cx| notices.set(Some(latest), cx));
                }
                Step::Download => {
                    if matches!(
                        value["state"].as_str(),
                        Some("no_verified_update" | "up_to_date")
                    ) {
                        self.message = Some((false, "No verified update available.".into()));
                        self.notices.update(cx, |notices, cx| notices.set(None, cx));
                    } else {
                        self.notices.update(cx, |notices, cx| {
                            notices.downloaded = notices.offer().cloned();
                            notices.set(None, cx);
                        });
                    }
                }
                Step::Install => {
                    let handed_off = value["state"] == "handed_off";
                    self.restart = Some(if handed_off {
                        Restart::QuitForRunner
                    } else {
                        Restart::Relaunch
                    });
                    self.message = Some((
                        false,
                        if handed_off {
                            "Quit BroScribe so the update can replace it. Recording stays off \
                             until you keep the new version or roll back."
                        } else {
                            "The new version is installed. Restart BroScribe, then keep it or \
                             roll back. Recording stays off until then."
                        }
                        .into(),
                    ));
                }
                Step::Confirm => {
                    self.kept = true;
                    let version =
                        self.snapshot
                            .as_ref()
                            .and_then(|snapshot| match &snapshot.state {
                                UpdateState::AwaitingConfirmation(update) => {
                                    Some(update.to.version.as_str())
                                }
                                _ => None,
                            });
                    self.message = Some((
                        false,
                        version.map_or_else(
                            || "This version is kept. Recording is available again.".into(),
                            |version| {
                                format!(
                                    "BroScribe {version} is kept. Recording is available again."
                                )
                            },
                        ),
                    ));
                    self.restart = None;
                }
                Step::Rollback => {
                    self.restart = None;
                    self.message = Some((
                        false,
                        if value["state"] == "handed_off" {
                            "Quit BroScribe so the previous version can be restored."
                        } else {
                            "The previous version is back. Restart BroScribe to use it."
                        }
                        .into(),
                    ));
                    if value["state"] == "handed_off" {
                        self.restart = Some(Restart::QuitForRunner);
                    } else {
                        self.restart = Some(Restart::Relaunch);
                    }
                }
                Step::Discard => {
                    self.message = Some((false, "The download was removed.".into()));
                }
                Step::Recover => {
                    self.message = Some((false, "Recovery finished.".into()));
                }
                Step::SettingsSet { .. } => {
                    self.message = Some((false, "Automatic update preference saved.".into()));
                }
                Step::Status => {}
            },
            Err(e) => {
                if step == Step::Check && e.code != "cancelled" {
                    self.notices.update(cx, |notices, cx| {
                        notices.set(Some(AutoCheck::Failed(e.clone())), cx)
                    });
                } else {
                    self.message = Some((true, refusal_text(&e)));
                }
            }
        }
        self.refresh(cx);
    }

    pub fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        self.run(Step::Check, None, cx);
    }

    pub(crate) fn check_from_menu(&mut self, cx: &mut Context<Self>) {
        if self.phase == Phase::Loading {
            self.check_when_ready = true;
        } else if self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| matches!(snapshot.state, UpdateState::Idle))
        {
            self.check_for_updates(cx);
        }
    }

    pub fn download(&mut self, cx: &mut Context<Self>) {
        self.run(Step::Download, None, cx);
    }

    /// Stops a check or a download (nothing is replaced by either): their
    /// contained child is stopped, unless it already answered, whose real
    /// answer then stands. Any other step keeps running to its own result.
    pub fn cancel(&mut self, _cx: &mut Context<Self>) {
        if self.cancellable() {
            self.cancel.store(true, Ordering::SeqCst);
        }
    }

    pub fn discard(&mut self, cx: &mut Context<Self>) {
        self.run(Step::Discard, None, cx);
    }

    /// The replacement awaiting confirmation: set only together with
    /// [`Phase::Confirming`], cleared by every other phase.
    #[cfg(all(test, unix))]
    pub(crate) fn pending(&self) -> Option<&Pending> {
        self.pending.as_ref()
    }

    /// Every phase change but showing a confirmation; a shown confirmation
    /// ends with it.
    fn enter(&mut self, phase: Phase) {
        self.pending = None;
        self.confirmation_window = None;
        self.phase = phase;
    }

    fn ask(&mut self, pending: Pending, cx: &mut Context<Self>) {
        self.pending = Some(pending);
        self.phase = Phase::Confirming;
        cx.notify();
    }

    /// Shows the install confirmation for exactly the staged update the
    /// last status reported; nothing is sent yet. Not while the agent holds
    /// an install admission: no install can take one until that hold ends
    /// (Finish recovery), so it would only be refused.
    pub fn request_install(&mut self, cx: &mut Context<Self>) {
        if self.phase != Phase::Ready || self.recording(cx) {
            return;
        }
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let UpdateState::Staged(update) = &snapshot.state else {
            return;
        };
        if snapshot.admission_held || !snapshot.agent_running {
            return;
        }
        self.ask(Pending::Install(update.clone()), cx);
    }

    /// Shows the rollback confirmation for exactly the installed update the
    /// last status reported; nothing is sent yet.
    pub fn request_rollback(&mut self, cx: &mut Context<Self>) {
        if self.phase != Phase::Ready || self.recording(cx) {
            return;
        }
        let Some(UpdateState::AwaitingConfirmation(update)) =
            self.snapshot.as_ref().map(|s| &s.state)
        else {
            return;
        };
        self.ask(Pending::Rollback(update.clone()), cx);
    }

    fn open_replacement(
        &mut self,
        install: bool,
        opener: gpui::FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.phase != Phase::Ready || window.has_active_dialog(cx) {
            return;
        }
        if install {
            self.request_install(cx);
        } else {
            self.request_rollback(cx);
        }
        let Some(pending) = self.pending.clone() else {
            return;
        };
        self.confirmation_window = Some(window.window_handle());
        let cancel = cx.weak_entity();
        let confirm = cancel.clone();
        kit::confirm_dialog(
            opener,
            window,
            cx,
            move |_| Some(replacement_spec(&pending)),
            move |_, cx| {
                let _ = cancel.update(cx, |this, cx| this.cancel_pending(cx));
                true
            },
            move |_, cx| {
                let _ = confirm.update(cx, |this, cx| this.confirm_pending(cx));
                true
            },
        );
    }

    fn replacement_button(
        &self,
        install: bool,
        id: &'static str,
        disabled: bool,
        reason: &'static str,
        cx: &Context<Self>,
    ) -> Button {
        let focus = match id {
            "updates-install" => &self.install_focus,
            "updates-rollback" => &self.rollback_focus,
            _ => &self.banner_rollback_focus,
        }
        .clone();
        Button::new(id)
            .debug_selector(move || id.to_owned())
            .label(if install { "Install" } else { "Roll back" })
            .outline()
            .track_focus(&focus)
            .map(|button| kit::disable_with_reason(button, disabled, reason, cx))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_replacement(install, focus.clone(), window, cx)
            }))
    }

    /// Runs the confirmed replacement for exactly the update confirmed
    /// (`--expectation-stdin`). A refusal because the pending update changed
    /// (`update_changed`) only reloads the state: the user confirms again.
    pub fn confirm_pending(&mut self, cx: &mut Context<Self>) {
        if self.phase != Phase::Confirming {
            return;
        }
        let Some(pending) = self.pending.take() else {
            return;
        };
        self.enter(Phase::Ready);
        if self.recording(cx) {
            self.message = Some((true, "Finish recording before replacing BroScribe.".into()));
            cx.notify();
            return;
        }
        let (step, expected) = pending.step();
        match serde_json::to_vec(expected) {
            Ok(expected) => self.run(step, Some(expected), cx),
            Err(e) => {
                tracing::warn!(error = %e, "writing the confirmed update");
                self.message = Some((true, refusal_text(&CliError::new("internal", ""))));
                cx.notify();
            }
        }
    }

    pub fn cancel_pending(&mut self, cx: &mut Context<Self>) {
        if self.phase == Phase::Confirming {
            self.enter(Phase::Ready);
            cx.notify();
        }
    }

    pub fn keep(&mut self, cx: &mut Context<Self>) {
        if self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| matches!(snapshot.state, UpdateState::AwaitingConfirmation(_)))
        {
            self.run(Step::Confirm, None, cx);
        }
    }

    pub fn recover(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.snapshot.as_ref().and_then(UpdateHold::of),
            Some(UpdateHold::Interrupted(_) | UpdateHold::Held { handed_off: false })
        ) {
            self.run(Step::Recover, None, cx);
        }
    }

    /// Saves the check preference only if the shown revision still applies.
    pub fn set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let Some(expect_revision) = self.settings_revision else {
            return;
        };
        self.run(
            Step::SettingsSet {
                enabled,
                expect_revision,
            },
            None,
            cx,
        );
    }
}

impl Drop for UpdateView {
    fn drop(&mut self) {
        // A state read, a check or a download stops with the view; anything
        // else finishes.
        self.reading.store(true, Ordering::SeqCst);
        if self.cancellable() {
            self.cancel.store(true, Ordering::SeqCst);
        }
    }
}

/// The download's fraction and its words.
fn download_progress(bytes: u64, total: u64) -> (f32, String) {
    let fraction = if total == 0 {
        0.0
    } else {
        (bytes as f64 / total as f64) as f32
    };
    let text = if total == 0 {
        "Downloading…".to_owned()
    } else {
        let percent = crate::ui::format::progress_percent(f64::from(fraction))
            .expect("a ratio between two byte counts is finite");
        format!("Downloading · {percent}")
    };
    (fraction, text)
}

/// One piece of the view, top to bottom.
#[allow(
    clippy::large_enum_variant,
    reason = "Each part moves straight into this render's element tree; boxing would allocate \
              every line on every render."
)]
enum Part {
    /// An identified accessible node labelled with what it shows: a line of
    /// text, the download's progress, or the confirmation naming the update
    /// (its description the whole disclosure).
    Node(Stateful<Div>),
    /// A button or a row of them.
    Control(AnyElement),
}

impl Part {
    fn control(element: impl IntoElement) -> Self {
        Part::Control(element.into_any_element())
    }
}

impl UpdateView {
    fn release_notes(notes: Option<&str>) -> Part {
        Part::Node(
            text_line(
                "updates-release-notes",
                Role::Label,
                notes
                    .unwrap_or("Release notes aren't available for this update.")
                    .to_owned(),
            )
            .text_role(TextRole::Body),
        )
    }

    fn facts(&self, cx: &Context<Self>) -> Vec<Part> {
        let Some(snapshot) = &self.snapshot else {
            return Vec::new();
        };
        let notices = self.notices.read(cx);
        let checked = notices
            .checked_at
            .as_ref()
            .map(crate::ui::format::local_date_time)
            .unwrap_or_else(|| "Not checked yet".into());
        let installed = snapshot
            .installed
            .as_ref()
            .map(|release| format!("BroScribe {}", release.label()))
            .unwrap_or_else(|| {
                "This copy can't update itself (a development build or an unsupported location)."
                    .into()
            });
        let idle = self.phase == Phase::Ready;
        let recording = self.recording(cx);
        let can_check = matches!(snapshot.state, UpdateState::Idle)
            && idle
            && !recording
            && !snapshot.admission_held;
        let mut parts = vec![
            Part::Node(kit::fact_row("updates-version", "Version", installed, cx)),
            Part::control(
                crate::views::settings::row()
                    .child(kit::fact_row(
                        "updates-last-checked",
                        "Last checked",
                        checked,
                        cx,
                    ))
                    .child(kit::disable_with_reason(
                        Button::new("updates-check")
                            .debug_selector(|| "updates-check".into())
                            .label("Check for updates")
                            .outline()
                            .on_click(cx.listener(|this, _, _, cx| this.check_for_updates(cx))),
                        !can_check,
                        if recording {
                            "Finish recording before checking for updates."
                        } else if !snapshot.configured {
                            "Updates aren't set up for this copy."
                        } else if !snapshot.enabled {
                            "Turn on automatic checks to use this update source."
                        } else {
                            "Finish the current update step first."
                        },
                        cx,
                    )),
            ),
            Part::control(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        crate::views::settings::row()
                            .child(text_line("updates-auto-label", Role::Label, AUTO_LABEL))
                            .child(
                                div().debug_selector(|| "updates-auto".into()).child(
                                    Switch::new("updates-auto")
                                        .accessibility_label(AUTO_LABEL)
                                        .small()
                                        .checked(snapshot.enabled)
                                        .disabled(
                                            !snapshot.configured
                                                || self.settings_revision.is_none()
                                                || !idle,
                                        )
                                        .on_click(cx.listener(|this, enabled: &bool, _, cx| {
                                            this.set_enabled(*enabled, cx)
                                        })),
                                ),
                            ),
                    )
                    .child(
                        text_line("updates-auto-help", Role::Label, AUTO_HELP)
                            .text_role(TextRole::Caption),
                    ),
            ),
        ];
        if let Some(AutoCheck::Failed(error)) = notices.latest() {
            parts.push(Part::Node(kit::status_line(
                "updates-last-check",
                StatusKind::Failed,
                last_check_failure(error),
                cx,
            )));
        }
        if !snapshot.configured {
            parts.push(Part::Node(kit::status_line("updates-not-configured", StatusKind::Info,
                "Updates aren't set up for this copy of BroScribe, so it can't check for new versions.", cx)));
            parts.push(Part::control(
                kit::disclosure(
                    "updates-admin",
                    "For administrators",
                    self.admin_open,
                    cx,
                    |this, _, cx| {
                        this.admin_open = !this.admin_open;
                        this.admin_copied = false;
                        cx.notify();
                    },
                )
                .debug_selector(|| "updates-admin".into()),
            ));
            if self.admin_open {
                parts.push(Part::Node(text_line(
                    "updates-admin-help",
                    Role::Label,
                    "Set the update feed and trusted release keys with this command.",
                )));
                parts.push(Part::control(
                    crate::views::settings::row()
                        .child(
                            text_line("updates-admin-command", Role::Code, ADMIN_COMMAND)
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_role(TextRole::Mono),
                        )
                        .child(
                            kit::button("updates-admin-copy", "Copy", true, cx, |this, _, cx| {
                                cx.write_to_clipboard(gpui::ClipboardItem::new_string(
                                    ADMIN_COMMAND.into(),
                                ));
                                this.admin_copied = true;
                                cx.notify();
                            })
                            .debug_selector(|| "updates-admin-copy".into())
                            .outline(),
                        ),
                ));
                if self.admin_copied {
                    parts.push(Part::Node(kit::status_line(
                        "updates-admin-copied",
                        StatusKind::Done,
                        "Copied.",
                        cx,
                    )));
                }
            }
        }
        parts
    }

    fn hold_banner(&self, hold: &UpdateHold, id: &'static str, cx: &Context<Self>) -> Div {
        let idle = self.phase == Phase::Ready && !self.recording(cx);
        kit::banner(id, StatusKind::Attention, Role::Status, hold.words(), cx).child(
            crate::views::settings::row().map(|row| match hold {
                UpdateHold::Waiting(_) => row
                    .child(
                        kit::button(
                            "updates-keep",
                            "Keep this version",
                            idle,
                            cx,
                            |this, _, cx| this.keep(cx),
                        )
                        .debug_selector(|| "updates-keep".into())
                        .map(|button| kit::primary(button, !idle, cx)),
                    )
                    .child(self.replacement_button(
                        false,
                        if id == "main-update-hold" {
                            "main-update-rollback"
                        } else {
                            "updates-rollback"
                        },
                        !idle,
                        "Finish the current update step first.",
                        cx,
                    )),
                UpdateHold::Interrupted(_) => row.child(
                    kit::button("updates-recover", "Recover", idle, cx, |this, _, cx| {
                        this.recover(cx)
                    })
                    .map(|button| kit::primary(button, !idle, cx)),
                ),
                UpdateHold::Held { handed_off: false } => row.child(
                    kit::button(
                        "updates-release",
                        "Finish recovery",
                        idle,
                        cx,
                        |this, _, cx| this.recover(cx),
                    )
                    .debug_selector(|| "updates-release".into())
                    .outline(),
                ),
                UpdateHold::Held { handed_off: true } => row,
            }),
        )
    }

    /// The running step can be stopped: a check or a download.
    fn cancellable(&self) -> bool {
        match self.phase {
            Phase::Running(step) => step.cancellable(),
            Phase::Downloading { .. } => true,
            _ => false,
        }
    }

    /// Everything the view shows, in order. Every text, status, alert and
    /// the confirmation is a [`Part::Node`] carrying its actual words.
    fn parts(&self, cx: &Context<Self>) -> Vec<Part> {
        use crate::ui::kit::{StatusKind, status_line, status_without_wait_clock};
        let busy = self.busy();
        let mut parts = vec![Part::Node(title("updates-title", "Updates"))];
        parts.extend(self.facts(cx));
        match (&self.phase, &self.pending) {
            (Phase::Loading, _) => parts.push(Part::Node(status_line(
                "updates-status",
                StatusKind::Pending,
                "Reading the update state…",
                cx,
            ))),
            (Phase::Running(step), _) => parts.push(Part::Node(status_line(
                "updates-status",
                StatusKind::Pending,
                step.running_text(),
                cx,
            ))),
            (Phase::Downloading { bytes, total }, _) => {
                let (fraction, label) = download_progress(*bytes, *total);
                let status = if *total > 0 {
                    status_without_wait_clock(
                        "updates-status",
                        StatusKind::Pending,
                        label.clone(),
                        cx,
                    )
                } else {
                    status_line("updates-status", StatusKind::Pending, label.clone(), cx)
                };
                parts.push(Part::Node(status));
                if *total > 0 {
                    parts.push(Part::control(crate::ui::kit::progress(
                        "updates-progress",
                        label,
                        fraction,
                    )));
                }
            }
            (Phase::Ready | Phase::Confirming, _) => {}
        }
        if let Some((error, text)) = &self.message {
            parts.push(Part::Node(status_line(
                "updates-message",
                if *error {
                    StatusKind::Failed
                } else {
                    StatusKind::Done
                },
                text.clone(),
                cx,
            )));
        }
        if self.cancellable() {
            parts.push(Part::control(
                Button::new("updates-cancel")
                    .label("Cancel")
                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
            ));
        }
        if let Some(restart) = self.restart {
            parts.push(Part::control(match restart {
                Restart::Relaunch => Button::new("updates-restart")
                    .label("Restart BroScribe")
                    .map(|button| {
                        crate::ui::kit::disable_with_reason(
                            button,
                            busy,
                            "Another update action is already running.",
                            cx,
                        )
                    })
                    .on_click(|_, _, cx| cx.restart()),
                Restart::QuitForRunner => Button::new("updates-quit")
                    .label("Quit BroScribe")
                    .map(|button| {
                        crate::ui::kit::disable_with_reason(
                            button,
                            busy,
                            "Another update action is already running.",
                            cx,
                        )
                    })
                    .on_click(|_, _, cx| cx.quit()),
            }));
        }
        let Some(snapshot) = &self.snapshot else {
            return parts;
        };
        let note = |id: &'static str, role: Role, text: String| {
            Part::Node(match role {
                Role::Alert => status_line(id, StatusKind::Failed, text, cx),
                Role::Status => status_line(id, StatusKind::Info, text, cx),
                _ => text_line(id, role, text).text_role(TextRole::Caption),
            })
        };
        if let Some(runner) = &snapshot.runner {
            parts.push(note("updates-runner", Role::Status, runner.clone()));
        }
        if snapshot.handed_off {
            parts.push(note(
                "updates-handed-off",
                Role::Status,
                "An update step is waiting for BroScribe to quit; it continues on its own.".into(),
            ));
        }
        let confirming = self.phase == Phase::Confirming;
        let idle = !busy && !confirming;
        match &snapshot.state {
            UpdateState::Off => {}
            UpdateState::Unavailable { code, message } => {
                parts.push(note(
                    "updates-state",
                    Role::Alert,
                    refusal_text(&CliError::new(code, message.clone())),
                ));
                parts.push(Part::control(
                    Button::new("updates-retry")
                        .label("Try again")
                        .map(|button| {
                            crate::ui::kit::disable_with_reason(
                                button,
                                !idle,
                                "Another update action is already running.",
                                cx,
                            )
                        })
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                ));
            }
            UpdateState::Idle => {
                if !self.recording(cx)
                    && let Some(offer) = self.notices.read(cx).offer().cloned()
                {
                    parts.push(note(
                        "updates-offer",
                        Role::Status,
                        format!(
                            "BroScribe {} is available · {} · you have {}. Downloading doesn't install anything.",
                            offer.release.version,
                            crate::ui::format::bytes(offer.size),
                            offer.installed.version,
                        ),
                    ));
                    parts.push(Self::release_notes(offer.release_notes.as_deref()));
                    parts.push(Part::control(
                        Button::new("updates-download")
                            .label("Download")
                            .map(|button| {
                                crate::ui::kit::disable_with_reason(
                                    button,
                                    !idle || self.recording(cx),
                                    "Another update action is already running.",
                                    cx,
                                )
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.download(cx))),
                    ));
                }
            }
            UpdateState::Staged(update) => {
                parts.push(note(
                    "updates-state",
                    Role::Label,
                    format!(
                        "BroScribe {} is downloaded and verified.",
                        Release::from(&update.to).label()
                    ),
                ));
                let notes = self
                    .notices
                    .read(cx)
                    .downloaded
                    .as_ref()
                    .filter(|offer| offer.release == Release::from(&update.to))
                    .and_then(|offer| offer.release_notes.as_deref());
                parts.push(Self::release_notes(notes));
                parts.push(Part::control(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(self.replacement_button(
                            true,
                            "updates-install",
                            !idle
                                || !snapshot.agent_running
                                || snapshot.admission_held
                                || self.recording(cx),
                            if !snapshot.agent_running {
                                "Start BroScribe Agent before installing the update."
                            } else if snapshot.admission_held {
                                "Finish recovery before installing the update."
                            } else if self.recording(cx) {
                                "Finish recording before installing the update."
                            } else {
                                "Another update action is already running."
                            },
                            cx,
                        ))
                        .child(
                            Button::new("updates-discard")
                                .label("Remove download")
                                .map(|button| {
                                    crate::ui::kit::disable_with_reason(
                                        button,
                                        !idle,
                                        "Another update action is already running.",
                                        cx,
                                    )
                                })
                                .on_click(cx.listener(|this, _, _, cx| this.discard(cx))),
                        ),
                ));
                if !snapshot.agent_running {
                    parts.push(note(
                        "updates-agent-needed",
                        Role::Status,
                        "BroScribe Agent must be running to install: it keeps recording off \
                         while BroScribe is replaced."
                            .into(),
                    ));
                }
            }
            UpdateState::AwaitingConfirmation(_) | UpdateState::Interrupted { .. } => {}
        }
        if let Some(hold) = UpdateHold::of(snapshot) {
            parts.push(Part::control(self.hold_banner(&hold, "updates-hold", cx)));
        }
        parts
    }
}

impl Render for UpdateView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("updates")
            .min_w_0()
            .role(Role::Group)
            .aria_label("Update settings")
            .flex()
            .flex_col()
            .gap_2()
            .p_4()
            .children(self.parts(cx).into_iter().map(|part| match part {
                Part::Node(node) => node.into_any_element(),
                Part::Control(control) => control,
            }))
    }
}

/// The state the main window draws. Download progress and configuration
/// changes while there is no hold do not repaint the shell.
#[derive(PartialEq, Eq)]
pub(crate) struct UpdateBannerState {
    hold: Option<UpdateHold>,
    enabled: bool,
    pending: Option<&'static str>,
    message: Option<(bool, String)>,
}

impl UpdateView {
    pub(crate) fn banner_state(&self, cx: &App) -> Option<UpdateBannerState> {
        let hold = self.notices.read(cx).hold.clone();
        if hold.is_none() && !self.kept {
            return None;
        }
        let pending = match self.phase {
            Phase::Running(step) => Some(step.running_text()),
            Phase::Loading => Some(Step::Status.running_text()),
            _ => None,
        };
        Some(UpdateBannerState {
            hold,
            enabled: self.phase == Phase::Ready && !self.recording(cx),
            pending,
            message: self.message.clone(),
        })
    }
}

/// The main window's controls use the same entity and admission workflow as Settings.
#[derive(gpui::IntoElement)]
pub(crate) struct UpdateBanner(pub Entity<UpdateView>);

impl gpui::RenderOnce for UpdateBanner {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        self.0.update(cx, |this, cx| {
            div().when_some(this.banner_state(cx), |el, state| {
                el.when_some(state.hold, |el, hold| {
                    el.child(this.hold_banner(&hold, "main-update-hold", cx))
                })
                .when_some(state.message, |el, (error, text)| {
                    el.child(kit::status_line(
                        "main-update-message",
                        if error {
                            StatusKind::Failed
                        } else {
                            StatusKind::Done
                        },
                        text,
                        cx,
                    ))
                })
                .when_some(state.pending, |el, words| {
                    el.child(kit::status_line(
                        "main-update-pending",
                        StatusKind::Pending,
                        words,
                        cx,
                    ))
                })
            })
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn main_menu_update_label_follows_offer_recording_and_confirmation() {
        let release = Release {
            version: "0.11.0".into(),
            build: 11,
            target: "synthetic-target".into(),
        };
        let installed = Release {
            version: "0.10.0".into(),
            build: 10,
            target: "synthetic-target".into(),
        };
        let mut notices = UpdateNotices::default();
        assert_eq!(
            notices.menu_state(false).label(false, false).as_deref(),
            Some("Check for updates")
        );
        notices.latest = Some(AutoCheck::Checked(CheckResult::Available(Offer {
            release,
            installed,
            size: 1024,
            release_notes: None,
            signer_pin: None,
        })));
        assert_eq!(
            notices.menu_state(false).label(false, false).as_deref(),
            Some("Update available · 0.11.0")
        );
        assert_eq!(
            notices.menu_state(false).label(true, false).as_deref(),
            Some("Update Available")
        );
        assert_eq!(
            notices.menu_state(false).label(false, true).as_deref(),
            Some("Update available · 0.11.0")
        );
        assert_eq!(
            notices.menu_state(true).label(false, false).as_deref(),
            Some("Check for updates")
        );
        assert_eq!(notices.menu_state(true).label(false, true), None);
        let mut update = exact(10, 11);
        update.to.version = "0.11.0".into();
        notices.hold = Some(UpdateHold::Waiting(Release::from(&update.to)));
        assert_eq!(
            notices.menu_state(false).label(false, false).as_deref(),
            Some("Finish updating")
        );
        assert_eq!(
            notices.menu_state(true).label(false, true).as_deref(),
            Some("Keep BroScribe 0.11.0")
        );
    }

    use super::*;
    use serde_json::json;
    #[cfg(unix)]
    use std::time::Instant;

    fn release(build: u64) -> Release {
        Release {
            version: "1.2.0".into(),
            build,
            target: "aarch64-apple-darwin".into(),
        }
    }

    fn identity(build: u64) -> Value {
        json!({ "product": "BroScribe", "app_id": "org.broscribe.BroScribe", "version": "1.2.0",
                "build": build, "target": "aarch64-apple-darwin" })
    }

    fn signer() -> Value {
        json!({ "facility": "simulated", "signer": "TEAMID1234", "requirement": null })
    }

    /// `from` → `to` as `update status` reports a pending update.
    fn exact(from: u64, to: u64) -> Expectation {
        serde_json::from_value(json!({ "from": identity(from), "to": identity(to),
                                       "signer": signer() }))
        .unwrap()
    }

    #[test]
    fn update_location_recovery_follows_the_host() {
        assert!(
            update_location_instructions(HostPlatform::MacOs).contains("Move it to Applications")
        );
        for host in [HostPlatform::Windows, HostPlatform::Linux] {
            assert_eq!(
                update_location_instructions(host),
                "BroScribe can't update itself where it's installed. Reinstall it in a folder you can write to, then try again."
            );
        }
    }

    #[test]
    fn status_reports_state_admission_and_runner_without_feed_or_keys() {
        let value = json!({
            "settings": { "enabled": true, "feed_configured": true, "channel": "stable",
                          "trusted_keys": 1, "signer": "TEAMID1234" },
            "update": { "state": "awaiting_confirmation", "from": identity(10), "to": identity(11),
                        "signer": signer() },
            "agent": { "running": true, "pid": 9, "version": "1.2.0",
                       "install": { "fence": 4, "durable": true } },
            "handed_off": null,
            "runner": { "schema": 1, "fence": 4, "finished_at": "2026-11-17T00:00:00Z",
                        "outcome": { "state": "installed", "from": identity(10), "to": identity(11) },
                        "agent_start_error": "service refused" },
        });
        let snapshot = parse_status(&value);
        assert_eq!(
            snapshot.state,
            UpdateState::AwaitingConfirmation(exact(10, 11))
        );
        assert!(snapshot.enabled && snapshot.configured && snapshot.admission_held);
        assert!(!snapshot.handed_off);
        assert_eq!(snapshot.signer_pin.as_deref(), Some("TEAMID1234"));
        let runner = snapshot.runner.unwrap();
        assert!(runner.contains("1.2.0 (build 11) was installed"));
        assert!(runner.contains("BroScribe Agent didn't restart"));

        let off = parse_status(&json!({
            "settings": { "enabled": false, "feed_configured": false, "trusted_keys": 0 },
            "update": { "state": "off" }, "agent": { "running": false },
            "handed_off": { "kind": "recover" }, "runner": null,
        }));
        assert_eq!(off.state, UpdateState::Off);
        assert!(!off.configured && !off.agent_running && off.handed_off);
        // A pending update without its exact identities or signer can never
        // be confirmed.
        for update in [
            json!({ "state": "staged" }),
            json!({ "state": "staged", "from": identity(10), "to": identity(11) }),
            json!({ "state": "awaiting_confirmation", "from": identity(10), "to": identity(11) }),
        ] {
            assert!(matches!(
                parse_status(&json!({ "update": update })).state,
                UpdateState::Unavailable { .. }
            ));
        }
    }

    #[test]
    fn checks_and_progress_lines_parse() {
        let available = json!({
            "state": "available", "version": "1.2.0", "build": 11,
            "target": "aarch64-apple-darwin", "size": 3_145_728, "installed": identity(10),
            "signer_pin": "TEAMID1234",
        });
        assert_eq!(
            parse_check(&available),
            Some(CheckResult::Available(Offer {
                release: release(11),
                size: 3_145_728,
                release_notes: None,
                installed: release(10),
                signer_pin: Some("TEAMID1234".into()),
            }))
        );
        assert_eq!(
            parse_check(&json!({ "state": "up_to_date", "installed": identity(11) })),
            Some(CheckResult::UpToDate {
                installed: release(11)
            })
        );
        assert_eq!(
            parse_progress(r#"{"progress":{"step":"download","bytes":512,"total":1024}}"#),
            Some((512, 1024))
        );
        assert_eq!(parse_progress("downloading: 50%"), None);
        assert_eq!(
            parse_progress(r#"{"progress":{"step":"other","bytes":1,"total":2}}"#),
            None
        );
    }

    #[test]
    fn replacements_name_their_exact_target_and_signer() {
        let install = replacement_spec(&Pending::Install(exact(10, 11)));
        assert_eq!(
            install.title.1.as_ref(),
            "Install BroScribe 1.2.0 (build 11)?"
        );
        assert!(install.warning.unwrap().1.contains("Recording stays off"));
        assert!(
            install.changes[0]
                .1
                .contains("Replaces 1.2.0 (build 10). Signed by TEAMID1234.")
        );
        let rollback = replacement_spec(&Pending::Rollback(exact(10, 11)));
        assert_eq!(
            rollback.title.1.as_ref(),
            "Roll back to BroScribe 1.2.0 (build 10)?"
        );
        assert!(
            rollback
                .warning
                .unwrap()
                .1
                .contains("BroScribe Agent restarts")
        );

        assert_eq!(
            Step::Install.args(),
            ["update", "install", "--expectation-stdin"].map(OsString::from)
        );
        assert_eq!(
            Step::Rollback.args(),
            ["update", "rollback", "--expectation-stdin"].map(OsString::from)
        );
        assert_eq!(
            Step::SettingsSet {
                enabled: true,
                expect_revision: 7
            }
            .args(),
            [
                "update",
                "settings",
                "set",
                "--enabled",
                "true",
                "--expect-revision",
                "7"
            ]
            .map(OsString::from)
        );
        assert!(Step::Download.cancellable() && Step::Check.cancellable());
        for step in [Step::Install, Step::Rollback, Step::Recover, Step::Confirm] {
            assert!(
                !step.cancellable(),
                "{step:?} replaces files: never cancelled midway"
            );
        }
        let refusal = |code: &str, message: &str| refusal_text(&CliError::new(code, message));
        assert!(refusal("recording_active", "").contains("after it is saved"));
        assert!(
            refusal(
                "update_installation_contents",
                "Unexpected installation entries: meeting-library. Move them out before retrying."
            )
            .contains("meeting-library. Move them out")
        );
        assert!(
            refusal(
                "update_restart_library_mismatch",
                "Restart BroScribe Agent first so it uses the configured library, then update."
            )
            .contains("Restart BroScribe Agent first")
        );
        assert!(refusal("update_signer_mismatch", "").contains("Nothing was installed"));
        assert_eq!(
            refusal("update_hash_mismatch", "bad hash"),
            "Couldn't continue the update. Bad hash."
        );
        assert_eq!(
            last_check_failure(&CliError::new("spawn", "/x/broscribe: not found")),
            "Couldn't check for updates. BroScribe's command-line tool couldn't start."
        );
        assert!(
            last_check_failure(&CliError::new("trust_store_unavailable", "x"))
                .starts_with("The last check didn't finish. ")
        );
    }

    /// Automatic checks start no child (and so use no network) while
    /// updates are off or not set up, and publish the offer when on.
    #[cfg(unix)]
    #[test]
    fn automatic_checks_run_nothing_while_off_and_report_offers_when_on() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let log = dir.path().join("args.log");
        let cli_path = dir.path().join("synthetic-cli");
        let offer = json!({
            "state": "available", "version": "1.2.0", "build": 11,
            "target": "aarch64-apple-darwin", "size": 1024, "installed": identity(10),
            "signer_pin": "TEAMID1234",
        });
        std::fs::write(
            &cli_path,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\necho '{offer}'\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&cli_path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cli = crate::test_support::runner(cli_path, dir.path().join("lib"));

        assert_eq!(
            automatic_check(&data, &cli),
            AutoCheck::Checked(parse_check(&offer).unwrap()),
            "the built-in source is on by default"
        );
        std::fs::remove_file(&log).unwrap();
        let config = data.join("config.toml");
        std::fs::write(&config, "[updates]\nenabled = false\n").unwrap();
        assert_eq!(automatic_check(&data, &cli), AutoCheck::Off);
        std::fs::write(
            &config,
            "[updates]\nenabled = true\nfeed_url = 'https://updates.example/feed.json'\n",
        )
        .unwrap();
        assert_eq!(
            automatic_check(&data, &cli),
            AutoCheck::Off,
            "no trusted key"
        );
        assert!(!log.exists(), "nothing ran while off");

        std::fs::write(
            &config,
            format!(
                "[updates]\nenabled = true\nfeed_url = 'https://updates.example/feed.json'\n\
                 trusted_keys = ['{}']\n",
                "ab".repeat(32)
            ),
        )
        .unwrap();
        assert_eq!(
            automatic_check(&data, &cli),
            AutoCheck::Checked(CheckResult::Available(Offer {
                release: release(11),
                size: 1024,
                release_notes: None,
                installed: release(10),
                signer_pin: Some("TEAMID1234".into()),
            }))
        );
        let ran = std::fs::read_to_string(&log).unwrap();
        assert!(ran.contains("--json update check"), "{ran}");
        assert!(!ran.contains("download") && !ran.contains("install"));
    }

    /// A synthetic `broscribe` in `dir`, run through the app's shared
    /// [`CliRunner`] as the view runs it, that logs its arguments, answers
    /// `update status` with `dir/status.json`, `update settings show` with
    /// `dir/settings.json` when there is one (its whole stdout before it
    /// writes to stderr and exits), `update download` by running
    /// `dir/download.sh`, `update recover` by running `dir/recover.sh`, and
    /// accepts an install or rollback only when the
    /// expectation on its stdin (kept in `dir/stdin.json`) names the update
    /// build in `dir/pending`, as the real updater compares it under its
    /// locks; anything else is refused with `update_changed`.
    #[cfg(unix)]
    fn synthetic_cli(dir: &Path) -> CliRunner {
        use std::os::unix::fs::PermissionsExt as _;

        let script = r#"#!/bin/sh
dir='@DIR@'
echo "$*" >> "$dir/args.log"
case "$*" in
*'update status'*) cat "$dir/status.json" ;;
*'settings show'*)
  if [ -f "$dir/settings.json" ]; then
    cat "$dir/settings.json"
  else
    echo '{"revision":3,"updates":{"enabled":true}}'
  fi
  echo 'settings shown' >&2 ;;
*'update check'*) . "$dir/check.sh" ;;
*'update confirm'*) . "$dir/confirm.sh" ;;
*'settings set'*) echo '{"revision":4}' ;;
*'update download'*) . "$dir/download.sh" ;;
*'update recover'*) . "$dir/recover.sh" ;;
*'update install --expectation-stdin'*|*'update rollback --expectation-stdin'*)
  cat > "$dir/stdin.json"
  if grep -q "\"to\":{[^}]*\"build\":$(cat "$dir/pending")," "$dir/stdin.json"; then
    echo '{"state":"awaiting_confirmation","fence":4}'
  else
    echo '{"error":{"code":"update_changed","message":"the pending update changed"}}'
    exit 1
  fi ;;
*) echo '{"error":{"code":"internal","message":"unexpected"}}'; exit 1 ;;
esac
"#
        .replace("@DIR@", &dir.display().to_string());
        let cli = dir.join("synthetic-cli");
        std::fs::write(&cli, script).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::test_support::runner(cli, dir.join("library"))
    }

    /// How many times the synthetic CLI was asked for `what`.
    #[cfg(unix)]
    fn ran(dir: &Path, what: &str) -> usize {
        std::fs::read_to_string(dir.join("args.log"))
            .unwrap_or_default()
            .matches(what)
            .count()
    }

    /// What the synthetic updater has pending: `update` as `update status`
    /// reports it, and the build an expectation must name.
    #[cfg(unix)]
    fn set_pending(dir: &Path, update: Value, build: &str) {
        let status = json!({
            "settings": { "enabled": true, "feed_configured": true, "trusted_keys": 1,
                          "signer": "TEAMID1234" },
            "update": update,
            "agent": { "running": true }, "handed_off": null, "runner": null,
            "installed": identity(10),
        });
        std::fs::write(dir.join("status.json"), status.to_string()).unwrap();
        std::fs::write(dir.join("pending"), build).unwrap();
    }

    #[cfg(unix)]
    fn staged(from: u64, to: u64) -> Value {
        json!({ "state": "staged", "from": identity(from), "to": identity(to), "signer": signer() })
    }

    #[cfg(unix)]
    fn sent_expectation(dir: &Path) -> Expectation {
        serde_json::from_slice(&std::fs::read(dir.join("stdin.json")).unwrap()).unwrap()
    }

    /// Build 11 is confirmed, then another process stages build 12 before
    /// the click: the confirmation carries exactly 11, is refused, the view
    /// reloads and nothing is retried; build 12 installs only after its own
    /// confirmation. `--yes` is never sent.
    #[cfg(unix)]
    #[gpui::test]
    fn a_confirmation_installs_only_the_update_it_named(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = synthetic_cli(dir.path());
        set_pending(dir.path(), staged(10, 11), "11");
        cx.update(gpui::init);
        let window = cx.add_window(|_, cx| UpdateView::new(cli.clone(), cx));
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                this.request_install(cx);
                assert_eq!(this.phase(), &Phase::Confirming);
                assert_eq!(this.pending(), Some(&Pending::Install(exact(10, 11))));
            })
            .unwrap();
        cx.run_until_parked();
        let log = dir.path().join("args.log");
        assert!(
            !std::fs::read_to_string(&log).unwrap().contains("install"),
            "nothing is installed before confirming"
        );

        set_pending(dir.path(), staged(10, 12), "12");
        window
            .update(cx, |this, _, cx| this.confirm_pending(cx))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(sent_expectation(dir.path()), exact(10, 11));
        window
            .update(cx, |this, _, _| {
                assert_eq!(this.phase(), &Phase::Ready, "no confirmation is reused");
                assert_eq!(this.pending(), None);
                assert_eq!(
                    this.message(),
                    Some(&(true, refusal_text(&CliError::new("update_changed", ""))))
                );
                assert_eq!(this.restart(), None, "nothing was installed");
                assert_eq!(
                    this.snapshot().map(|s| &s.state),
                    Some(&UpdateState::Staged(exact(10, 12))),
                    "reloaded"
                );
            })
            .unwrap();
        let sent = std::fs::read_to_string(&log).unwrap();
        assert_eq!(sent.matches("update install").count(), 1, "{sent}");

        window
            .update(cx, |this, _, cx| {
                this.request_install(cx);
                this.confirm_pending(cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(sent_expectation(dir.path()), exact(10, 12));
        window
            .update(cx, |this, _, _| {
                assert_eq!(this.restart(), Some(Restart::Relaunch))
            })
            .unwrap();
        assert!(!std::fs::read_to_string(&log).unwrap().contains("--yes"));
    }

    /// The installed update is confirmed for rollback, then kept elsewhere
    /// before the click: the rollback names it, is refused, and the view
    /// shows the updater idle again. `--yes` is never sent.
    #[cfg(unix)]
    #[gpui::test]
    fn a_rollback_confirmation_rolls_back_only_the_update_it_named(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = synthetic_cli(dir.path());
        let awaiting = json!({ "state": "awaiting_confirmation", "from": identity(10),
                               "to": identity(11), "signer": signer() });
        set_pending(dir.path(), awaiting, "11");
        cx.update(gpui::init);
        let window = cx.add_window(|_, cx| UpdateView::new(cli.clone(), cx));
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                this.request_rollback(cx);
                assert_eq!(this.phase(), &Phase::Confirming);
                assert_eq!(this.pending(), Some(&Pending::Rollback(exact(10, 11))));
            })
            .unwrap();
        set_pending(dir.path(), json!({ "state": "idle" }), "none");
        window
            .update(cx, |this, _, cx| this.confirm_pending(cx))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(sent_expectation(dir.path()), exact(10, 11));
        window
            .update(cx, |this, _, _| {
                assert_eq!(
                    this.message(),
                    Some(&(true, refusal_text(&CliError::new("update_changed", ""))))
                );
                assert_eq!(this.restart(), None);
                assert_eq!(this.pending(), None);
                assert_eq!(this.snapshot().map(|s| &s.state), Some(&UpdateState::Idle));
            })
            .unwrap();
        let sent = std::fs::read_to_string(dir.path().join("args.log")).unwrap();
        assert_eq!(sent.matches("update rollback").count(), 1, "{sent}");
        assert!(!sent.contains("--yes"));
    }

    /// `update settings show` writes a valid answer far larger than a pipe
    /// before its stderr closes: the view reads it whole and is ready, never
    /// left loading; past the runner's output limit the same answer is an
    /// explicit error, still not loading.
    #[cfg(unix)]
    #[gpui::test]
    fn a_large_settings_answer_completes_or_fails_as_too_large(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = synthetic_cli(dir.path());
        set_pending(dir.path(), json!({ "state": "idle" }), "none");
        let keys: Vec<String> = (0..20_000u32).map(|i| format!("{i:064x}")).collect();
        let settings = json!({
            "revision": 3,
            "updates": { "enabled": true, "channel": "stable",
                         "feed_url": "https://updates.example/feed.json", "trusted_keys": keys },
        })
        .to_string();
        assert!(settings.len() > 1 << 20, "larger than any pipe buffer");
        std::fs::write(dir.path().join("settings.json"), &settings).unwrap();
        cx.update(gpui::init);

        let window = cx.add_window({
            let cli = cli.clone();
            move |_, cx| UpdateView::new(cli, cx)
        });
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert_eq!(this.phase(), &Phase::Ready);
                assert_eq!(this.message(), None);
                assert_eq!(this.settings_revision, Some(3));
            })
            .unwrap();

        let mut limited = cli;
        limited.set_output_limit(64 << 10);
        let window = cx.add_window(move |_, cx| UpdateView::new(limited, cx));
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert_eq!(this.phase(), &Phase::Ready, "never left loading");
                assert_eq!(this.settings_revision, None);
                let (error, text) = this.message().expect("an explicit error");
                assert!(*error);
                // Too large is a failure of the tool in words; its code and
                // the byte count stay in the log (WRT-04).
                assert_eq!(
                    text,
                    "Couldn't continue the update. BroScribe's command-line tool stopped \
                     unexpectedly."
                );
            })
            .unwrap();
        assert_eq!(ran(dir.path(), "settings show"), 2);
    }

    /// A download floods progress while nothing reads it, among malformed,
    /// non-UTF-8 and over-long stderr lines: only the newest value and one
    /// wake-up wait, the progress after the bad lines still arrives, and the
    /// child's own answer ends the step.
    #[cfg(unix)]
    #[test]
    fn a_progress_flood_holds_only_the_newest_value_and_bad_stderr_never_stops_it() {
        let dir = tempfile::tempdir().unwrap();
        let cli = synthetic_cli(dir.path());
        let download = format!(
            r#"i=0
while [ $i -lt 5000 ]; do
  echo "{{\"progress\":{{\"step\":\"download\",\"bytes\":$i,\"total\":5000}}}}" >&2
  i=$((i+1))
done
head -c {long} /dev/zero | tr '\000' x >&2
echo >&2
echo '{{"progress":' >&2
printf '\377\376 not text\n' >&2
echo '{{"progress":{{"step":"download","bytes":5000,"total":5000}}}}' >&2
echo '{{"state":"staged","size":5000}}'
"#,
            long = crate::jobs::MAX_PROGRESS_LINE + 1
        );
        std::fs::write(dir.path().join("download.sh"), download).unwrap();
        let (progress, mut wake) = DownloadProgress::new();
        let cancel = Arc::new(AtomicBool::new(false));

        let answer = run_step(&cli, Step::Download, None, &cancel, &progress);
        assert_eq!(answer, Ok(json!({ "state": "staged", "size": 5000 })));
        assert_eq!(progress.take(), Some((5000, 5000)), "the newest progress");
        assert_eq!(progress.take(), None);
        progress.close();
        assert_eq!(wake.try_recv().ok(), Some(()), "one pending wake-up");
        assert!(
            matches!(wake.try_recv(), Err(mpsc::TryRecvError::Closed)),
            "and no queue behind it"
        );
        assert_eq!(ran(dir.path(), "update download"), 1);
    }

    /// Cancelling a download stops its contained child mid-step: the step
    /// ends as cancelled at once, not when the child would have finished.
    #[cfg(unix)]
    #[test]
    fn a_cancelled_download_stops_its_child_and_ends_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let cli = synthetic_cli(dir.path());
        std::fs::write(
            dir.path().join("download.sh"),
            "echo '{\"progress\":{\"step\":\"download\",\"bytes\":1,\"total\":2}}' >&2\n\
             exec sleep 60\n",
        )
        .unwrap();
        let (progress, mut wake) = DownloadProgress::new();
        let cancel = Arc::new(AtomicBool::new(false));
        let started = Instant::now();
        let step = std::thread::spawn({
            let (cancel, progress) = (Arc::clone(&cancel), Arc::clone(&progress));
            move || {
                let answer = run_step(&cli, Step::Download, None, &cancel, &progress);
                progress.close();
                answer
            }
        });
        // The child runs: its first progress arrived.
        assert_eq!(futures::executor::block_on(wake.next()), Some(()));
        assert_eq!(progress.take(), Some((1, 2)));
        cancel.store(true, Ordering::SeqCst);
        let answer = step.join().unwrap();
        assert_eq!(answer.map_err(|e| e.code), Err("cancelled".to_owned()));
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "stopped, not waited for"
        );
    }

    /// Through the view: a check or download cancelled before its child
    /// starts ends as cancelled and runs nothing; a download ends with its
    /// own answer; an install keeps its phase through a cancel and a
    /// re-show, and ends with its own answer, never relabelled cancelled.
    #[cfg(unix)]
    #[gpui::test]
    fn only_checks_and_downloads_cancel_and_every_step_ends_with_its_own_answer(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let cli = synthetic_cli(dir.path());
        set_pending(dir.path(), json!({ "state": "idle" }), "none");
        std::fs::write(
            dir.path().join("download.sh"),
            "echo '{\"progress\":{\"step\":\"download\",\"bytes\":2,\"total\":2}}' >&2\n\
             echo '{\"state\":\"staged\",\"size\":2}'\n",
        )
        .unwrap();
        cx.update(gpui::init);
        let window = cx.add_window(|_, cx| UpdateView::new(cli.clone(), cx));
        cx.run_until_parked();

        let starts: [fn(&mut UpdateView, &mut Context<UpdateView>); 2] =
            [UpdateView::check_for_updates, UpdateView::download];
        for start in starts {
            window
                .update(cx, |this, _, cx| {
                    start(this, cx);
                    this.cancel(cx);
                })
                .unwrap();
            cx.run_until_parked();
            window
                .update(cx, |this, _, _| {
                    assert_eq!(this.phase(), &Phase::Ready);
                    assert_eq!(
                        this.message(),
                        Some(&(true, refusal_text(&CliError::new("cancelled", ""))))
                    );
                })
                .unwrap();
        }
        assert_eq!(ran(dir.path(), "update check"), 0, "nothing started");
        assert_eq!(ran(dir.path(), "update download"), 0, "nothing started");

        window.update(cx, |this, _, cx| this.download(cx)).unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert_eq!(this.phase(), &Phase::Ready);
                assert_eq!(this.message(), None, "staged: its own answer");
            })
            .unwrap();
        assert_eq!(ran(dir.path(), "update download"), 1);

        set_pending(dir.path(), staged(10, 11), "11");
        window.update(cx, |this, _, cx| this.refresh(cx)).unwrap();
        cx.run_until_parked();
        let reads = ran(dir.path(), "update status");
        window
            .update(cx, |this, _, cx| {
                this.request_install(cx);
                this.confirm_pending(cx);
                // Neither stops nor ends the install.
                this.cancel(cx);
                this.refresh(cx);
                assert_eq!(this.phase(), &Phase::Running(Step::Install));
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert_eq!(this.phase(), &Phase::Ready);
                assert_eq!(this.restart(), Some(Restart::Relaunch));
                let (error, text) = this.message().expect("the install's answer");
                assert!(!*error && text.contains("is installed"), "{text}");
            })
            .unwrap();
        assert_eq!(sent_expectation(dir.path()), exact(10, 11));
        assert_eq!(ran(dir.path(), "update install"), 1);
        assert_eq!(
            ran(dir.path(), "update status"),
            reads + 1,
            "read again only once the install answered"
        );
    }

    /// What assistive technology gets for `element`: its ID, role, name
    /// ([`crate::ui::a11y_name`]) and description; `None` when it gets no
    /// node (no ID or no role).
    #[cfg(unix)]
    fn a11y_node<E: gpui::Element>(element: &E) -> Option<A11yNode> {
        let (id, role, name) = crate::ui::a11y_name(element)?;
        Some((id, role, name, crate::ui::a11y_description(element)))
    }

    #[cfg(unix)]
    type A11yNode = (String, Role, String, String);

    /// Every node the view shows, through the node writer.
    #[cfg(unix)]
    fn nodes(this: &UpdateView, cx: &Context<UpdateView>) -> Vec<A11yNode> {
        let nodes: Vec<A11yNode> = this
            .parts(cx)
            .iter()
            .filter_map(|part| match part {
                Part::Node(node) => Some(a11y_node(node).expect("every node is identified")),
                Part::Control(_) => None,
            })
            .collect();
        let ids: std::collections::BTreeSet<&str> =
            nodes.iter().map(|(id, ..)| id.as_str()).collect();
        assert_eq!(ids.len(), nodes.len(), "IDs are unique: {nodes:?}");
        assert!(
            nodes
                .iter()
                .all(|(_, _, label, _)| !label.trim().is_empty())
        );
        nodes
    }

    #[cfg(unix)]
    fn find<'a>(nodes: &'a [A11yNode], id: &str) -> &'a A11yNode {
        nodes
            .iter()
            .find(|(i, ..)| i == id)
            .unwrap_or_else(|| panic!("no {id} in {nodes:?}"))
    }

    /// The install and rollback confirmations carry their whole disclosure
    /// (both builds, both targets, the signer) to assistive technology, and
    /// every status, progress, refusal and state line carries its words.
    #[cfg(unix)]
    #[gpui::test]
    fn update_disclosures_statuses_and_refusals_reach_assistive_technology(
        cx: &mut gpui::TestAppContext,
    ) {
        let on = |build: u64, target: &str| {
            json!({ "product": "BroScribe", "app_id": "org.broscribe.BroScribe",
                    "version": "1.2.0", "build": build, "target": target })
        };
        let x86 = "x86_64-apple-darwin";
        let dir = tempfile::tempdir().unwrap();
        let cli = synthetic_cli(dir.path());
        let staged_on = |to: u64| {
            json!({ "state": "staged", "from": identity(10), "to": on(to, x86),
                    "signer": signer() })
        };
        set_pending(dir.path(), staged_on(11), "11");
        cx.update(gpui::init);
        let window = cx.add_window(|_, cx| UpdateView::new(cli.clone(), cx));
        window
            .update(cx, |this, _, cx| {
                let (_, role, label, _) = find(&nodes(this, cx), "updates-status").clone();
                assert_eq!(
                    (role, label.as_str()),
                    (Role::Status, "Reading the update state…")
                );
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                let shown = nodes(this, cx);
                let (_, role, title, _) = find(&shown, "updates-title");
                assert_eq!((*role, title.as_str()), (Role::Heading, "Updates"));
                let (_, role, state, _) = find(&shown, "updates-state");
                assert_eq!(*role, Role::Label);
                assert_eq!(
                    state,
                    "BroScribe 1.2.0 (build 11) is downloaded and verified."
                );

                this.request_install(cx);
                let spec = replacement_spec(this.pending().unwrap());
                assert_eq!(
                    spec.title.1.as_ref(),
                    "Install BroScribe 1.2.0 (build 11) for Intel Macs?"
                );
                assert!(
                    spec.warning
                        .unwrap()
                        .1
                        .starts_with("BroScribe and BroScribe Agent restart.")
                );
                assert!(
                    spec.changes[0]
                        .1
                        .contains("for Apple silicon. Signed by TEAMID1234.")
                );
            })
            .unwrap();

        // Another process stages build 12 before the click: the refusal is
        // an Alert carrying its words, and the reloaded state names 12.
        set_pending(dir.path(), staged_on(12), "12");
        window
            .update(cx, |this, _, cx| {
                this.confirm_pending(cx);
                let shown = nodes(this, cx);
                let (_, role, running, _) = find(&shown, "updates-status");
                assert_eq!(
                    (*role, running.as_str()),
                    (Role::Status, Step::Install.running_text())
                );
                assert!(shown.iter().all(|(id, ..)| id != "updates-confirm"));
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                let shown = nodes(this, cx);
                let (_, role, refusal, _) = find(&shown, "updates-message");
                assert_eq!(*role, Role::Alert);
                assert_eq!(refusal, &refusal_text(&CliError::new("update_changed", "")));
                let (_, _, state, _) = find(&shown, "updates-state");
                assert!(state.contains("(build 12)"), "{state}");
            })
            .unwrap();

        let awaiting = json!({ "state": "awaiting_confirmation", "from": identity(10),
                               "to": on(11, x86), "signer": signer() });
        set_pending(dir.path(), awaiting, "11");
        window.update(cx, |this, _, cx| this.refresh(cx)).unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                this.request_rollback(cx);
                let spec = replacement_spec(this.pending().unwrap());
                assert_eq!(
                    spec.title.1.as_ref(),
                    "Roll back to BroScribe 1.2.0 (build 10) for Apple silicon?"
                );
                assert!(spec.warning.unwrap().1.contains("BroScribe Agent restarts"));
                this.cancel_pending(cx);

                // How a running download reads.
                this.enter(Phase::Downloading {
                    bytes: 512,
                    total: 1024,
                });
                let shown = nodes(this, cx);
                let (_, role, status, _) = find(&shown, "updates-status");
                assert_eq!(
                    (*role, status.as_str()),
                    (Role::Status, "Downloading · 50%")
                );
                this.enter(Phase::Ready);
            })
            .unwrap();

        let unavailable = json!({ "state": "unavailable", "code": "update_location_unsupported",
                                  "message": "read-only volume" });
        set_pending(dir.path(), unavailable, "none");
        window.update(cx, |this, _, cx| this.refresh(cx)).unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                let (_, role, state, _) = find(&nodes(this, cx), "updates-state").clone();
                assert_eq!(role, Role::Alert);
                assert_eq!(
                    state,
                    refusal_text(&CliError::new("update_location_unsupported", ""))
                );
            })
            .unwrap();
    }

    /// The runner gave up before replacing anything (a failed hand-off):
    /// the build stays staged and the agent still holds the step's durable
    /// admission. While a hand-off is still active nothing but the runner
    /// acts. Once it is gone, Install cannot take an admission and is not
    /// offered; the visible Finish recovery runs `update recover`, which
    /// (like the real CLI) removes the download and releases the admission
    /// after the unchanged build is proven, and the state read after it is
    /// idle with recording free. No install is ever sent.
    #[cfg(unix)]
    #[gpui::test]
    fn a_held_staged_update_offers_finish_recovery_instead_of_install(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let cli = synthetic_cli(dir.path());
        set_pending(dir.path(), staged(10, 11), "11");
        let status_path = dir.path().join("status.json");
        let mut held: Value =
            serde_json::from_slice(&std::fs::read(&status_path).unwrap()).unwrap();
        held["agent"]["install"] = json!({ "fence": 4, "durable": true });
        held["runner"] = json!({
            "schema": 1, "fence": 4, "finished_at": "2026-11-17T00:00:00Z",
            "outcome": { "state": "failed", "code": "update_runner_timeout",
                         "message": "gave up waiting for BroScribe to quit; nothing was replaced" },
        });
        let mut handing_off = held.clone();
        handing_off["handed_off"] = json!({ "kind": "install" });
        std::fs::write(&status_path, handing_off.to_string()).unwrap();
        // What `update status` reads once recovery discarded the stage and
        // released the admission.
        let mut recovered = held.clone();
        recovered["update"] = json!({ "state": "idle" });
        recovered["agent"]["install"] = Value::Null;
        recovered["runner"] = Value::Null;
        std::fs::write(dir.path().join("recovered.json"), recovered.to_string()).unwrap();
        std::fs::write(
            dir.path().join("recover.sh"),
            "cp \"$dir/recovered.json\" \"$dir/status.json\"\n\
             echo '{\"state\":\"discarded_staged\",\"admission\":{\"released\":4}}'\n",
        )
        .unwrap();
        cx.update(gpui::init);
        let (view, cx) = cx.add_window_view(move |_, cx| UpdateView::new(cli, cx));
        cx.run_until_parked();
        cx.simulate_resize(gpui::size(gpui::px(900.), gpui::px(1400.)));
        cx.update(|window, cx| window.draw(cx).clear(cx));
        // An active hand-off is never bypassed.
        assert!(cx.debug_bounds("updates-release").is_none());
        view.update(cx, |this, cx| {
            let snapshot = this.snapshot().expect("the state was read");
            assert!(snapshot.admission_held && snapshot.handed_off);
            assert!(nodes(this, cx).iter().all(|(id, ..)| id != "updates-held"));
        });

        std::fs::write(&status_path, held.to_string()).unwrap();
        view.update(cx, |this, cx| this.refresh(cx));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        view.update(cx, |this, cx| {
            let snapshot = this.snapshot().expect("the state was read");
            assert_eq!(snapshot.state, UpdateState::Staged(exact(10, 11)));
            assert!(snapshot.admission_held && !snapshot.handed_off);
            let shown = nodes(this, cx);
            assert_eq!(
                UpdateHold::of(snapshot),
                Some(UpdateHold::Held { handed_off: false })
            );
            let (_, _, runner, _) = find(&shown, "updates-runner");
            assert!(runner.starts_with("The update did not finish"), "{runner}");
        });
        let install = cx
            .debug_bounds("updates-install")
            .expect("Install is shown");
        cx.simulate_click(install.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        view.update(cx, |this, cx| {
            // Also asked directly: no confirmation while the hold stands.
            this.request_install(cx);
            assert_eq!(this.phase(), &Phase::Ready);
            assert_eq!(this.pending(), None);
        });

        cx.update(|window, cx| window.draw(cx).clear(cx));
        let finish = cx
            .debug_bounds("updates-release")
            .expect("Finish recovery is shown");
        cx.simulate_click(finish.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        view.update(cx, |this, cx| {
            assert_eq!(this.phase(), &Phase::Ready);
            assert_eq!(
                this.message(),
                Some(&(false, "Recovery finished.".to_owned()))
            );
            let snapshot = this.snapshot().expect("read again after recovering");
            assert_eq!(snapshot.state, UpdateState::Idle);
            assert!(!snapshot.admission_held, "the admission was released");
            assert_eq!(snapshot.runner, None);
            assert!(nodes(this, cx).iter().all(|(id, ..)| id != "updates-held"));
        });
        assert!(cx.debug_bounds("updates-release").is_none());
        assert_eq!(ran(dir.path(), "update recover"), 1);
        assert_eq!(ran(dir.path(), "update install"), 0, "no install was sent");
        assert_eq!(ran(dir.path(), "update discard"), 0);
    }
    #[cfg(unix)]
    fn modal_window(
        cx: &mut gpui::TestAppContext,
        cli: CliRunner,
    ) -> (Entity<UpdateView>, &mut gpui::VisualTestContext) {
        cx.update(gpui::init);
        let mut view = None;
        let (_, cx) = cx.add_window_view(|window, cx| {
            let updates = cx.new(|cx| UpdateView::new(cli, cx));
            view = Some(updates.clone());
            gpui::component::Root::new(updates, window, cx)
        });
        cx.run_until_parked();
        cx.simulate_resize(gpui::size(gpui::px(770.), gpui::px(960.)));
        (view.unwrap(), cx)
    }

    #[cfg(unix)]
    #[gpui::test]
    fn update_facts_automatic_switch_and_admin_command_are_actionable(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::ui::a11y_tree::draw_and_read;

        let dir = tempfile::tempdir().unwrap();
        let cli = synthetic_cli(dir.path());
        let status = dir.path().join("status.json");
        std::fs::write(
            &status,
            json!({
                "settings": {"enabled": true, "feed_configured": true, "trusted_keys": 1},
                "installed": identity(10), "update": {"state": "idle"}
            })
            .to_string(),
        )
        .unwrap();
        let (view, cx) = modal_window(cx, cli);
        let tree = cx.update(draw_and_read);
        assert_eq!(
            tree.nodes[tree.find("updates-version")].name,
            "Version: BroScribe 1.2.0 (build 10)"
        );
        assert_eq!(
            tree.nodes[tree.find("updates-last-checked")].name,
            "Last checked: Not checked yet"
        );
        assert_eq!(tree.nodes[tree.find("updates-auto")].role, "Switch");
        let pos = cx.debug_bounds("updates-auto").unwrap().center();
        cx.simulate_click(pos, gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(
            ran(
                dir.path(),
                "settings set --enabled false --expect-revision 3"
            ),
            1
        );

        let result = json!({"state": "available", "version": "1.3.0", "build": 11,
            "target": "aarch64-apple-darwin", "size": 1024, "installed": identity(10),
            "release_notes": "Synthetic change in this release."});
        let check = dir.path().join("check.sh");
        std::fs::write(&check, format!("echo '{result}'")).unwrap();
        let pos = cx.debug_bounds("updates-check").unwrap().center();
        cx.simulate_click(pos, gpui::Modifiers::none());
        cx.run_until_parked();
        let tree = cx.update(draw_and_read);
        let checked_at = view.read_with(cx, |this, cx| this.notices.read(cx).checked_at.unwrap());
        assert_eq!(
            tree.nodes[tree.find("updates-last-checked")].name,
            format!(
                "Last checked: {}",
                crate::ui::format::local_date_time(&checked_at)
            )
        );
        assert_eq!(
            tree.nodes[tree.find("updates-release-notes")].name,
            "Synthetic change in this release."
        );

        std::fs::write(
            &check,
            r#"echo '{"error":{"code":"network","message":"synthetic outage"}}'; exit 1"#,
        )
        .unwrap();
        view.update(cx, |this, cx| this.check_for_updates(cx));
        cx.run_until_parked();
        let tree = cx.update(draw_and_read);
        assert_eq!(tree.nodes[tree.find("updates-last-check")].role, "Alert");

        std::fs::write(&status, json!({"update": {"state": "off"}}).to_string()).unwrap();
        view.update(cx, |this, cx| this.refresh(cx));
        cx.run_until_parked();
        let tree = cx.update(draw_and_read);
        assert!(
            tree.nodes[tree.find("updates-not-configured")]
                .name
                .contains("can't check for new versions")
        );
        assert!(
            tree.nodes
                .iter()
                .all(|node| !node.name.contains(ADMIN_COMMAND))
        );
        let pos = cx.debug_bounds("updates-admin").unwrap().center();
        cx.simulate_click(pos, gpui::Modifiers::none());
        cx.update(draw_and_read);
        let pos = cx.debug_bounds("updates-admin-copy").unwrap().center();
        cx.simulate_click(pos, gpui::Modifiers::none());
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some(ADMIN_COMMAND)
        );
    }

    #[cfg(unix)]
    #[gpui::test]
    fn install_and_rollback_are_modal_and_cancel_restores_focus(cx: &mut gpui::TestAppContext) {
        use crate::ui::a11y_tree::draw_and_read;

        let dir = tempfile::tempdir().unwrap();
        let cli = synthetic_cli(dir.path());
        set_pending(dir.path(), staged(10, 11), "11");
        let (view, cx) = modal_window(cx, cli);
        for (button, title) in [
            ("updates-install", "Install BroScribe 1.2.0 (build 11)?"),
            (
                "updates-rollback",
                "Roll back to BroScribe 1.2.0 (build 10)?",
            ),
        ] {
            cx.update(draw_and_read);
            let bounds = cx.debug_bounds(button).unwrap();
            cx.simulate_click(bounds.center(), gpui::Modifiers::none());
            cx.run_until_parked();
            let tree = cx.update(draw_and_read);
            let dialog = tree
                .nodes
                .iter()
                .find(|node| node.role == "AlertDialog")
                .unwrap();
            assert_eq!(dialog.name, title);
            assert!(tree.nodes[tree.find("updates-restart-warning")].role == "Alert");
            assert!(tree.find("updates-restart-warning") < tree.find("updates-replacement"));
            cx.simulate_keystrokes("enter");
            cx.run_until_parked();
            assert_eq!(ran(dir.path(), "update install"), 0);
            assert_eq!(ran(dir.path(), "update rollback"), 0);
            cx.simulate_keystrokes("escape");
            cx.run_until_parked();
            cx.update(|window, cx| {
                let expected = view.read(cx);
                let expected = if button == "updates-install" {
                    &expected.install_focus
                } else {
                    &expected.rollback_focus
                };
                assert!(expected.is_focused(window));
            });
            set_pending(
                dir.path(),
                json!({"state":"awaiting_confirmation", "from":identity(10), "to":identity(11), "signer":signer()}),
                "11",
            );
            view.update(cx, |this, cx| this.refresh(cx));
            cx.run_until_parked();
        }
    }

    #[cfg(unix)]
    #[gpui::test]
    fn all_update_holds_reach_the_shell_and_keep_releases_the_shared_step(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::ui::a11y_tree::draw_and_read;

        struct BannerHost(Entity<UpdateView>);
        impl Render for BannerHost {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                UpdateBanner(self.0.clone())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let cli = synthetic_cli(dir.path());
        let install = json!({"fence": 4, "durable": true});
        let status_path = dir.path().join("status.json");
        let status = |update: Value, install: Value| {
            json!({
            "update": update, "agent": {"install": install, "running": true}, "installed": identity(11)
        }).to_string()
        };
        let waiting = json!({"state":"awaiting_confirmation", "from":identity(10), "to":identity(11), "signer":signer()});
        let interrupted = json!({"state":"interrupted", "from":identity(10), "to":identity(11)});
        std::fs::write(&status_path, status(interrupted.clone(), install.clone())).unwrap();
        cx.update(gpui::init);
        let view = cx.new(|cx| UpdateView::new(cli, cx));
        let observe = view.clone();
        let (_, cx) = cx.add_window_view(move |_, cx| {
            cx.observe(&observe, |_, _, cx| cx.notify()).detach();
            BannerHost(observe)
        });
        cx.simulate_resize(gpui::size(gpui::px(770.), gpui::px(420.)));
        cx.run_until_parked();

        for (state, expected) in [
            (interrupted, "An update didn't finish."),
            (json!({"state":"idle"}), "An update step didn't finish"),
            (waiting, "BroScribe 1.2.0 is installed."),
        ] {
            std::fs::write(&status_path, status(state, install.clone())).unwrap();
            view.update(cx, |this, cx| this.refresh(cx));
            cx.run_until_parked();
            let tree = cx.update(draw_and_read);
            let banner = &tree.nodes[tree.find("main-update-hold")];
            assert_eq!(banner.role, "Status");
            assert!(banner.name.starts_with(expected), "{}", banner.name);
            assert!(cx.update(|_, cx| crate::menu::update_waiting(cx)));
            assert_ne!(
                cx.update(|_, cx| UpdateNotices::current(cx).unwrap().menu_state(false)),
                crate::menu::UpdateMenu::Check
            );
        }
        std::fs::write(
            dir.path().join("ready.json"),
            status(json!({"state":"idle"}), Value::Null),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("confirm.sh"),
            r#"cat "$dir/ready.json" > "$dir/status.json"; echo '{"state":"confirmed"}'"#,
        )
        .unwrap();
        let keep = cx.debug_bounds("updates-keep").unwrap();
        cx.simulate_click(keep.center(), gpui::Modifiers::none());
        view.update(cx, |this, cx| this.keep(cx));
        cx.run_until_parked();
        let tree = cx.update(draw_and_read);
        assert_eq!(
            tree.nodes[tree.find("main-update-message")].name,
            "BroScribe 1.2.0 is kept. Recording is available again."
        );
        assert!(!cx.update(|_, cx| crate::menu::update_waiting(cx)));
        assert_eq!(ran(dir.path(), "update confirm"), 1);
    }
}
