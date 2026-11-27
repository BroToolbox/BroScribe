//! Settings (plan §4.5; §8 items 10, 14, 17, 18, 21; design §7.2).
//!
//! Sections: General (library folder, default language, microphone for
//! recordings, menu bar at login), Storage & retention, Models, Privacy, AI
//! providers, Summaries (what summaries focus on), Templates and Agents.
//! Every change goes through the same files and
//! commands the CLI uses: config edits apply to a fresh `config.toml` under
//! the data directory's config lock (`Config::update`) on the background
//! executor, so a concurrent `broscribe setup` is never overwritten with
//! stale values, and the saved revision goes to [`AppState`], which keeps
//! only the newest. Model,
//! enrollment, microphone, open-at-login, provider and agent actions run CLI
//! children. The setup assistant opens inside this
//! view (see [`SettingsView::open_setup`]).
//!
//! Mounting: `cx.new(|cx| SettingsView::new(state, deps, window, cx))` and
//! render in the standalone Settings window.

use broscribe_core::{fsutil::directory_bytes, ops::enrollment_files};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ChildStdin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use broscribe_core::config::{AutoEnrich, Config, ReadingSize, Retention, ThemeChoice};
use broscribe_core::retention::PreviewFailure;
use broscribe_core::schema::Handling;
use broscribe_core::{Library, Result};
use broscribe_platform_native::NativePlatform;
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::input::{InputEvent, InputState, Textarea, TextareaState};
use gpui::component::switch::Switch;
use gpui::component::{ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, BackgroundExecutor, ClickEvent, Context, Div, ElementId,
    Entity, FocusHandle, Focusable as _, InteractiveElement as _, IntoElement, ParentElement as _,
    PathPromptOptions, Render, Role, SharedString, Stateful, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, WeakEntity, Window, div, px,
};
use serde_json::Value;

use crate::jobs::{ChildKind, CliError, CliRunner, WorkerState, core_user_text, failure_text};
use crate::state::{AppState, OtherLibrary, SettingsPage};
use crate::ui::format::bytes;
use crate::ui::kit::{self, StatusKind};
use crate::ui::type_scale::{TextRole, TextRoleExt as _};
use crate::ui::{heading, text_line, title};
use crate::views::agents::AgentsView;
use crate::views::generate::{save_instructions_command, saved_instructions};
use crate::views::import_sheet::ImportRuns;
use crate::views::providers::{ProviderScope, ProvidersView};
use crate::views::setup::{self, MeetingApp, SetupAssistant, SetupDeps, Step};
use crate::views::templates::TemplatesSettings;
use crate::views::update::UpdateView;

mod speaker_grouping;
mod uninstall;
use speaker_grouping::SpeakerGroupingEditor;

/// Roles shown in the Models section, in this order; other roles a model
/// declares are listed after them.
pub(crate) const MODEL_ROLES: [&str; 4] = ["vad", "asr", "diarization", "speaker_embedding"];

fn ssd_caveat() -> String {
    ssd_caveat_for(NativePlatform::host_platform())
}

fn ssd_caveat_for(host: broscribe_platform::HostPlatform) -> String {
    use broscribe_platform::HostPlatform;
    let backups = match host {
        HostPlatform::MacOs => "Time Machine or synced backups",
        HostPlatform::Windows => "File History or synced backups",
        HostPlatform::Linux => "your backups",
    };
    format!(
        "Deleting removes the file from the library folder only. On SSDs the data can stay \
         recoverable until the space is reused. Copies in {backups} stay."
    )
}

// ---------------------------------------------------------------------------
// Shared helpers (also used by the setup assistant)
// ---------------------------------------------------------------------------

/// Applies `edit` to a fresh `config.toml` and saves it as the next
/// revision under the data directory's config lock (`Config::update`;
/// blocking, call off the main thread). Returns the saved config. `edit`
/// only changes values: it takes no other lock and does no I/O.
pub(crate) fn edit_config(data_dir: &Path, edit: impl FnOnce(&mut Config)) -> Result<Config> {
    Config::update(data_dir, |config| {
        edit(config);
        Ok(())
    })
    .map(|(config, ())| config)
}

/// [`edit_config`] on the background executor, then hands the saved config
/// to [`AppState`] (which drops it if a newer revision is already applied).
/// Resolves to the failure line on failure ("Couldn't save the settings.
/// {Cause}."), the error itself going to the log.
pub(crate) fn update_config(
    state: &Entity<AppState>,
    edit: impl FnOnce(&mut Config) + Send + 'static,
    cx: &mut App,
) -> Task<std::result::Result<(), String>> {
    let data_dir = state.read(cx).paths().data_dir.clone();
    let task = state
        .read(cx)
        .cli()
        .spawn(cx.background_executor(), async move {
            edit_config(&data_dir, edit)
        });
    let state = state.clone();
    cx.spawn(async move |cx| {
        let config = task.await.map_err(|e| {
            tracing::warn!(code = e.code(), "saving the settings");
            failure_text("Couldn't save the settings.", &core_user_text(&e), "")
        })?;
        state.update(cx, |s, cx| s.set_config(config, cx));
        Ok(())
    })
}

/// Re-reads `config.toml` (after a CLI child changed it) into [`AppState`].
pub(crate) fn reload_config(state: &Entity<AppState>, cx: &mut App) -> Task<()> {
    let data_dir = state.read(cx).paths().data_dir.clone();
    let task =
        state.read(cx).cli().spawn(
            cx.background_executor(),
            async move { Config::load(&data_dir) },
        );
    let state = state.clone();
    cx.spawn(async move |cx| match task.await {
        Ok(config) => state.update(cx, |s, cx| s.set_config(config, cx)),
        Err(e) => tracing::warn!(code = e.code(), "reloading the config"),
    })
}

/// Marks progress without conflating it with a measured calibration.
pub(crate) fn mark_setup_completed(config: &mut Config, step: &str) -> bool {
    if config.setup.completed_steps.iter().any(|s| s == step) {
        return false;
    }
    config.setup.completed_steps.push(step.to_owned());
    true
}

/// Marks a setup-assistant step completed in the config.
pub(crate) fn mark_step(
    state: &Entity<AppState>,
    step: &'static str,
    cx: &mut App,
) -> Task<std::result::Result<(), String>> {
    update_config(
        state,
        move |c| {
            mark_setup_completed(c, step);
        },
        cx,
    )
}

/// `OsString` arguments from string slices.
pub(crate) fn args<I, S>(items: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    items.into_iter().map(Into::into).collect()
}

/// Whether background processing is busy (library switching waits for it):
/// the queue worker, running jobs, or imports from the import sheet.
pub(crate) fn jobs_busy(state: &AppState, imports: &ImportRuns) -> bool {
    let jobs = state.jobs();
    !jobs.active.is_empty() || matches!(jobs.worker, WorkerState::Running) || imports.is_busy()
}

/// A library switch waits for recording and for the desktop's local jobs.
pub fn library_switch_busy(state: &AppState, imports: &ImportRuns, cx: &App) -> bool {
    state
        .daemon()
        .is_some_and(|daemon| daemon.read(cx).panel.recording.is_some())
        || jobs_busy(state, imports)
}

/// Which control initiated a retention change, so closing the confirmation
/// dialog returns keyboard focus to it (SET-01, §4.2 Focus).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetentionTrigger {
    Keep,
    Immediate,
    Named,
    Days,
    DaysSave,
}

/// Status line shown under the retention group during or after a failed dry
/// run (SET-01).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RetentionStatus {
    Pending,
    Saving,
    Failed(String),
}

/// Shown under the retention group when a save found other saved settings
/// than the choice was checked against, and checked it again.
const RETENTION_CHANGED_ELSEWHERE: &str =
    "Keep recordings changed elsewhere, so BroScribe checked your choice again.";

/// Shared state for retention-policy confirmation in Settings and Setup's
/// Storage step (SET-01).
pub(crate) struct RetentionGuard {
    status: Option<RetentionStatus>,
    /// A save found the saved policy or library changed and started over.
    changed_elsewhere: bool,
    generation: u64,
    keep_focus: FocusHandle,
    immediate_focus: FocusHandle,
    named_focus: FocusHandle,
    days_focus: FocusHandle,
    days_save_focus: FocusHandle,
    days_input: Option<Entity<InputState>>,
    confirming: bool,
}

impl RetentionGuard {
    pub(crate) fn new(cx: &App) -> Self {
        Self {
            status: None,
            changed_elsewhere: false,
            generation: 0,
            keep_focus: cx.focus_handle(),
            immediate_focus: cx.focus_handle(),
            named_focus: cx.focus_handle(),
            days_focus: cx.focus_handle(),
            days_save_focus: cx.focus_handle(),
            days_input: None,
            confirming: false,
        }
    }

    pub(crate) fn status(&self) -> Option<&RetentionStatus> {
        self.status.as_ref()
    }

    pub(crate) fn changed_elsewhere(&self) -> bool {
        self.changed_elsewhere
    }

    pub(crate) fn focus_for(&self, trigger: RetentionTrigger) -> &FocusHandle {
        match trigger {
            RetentionTrigger::Keep => &self.keep_focus,
            RetentionTrigger::Immediate => &self.immediate_focus,
            RetentionTrigger::Named => &self.named_focus,
            RetentionTrigger::Days => &self.days_focus,
            RetentionTrigger::DaysSave => &self.days_save_focus,
        }
    }

    pub(crate) fn bind_days_input(&mut self, input: &Entity<InputState>, cx: &App) {
        self.days_save_focus = input.read(cx).focus_handle(cx);
        self.days_input = Some(input.clone());
    }

    pub(crate) fn checking_or_confirming(&self) -> bool {
        self.confirming
            || matches!(
                self.status,
                Some(RetentionStatus::Pending | RetentionStatus::Saving)
            )
    }

    /// Voids the check in flight and any confirmation it opened: a late
    /// answer is dropped, and Delete in a confirmation still on screen
    /// closes it without saving.
    pub(crate) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.changed_elsewhere = false;
        self.confirming = false;
        if matches!(
            self.status,
            Some(RetentionStatus::Pending | RetentionStatus::Saving)
        ) {
            self.status = None;
        }
    }
}

/// The saved settings a retention choice was decided against. A save writes
/// only while `config.toml` still holds them (SET-01).
#[derive(Clone, Debug, PartialEq)]
struct RetentionBaseline {
    retention: Retention,
    /// The saved library: the check ran on the library it names.
    library: Option<PathBuf>,
}

impl RetentionBaseline {
    fn of(config: &Config) -> Self {
        Self {
            retention: config.retention,
            library: config.library.clone(),
        }
    }

    fn holds_in(&self, config: &Config) -> bool {
        config.retention == self.retention && config.library == self.library
    }
}

/// What a proposed policy deletes, from `broscribe retention apply --dry-run
/// --policy <policy> --json` (SET-01).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RetentionPreview {
    /// Meeting recordings deleted at once, and their size.
    recordings: u64,
    bytes: u64,
    /// Meeting recordings deleted once the jobs working on them finish.
    held: u64,
    held_bytes: u64,
    /// Kept voice readings (`people/*/enrollment/*.wav`).
    voice_readings: u64,
}

impl RetentionPreview {
    fn meetings(self) -> u64 {
        self.recordings.saturating_add(self.held)
    }

    fn meeting_bytes(self) -> u64 {
        self.bytes.saturating_add(self.held_bytes)
    }

    fn deletes_nothing(self) -> bool {
        self.meetings() == 0 && self.voice_readings == 0
    }
}

const UNEXPECTED_ANSWER: &str = "The check gave an unexpected answer";

/// Reads the dry run's answer for `proposed`: a dry run of that very policy
/// with every count, or the cause for the Failed line. A meeting, or a
/// profile holding a kept voice reading, that the check couldn't read
/// leaves what goes unknown, so it fails too.
fn parse_retention_dry_run(
    value: &Value,
    proposed: Retention,
) -> std::result::Result<RetentionPreview, String> {
    if value["dry_run"].as_bool() != Some(true)
        || value["policy"].as_str() != Some(proposed.cli_value().as_str())
    {
        return Err(UNEXPECTED_ANSWER.to_owned());
    }
    let count = |key: &str| value[key].as_u64();
    let (
        Some(recordings),
        Some(bytes),
        Some(held),
        Some(held_bytes),
        Some(voice_readings),
        Some(unreadable),
        Some(unreadable_people),
    ) = (
        count("recordings"),
        count("bytes"),
        count("kept_for_live_jobs"),
        count("kept_for_live_jobs_bytes"),
        count("enrollment_files_removed"),
        count("unreadable"),
        count("unreadable_people"),
    )
    else {
        return Err(UNEXPECTED_ANSWER.to_owned());
    };
    let meetings =
        (unreadable > 0).then(|| crate::ui::format::count(unreadable, "meeting", "meetings"));
    let profiles = (unreadable_people > 0).then(|| {
        if unreadable_people == 1 {
            "1 person's profile".to_owned()
        } else {
            format!("{unreadable_people} people's profiles")
        }
    });
    match (meetings, profiles) {
        (None, None) => {}
        (Some(what), None) | (None, Some(what)) => return Err(format!("{what} couldn't be read")),
        (Some(meetings), Some(profiles)) => {
            return Err(format!("{meetings} and {profiles} couldn't be read"));
        }
    }
    Ok(RetentionPreview {
        recordings,
        bytes,
        held,
        held_bytes,
        voice_readings,
    })
}

/// The cause shown for a failed check, never a path or an ID: the CLI's own
/// cause when it is a [`PreviewFailure`], else one for the error's code (the
/// message itself is never shown).
fn retention_check_cause(err: &CliError) -> &'static str {
    // raw-text-allowed: read as a known PreviewFailure, never shown.
    if let Some(failure) = PreviewFailure::from_text(err.message.trim()) {
        return failure.text();
    }
    match err.code.as_str() {
        "spawn" => "BroScribe's command-line tool didn't start",
        "cancelled" => "The check was cancelled",
        "bad_output" | "output_too_large" => UNEXPECTED_ANSWER,
        // The runner's own I/O failures share the CLI's `io` code.
        "io" | "exit" => "The check didn't finish",
        code if code == broscribe_core::maintenance::ERROR_CODE => {
            "BroScribe is in maintenance mode"
        }
        code => PreviewFailure::from_code(code, false).text(),
    }
}

fn retention_check_failed(cause: &str) -> RetentionStatus {
    RetentionStatus::Failed(format!(
        "Couldn't check which recordings this deletes. {cause}. Nothing changed."
    ))
}

/// The confirmation's words for what `preview` deletes (SET-01).
#[derive(Debug, PartialEq, Eq)]
struct RetentionConfirmText {
    title: String,
    warning: &'static str,
    goes: Vec<String>,
    action: String,
}

fn retention_confirm_text(preview: RetentionPreview) -> RetentionConfirmText {
    let meetings = preview.meetings();
    let mut goes = Vec::new();
    if meetings > 0 {
        goes.push(format!(
            "The recordings of {} already in your library ({})",
            crate::ui::format::count(meetings, "meeting", "meetings"),
            bytes(preview.meeting_bytes())
        ));
        let held = preview.held;
        if held > 0 {
            goes.push(match (held == meetings, held == 1) {
                (true, true) => "It goes when its processing finishes".to_owned(),
                (true, false) => "They go when their processing finishes".to_owned(),
                (false, true) => "1 of them goes when its processing finishes".to_owned(),
                (false, false) => format!("{held} of them go when their processing finishes"),
            });
        }
    }
    goes.push("Later recordings, once they qualify".to_owned());
    match preview.voice_readings {
        0 => {}
        1 => goes.push("Your kept voice reading".to_owned()),
        n => goes.push(format!("Your {n} kept voice readings")),
    }
    if meetings > 0 {
        let recordings = crate::ui::format::count(meetings, "recording", "recordings");
        RetentionConfirmText {
            title: format!("Delete {recordings} now?"),
            warning: "Without the audio you can't replay these meetings or transcribe them \
                      again with another model.",
            goes,
            action: format!("Delete {recordings}"),
        }
    } else if preview.voice_readings == 1 {
        RetentionConfirmText {
            title: "Delete your kept voice reading now?".to_owned(),
            warning: "Without it you can't replay the passage you read aloud or re-embed it \
                      with another speaker model.",
            goes,
            action: "Delete voice reading".to_owned(),
        }
    } else {
        RetentionConfirmText {
            title: "Delete your kept voice readings now?".to_owned(),
            warning: "Without them you can't replay the passages you read aloud or re-embed \
                      them with another speaker model.",
            goes,
            action: "Delete voice readings".to_owned(),
        }
    }
}

/// Told once a choice is saved; a failed save is the retention group's own
/// line ([`RetentionStatus::Failed`]), beside the choice.
type RetentionSaved<V> = Rc<dyn Fn(&mut V, &mut Context<V>)>;

/// One retention choice on its way to `config.toml` (SET-01).
struct RetentionRequest<V> {
    state: Entity<AppState>,
    guard: fn(&mut V) -> &mut RetentionGuard,
    proposed: Retention,
    trigger: RetentionTrigger,
    on_saved: RetentionSaved<V>,
    /// This choice's number, and the newest choice's
    /// ([`AppState::retention_choices`]): a save writes only while they
    /// match.
    choice: u64,
    latest_choice: Arc<AtomicU64>,
}

impl<V> Clone for RetentionRequest<V> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            guard: self.guard,
            proposed: self.proposed,
            trigger: self.trigger,
            on_saved: self.on_saved.clone(),
            choice: self.choice,
            latest_choice: self.latest_choice.clone(),
        }
    }
}

/// What a retention save found under the config lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetentionWrite {
    Saved,
    /// A newer choice was made since: it decides, this one writes nothing.
    Superseded,
    /// `config.toml` no longer holds the baseline: nothing was written.
    ChangedElsewhere,
    /// A stricter choice, and the connected recording agent now runs on
    /// another library than the one checked: nothing was written.
    AgentElsewhere,
}

/// The line shown instead of a check while the saved settings or the
/// recording agent name another library than the open one: what goes there
/// is unknown here.
fn retention_other_library_line(other: OtherLibrary) -> String {
    let (cause, fix) = match other {
        OtherLibrary::SwitchPending => (
            "BroScribe opens the other library when it restarts",
            "Restart BroScribe, then choose again",
        ),
        OtherLibrary::LaunchEnv | OtherLibrary::LaunchFlag => (
            "BroScribe opened a different library at startup",
            "Restart BroScribe normally, then choose again",
        ),
        OtherLibrary::AgentElsewhere => (
            "BroScribe Agent is running on another library",
            "Restart BroScribe Agent, then choose again",
        ),
    };
    format!("Couldn't check which recordings this deletes. {cause}. Nothing changed. {fix}.")
}

/// Runs the retention safeguard shared by Settings and Setup's Storage step
/// (SET-01), against the saved settings as this view knows them:
/// - If `proposed` cannot delete more than the saved policy, saves at once.
/// - While the saved settings name another library than the open one (a
///   switch waiting for the restart, or a launch with `--library` or
///   `BROSCRIBE_LIBRARY`), or the connected recording agent runs on another
///   one, a stricter choice fails and says what to do: a check here would
///   describe the wrong library. The agent's library is checked again
///   when the save is written, under the config lock.
/// - Otherwise leaves the radios and config untouched, shows a Pending status
///   line, runs `broscribe retention apply --dry-run --policy <proposed> --json`
///   off the UI thread (generation-fenced), and:
///   - on a failure or an incomplete answer: shows a `Role::Alert` Failed
///     line and changes nothing;
///   - when nothing would go (no recording, held or not, and no kept voice
///     reading): saves at once with no dialog;
///   - otherwise: opens a modal `AlertDialog` and saves only after explicit
///     confirmation.
///
/// Every save writes only if this is still the newest choice and
/// `config.toml` still holds the policy and library the choice was decided
/// against, deciding under the config lock. A newer choice silently wins;
/// a changed file writes nothing, loads the saved settings, says so and
/// checks the choice again.
#[allow(clippy::too_many_arguments)]
pub(crate) fn request_retention_change<V: 'static>(
    this: &mut V,
    state: Entity<AppState>,
    guard: fn(&mut V) -> &mut RetentionGuard,
    proposed: Retention,
    trigger: RetentionTrigger,
    on_saved: impl Fn(&mut V, &mut Context<V>) + 'static,
    window: &mut Window,
    cx: &mut Context<V>,
) {
    guard(this).changed_elsewhere = false;
    let latest_choice = state.read(cx).retention_choices().clone();
    let choice = latest_choice.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
    let request = RetentionRequest {
        state,
        guard,
        proposed,
        trigger,
        on_saved: Rc::new(on_saved),
        choice,
        latest_choice,
    };
    start_retention_change(this, request, window, cx);
}

fn start_retention_change<V: 'static>(
    this: &mut V,
    request: RetentionRequest<V>,
    window: &mut Window,
    cx: &mut Context<V>,
) {
    let guard = request.guard;
    let (baseline, other_library) = {
        let state = request.state.read(cx);
        (
            RetentionBaseline::of(state.config()),
            state.settings_library_mismatch(),
        )
    };
    let generation = {
        let g = guard(this);
        g.generation = g.generation.wrapping_add(1);
        g.status = None;
        g.generation
    };
    cx.notify();

    if !request.proposed.may_delete_more_than(baseline.retention) {
        guard(this).status = Some(RetentionStatus::Saving);
        save_retention_change(cx.weak_entity(), request, baseline, generation, window, cx);
        return;
    }
    if let Some(other) = other_library {
        guard(this).status = Some(RetentionStatus::Failed(retention_other_library_line(other)));
        return;
    }

    let trigger_focus = {
        let g = guard(this);
        g.status = Some(RetentionStatus::Pending);
        g.focus_for(request.trigger).clone()
    };
    let policy_arg = request.proposed.cli_value();
    let dry_run = request.state.read(cx).run_cli(
        args(["retention", "apply", "--dry-run", "--policy", &policy_arg]),
        ChildKind::Command,
        cx,
    );
    cx.spawn_in(window, async move |this, cx| {
        let outcome = dry_run.await;
        let _ = this.update_in(cx, |this, window, cx| {
            if guard(this).generation != generation {
                return;
            }
            guard(this).status = None;
            cx.notify();
            let preview = outcome
                .map_err(|err| {
                    // The code only: the message may name a path.
                    tracing::warn!(code = %err.code, "checking a retention policy failed");
                    retention_check_cause(&err).to_owned()
                })
                .and_then(|value| parse_retention_dry_run(&value, request.proposed));
            match preview {
                Err(cause) => guard(this).status = Some(retention_check_failed(&cause)),
                Ok(preview) if preview.deletes_nothing() => {
                    guard(this).status = Some(RetentionStatus::Saving);
                    save_retention_change(
                        cx.weak_entity(),
                        request,
                        baseline,
                        generation,
                        window,
                        cx,
                    );
                }
                Ok(preview) => {
                    guard(this).confirming = true;
                    open_retention_confirm_dialog(
                        cx.weak_entity(),
                        request,
                        baseline,
                        generation,
                        preview,
                        trigger_focus,
                        window,
                        cx,
                    );
                }
            }
        });
    })
    .detach();
}

#[cfg(test)]
type WriteHold = futures::channel::oneshot::Receiver<()>;

#[cfg(test)]
thread_local! {
    /// Test hook: the next retention save waits for this before it takes
    /// the config lock (it then writes or not as usual).
    static HOLD_NEXT_RETENTION_WRITE: std::cell::RefCell<Option<WriteHold>> =
        const { std::cell::RefCell::new(None) };
}

/// Saves `request.proposed` only while it is still the newest choice and
/// `config.toml` still holds `baseline`, deciding both under the config lock
/// (`Config::update`). A superseded choice writes nothing and says nothing:
/// the newer one decides. A changed file writes nothing, hands the saved
/// settings to [`AppState`] and, unless the view moved on, says so and
/// checks the choice again. A stricter choice also writes only while the
/// connected recording agent's library, as it reported it last, is the open
/// one, decided under the same lock: an agent that has since connected on
/// another library saves nothing and the line says what to do. That check
/// loads the comparison the daemon client published when the state arrived
/// ([`crate::daemon::AgentLibraryFlag`], one atomic value), so the config
/// lock stays the last lock taken: the closure takes no other lock. A save
/// that fails (an unreadable `config.toml`, a full disk) says so on the
/// retention group's line, unless the view moved on; only a saved choice
/// reaches `on_saved`.
fn save_retention_change<V: 'static>(
    view: WeakEntity<V>,
    request: RetentionRequest<V>,
    baseline: RetentionBaseline,
    generation: u64,
    window: &mut Window,
    cx: &mut App,
) {
    let state = request.state.clone();
    let stricter = request.proposed.may_delete_more_than(baseline.retention);
    // An atomic value the daemon client publishes: the closure below reads
    // it under the config lock without taking another lock.
    let agent = state.read(cx).agent_library().clone();
    let data_dir = state.read(cx).paths().data_dir.clone();
    let proposed = request.proposed;
    let (choice, latest_choice) = (request.choice, request.latest_choice.clone());
    #[cfg(test)]
    let hold = HOLD_NEXT_RETENTION_WRITE.with(|hold| hold.borrow_mut().take());
    let write = state
        .read(cx)
        .cli()
        .spawn(cx.background_executor(), async move {
            #[cfg(test)]
            if let Some(hold) = hold {
                let _ = hold.await;
            }
            Config::update(&data_dir, |config| {
                if latest_choice.load(Ordering::SeqCst) != choice {
                    return Ok(RetentionWrite::Superseded);
                }
                if !baseline.holds_in(config) {
                    return Ok(RetentionWrite::ChangedElsewhere);
                }
                if stricter && agent.elsewhere() {
                    return Ok(RetentionWrite::AgentElsewhere);
                }
                config.retention = proposed;
                Ok(RetentionWrite::Saved)
            })
        });
    window
        .spawn(cx, async move |cx| {
            let outcome = write.await;
            let _ = view.update_in(cx, |this, window, cx| match outcome {
                Ok((config, RetentionWrite::Saved)) => {
                    state.update(cx, |s, cx| s.set_config(config, cx));
                    if (request.guard)(this).generation == generation {
                        (request.guard)(this).status = None;
                        (request.on_saved)(this, cx);
                    }
                    cx.notify();
                }
                Ok((config, RetentionWrite::Superseded)) => {
                    state.update(cx, |s, cx| s.set_config(config, cx));
                    if (request.guard)(this).generation == generation {
                        (request.guard)(this).status = None;
                        cx.notify();
                    }
                }
                Ok((config, RetentionWrite::ChangedElsewhere)) => {
                    state.update(cx, |s, cx| s.set_config(config, cx));
                    let guard = request.guard;
                    if guard(this).generation == generation {
                        guard(this).changed_elsewhere = true;
                        start_retention_change(this, request, window, cx);
                    }
                    cx.notify();
                }
                Ok((config, RetentionWrite::AgentElsewhere)) => {
                    state.update(cx, |s, cx| s.set_config(config, cx));
                    let guard = request.guard;
                    if guard(this).generation == generation {
                        guard(this).status = Some(RetentionStatus::Failed(
                            retention_other_library_line(OtherLibrary::AgentElsewhere),
                        ));
                    }
                    cx.notify();
                }
                // Beside the choice, on the retention group's own line, as a
                // failed check is: never beside the page's title.
                Err(e) => {
                    tracing::warn!(code = e.code(), "saving the retention choice");
                    let guard = request.guard;
                    if guard(this).generation == generation {
                        guard(this).status = Some(RetentionStatus::Failed(failure_text(
                            "Couldn't save the retention choice.",
                            &core_user_text(&e),
                            "",
                        )));
                    }
                    cx.notify();
                }
            });
        })
        .detach();
}

#[allow(clippy::too_many_arguments)]
fn open_retention_confirm_dialog<V: 'static>(
    view: WeakEntity<V>,
    request: RetentionRequest<V>,
    baseline: RetentionBaseline,
    generation: u64,
    preview: RetentionPreview,
    trigger_focus: FocusHandle,
    window: &mut Window,
    cx: &mut App,
) {
    use kit::confirm_line as line;
    let text = retention_confirm_text(preview);
    let acted = Rc::new(Cell::new(false));
    let cancel_view = view.clone();
    let cancel_request = request.clone();

    kit::confirm_dialog(
        trigger_focus,
        window,
        cx,
        move |_| {
            let mut spec = kit::ConfirmSpec::new(
                line("retention-confirm-title", text.title.clone()),
                "retention-confirm-body",
                "retention-confirm-footer",
                "retention-confirm-cancel",
                line("retention-confirm-delete", text.action.clone()),
            );
            spec.headings = [
                "retention-confirm-goes-heading",
                "retention-confirm-changes-heading",
                "retention-confirm-stays-heading",
            ];
            spec.warning = Some(line("retention-confirm-warning", text.warning));
            spec.goes = text
                .goes
                .iter()
                .enumerate()
                .map(|(ix, words)| line(("retention-confirm-goes", ix), words.clone()))
                .collect();
            spec.stays = vec![
                line(
                    ("retention-confirm-stays", 0usize),
                    "Transcripts, summaries, names and voice data",
                ),
                line(
                    ("retention-confirm-stays", 1usize),
                    "Files left in place at import",
                ),
            ];
            spec.caveat = Some(line("retention-confirm-ssd-caveat", ssd_caveat()));
            spec.irreversible_id = "retention-confirm-irreversible";
            Some(spec)
        },
        move |window, cx| {
            let _ = cancel_view.update(cx, |this, cx| {
                let guard = (cancel_request.guard)(this);
                if guard.generation == generation {
                    guard.confirming = false;
                    if cancel_request.trigger == RetentionTrigger::DaysSave
                        && let Some(input) = &guard.days_input
                    {
                        let saved = retention_days_text(cancel_request.state.read(cx).config());
                        input.update(cx, |input, cx| input.set_value(saved, window, cx));
                    }
                    cx.notify();
                }
            });
            true
        },
        move |window, cx| {
            if acted.replace(true) {
                return false;
            }
            // An answer for a previous check must never save the proposed policy.
            let guard = request.guard;
            let current = view
                .update(cx, |this, _| {
                    let guard = guard(this);
                    guard.confirming = false;
                    guard.generation == generation
                })
                .unwrap_or(false);
            if current {
                save_retention_change(
                    view.clone(),
                    request.clone(),
                    baseline.clone(),
                    generation,
                    window,
                    cx,
                );
            }
            true
        },
    );
}

pub(crate) use broscribe_core::config::valid_language_tag;

/// A secondary line of text, read as itself (`Role::Label`).
pub(crate) fn muted(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    text_line(id, Role::Label, text)
        .text_role(TextRole::Caption)
        .text_color(cx.theme().muted_foreground)
}

/// An error line, announced as an alert carrying its words.
pub(crate) fn error_line(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    cx: &App,
) -> Stateful<Div> {
    kit::status_line(id, StatusKind::Failed, text, cx)
}

/// The status-line kind and words of an action's outcome: `Done` for a
/// success, `Failed` (announced as an alert) for an error.
pub(crate) fn outcome(result: std::result::Result<String, String>) -> (StatusKind, String) {
    match result {
        Ok(text) => (StatusKind::Done, text),
        Err(text) => (StatusKind::Failed, text),
    }
}

pub(crate) fn row() -> Div {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap(kit::CONTROL_GAP)
}

// ---- models ----

/// One registered model, from `broscribe models list --json`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ModelRow {
    pub id: String,
    pub display_name: Option<String>,
    pub roles: Vec<String>,
    pub license: String,
    pub license_verified: bool,
    pub bytes: u64,
    /// `installed`, `missing` or `corrupt: …`.
    pub state: String,
    pub active_for: Vec<String>,
    /// Whether the speech model takes term hints (`None`: not reported).
    pub accepts_hotwords: Option<bool>,
    pub download_hosts: Vec<String>,
}

impl ModelRow {
    pub fn installed(&self) -> bool {
        self.state == "installed"
    }

    pub(crate) fn state_label(&self) -> &'static str {
        model_state_label(&self.state)
    }
}

fn model_state_label(state: &str) -> &'static str {
    match state {
        "installed" => "Downloaded",
        "missing" => "Not downloaded",
        state if state.starts_with("corrupt:") => "Damaged. Download again.",
        _ => "Download status unavailable",
    }
}

/// A profile `models list` offers (design §4.1 step 3): a choice of speech
/// models from the built-in registry.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ProfileChoice {
    pub id: String,
    /// Names the speech model.
    pub description: String,
    pub asr: String,
    /// The ASR model's registry languages (`*`: any).
    pub languages: Vec<String>,
    pub active: bool,
}

/// `models list --json`: rows plus the registry's profiles.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ModelsList {
    pub rows: Vec<ModelRow>,
    pub profiles: Vec<ProfileChoice>,
    /// Every model the active profile needs (VAD, ASR, live preview,
    /// diarization, speaker embedding, each once), as the CLI resolved them
    /// through the registry's `profile_models`; the error when it could not.
    pub profile_models: std::result::Result<Vec<String>, String>,
}

impl ModelsList {
    /// The name model `id` is shown by: the description of the profile whose
    /// speech model it is ("Parakeet TDT 0.6B v3") if its registry name is missing.
    pub(crate) fn model_name(&self, id: &str) -> String {
        self.rows
            .iter()
            .find(|row| row.id == id)
            .and_then(|row| row.display_name.as_deref())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .or_else(|| {
                self.profiles
                    .iter()
                    .find(|p| p.asr == id)
                    .map(|p| p.description.as_str())
            })
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or("Unnamed model")
            .to_owned()
    }
}

impl ModelsList {
    pub(crate) fn missing_profile_models(&self) -> Vec<&ModelRow> {
        self.profile_models.as_ref().map_or_else(
            |_| Vec::new(),
            |ids| {
                ids.iter()
                    .filter_map(|id| {
                        self.rows
                            .iter()
                            .find(|row| row.id == *id && !row.installed())
                    })
                    .collect()
            },
        )
    }
}

pub(crate) fn transcription_block(missing: &[&ModelRow]) -> Option<String> {
    (!missing.is_empty()).then(|| {
        let count = missing.len();
        let size = bytes(missing.iter().map(|row| row.bytes).sum());
        format!(
            "Transcription can’t run until {count} {} downloaded ({size}).",
            if count == 1 { "model is" } else { "models are" }
        )
    })
}

impl Default for ModelsList {
    /// The empty list (nothing listed yet): no rows, no profile models.
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            profiles: Vec::new(),
            profile_models: Ok(Vec::new()),
        }
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn parse_models_list(v: &Value) -> ModelsList {
    let rows = v["models"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    Some(ModelRow {
                        id: r["id"].as_str()?.to_owned(),
                        display_name: r["display_name"].as_str().map(str::to_owned),
                        roles: strings(&r["roles"]),
                        license: r["license"].as_str().unwrap_or("").to_owned(),
                        license_verified: r["license_verified"].as_bool().unwrap_or(false),
                        bytes: r["bytes"].as_u64().unwrap_or(0),
                        state: r["state"].as_str().unwrap_or("").to_owned(),
                        active_for: strings(&r["active_for"]),
                        accepts_hotwords: r["accepts_hotwords"].as_bool(),
                        download_hosts: strings(&r["download_hosts"]),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let profiles = v["profiles"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    Some(ProfileChoice {
                        id: p["id"].as_str()?.to_owned(),
                        description: p["description"].as_str().unwrap_or("").to_owned(),
                        asr: p["asr"].as_str().unwrap_or("").to_owned(),
                        languages: strings(&p["languages"]),
                        active: p["active"].as_bool().unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let profile_models = match (v["profile_models"].as_array(), &v["profile_models_error"]) {
        (Some(_), _) => Ok(strings(&v["profile_models"])),
        (None, Value::String(e)) => Err(e.clone()),
        (None, _) => Err("the model list does not name the profile's models".to_owned()),
    };
    ModelsList {
        rows,
        profiles,
        profile_models,
    }
}

/// The Words step's note when the active speech model ignores term hints:
/// that model and the registry profiles whose speech model takes them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HotwordNote {
    pub model: String,
    pub profiles: Vec<String>,
}

impl HotwordNote {
    pub fn text(&self) -> String {
        let mut text = format!("{} doesn’t use your word list.", self.model);
        if self.profiles.len() == 1 {
            text.push_str(&format!(" {} does.", self.profiles[0]));
        } else if !self.profiles.is_empty() {
            text.push_str(&format!(" These models do: {}.", self.profiles.join(", ")));
        }
        text
    }
}

/// [`HotwordNote`] from `models list`: the model active for `asr` (else
/// `configured_asr`), only when the list says it takes no hints.
pub(crate) fn hotword_note(list: &ModelsList, configured_asr: &str) -> Option<HotwordNote> {
    let accepts = |id: &str| {
        list.rows
            .iter()
            .find(|r| r.id == id)
            .and_then(|r| r.accepts_hotwords)
    };
    let model = list
        .rows
        .iter()
        .find(|r| r.active_for.iter().any(|role| role == "asr"))
        .map_or(configured_asr, |r| r.id.as_str());
    if accepts(model) != Some(false) {
        return None;
    }
    Some(HotwordNote {
        model: list.model_name(model),
        profiles: list
            .profiles
            .iter()
            .filter(|p| accepts(&p.asr) == Some(true))
            .map(|p| list.model_name(&p.asr))
            .collect(),
    })
}

/// Download state of one model.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PullState {
    /// Percent done, once the CLI reported it.
    Running(Option<u8>),
    /// Cancel was asked; the CLI's own answer ends the pull.
    Cancelling,
    Done,
    /// The CLI confirmed the cancellation: nothing new was installed.
    Cancelled,
    /// The failure line shown under the model.
    Failed(String),
}

impl PullState {
    /// The state a finished `models pull` of the model shown as `name`
    /// ([`ModelsList::model_name`]) leaves.
    pub(crate) fn finished(name: &str, result: &std::result::Result<Value, CliError>) -> Self {
        match result {
            Ok(_) => PullState::Done,
            Err(e) if e.code == "cancelled" => PullState::Cancelled,
            Err(e) => PullState::Failed(download_failure(name, e)),
        }
    }

    pub(crate) fn in_progress(&self) -> bool {
        matches!(self, PullState::Running(_) | PullState::Cancelling)
    }
}

/// “Couldn't download {model name}. {Cause}. Check your connection, then
/// try again.” (WRT-08): `name` is [`ModelsList::model_name`]; the CLI's own
/// `io` there is the download ([`CliError::download_text`]).
pub(crate) fn download_failure(name: &str, error: &CliError) -> String {
    failure_text(
        &format!("Couldn't download {name}."),
        &error.download_text(),
        "Check your connection, then try again.",
    )
}

/// “Couldn't open that folder as a library. {Cause}.” (SET-64, WRT-08).
pub(crate) fn library_open_failure(cause: &str) -> String {
    failure_text("Couldn't open that folder as a library.", cause, "")
}

/// Only an empty folder or a recognizable library can be opened for writing.
fn open_chosen_library(path: &Path) -> Result<Library> {
    use broscribe_core::Error;
    use broscribe_core::paths::{INDEX_DIR, PEOPLE_DIR, QUEUE_DIR};

    let entries = std::fs::read_dir(path).map_err(|e| Error::io_at(path, e))?;
    let mut has_content = false;
    for entry in entries {
        let entry = entry.map_err(|e| Error::io_at(path, e))?;
        let name = entry.file_name();
        let os_metadata = name.to_str().is_some_and(|name| {
            name == ".DS_Store"
                || name.eq_ignore_ascii_case("desktop.ini")
                || name.eq_ignore_ascii_case("Thumbs.db")
        }) || name.as_encoded_bytes().starts_with(b"._");
        if !os_metadata
            || !entry
                .file_type()
                .map_err(|e| Error::io_at(&entry.path(), e))?
                .is_file()
        {
            has_content = true;
            break;
        }
    }
    if !has_content {
        return Library::open(path);
    }
    match Library::open_existing(path) {
        Ok(lib) => {
            // Core's read-only open also recognizes a lone people or year
            // folder. Require actual library structure before writing here.
            let initialized = [PEOPLE_DIR, QUEUE_DIR, INDEX_DIR]
                .into_iter()
                .all(|name| path.join(name).is_dir());
            if initialized || !lib.list_meeting_dirs()?.is_empty() {
                return Library::open(path);
            }
        }
        Err(Error::NotFound(_)) => {}
        Err(e) => return Err(e),
    }
    Err(Error::Service {
        code: "not_a_library",
        message: "This folder already contains files and isn't a BroScribe library. Choose an empty folder or an existing BroScribe library.".into(),
    })
}

#[test]
fn folder_picker_refuses_to_initialize_a_nonempty_nonlibrary_folder() {
    use broscribe_core::paths::{INDEX_DIR, PEOPLE_DIR, QUEUE_DIR};

    let dir = tempfile::tempdir().unwrap();
    let unrelated = dir.path().join("unrelated");
    std::fs::create_dir(&unrelated).unwrap();
    let existing = unrelated.join(".synthetic-file");
    std::fs::write(&existing, b"synthetic").unwrap();
    for folder in ["2026", PEOPLE_DIR, INDEX_DIR] {
        assert_eq!(
            open_chosen_library(&unrelated).unwrap_err().code(),
            "not_a_library"
        );
        std::fs::create_dir(unrelated.join(folder)).unwrap();
    }
    assert_eq!(
        open_chosen_library(&unrelated).unwrap_err().code(),
        "not_a_library"
    );
    assert!(!unrelated.join(QUEUE_DIR).exists());
    assert_eq!(std::fs::read(existing).unwrap(), b"synthetic");

    let empty = dir.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    let first = open_chosen_library(&empty).unwrap();
    assert_eq!(open_chosen_library(&empty).unwrap().root(), first.root());
}

#[test]
fn folder_picker_accepts_folders_with_only_os_metadata_files() {
    let dir = tempfile::tempdir().unwrap();
    let names = [".DS_Store", "Desktop.INI", "THUMBS.DB", "._synthetic"];
    for name in names {
        std::fs::write(dir.path().join(name), b"metadata").unwrap();
    }
    open_chosen_library(dir.path()).unwrap();
    for name in names {
        assert_eq!(std::fs::read(dir.path().join(name)).unwrap(), b"metadata");
    }

    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("._directory")).unwrap();
    assert_eq!(
        open_chosen_library(dir.path()).unwrap_err().code(),
        "not_a_library"
    );
}

/// "Couldn't check the models. {Cause}." (WRT-08): `models doctor` failed.
pub(crate) fn verify_failure(cause: &str) -> String {
    failure_text("Couldn't check the models.", cause, "")
}

/// Cancels a running [`spawn_pull`] the way the CLI offers it: closing the
/// child's stdin, which `models pull --cancel-on-stdin-eof` takes as
/// cancellation (it removes what it staged, keeps an earlier install and
/// fails with `cancelled`). The child is never killed; its own answer ends
/// the pull.
#[derive(Clone, Default)]
pub(crate) struct PullCancel(Arc<PullCancelInner>);

#[derive(Default)]
struct PullCancelInner {
    requested: AtomicBool,
    stdin: Mutex<Option<ChildStdin>>,
}

impl PullCancel {
    pub(crate) fn cancel(&self) {
        self.0.requested.store(true, Ordering::SeqCst);
        drop(self.stdin().take());
    }

    fn stdin(&self) -> std::sync::MutexGuard<'_, Option<ChildStdin>> {
        self.0.stdin.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Holds the child's stdin open, or closes it at once when cancel came
    /// before the child started.
    fn attach(&self, stdin: ChildStdin) {
        let mut slot = self.stdin();
        if !self.0.requested.load(Ordering::SeqCst) {
            *slot = Some(stdin);
        }
    }
}

/// `<id>: <pct>% of <size>`, the progress line `models pull` writes to stderr.
pub(crate) fn parse_pull_line(line: &str, id: &str) -> Option<u8> {
    let rest = line.strip_prefix(id)?.strip_prefix(": ")?;
    let pct = rest.split('%').next()?.trim().parse::<u64>().ok()?;
    Some(pct.min(100) as u8)
}

/// The progress slot holding no unread percent.
const NO_PROGRESS: u8 = u8::MAX;

/// The newest percent of a running [`spawn_pull`], coalesced: the reader
/// overwrites one slot and wakes the UI at most once until it reads, so a
/// flood of progress while the UI is busy holds one value, never a queue.
pub(crate) struct PullProgress {
    shared: Arc<PullProgressSlot>,
    wake: mpsc::Receiver<()>,
}

struct PullProgressSlot {
    latest: AtomicU8,
    /// `None` once the pull ended: no more wake-ups.
    wake: Mutex<Option<mpsc::Sender<()>>>,
}

impl PullProgressSlot {
    fn report(&self, pct: u8) {
        self.latest.store(pct, Ordering::SeqCst);
        if let Some(wake) = self.wake().as_mut() {
            // Full: a wake-up is already pending and reads this value.
            let _ = wake.try_send(());
        }
    }

    fn close(&self) {
        self.wake().take();
    }

    fn wake(&self) -> std::sync::MutexGuard<'_, Option<mpsc::Sender<()>>> {
        self.wake.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl PullProgress {
    fn new() -> Self {
        let (tx, wake) = mpsc::channel(0);
        Self {
            shared: Arc::new(PullProgressSlot {
                latest: AtomicU8::new(NO_PROGRESS),
                wake: Mutex::new(Some(tx)),
            }),
            wake,
        }
    }

    /// The newest percent not read yet, once there is one; `None` once the
    /// pull ended.
    pub(crate) async fn next(&mut self) -> Option<u8> {
        loop {
            self.wake.next().await?;
            let pct = self.shared.latest.swap(NO_PROGRESS, Ordering::SeqCst);
            if pct != NO_PROGRESS {
                return Some(pct);
            }
        }
    }
}

/// Runs `broscribe models pull <id> --cancel-on-stdin-eof` through `cli`
/// on the background executor ([`CliRunner::run_blocking_cooperative`]:
/// stdout and stderr drained at once, both bounded), with its percent
/// progress (parsed from stderr) on the returned [`PullProgress`]; the
/// [`PullCancel`] cancels it. The task's result is the CLI's own answer.
pub(crate) fn spawn_pull(
    cli: CliRunner,
    id: String,
    allow_unverified_license: bool,
    executor: &BackgroundExecutor,
) -> (
    Task<std::result::Result<Value, CliError>>,
    PullProgress,
    PullCancel,
) {
    let progress = PullProgress::new();
    let cancel = PullCancel::default();
    let handle = cancel.clone();
    let slot = Arc::clone(&progress.shared);
    let task = executor.spawn(async move {
        let result = run_pull(&cli, &id, allow_unverified_license, &handle, &slot);
        slot.close();
        result
    });
    (task, progress, cancel)
}

/// [`spawn_pull`] on the calling thread.
fn run_pull(
    cli: &CliRunner,
    id: &str,
    allow_unverified_license: bool,
    cancel: &PullCancel,
    progress: &Arc<PullProgressSlot>,
) -> std::result::Result<Value, CliError> {
    let mut pull = args(["models", "pull", id, "--cancel-on-stdin-eof"]);
    if allow_unverified_license {
        pull.push("--allow-unverified-license".into());
    }
    let reader = Arc::clone(progress);
    let model = id.to_owned();
    let result = cli.run_blocking_cooperative(
        &pull,
        |stdin| cancel.attach(stdin),
        move |bytes| {
            let line = String::from_utf8_lossy(bytes);
            match parse_pull_line(&line, &model) {
                Some(pct) => reader.report(pct),
                None if !line.trim().is_empty() => tracing::info!("models pull: {line}"),
                None => {}
            }
        },
    );
    drop(cancel.stdin().take());
    result
}

// ---- enrollment passage files ----

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
struct Notice {
    text: String,
    error: bool,
}

#[derive(Default)]
struct ModelsModel {
    list: ModelsList,
    loading: bool,
    loaded: bool,
    error: Option<String>,
    generation: u64,
    pulls: BTreeMap<String, PullState>,
    /// Cancels of the downloads still running.
    pull_cancels: BTreeMap<String, PullCancel>,
    advanced: bool,
    batch: Option<std::collections::VecDeque<String>>,
    batch_status: Option<(StatusKind, String)>,
    outcomes: BTreeMap<(String, String), (StatusKind, String)>,
    using: Option<(String, String)>,
    doctor: Option<std::result::Result<Value, String>>,
    doctor_running: bool,
}

#[derive(Default)]
struct EnrollmentModel {
    loaded: bool,
    files: Vec<(PathBuf, u64)>,
    saved: bool,
    details: bool,
    error: Option<String>,
    deleting: bool,
}

/// One input device from `broscribe audio devices --json`.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct InputDevice {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) is_default: bool,
}

/// One "Microphone for recordings" option.
#[derive(Clone, Debug, PartialEq)]
struct MicOption {
    /// The device `audio pin` pins; `None` is the system default.
    target: Option<String>,
    label: String,
    #[cfg(test)]
    selected: bool,
}

#[derive(Default)]
struct MicsModel {
    /// The last list `audio devices` returned; `None` until one loaded, or
    /// after it failed (whether the pinned device is connected is unknown).
    devices: Option<Vec<InputDevice>>,
    loading: bool,
    generation: u64,
    error: Option<String>,
    /// An `audio pin` child is running.
    pinning: bool,
    result: Option<std::result::Result<String, String>>,
}

/// "Start BroScribe at login" (`app open-at-login`).
#[derive(Default)]
struct LoginModel {
    /// The CLI's last answer; `None` until known.
    value: Option<bool>,
    /// An `app open-at-login on|off` child is running.
    running: bool,
    generation: u64,
    error: Option<String>,
}

impl LoginModel {
    fn apply(&mut self, result: std::result::Result<Value, CliError>) {
        match result {
            Ok(v) => match v["open_at_login"].as_bool() {
                Some(on) => {
                    self.value = Some(on);
                    self.error = None;
                }
                None => self.error = Some("BroScribe got an answer it didn't expect.".into()),
            },
            Err(e) => self.error = Some(e.user_text()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum LibrarySwitch {
    Idle,
    /// Chosen and saved; waits for running jobs to finish.
    Waiting(PathBuf),
}

pub struct SettingsView {
    state: Entity<AppState>,
    /// The import sheet's runs; a library switch waits for them too.
    imports: Entity<ImportRuns>,
    deps: SetupDeps,
    language: Entity<InputState>,
    /// The saved value the Default language field was last set from.
    seen_language: String,
    retention_days: Entity<InputState>,
    /// The saved value the retention-days field was last set from.
    seen_retention_days: String,
    retention: RetentionGuard,
    speaker_grouping: SpeakerGroupingEditor,
    setting_notices: std::collections::BTreeMap<&'static str, Notice>,
    saving: std::collections::BTreeSet<&'static str>,
    window_page: Option<SettingsPage>,
    page_scroll: gpui::ScrollHandle,
    page_list_focus: FocusHandle,
    /// Not a tab stop: the start of the page's content, so the next tab stop
    /// after it is the page's first control (ACC-07).
    pane_focus: FocusHandle,
    /// Move focus from `pane_focus` to the first control once the page is
    /// drawn.
    first_control_pending: bool,
    updates: Option<Entity<UpdateView>>,
    uninstall: uninstall::RemovalDialog,
    uninstall_focus: FocusHandle,
    tray_place: broscribe_platform::StatusIconPlace,
    library_switch: LibrarySwitch,
    library_change_focus: FocusHandle,
    disk_use: Option<(Option<u64>, Option<u64>)>,
    disk_use_generation: u64,
    models: ModelsModel,
    download_all_focus: FocusHandle,
    search_model_focus: FocusHandle,
    enrollment: EnrollmentModel,
    enrollment_delete_focus: FocusHandle,
    /// AI providers, every purpose; the assistant's Summaries step has its own.
    providers: Entity<ProvidersView>,
    /// Agent access (CLI link, skill, MCP), shared with the assistant.
    agents: Entity<AgentsView>,
    login: LoginModel,
    mics: MicsModel,
    meeting_apps: Option<std::result::Result<Vec<MeetingApp>, String>>,
    detection_off: bool,
    detection_results: BTreeMap<String, (StatusKind, String)>,
    permissions: Option<std::result::Result<Value, String>>,
    permission_generation: u64,
    audio_check_names: Option<(String, String)>,
    notifications_permission: Option<&'static str>,
    words: Entity<TextareaState>,
    setup: Option<Entity<SetupAssistant>>,
    /// The assistant is cancelling its work before it closes.
    integrations: Entity<crate::views::integrations::IntegrationsView>,
    people_revision: u64,
    /// Settings → Summaries: the default summary instructions.
    summary_instructions: Entity<TextareaState>,
    /// The saved text the field was last set from.
    seen_instructions: String,
    instructions_saving: bool,
    close_on_summary_save: Option<gpui::AnyWindowHandle>,
    instructions_notice: Option<(String, bool)>,
    /// Settings → Templates.
    templates: Entity<TemplatesSettings>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    pub fn new(
        state: Entity<AppState>,
        deps: SetupDeps,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let config = state.read(cx).config().clone();
        let speaker_grouping = SpeakerGroupingEditor::new(&config, window, cx);
        let seen_language = language_text(&config);
        let language = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Automatic")
                .default_value(seen_language.clone())
        });
        let seen_retention_days = retention_days_text(&config);
        let retention_days = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Days")
                .default_value(seen_retention_days.clone())
        });
        let people_revision = state.read(cx).people_revision();
        let imports = ImportRuns::global(cx);
        let seen_instructions = saved_instructions(&config).to_owned();
        let summary_instructions = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Keep it short and focus on budget decisions")
                .default_value(seen_instructions.clone())
        });
        let words = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("One per line: product names, acronyms, colleagues’ names")
        });
        let settings = cx.weak_entity();
        let assistant_closed = crate::ui::setup_window::on_closed(cx, move |cx| {
            let _ = settings.update(cx, |view, cx| {
                view.setup = None;
                view.load_models(cx);
                view.load_enrollment(cx);
                if view.window_page == Some(SettingsPage::Recording) {
                    view.load_permissions(cx);
                }
                view.providers
                    .update(cx, |providers, cx| providers.reload(cx));
                view.agents.update(cx, |agents, cx| agents.reload(cx));
                cx.notify();
            });
        });
        let mut subscriptions = vec![
            assistant_closed,
            cx.observe(&summary_instructions, |_, _, cx| cx.notify()),
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    reload_config(&this.state, cx).detach();
                    if this.window_page == Some(SettingsPage::Recording) {
                        this.load_permissions(cx);
                    }
                    if this.window_page == Some(SettingsPage::Calendars) {
                        this.integrations.update(cx, |view, cx| view.reload(cx));
                    }
                }
            }),
            cx.subscribe_in(&language, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    this.save_language(window, cx);
                }
            }),
            cx.subscribe_in(&retention_days, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    this.save_retention_days(window, cx);
                }
            }),
            cx.observe(&state, |this, state, cx| {
                let revision = state.read(cx).people_revision();
                if revision != this.people_revision {
                    this.people_revision = revision;
                    this.load_enrollment(cx);
                }
                this.maybe_switch_library(cx);
                cx.notify();
            }),
            // A deferred library switch also waits for the sheet's imports.
            cx.observe(&imports, |this, _, cx| {
                this.maybe_switch_library(cx);
                cx.notify();
            }),
            // Saved values changed (here, from a meeting's "Save as default"
            // or a terminal): an untouched field follows them, a draft stays.
            cx.observe_in(&state, window, |this, state, window, cx| {
                let config = state.read(cx).config();
                let saved = saved_instructions(config).to_owned();
                let language = language_text(config);
                let days = retention_days_text(config);
                let model = config.profile.speaker_embedding.clone();
                let grouping = config.speaker_grouping(&model);
                this.speaker_grouping.sync(model, grouping, window, cx);
                if saved != this.seen_instructions {
                    let current = this.summary_instructions.read(cx).value().to_string();
                    if current.trim() == this.seen_instructions {
                        this.summary_instructions
                            .update(cx, |input, cx| input.set_value(saved.clone(), window, cx));
                    }
                    this.seen_instructions = saved;
                }
                if language != this.seen_language {
                    let current = this.language.read(cx).value().to_string();
                    if current.trim() == this.seen_language {
                        this.language.update(cx, |input, cx| {
                            input.set_value(language.clone(), window, cx)
                        });
                    }
                    this.seen_language = language;
                }
                if days != this.seen_retention_days {
                    let current = this.retention_days.read(cx).value().to_string();
                    if current.trim() == this.seen_retention_days {
                        this.retention_days
                            .update(cx, |input, cx| input.set_value(days.clone(), window, cx));
                    }
                    this.seen_retention_days = days;
                }
            }),
        ];
        subscriptions.extend(speaker_grouping.subscribe(window, cx));
        let mut retention = RetentionGuard::new(cx);
        retention.bind_days_input(&retention_days, cx);
        let mut this = Self {
            state: state.clone(),
            imports,
            deps,
            language,
            seen_language,
            retention_days,
            seen_retention_days,
            retention,
            speaker_grouping,
            setting_notices: std::collections::BTreeMap::new(),
            saving: std::collections::BTreeSet::new(),
            window_page: None,
            page_scroll: gpui::ScrollHandle::new(),
            page_list_focus: cx.focus_handle(),
            pane_focus: cx.focus_handle().tab_stop(false),
            first_control_pending: false,
            updates: None,
            uninstall: uninstall::RemovalDialog::default(),
            uninstall_focus: cx.focus_handle(),
            tray_place: crate::ui::status_icon_place(cx),
            library_switch: LibrarySwitch::Idle,
            library_change_focus: cx.focus_handle(),
            disk_use: None,
            disk_use_generation: 0,
            models: ModelsModel::default(),
            download_all_focus: cx.focus_handle(),
            search_model_focus: cx.focus_handle(),
            enrollment: EnrollmentModel::default(),
            enrollment_delete_focus: cx.focus_handle(),
            providers: cx.new(|_| ProvidersView::new(state.clone(), ProviderScope::All)),
            agents: cx.new(|_| AgentsView::new(state.clone())),
            login: LoginModel::default(),
            mics: MicsModel::default(),
            meeting_apps: None,
            detection_off: false,
            detection_results: BTreeMap::new(),
            permissions: None,
            permission_generation: 0,
            audio_check_names: None,
            notifications_permission: None,
            words,
            setup: None,
            integrations: cx.new(|cx| {
                crate::views::integrations::IntegrationsView::new(state.clone(), window, cx)
            }),
            people_revision,
            summary_instructions,
            seen_instructions,
            instructions_saving: false,
            close_on_summary_save: None,
            instructions_notice: None,
            templates: cx.new(|cx| TemplatesSettings::new(state.clone(), cx)),
            _subscriptions: subscriptions,
        };
        let downloads = state.read(cx).model_downloads().clone();
        this._subscriptions
            .push(cx.observe(&downloads, |this, downloads, cx| {
                if this.window_page == Some(SettingsPage::Transcription) {
                    if !downloads.read(cx).running() {
                        this.load_models(cx);
                    }
                    cx.notify();
                }
            }));
        this._subscriptions
            .push(cx.observe(&this.providers, |_, _, cx| cx.notify()));
        this.templates.update(cx, |view, cx| {
            view.use_summary_providers(this.providers.clone(), cx)
        });
        if let Some(daemon) = state.read(cx).daemon().cloned() {
            this._subscriptions.push(cx.subscribe(
                &daemon,
                |view, _, event: &crate::daemon::DaemonEvent, cx| {
                    if matches!(
                        event,
                        crate::daemon::DaemonEvent::Received(broscribe_ipc::Event::State(_))
                    ) {
                        view.maybe_switch_library(cx);
                    }
                },
            ));
        }
        this.load_models(cx);
        this.load_enrollment(cx);
        this.entered_settings(cx);
        this
    }

    fn report_setting(
        &mut self,
        control: &'static str,
        result: std::result::Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        let notice = match result {
            Ok(text) => Notice { text, error: false },
            Err(text) => Notice { text, error: true },
        };
        self.setting_notices.insert(control, notice);
        cx.notify();
    }

    fn setting_notice(&self, control: &'static str, cx: &App) -> Option<Stateful<Div>> {
        if self.saving.contains(control) {
            return Some(kit::status_line(
                format!("status-{control}"),
                StatusKind::Pending,
                "Saving…",
                cx,
            ));
        }
        let notice = self.setting_notices.get(control)?;
        if !notice.error
            && matches!(
                control,
                "language"
                    | "consent-reminder"
                    | "live-identity"
                    | "reading-size"
                    | "summary-mode"
                    | "speech-model"
            )
        {
            return None;
        }
        Some(kit::status_line(
            format!("status-{control}"),
            if notice.error {
                StatusKind::Failed
            } else {
                StatusKind::Done
            },
            notice.text.clone(),
            cx,
        ))
    }

    fn begin_save(&mut self, control: &'static str, cx: &mut Context<Self>) -> bool {
        if !self.saving.insert(control) {
            return false;
        }
        self.setting_notices.remove(control);
        cx.notify();
        true
    }

    fn finish_save(
        &mut self,
        control: &'static str,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        self.saving.remove(control);
        if let Err(error) = result {
            self.report_setting(control, Err(error), cx);
        } else {
            cx.notify();
        }
    }

    fn apply_setting(
        &mut self,
        control: &'static str,
        edit: impl FnOnce(&mut Config) + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if !self.begin_save(control, cx) {
            return;
        }
        let task = update_config(&self.state, edit, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| this.finish_save(control, result, cx));
        })
        .detach();
    }

    /// The page anchor and the page list's focus, and the page shown.
    #[cfg(test)]
    pub(crate) fn page_focus_for_test(&self) -> (FocusHandle, FocusHandle, Option<SettingsPage>) {
        (
            self.pane_focus.clone(),
            self.page_list_focus.clone(),
            self.window_page,
        )
    }

    /// Focuses the shown page's first control (ACC-07): when the Settings
    /// window opens, and when another window opens it at a page. Moving
    /// through the page list keeps focus in the list.
    pub(crate) fn focus_first_control(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pane_focus.focus(window, cx);
        self.first_control_pending = true;
        cx.notify();
    }

    pub(crate) fn set_page(
        &mut self,
        page: SettingsPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.window_page == Some(page) {
            return;
        }
        match self.window_page {
            Some(SettingsPage::AiProviders) => {
                self.providers.update(cx, |view, cx| view.reset_drafts(cx))
            }
            Some(SettingsPage::Calendars) => {
                self.integrations.update(cx, |view, cx| view.deactivate(cx))
            }
            Some(SettingsPage::Storage) => self.retention.invalidate(),
            _ => {}
        }
        match page {
            SettingsPage::Recording => {
                self.load_mics(cx);
                self.load_meeting_apps(cx);
                self.load_permissions(cx);
            }
            SettingsPage::Storage => self.measure_disk_use(cx),
            SettingsPage::AiProviders | SettingsPage::Summaries => {
                self.providers.update(cx, |view, cx| view.reload(cx))
            }
            SettingsPage::AiTools => self.agents.update(cx, |view, cx| view.reload(cx)),
            SettingsPage::Calendars => self
                .integrations
                .update(cx, |view, cx| view.set_window_target(None, cx)),
            SettingsPage::Updates => {
                if let Some(updates) = &self.updates {
                    updates.update(cx, |view, cx| view.refresh(cx));
                } else {
                    self.updates = Some(UpdateView::shared(&self.state, cx));
                    if let Some(updates) = &self.updates {
                        updates.update(cx, |view, cx| view.refresh(cx));
                    }
                }
            }
            _ => {}
        }
        self.window_page = Some(page);
        self.page_scroll.set_offset(gpui::point(px(0.), px(0.)));
        window.set_window_title(&crate::ui::windows::title(page.title()));
        crate::ui_state::remember_settings_page(page, cx);
        cx.notify();
    }

    pub(crate) fn scroll_to_your_voice(&self, window: &mut Window) {
        let scroll = self.page_scroll.clone();
        window.on_next_frame(move |_, _| {
            // Title, description, speech models and language precede Your voice.
            scroll.scroll_to_top_of_item(4);
        });
    }

    pub(crate) fn focus_word_list(&self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.words.read(cx).focus_handle(cx);
        let scroll = self.page_scroll.clone();
        window.on_next_frame(move |window, cx| {
            // Title, description, speech models, language and Your voice.
            scroll.scroll_to_top_of_item(5);
            focus.focus(window, cx);
        });
    }

    pub(crate) fn check_for_updates_from_menu(&self, cx: &mut Context<Self>) {
        if let Some(updates) = &self.updates {
            updates.update(cx, |updates, cx| updates.check_from_menu(cx));
        }
    }

    pub(crate) fn set_calendar_target(&mut self, provider: &str, cx: &mut Context<Self>) {
        let provider = (!provider.is_empty()).then(|| provider.to_owned());
        self.integrations.update(cx, |view, cx| {
            view.set_target_scroll(self.page_scroll.clone());
            view.set_window_target(provider, cx);
        });
    }

    /// Brings up the standalone setup assistant.
    pub fn open_setup(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let assistant = match crate::ui::setup_window::show(&self.state, &self.deps, cx) {
            Ok(assistant) => assistant,
            Err(error) => {
                tracing::error!(%error, "cannot open the setup assistant");
                self.report_setting(
                    "setup",
                    Err("Couldn't open the setup assistant.".into()),
                    cx,
                );
                return;
            }
        };
        if self.setup.as_ref() == Some(&assistant) {
            return;
        }
        self.retention.invalidate();
        self.setup = Some(assistant);
        cx.notify();
    }

    // ---- General ----

    fn open_setup_step(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        self.open_setup(window, cx);
        if self.setup.is_some() {
            crate::ui::setup_window::request_step(step, cx);
        }
    }

    fn load_meeting_apps(&mut self, cx: &mut Context<Self>) {
        let task = self
            .state
            .read(cx)
            .run_cli(args(["detect", "status"]), ChildKind::Command, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.meeting_apps = Some(
                    result
                        .map(|v| {
                            this.detection_off = setup::detection_off(&v);
                            let mut apps = setup::parse_meeting_apps(&v);
                            apps.sort_by_key(|app| app.installed == Some(false));
                            apps
                        })
                        .map_err(|e| {
                            failure_text("Couldn’t check meeting apps.", &e.user_text(), "")
                        }),
                );
                cx.notify();
            });
        })
        .detach();
    }

    fn change_meeting_policy(&mut self, id: String, policy: String, cx: &mut Context<Self>) {
        if self
            .detection_results
            .get(&id)
            .is_some_and(|(kind, _)| *kind == StatusKind::Pending)
        {
            return;
        }
        self.detection_results
            .insert(id.clone(), (StatusKind::Pending, "Saving…".into()));
        cx.notify();
        let task = self.state.read(cx).run_cli(
            setup::detect_policy_args(&id, &policy),
            ChildKind::Command,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let message = match result {
                    Ok(v) => {
                        if let Some(Ok(apps)) = &mut this.meeting_apps
                            && let Some(app) = apps.iter_mut().find(|app| app.id == id)
                        {
                            app.policy = Some(policy);
                        }
                        if v["daemon_reloaded"] == false {
                            Some((
                                StatusKind::Attention,
                                "Saved. BroScribe Agent applies this change when it next starts."
                                    .into(),
                            ))
                        } else {
                            None
                        }
                    }
                    Err(e) => Some((
                        StatusKind::Failed,
                        failure_text("Couldn’t save the meeting policy.", &e.user_text(), ""),
                    )),
                };
                if let Some(message) = message {
                    this.detection_results.insert(id, message);
                } else {
                    this.detection_results.remove(&id);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn load_permissions(&mut self, cx: &mut Context<Self>) {
        use broscribe_platform::{SettingsPane, SystemSettingsOpener as _};
        self.permission_generation += 1;
        let generation = self.permission_generation;
        let state = self.state.read(cx);
        let measured = state.config().audio_checks.last().cloned();
        let data = state.paths().data_dir.clone();
        let task = cx.background_executor().spawn(async move {
            let opener = NativePlatform::system_settings();
            let permissions = if opener.supports(SettingsPane::Microphone)
                || opener.supports(SettingsPane::SystemAudio)
            {
                let (call, timeout) = setup::permission_call(None);
                setup::helper_call(&data, call, timeout)
            } else {
                Ok(Value::Null)
            };
            let audio = measured.and_then(|check| {
                let devices = NativePlatform::device_info().devices().ok()?;
                let mic = devices
                    .iter()
                    .find(|device| device.id == check.input_uid)?
                    .name
                    .clone();
                let output = check.output_data_source.or_else(|| {
                    devices
                        .iter()
                        .find(|device| device.id == check.output_uid)
                        .map(|device| device.name.clone())
                })?;
                Some((mic, output))
            });
            (permissions, audio, opener.notifications_permission())
        });
        cx.spawn(async move |this, cx| {
            let (permissions, audio, notifications) = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.window_page == Some(SettingsPage::Recording)
                    && this.permission_generation == generation
                    && (this.permissions.as_ref() != Some(&permissions)
                        || this.audio_check_names != audio
                        || this.notifications_permission != notifications)
                {
                    this.permissions = Some(permissions);
                    this.audio_check_names = audio;
                    this.notifications_permission = notifications;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn save_words(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.begin_save("word-list", cx) {
            return;
        }
        let text = self.words.read(cx).value().to_string();
        let Some(task) = setup::save_word_list(&self.state, &text, cx) else {
            self.finish_save("word-list", Err("Enter at least one word.".into()), cx);
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.saving.remove("word-list");
                let message = match result {
                    Ok(_) => {
                        if this.words.read(cx).value().as_ref() == text {
                            this.words
                                .update(cx, |input, cx| input.set_value(String::new(), window, cx));
                        }
                        reload_config(&this.state, cx).detach();
                        Ok("Word list saved.".into())
                    }
                    Err(cause) => Err(failure_text("Couldn’t save the word list.", &cause, "")),
                };
                this.report_setting("word-list", message, cx);
            });
        })
        .detach();
    }

    fn save_language(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.language.read(cx).value().trim().to_owned();
        if text == self.seen_language || self.saving.contains("language") {
            return;
        }
        if !text.is_empty() && !valid_language_tag(&text) {
            self.report_setting(
                "language",
                Err("Enter a language tag such as en or pt-BR.".into()),
                cx,
            );
            return;
        }
        if !self.begin_save("language", cx) {
            return;
        }
        let value = (!text.is_empty()).then_some(text.clone());
        let task = update_config(&self.state, move |c| c.default_language = value, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if result.is_err() && this.language.read(cx).value().trim() == text {
                    let saved = this.seen_language.clone();
                    this.language
                        .update(cx, |input, cx| input.set_value(saved, window, cx));
                }
                this.finish_save("language", result, cx);
            });
        })
        .detach();
    }

    fn choose_library(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Use this library".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            if let Some(path) = paths.into_iter().next() {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.confirm_library(path, window, cx)
                });
            }
        })
        .detach();
    }

    fn confirm_library(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let current = self.state.read(cx).library().root();
        if current == path || self.state.read(cx).paths().library_from_env.is_some() {
            return;
        }
        let label = folder_name(&path);
        let mut spec = kit::ConfirmSpec::new(
            kit::confirm_line(
                "library-switch-title",
                format!("Switch to the library in “{label}”?"),
            ),
            "library-switch-dialog",
            "library-switch-actions",
            "library-switch-cancel",
            kit::confirm_line("library-switch-confirm", "Switch and restart"),
        );
        spec.primary_action = true;
        spec.irreversible = false;
        spec.changes = vec![kit::confirm_line(
            "library-switch-change",
            "BroScribe restarts after recording and background jobs finish. Choose an existing BroScribe library or an empty folder to create one. Your saved Keep recordings policy applies there.",
        )];
        spec.stays = vec![kit::confirm_line(
            "library-switch-stays",
            format!(
                "Your current library stays in {}. Files stay in their existing folders.",
                current.display()
            ),
        )];
        let settings = cx.weak_entity();
        kit::confirm_dialog(
            self.library_change_focus.clone(),
            window,
            cx,
            move |_| Some(spec.clone()),
            |_, _| true,
            move |_, cx| {
                let _ = settings.update(cx, |this, cx| this.select_library(path.clone(), cx));
                true
            },
        );
    }

    fn measure_disk_use(&mut self, cx: &mut Context<Self>) {
        self.disk_use_generation += 1;
        let generation = self.disk_use_generation;
        self.disk_use = None;
        let root = self.state.read(cx).library().root().to_owned();
        let system = self.deps.system.clone();
        let task = cx.background_executor().spawn(async move {
            (
                directory_bytes(&root).ok(),
                system.free_disk_bytes(&root).ok(),
            )
        });
        cx.spawn(async move |this, cx| {
            let measured = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.disk_use_generation == generation {
                    this.disk_use = Some(measured);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Opens (creating if empty) the chosen library, saves it in the config
    /// and restarts the app on it once no job is running. Nothing is moved
    /// between libraries.
    fn select_library(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.state.read(cx).library().root() == path {
            self.report_setting("library", Ok("That library is already open.".into()), cx);
            return;
        }
        let data_dir = self.state.read(cx).paths().data_dir.clone();
        let target = path.clone();
        let task = self
            .state
            .read(cx)
            .cli()
            .spawn(cx.background_executor(), async move {
                open_chosen_library(&target)?;
                edit_config(&data_dir, |config| config.library = Some(target))
            });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| match result {
                Ok(config) => this.library_chosen(config, path, cx),
                Err(e) => {
                    tracing::warn!(code = e.code(), "opening the chosen library");
                    this.report_setting(
                        "library",
                        Err(library_open_failure(&core_user_text(&e))),
                        cx,
                    )
                }
            })
            .ok();
        })
        .detach();
    }

    /// The chosen library is saved (`config`): a retention check or
    /// confirmation taken on the open library no longer applies, and the app
    /// restarts on the new one once no job runs.
    fn library_chosen(&mut self, config: Config, path: PathBuf, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| s.set_config(config, cx));
        self.retention.invalidate();
        self.library_switch = LibrarySwitch::Waiting(path);
        self.maybe_switch_library(cx);
        cx.notify();
    }

    fn maybe_switch_library(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.library_switch, LibrarySwitch::Waiting(_)) {
            return;
        }
        if library_switch_busy(self.state.read(cx), self.imports.read(cx), cx) {
            return;
        }
        self.library_switch = LibrarySwitch::Idle;
        tracing::info!("restarting on the newly selected library");
        cx.restart();
    }

    /// Settings (re)opened: picks up config changes made from a terminal
    /// (`audio pin`, `providers enable`), drops the last microphone message,
    /// relists devices and refreshes provider detection, agent access and
    /// the open-at-login setting.
    fn entered_settings(&mut self, cx: &mut Context<Self>) {
        reload_config(&self.state, cx).detach();
        self.templates.update(cx, |t, cx| t.load(cx));
        self.mics.result = None;
        self.load_mics(cx);
        self.providers
            .update(cx, |providers, cx| providers.reload(cx));
        self.agents.update(cx, |agents, cx| agents.reload(cx));
        self.load_login(cx);
    }

    /// Reads the open-at-login setting with `app open-at-login status`.
    fn load_login(&mut self, cx: &mut Context<Self>) {
        if self.login.running {
            return;
        }
        self.login.generation += 1;
        let generation = self.login.generation;
        let task = self.state.read(cx).run_cli(
            args(["app", "open-at-login", "status"]),
            ChildKind::Command,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.login.generation != generation {
                    return;
                }
                this.login.apply(result);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// `app open-at-login on|off`; one at a time, nothing when already set.
    fn set_open_at_login(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.login.running || self.login.value == Some(on) {
            return;
        }
        self.login.running = true;
        self.login.generation += 1;
        cx.notify();
        let task = self.state.read(cx).run_cli(
            args(["app", "open-at-login", if on { "on" } else { "off" }]),
            ChildKind::Command,
            cx,
        );
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            // The child saved `[app] open_at_login`.
            cx.update(|cx| reload_config(&state, cx)).await;
            this.update(cx, |this, cx| {
                this.login.running = false;
                this.login.apply(result);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Lists the input devices with `audio devices`.
    fn load_mics(&mut self, cx: &mut Context<Self>) {
        self.mics.generation += 1;
        let generation = self.mics.generation;
        self.mics.loading = true;
        self.mics.error = None;
        cx.notify();
        let task = self
            .state
            .read(cx)
            .run_cli(args(["audio", "devices"]), ChildKind::Command, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.mics.generation != generation {
                    return;
                }
                this.mics.loading = false;
                match result {
                    Ok(v) => {
                        this.mics.devices = Some(parse_input_devices(&v));
                        this.mics.error = None;
                    }
                    Err(e) => {
                        this.mics.devices = None;
                        this.mics.error = Some(failure_text(
                            "Couldn't list the microphones.",
                            &e.user_text(),
                            "",
                        ));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Pins `target` with `audio pin`, or goes back to the system default
    /// with `audio pin --clear` (`None`). The options stay disabled until the
    /// changed config is loaded, so the highlighted option is never stale.
    fn pin_mic(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        if self.mics.pinning {
            return;
        }
        self.mics.pinning = true;
        self.mics.result = None;
        cx.notify();
        let name = target.as_ref().map(|id| {
            self.mics
                .devices
                .iter()
                .flatten()
                .find(|d| &d.id == id)
                .map_or_else(|| id.clone(), |d| d.name.clone())
        });
        let task =
            self.state
                .read(cx)
                .run_cli(pin_mic_args(target.as_deref()), ChildKind::Command, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let Ok(reload) = this.update(cx, |this, cx| {
                this.mics.result = Some(match (result, name) {
                    (Ok(_), Some(name)) => {
                        Ok(format!("Recording with {name} while it is connected."))
                    }
                    (Ok(_), None) => Ok("Recording with the system default microphone.".into()),
                    (Err(e), _) => Err(failure_text(
                        "Couldn't change the microphone.",
                        &e.user_text(),
                        "",
                    )),
                });
                this.load_mics(cx);
                reload_config(&this.state, cx)
            }) else {
                return;
            };
            reload.await;
            this.update(cx, |this, cx| {
                this.mics.pinning = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---- Storage ----

    fn set_handling(&mut self, handling: Handling, cx: &mut Context<Self>) {
        self.apply_setting("import-handling", move |c| c.import_handling = handling, cx);
    }

    fn set_retention(
        &mut self,
        retention: Retention,
        trigger: RetentionTrigger,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.setting_notices.remove("retention");
        let state = self.state.clone();
        request_retention_change(
            self,
            state,
            |this| &mut this.retention,
            retention,
            trigger,
            |this, cx| {
                this.setting_notices.remove("retention");
                cx.notify();
            },
            window,
            cx,
        );
    }

    fn save_retention_days(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.retention_days.read(cx).value().trim().to_owned();
        if text == self.seen_retention_days || self.retention.checking_or_confirming() {
            return;
        }
        match parse_retention_days(&text) {
            Some(days) => {
                self.setting_notices.remove("retention-days");
                self.set_retention(
                    Retention::DeleteAfterDays { days },
                    RetentionTrigger::DaysSave,
                    window,
                    cx,
                );
            }
            _ => self.report_setting(
                "retention-days",
                Err("Use a whole number of days from 1 to 3,650.".into()),
                cx,
            ),
        }
    }

    fn instructions_dirty(&self, cx: &App) -> bool {
        self.summary_instructions.read(cx).value().trim() != self.seen_instructions
    }

    fn revert_instructions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.summary_instructions.update(cx, |input, cx| {
            input.set_value(self.seen_instructions.clone(), window, cx)
        });
        self.instructions_notice = None;
        cx.notify();
    }

    /// Returns true when native or app window chrome may close immediately.
    pub(crate) fn should_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        use gpui::component::WindowExt as _;
        if self.uninstall.applying() {
            return false;
        }
        if self.templates.update(cx, |templates, cx| {
            templates.handle_window_close(window, cx)
        }) {
            return false;
        }
        if !self.instructions_dirty(cx) {
            return !self.instructions_saving;
        }
        if self.instructions_saving || window.has_active_dialog(cx) {
            return false;
        }
        let view = cx.weak_entity();
        let focus_cancel = Cell::new(true);
        let cancel_focus = cx.focus_handle();
        let invalid = broscribe_core::config::validate_instructions(
            self.summary_instructions.read(cx).value().as_ref(),
        )
        .is_err();
        window.open_alert_dialog(cx, move |alert, window, cx| {
            // BroScribe's dialogs stand on the popover ground, not the desk.
            let alert = alert.bg(cx.theme().popover);
            let discard = view.clone();
            let save = view.clone();
            if focus_cancel.replace(false) {
                cancel_focus.focus(window, cx);
            }
            alert
                .title("Save the summary instructions?")
                .width(px(440.))
                .entrance(gpui::component::dialog::DialogEntrance::None)
                .on_ok(|_, _, _| false)
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap(kit::CONTROL_GAP)
                        .child(
                            Button::new("summary-close-discard")
                                .label("Don’t save")
                                .ghost()
                                .on_click(move |_, window, cx| {
                                    let _ = discard.update(cx, |this, cx| {
                                        this.revert_instructions(window, cx)
                                    });
                                    window.remove_window();
                                }),
                        )
                        .child(kit::confirm_actions(
                            Button::new("summary-close-cancel")
                                .label("Cancel")
                                .outline()
                                .track_focus(&cancel_focus)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                            Button::new("summary-close-save")
                                .label("Save")
                                .map(|button| kit::primary(button, invalid, cx))
                                .disabled(invalid)
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                    let _ = save.update(cx, |this, cx| {
                                        this.close_on_summary_save = Some(window.window_handle());
                                        this.save_summary_instructions(cx);
                                    });
                                }),
                            cx,
                        )),
                )
        });
        false
    }

    /// Saves the Summaries instructions (blank clears them); the text goes
    /// on stdin.
    pub fn save_summary_instructions(&mut self, cx: &mut Context<Self>) {
        if self.instructions_saving {
            return;
        }
        let text = self.summary_instructions.read(cx).value().to_string();
        if broscribe_core::config::validate_instructions(&text).is_err() {
            self.close_on_summary_save = None;
            return;
        }
        let (command, stdin) = save_instructions_command(&text);
        let task = {
            let state = self.state.read(cx);
            match stdin {
                Some(stdin) => state.cli().run_with_stdin(
                    command,
                    ChildKind::Command,
                    stdin,
                    cx.background_executor(),
                ),
                None => state.run_cli(command, ChildKind::Command, cx),
            }
        };
        self.instructions_saving = true;
        self.instructions_notice = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let reload = this
                .update(cx, |this, cx| {
                    this.instructions_saving = false;
                    let close = this.close_on_summary_save.take().filter(|_| result.is_ok());
                    this.instructions_notice = Some(match &result {
                        Ok(_) if text.trim().is_empty() => {
                            ("Summaries use no extra instructions.".to_owned(), false)
                        }
                        Ok(_) => ("Saved.".to_owned(), false),
                        Err(e) => (
                            failure_text(
                                "Couldn't save the instructions.",
                                &e.user_text(),
                                "Your text is kept.",
                            ),
                            true,
                        ),
                    });
                    cx.notify();
                    result
                        .is_ok()
                        .then(|| (reload_config(&this.state, cx), close))
                })
                .ok()
                .flatten();
            if let Some((reload, close)) = reload {
                reload.await;
                if let Some(handle) = close {
                    let _ = cx.update_window(handle, |_, window, _| window.remove_window());
                }
            }
        })
        .detach();
    }

    fn set_theme_choice(&mut self, choice: ThemeChoice, cx: &mut Context<Self>) {
        self.apply_setting("theme", move |c| c.appearance.theme = choice, cx);
    }

    fn set_reading_size(&mut self, size: ReadingSize, cx: &mut Context<Self>) {
        self.apply_setting(
            "reading-size",
            move |c| c.appearance.reading_size = size,
            cx,
        );
    }

    fn set_live_identity(&mut self, on: bool, cx: &mut Context<Self>) {
        self.apply_setting("live-identity", move |c| c.live.identity = Some(on), cx);
    }

    fn set_auto_enrich(&mut self, value: AutoEnrich, cx: &mut Context<Self>) {
        self.apply_setting("summary-mode", move |c| c.auto_enrich = value, cx);
    }

    // ---- Models ----

    fn load_models(&mut self, cx: &mut Context<Self>) {
        self.models.generation += 1;
        let generation = self.models.generation;
        self.models.loading = true;
        self.models.error = None;
        cx.notify();
        let task = self
            .state
            .read(cx)
            .run_cli(args(["models", "list"]), ChildKind::Command, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.models.generation != generation {
                    return;
                }
                this.models.loading = false;
                this.models.loaded = true;
                match result {
                    Ok(v) => {
                        this.models.list = parse_models_list(&v);
                        this.models.error = None;
                    }
                    Err(e) => this.models.error = Some(e.user_text()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    #[cfg(test)]
    fn pull(&mut self, id: String, allow_unverified: bool, cx: &mut Context<Self>) {
        self.pull_model(id, allow_unverified, None, None, cx);
    }

    fn pull_model(
        &mut self,
        id: String,
        allow_unverified: bool,
        role: Option<String>,
        batch_window: Option<gpui::AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) {
        if self
            .models
            .pulls
            .get(&id)
            .is_some_and(PullState::in_progress)
        {
            return;
        }
        self.models
            .pulls
            .insert(id.clone(), PullState::Running(None));
        cx.notify();
        let cli = self.state.read(cx).cli().clone();
        let name = self.models.list.model_name(&id);
        let (task, mut progress, cancel) =
            spawn_pull(cli, id.clone(), allow_unverified, cx.background_executor());
        self.models.pull_cancels.insert(id.clone(), cancel);
        cx.spawn(async move |this, cx| {
            while let Some(pct) = progress.next().await {
                let id = id.clone();
                this.update(cx, |this, cx| {
                    // A cancel being processed keeps its own state.
                    if let Some(state @ PullState::Running(_)) = this.models.pulls.get_mut(&id)
                        && *state != PullState::Running(Some(pct))
                    {
                        *state = PullState::Running(Some(pct));
                        cx.notify();
                    }
                })
                .ok();
            }
            let result = task.await;
            this.update(cx, |this, cx| {
                this.models.pull_cancels.remove(&id);
                let state = PullState::finished(&name, &result);
                this.models.pulls.insert(id.clone(), state.clone());
                if state == PullState::Done {
                    if let Some(row) = this.models.list.rows.iter_mut().find(|row| row.id == id) {
                        row.state = "installed".into();
                    }
                    if let Some(role) = role {
                        this.use_model(role, id.clone(), allow_unverified, cx);
                    }
                } else if let Some(role) = role {
                    let previous = this
                        .models
                        .list
                        .rows
                        .iter()
                        .find(|row| row.active_for.contains(&role))
                        .map(|row| this.models.list.model_name(&row.id));
                    if let Some(previous) = previous {
                        this.models.outcomes.insert(
                            (role, id.clone()),
                            (StatusKind::Info, format!("Still using {previous}.")),
                        );
                    }
                }
                if batch_window.is_some() && state != PullState::Done {
                    this.models.batch = None;
                    this.models.batch_status = Some((
                        if state == PullState::Cancelled {
                            StatusKind::Info
                        } else {
                            StatusKind::Failed
                        },
                        format!("Download stopped at {name}. Models already downloaded are kept."),
                    ));
                }
                this.load_models(cx);
                cx.notify();
            })
            .ok();
            if let Some(handle) = batch_window {
                let _ = cx.update_window(handle, |_, window, cx| {
                    let _ = this.update(cx, |this, cx| this.next_model_download(window, cx));
                });
            }
        })
        .detach();
    }

    /// Asks the running download of `id` to stop; its own answer ends it.
    fn cancel_pull(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(cancel) = self.models.pull_cancels.get(id) else {
            return;
        };
        cancel.cancel();
        self.models
            .pulls
            .insert(id.to_owned(), PullState::Cancelling);
        cx.notify();
    }

    fn use_model(
        &mut self,
        role: String,
        id: String,
        allow_unverified: bool,
        cx: &mut Context<Self>,
    ) {
        self.models.using = Some((role.clone(), id.clone()));
        cx.notify();
        let mut a = args(["models", "use", role.as_str(), id.as_str()]);
        if allow_unverified {
            a.push("--allow-unverified-license".into());
        }
        let task = self.state.read(cx).run_cli(a, ChildKind::Command, cx);
        // Named as the download line names it.
        let name = self
            .providers
            .read(cx)
            .rows()
            .find(|provider| provider.id == id)
            .map_or_else(
                || self.models.list.model_name(&id),
                |provider| provider.name.clone(),
            );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.models.using = None;
                if result
                    .as_ref()
                    .ok()
                    .and_then(|v| v["refresh_queued"].as_u64())
                    .is_some_and(|count| count > 0)
                {
                    this.state.update(cx, AppState::kick_jobs);
                }
                let message = match result {
                    Ok(v) => {
                        let mut text = format!(
                            "{name} is now used for {}.",
                            role_label(&role).to_lowercase()
                        );
                        if v["installed"].as_bool() == Some(false) {
                            text.push_str(" It is not downloaded yet.");
                        }
                        if v["thresholds_calibrated"].as_bool() == Some(false) {
                            text.push_str(
                                " Speakers are only suggested until this model is calibrated.",
                            );
                        }
                        Ok(text)
                    }
                    Err(e) => Err(failure_text(
                        &format!("Couldn't switch to {name}."),
                        &e.user_text(),
                        "",
                    )),
                };
                this.models.outcomes.insert((role, id), outcome(message));
                cx.notify();
                reload_config(&this.state, cx).detach();
                this.load_models(cx);
            })
            .ok();
        })
        .detach();
    }

    fn request_model(
        &mut self,
        id: String,
        role: Option<String>,
        from_batch: bool,
        opener: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.models.list.rows.iter().find(|row| row.id == id) else {
            return;
        };
        let installed = row.installed();
        let verified = row.license_verified;
        let name = self.models.list.model_name(&id);
        let license = row.license.clone();
        let handle = from_batch.then(|| window.window_handle());
        if verified {
            self.start_model_action(id, role, installed, false, handle, cx);
            return;
        }
        let mut spec = kit::ConfirmSpec::new(
            kit::confirm_line("license-title", "Use a model with an unverified licence?"),
            "license-confirm",
            "license-actions",
            "license-cancel",
            kit::confirm_line(
                "license-accept",
                if role.is_some() && installed {
                    "Use anyway"
                } else {
                    "Download anyway"
                },
            ),
        );
        spec.primary_action = true;
        spec.irreversible = false;
        spec.caveat = Some(kit::confirm_line(
            "license-confirm-question",
            format!(
                "BroScribe hasn’t verified the licence of {name} ({license}). Check its terms before you use it."
            ),
        ));
        let cancel = cx.weak_entity();
        let accept = cancel.clone();
        kit::confirm_dialog(
            opener,
            window,
            cx,
            move |_| Some(spec.clone()),
            move |_, cx| {
                if from_batch {
                    let _ = cancel.update(cx, |this, cx| {
                        this.models.batch = None;
                        this.models.batch_status = Some((
                            StatusKind::Info,
                            "Download cancelled. Models already downloaded are kept.".into(),
                        ));
                        cx.notify();
                    });
                }
                true
            },
            move |_, cx| {
                let _ = accept.update(cx, |this, cx| {
                    this.start_model_action(id.clone(), role.clone(), installed, true, handle, cx)
                });
                true
            },
        );
    }

    fn start_model_action(
        &mut self,
        id: String,
        role: Option<String>,
        installed: bool,
        unverified: bool,
        batch_window: Option<gpui::AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) {
        if installed && let Some(role) = role {
            self.use_model(role, id, unverified, cx);
        } else {
            self.pull_model(id, unverified, role, batch_window, cx);
        }
    }

    fn next_model_download(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(batch) = &mut self.models.batch else {
            return;
        };
        if let Some(id) = batch.pop_front() {
            self.request_model(id, None, true, self.download_all_focus.clone(), window, cx);
        } else {
            self.models.batch = None;
            self.models.batch_status =
                Some((StatusKind::Done, "All models in use are downloaded.".into()));
            cx.notify();
        }
    }

    fn select_profile(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.begin_save("speech-model", cx) {
            return;
        }
        let task = self.state.read(cx).run_cli(
            args(["setup", "--non-interactive", "--profile", &id]),
            ChildKind::Command,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.saving.remove("speech-model");
                this.report_setting(
                    "speech-model",
                    result.map(|_| "Speech model changed.".into()).map_err(|e| {
                        failure_text("Couldn’t change the speech model.", &e.user_text(), "")
                    }),
                    cx,
                );
                reload_config(&this.state, cx).detach();
                this.load_models(cx);
            });
        })
        .detach();
    }

    fn run_doctor(&mut self, cx: &mut Context<Self>) {
        self.models.doctor_running = true;
        cx.notify();
        let task = self
            .state
            .read(cx)
            .run_cli(args(["models", "doctor"]), ChildKind::Command, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.models.doctor_running = false;
                this.models.doctor = Some(result.map_err(|e| e.user_text()));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---- Privacy ----

    fn load_enrollment(&mut self, cx: &mut Context<Self>) {
        let library = self.state.read(cx).library().clone();
        let task = cx.background_executor().spawn(async move {
            let saved = match library.self_person()? {
                Some(me) => !library.load_voiceprints(&me.id)?.embeddings.is_empty(),
                None => false,
            };
            Ok::<_, broscribe_core::Error>((saved, enrollment_files(&library)?))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.enrollment.loaded = true;
                match result {
                    Ok((saved, files)) => {
                        this.enrollment.saved = saved;
                        this.enrollment.files = files;
                        this.enrollment.error = None;
                    }
                    Err(e) => {
                        tracing::warn!(code = e.code(), "reading the enrollment recordings");
                        this.enrollment.error = Some(failure_text(
                            "Couldn't check for your voice reading.",
                            &core_user_text(&e),
                            "",
                        ))
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn confirm_enrollment_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.enrollment.deleting || self.enrollment.files.is_empty() {
            return;
        }
        let files = self.enrollment.files.clone();
        let opener = self.enrollment_delete_focus.clone();
        let settings = cx.weak_entity();
        kit::confirm_dialog(
            opener,
            window,
            cx,
            |_| {
                use kit::confirm_line as line;
                let mut spec = kit::ConfirmSpec::new(
                    line("enrollment-confirm", "Delete your voice reading?"),
                    "enrollment-confirm-body",
                    "enrollment-confirm-footer",
                    "enrollment-delete-no",
                    line("enrollment-delete-yes", "Delete voice reading"),
                );
                spec.goes = vec![line(
                    "enrollment-confirm-question",
                    "The kept voice reading.",
                )];
                spec.changes = vec![line(
                    "enrollment-confirm-changes",
                    "The reading, so BroScribe can't learn your voice again from it if you change speaker models",
                )];
                spec.stays = vec![line(
                    "enrollment-confirm-stays",
                    "Your saved voice: BroScribe still recognises you",
                )];
                Some(spec)
            },
            |_, _| true,
            move |_, cx| {
                let _ = settings.update(cx, |this, cx| {
                    if this.enrollment.files == files && !this.enrollment.deleting {
                        this.delete_enrollment(cx);
                    } else {
                        let text = "The kept voice readings changed. Review them before deleting.";
                        this.report_setting("enrollment", Err(text.into()), cx);
                    }
                });
                true
            },
        );
    }

    fn delete_enrollment(&mut self, cx: &mut Context<Self>) {
        self.enrollment.deleting = true;
        cx.notify();
        let task = self.state.read(cx).run_cli(
            args(["setup", "voice", "--delete-enrollment"]),
            ChildKind::Command,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.enrollment.deleting = false;
                let message = match result {
                    Ok(v) => {
                        let count = v["deleted_files"].as_u64().unwrap_or(0);
                        let deleted = match count {
                            0 => "There was no voice reading to delete.".to_owned(),
                            1 => "Deleted your voice reading.".to_owned(),
                            _ => format!("Deleted {count} voice readings."),
                        };
                        Ok(format!("{deleted} BroScribe still recognises your voice."))
                    }
                    Err(e) => Err(failure_text(
                        "Couldn't delete your voice reading.",
                        &e.user_text(),
                        "",
                    )),
                };
                this.report_setting("enrollment", message, cx);
                this.load_enrollment(cx);
            })
            .ok();
        })
        .detach();
    }

    // ---- rendering ----

    fn render_library_setting(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let root = state.library().root().display().to_string();
        let env_locked = state.paths().library_from_env.is_some();
        let capacity = if self.use_vertical_fields(cx) { 32 } else { 48 };
        let shown = if root.chars().count() > capacity {
            let left: String = root.chars().take(capacity / 3).collect();
            let right: String = root
                .chars()
                .skip(root.chars().count() - (capacity * 2 / 3))
                .collect();
            format!("{left}…{right}")
        } else {
            root.clone()
        };
        let waiting = match &self.library_switch {
            LibrarySwitch::Waiting(p) => Some(folder_name(p)),
            LibrarySwitch::Idle => None,
        };
        kit::stack()
            .child(
                kit::fact_row_mono("settings-library-folder", "Library folder", shown, cx)
                    .aria_label(format!("Library folder: {root}"))
                    .tooltip(kit::title_tooltip(root)),
            )
            .when(env_locked, |el| {
                el.child(muted(
                    "settings-library-env",
                    "This library was chosen when BroScribe started, so you can't change it here.",
                    cx,
                ))
            })
            .when_some(waiting.clone(), |el, name| {
                el.child(kit::status_line(
                    "settings-library-switching",
                    StatusKind::Pending,
                    format!("Switches to “{name}” when recording and background jobs finish."),
                    cx,
                ))
            })
            .child(
                row()
                    .child(
                        Button::new("reveal-library")
                            .label(NativePlatform::host_platform().reveal_label())
                            .on_click(cx.listener(|this, _, _, cx| {
                                let path = this.state.read(cx).library().root().to_owned();
                                cx.reveal_path(&path);
                            })),
                    )
                    .child(
                        Button::new("choose-library")
                            .label("Change…")
                            .track_focus(&self.library_change_focus)
                            .disabled(env_locked || waiting.is_some())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.choose_library(window, cx)),
                            ),
                    ),
            )
            .children(self.setting_notice("library", cx))
    }

    fn render_disk_use(&self, cx: &App) -> impl IntoElement {
        kit::stack()
            .child(heading("storage-disk-use", "Disk use"))
            .child(match self.disk_use {
                None => kit::status_line(
                    "storage-disk-use-value",
                    StatusKind::Pending,
                    "Measuring…",
                    cx,
                ),
                Some((library, free)) => {
                    muted("storage-disk-use-value", disk_usage_text(library, free), cx)
                }
            })
    }

    fn render_default_language(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let config = self.state.read(cx).config();
        let preferred = self
            .models
            .list
            .profiles
            .iter()
            .find(|profile| profile.asr == config.profile.asr);
        let language = self.language.read(cx).value();
        let warning = preferred.filter(|profile| {
            !language.is_empty()
                && !profile.languages.iter().any(|code| {
                    code == "*"
                        || code.eq_ignore_ascii_case(language.as_ref())
                        || language
                            .split_once('-')
                            .is_some_and(|(base, _)| code.eq_ignore_ascii_case(base))
                })
        });
        kit::field(
            "settings-language-title", "Meeting language",
            Some(("settings-language-note".into(),
                "Used when a recording or import doesn't name its language, and to choose the passage you read in Your voice.".into())),
            |label| row()
                .child(div().min_w_0().w(gpui::rems(18.)).child(
                    crate::ui::select::Picker::languages(
                        "settings-language-picker", label,
                        preferred.map_or(&[], |profile| profile.languages.as_slice()), &language, cx,
                        |this, value, window, cx| {
                            this.language.update(cx, |input, cx| input.set_value(value.to_owned(), window, cx));
                            this.save_language(window, cx);
                        },
                    )
                )),
            cx,
        ).children(warning.map(|profile| kit::status_line(
            "settings-language-support", StatusKind::Attention,
            format!("{} doesn't list {}. Choose a multilingual speech model under Transcription.",
                profile.description, crate::ui::format::language_name(&language)),
            cx,
        ))).children(self.setting_notice("language", cx))
    }

    fn use_vertical_fields(&self, cx: &App) -> bool {
        matches!(
            self.state.read(cx).config().appearance.reading_size,
            ReadingSize::Larger | ReadingSize::Largest
        )
    }

    fn render_consent_reminder(&self, cx: &Context<Self>) -> impl IntoElement {
        kit::field_row(
            "consent-reminder-label", "Consent reminder",
            Some(("consent-reminder-help".into(), "Shows “Let everyone know this meeting is being recorded.” in the recording controls.".into())),
            self.use_vertical_fields(cx),
            |label| Switch::new("consent-reminder").small()
                    .accessibility_label(label)
                    .checked(self.state.read(cx).config().consent_reminder).disabled(self.saving.contains("consent-reminder"))
                    .on_change(cx.listener(|this, checked, _, cx| {
                        let enabled = *checked;
                        this.apply_setting("consent-reminder", move |config| config.consent_reminder = enabled, cx);
                    })),
            cx,
        ).children(self.setting_notice("consent-reminder", cx))
    }

    #[cfg(test)]
    fn render_general(&self, cx: &mut Context<Self>) -> impl IntoElement {
        kit::section(
            "settings-general",
            "General",
            true,
            div()
                .flex()
                .flex_col()
                .gap(kit::GROUP_GAP)
                .child(self.render_library_setting(cx))
                .child(self.render_default_language(cx))
                .child(self.render_mic(cx))
                .child(self.render_login(cx))
                .when(
                    broscribe_platform_native::NativePlatform::host_platform()
                        == broscribe_platform::HostPlatform::MacOs,
                    |el| el.child(self.render_status_item(cx)),
                )
                .child(self.render_consent_reminder(cx))
                .child(self.render_theme_choice(cx))
                .child(self.render_reading_size(cx))
                .child(self.render_live_identity(cx))
                .child(
                    Button::new("open-updates")
                        .self_start()
                        .label("Software updates")
                        .accessibility_label("Open software updates")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.state.update(cx, |_, cx| {
                                cx.emit(crate::state::AppEvent::OpenSettings(Some(
                                    SettingsPage::Updates,
                                )))
                            });
                        })),
                ),
        )
    }

    /// Appearance (`[appearance] theme`): System (the default), Light or
    /// Dark, one radio each.
    fn render_theme_choice(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.state.read(cx).config().appearance.theme;
        let mut group = ChoiceGroup::new(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(kit::CONTROL_GAP),
            "settings-theme",
            "Appearance",
        );
        for (ix, choice) in ThemeChoice::ALL.into_iter().enumerate() {
            group = group.option(kit::radio_option(
                ("theme", ix),
                theme_choice_label(choice),
                choice == current,
                true,
                cx,
                move |this, _, cx| this.set_theme_choice(choice, cx),
            ));
        }
        kit::field_heading(
            "settings-theme-title",
            "Appearance",
            Some((
                "settings-theme-note".into(),
                "System follows your computer's light or dark setting.".into(),
            )),
            |_| group,
            cx,
        )
        .children(self.setting_notice("theme", cx))
    }

    /// Reading size (`[appearance] reading_size`): one radio per step,
    /// smallest first. The app applies it to the theme's text sizes;
    /// colours and theme stay as they are.
    fn render_reading_size(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.state.read(cx).config().appearance.reading_size;
        let mut group = ChoiceGroup::new(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(kit::CONTROL_GAP),
            "settings-reading-size",
            "Text size",
        );
        for (ix, size) in ReadingSize::ALL.into_iter().enumerate() {
            let selected = size == current;
            let label = reading_size_label(size);
            group = group.option(kit::radio_option(
                ("reading-size", ix),
                label,
                selected,
                true,
                cx,
                move |this, _, cx| this.set_reading_size(size, cx),
            ));
        }
        kit::field_heading(
            "settings-reading-size-title",
            "Text size",
            Some((
                "settings-reading-size-note".into(),
                "Size of text in every BroScribe window.".into(),
            )),
            |_| group,
            cx,
        )
        .children(self.setting_notice("reading-size", cx))
    }

    /// `[live] identity`, read by the recording agent while it records.
    fn render_live_identity(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let on = self.state.read(cx).config().live_identity_enabled();
        kit::field_row(
            "settings-live-identity-title", "Live speaker recognition",
            Some(("settings-live-identity-note".into(),
                "While recording, the live transcript shows provisional voices and suggested names. Turning it off leaves recording, live transcription and final speaker recognition active.".into())),
            self.use_vertical_fields(cx),
            |label| Switch::new("live-identity").small().accessibility_label(label).checked(on).disabled(self.saving.contains("live-identity"))
                .on_change(cx.listener(|this, checked, _, cx| this.set_live_identity(*checked, cx))),
            cx,
        ).children(self.setting_notice("live-identity", cx))
    }

    fn mic_choices(&self, label: SharedString, cx: &Context<Self>) -> crate::ui::select::Picker {
        let pinned = self.state.read(cx).config().audio.pinned_mic.as_deref();
        let options = mic_options(self.mics.devices.as_deref(), pinned)
            .into_iter()
            .map(|option| {
                crate::ui::select::OptionItem::new(option.target.unwrap_or_default(), option.label)
            })
            .collect();
        crate::ui::select::Picker::new(
            "settings-mic-options",
            label,
            options,
            pinned.unwrap_or_default(),
            cx,
            |this, id, _, cx| this.pin_mic((!id.is_empty()).then(|| id.to_owned()), cx),
        )
        .disabled(self.mics.pinning)
    }

    fn render_mic(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let m = &self.mics;
        kit::stack()
            .id("settings-mic")
            .w_full()
            .child(kit::field_heading(
                "settings-mic-title",
                "Microphone",
                Some((
                    "settings-mic-note".into(),
                    "Used for the Me track. A chosen microphone is used only while it’s connected."
                        .into(),
                )),
                |label| self.mic_choices(label, cx),
                cx,
            ).w_full())
            .when(m.pinning, |el| {
                el.child(kit::status_line(
                    "settings-mic-changing",
                    StatusKind::Pending,
                    "Switching microphone…",
                    cx,
                ))
            })
            .when_some(
                self.state
                    .read(cx)
                    .config()
                    .audio
                    .pinned_mic
                    .as_deref()
                    .filter(|id| {
                        m.devices
                            .as_ref()
                            .is_some_and(|devices| devices.iter().all(|d| d.id != *id))
                    }),
                |el, id| {
                    el.child(kit::status_line(
                        "settings-mic-disconnected",
                        StatusKind::Attention,
                        format!("{id} isn’t connected, so recordings use the system default."),
                        cx,
                    ))
                },
            )
            .when(m.loading && m.devices.is_none(), |el| {
                el.child(kit::status_line(
                    "settings-mic-loading",
                    StatusKind::Pending,
                    "Looking for microphones…",
                    cx,
                ))
            })
            .when_some(m.error.clone(), |el, e| {
                el.child(kit::retry_line(
                    "settings-mic-error",
                    e,
                    cx,
                    |this, _, cx| this.load_mics(cx),
                ))
            })
            .when_some(m.result.clone(), |el, result| {
                let (kind, text) = outcome(result);
                el.child(kit::status_line("settings-mic-result", kind, text, cx))
            })
    }

    fn render_meeting_apps(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut group = kit::stack()
            .child(heading("recording-meeting-apps", "Meeting apps"))
            .child(muted(
                "recording-meeting-apps-help",
                "What BroScribe does when a meeting starts in each app.",
                cx,
            ));
        let apps = match &self.meeting_apps {
            None => {
                return group.child(kit::status_line(
                    "meeting-apps-loading",
                    StatusKind::Pending,
                    "Checking meeting apps…",
                    cx,
                ));
            }
            Some(Err(cause)) => {
                return group.child(kit::retry_line(
                    "meeting-apps-error",
                    cause.clone(),
                    cx,
                    |this, _, cx| this.load_meeting_apps(cx),
                ));
            }
            Some(Ok(apps)) => apps,
        };
        group = group.when_some(
            setup::meeting_apps_notice(apps, self.detection_off),
            |group, text| {
                group.child(kit::status_line(
                    "meeting-apps-attention",
                    StatusKind::Attention,
                    text,
                    cx,
                ))
            },
        );
        for app in apps {
            let id = app.id.clone();
            let status = self.detection_results.get(&id);
            let picker = crate::ui::select::Picker::new(
                format!("meeting-policy-{id}"),
                format!("{} meetings", app.name),
                [
                    ("auto", "Record automatically"),
                    ("ask", "Ask me"),
                    ("off", "Don’t record"),
                ]
                .into_iter()
                .map(|(id, label)| crate::ui::select::OptionItem::new(id, label))
                .collect(),
                app.policy.as_deref().unwrap_or_default(),
                cx,
                move |this, policy, _, cx| {
                    this.change_meeting_policy(id.clone(), policy.to_owned(), cx)
                },
            )
            .disabled(status.is_some_and(|(kind, _)| *kind == StatusKind::Pending));
            group = group.child(
                kit::field_row(
                    format!("meeting-app-{}", app.id),
                    app.name.clone(),
                    (app.installed == Some(false)).then(|| {
                        (
                            format!("meeting-app-absent-{}", app.id).into(),
                            "Not installed".into(),
                        )
                    }),
                    self.use_vertical_fields(cx),
                    |_| picker,
                    cx,
                )
                .children(status.map(|(kind, text)| {
                    kit::status_line(
                        format!("meeting-app-result-{}", app.id),
                        *kind,
                        text.clone(),
                        cx,
                    )
                })),
            );
        }
        group
    }

    fn render_system_checks(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use broscribe_platform::{HostPlatform, SettingsPane, SystemSettingsOpener as _};
        let opener = NativePlatform::system_settings();
        let platform = opener.host_platform();
        let mut group = kit::stack().child(heading("recording-system-checks", "System checks"));
        for (id, label, pane) in [
            ("microphone", "Microphone", SettingsPane::Microphone),
            ("system_audio", "System audio", SettingsPane::SystemAudio),
            (
                "accessibility",
                "Accessibility (optional)",
                SettingsPane::Accessibility,
            ),
            (
                "notifications",
                "Notifications",
                SettingsPane::Notifications,
            ),
        ] {
            if !opener.supports(pane) {
                continue;
            }
            let value = self
                .permissions
                .as_ref()
                .and_then(|result| result.as_ref().ok());
            let words = if pane == SettingsPane::Accessibility {
                if NativePlatform::app_ui_reader().is_trusted() {
                    "Allowed"
                } else {
                    "Not allowed"
                }
            } else {
                match if pane == SettingsPane::Notifications {
                    self.notifications_permission
                } else {
                    value.and_then(|v| v[id].as_str())
                } {
                    Some("granted") => "Allowed",
                    Some("denied" | "restricted") => "Not allowed",
                    Some("not_determined") => "Not asked yet",
                    _ => "Unknown",
                }
            };
            group = group.child(kit::field_row(
                format!("system-check-{id}"),
                label,
                Some((format!("system-check-{id}-state").into(), words.into())),
                self.use_vertical_fields(cx),
                |_| {
                    Button::new(format!("system-check-{id}-open"))
                        .label(if platform == HostPlatform::Windows {
                            "Open privacy settings"
                        } else {
                            "Open System Settings"
                        })
                        .accessibility_label(format!("Open system settings for {label}"))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            let task = cx
                                .background_executor()
                                .spawn(async move { NativePlatform::system_settings().open(pane) });
                            cx.spawn(async move |this, cx| {
                                if let Err(e) = task.await {
                                    let _ = this.update(cx, |this, cx| {
                                        this.report_setting(
                                            "system-checks",
                                            Err(failure_text(
                                                "Couldn’t open system settings.",
                                                &core_user_text(&e),
                                                "",
                                            )),
                                            cx,
                                        )
                                    });
                                }
                            })
                            .detach();
                        }))
                },
                cx,
            ));
        }
        let audio = match &self.audio_check_names {
            Some((mic, output)) => format!("Measured with {mic} and {output}"),
            None if !self.state.read(cx).config().audio_checks.is_empty() => {
                "Measured on a device pair whose names are unavailable.".into()
            }
            None => "Not run".into(),
        };
        group
            .child(kit::field_row(
                "recording-audio-check",
                "Audio check",
                Some(("recording-audio-check-state".into(), audio.into())),
                self.use_vertical_fields(cx),
                |_| {
                    Button::new("recording-run-audio-check")
                        .label("Run audio check")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_setup_step(Step::AudioCheck, window, cx)
                        }))
                },
                cx,
            ))
            .children(self.setting_notice("system-checks", cx))
            .children(self.setting_notice("setup", cx))
    }

    fn render_word_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let config = self.state.read(cx).config();
        let count = config.hotwords.len();
        let saving = self.saving.contains("word-list");
        kit::stack()
            .child(heading("settings-word-list", "Word list"))
            .child(muted(
                "settings-word-count",
                format!(
                    "{count} {} saved",
                    crate::ui::format::plural(count as u64, "word", "words")
                ),
                cx,
            ))
            .when_some(
                hotword_note(&self.models.list, &config.profile.asr),
                |group, note| {
                    group.child(kit::status_line(
                        "settings-word-model",
                        StatusKind::Attention,
                        note.text(),
                        cx,
                    ))
                },
            )
            .child(kit::field(
                "settings-words-title",
                "Words",
                None,
                |label| {
                    Textarea::new(&self.words)
                        .h(gpui::rems(7.5))
                        .aria_label(label)
                        .disabled(saving)
                },
                cx,
            ))
            .child(
                Button::new("settings-words-save")
                    .label("Save")
                    .map(|button| kit::primary(button, saving, cx))
                    .self_start()
                    .disabled(saving)
                    .on_click(cx.listener(|this, _, window, cx| this.save_words(window, cx))),
            )
            .children(self.setting_notice("word-list", cx))
    }

    fn render_speech_models(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let m = &self.models;
        let missing = m.list.missing_profile_models();
        let setup_downloads = self.state.read(cx).model_downloads().read(cx);
        let running = setup_downloads.running()
            || m.batch.is_some()
            || m.pulls.values().any(PullState::in_progress);
        kit::stack()
            .w_full()
            .when_some(setup_downloads.percent(), |group, percent| {
                group.child(kit::status_line(
                    "settings-setup-download",
                    StatusKind::Pending,
                    format!(
                        "Downloading speech models · {}",
                        crate::ui::format::percent(percent)
                    ),
                    cx,
                ))
            })
            .when_some(transcription_block(&missing), |group, words| {
                group
                    .child(kit::status_line(
                        "transcription-blocked",
                        StatusKind::Attention,
                        words,
                        cx,
                    ))
                    .child(
                        Button::new("transcription-download-all")
                            .label("Download all")
                            .map(|button| {
                                kit::primary(
                                    button,
                                    m.loading || running || self.saving.contains("speech-model"),
                                    cx,
                                )
                            })
                            .self_start()
                            .track_focus(&self.download_all_focus)
                            .disabled(m.loading || running || self.saving.contains("speech-model"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                let ids = this
                                    .models
                                    .list
                                    .missing_profile_models()
                                    .into_iter()
                                    .map(|row| row.id.clone())
                                    .collect();
                                this.models.batch_status = None;
                                this.models.batch = Some(ids);
                                this.next_model_download(window, cx);
                                cx.notify();
                            })),
                    )
            })
            .when(m.batch.is_some(), |group| {
                group.children(m.pulls.iter().filter_map(|(id, state)| {
                    let name = m.list.model_name(id);
                    let progress = match state {
                        PullState::Running(pct) => *pct,
                        PullState::Cancelling => None,
                        _ => return None,
                    };
                    let id = id.clone();
                    Some(
                        kit::stack()
                            .child(kit::status_line(
                                "transcription-download-progress",
                                StatusKind::Pending,
                                progress.map_or_else(
                                    || format!("Downloading {name}…"),
                                    |pct| {
                                        format!(
                                            "Downloading {name}: {}",
                                            crate::ui::format::percent(pct)
                                        )
                                    },
                                ),
                                cx,
                            ))
                            .children(progress.map(|pct| {
                                kit::progress(
                                    "transcription-progress-bar",
                                    format!(
                                        "Downloading {name}: {}",
                                        crate::ui::format::percent(pct)
                                    ),
                                    f32::from(pct) / 100.,
                                )
                            }))
                            .child(
                                Button::new("transcription-download-cancel")
                                    .label("Cancel")
                                    .small()
                                    .self_start()
                                    .disabled(*state == PullState::Cancelling)
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.cancel_pull(&id, cx)
                                        }),
                                    ),
                            ),
                    )
                }))
            })
            .when_some(m.batch_status.clone(), |group, (kind, text)| {
                group.child(kit::status_line(
                    "transcription-download-result",
                    kind,
                    text,
                    cx,
                ))
            })
            .child(kit::field_heading(
                "settings-speech-models",
                "Speech model",
                None,
                |_| {
                    speech_model_choices(
                        "settings-speech-options",
                        &m.list,
                        self.state.read(cx).config().default_language.as_deref(),
                        self.saving.contains("speech-model")
                            || running
                            || m.loading
                            || m.using.is_some(),
                        cx,
                        Self::select_profile,
                    )
                },
                cx,
            ))
            .when_some(m.error.as_ref(), |group, cause| {
                group.child(kit::retry_line(
                    "speech-model-list-error",
                    failure_text("Couldn’t list the models.", cause, ""),
                    cx,
                    |this, _, cx| this.load_models(cx),
                ))
            })
            .when(m.loading && !m.loaded, |group| {
                group.child(kit::status_line(
                    "speech-model-list-loading",
                    StatusKind::Pending,
                    "Checking models…",
                    cx,
                ))
            })
            .children(self.setting_notice("speech-model", cx))
    }

    fn render_advanced_models(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        kit::stack()
            .w_full()
            .child(kit::disclosure(
                "settings-models-advanced",
                "All models (advanced)",
                self.models.advanced,
                cx,
                |this, _, cx| {
                    this.models.advanced = !this.models.advanced;
                    cx.notify();
                },
            ))
            .when(self.models.advanced, |group| {
                group.child(self.render_models(
                    window.viewport_size().width - gpui::rems(12.).to_pixels(window.rem_size())
                        < px(520.),
                    cx,
                ))
            })
    }

    fn render_models(&self, narrow: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let m = &self.models;
        let mut roles: Vec<String> = MODEL_ROLES.iter().map(|r| (*r).to_owned()).collect();
        for r in m.list.rows.iter().flat_map(|r| r.roles.iter()) {
            if !roles.contains(r) {
                roles.push(r.clone());
            }
        }
        let mut el = div()
            .flex()
            .flex_col()
            .gap_2()
            .child(heading("settings-models", "Models").mt_3())
            .child(muted(
                "settings-models-note",
                "Models run on this computer. Each download is checked against a published checksum.",
                cx,
            ));
        if let Some(e) = &m.error {
            el = el.child(kit::retry_line(
                "settings-models-error",
                failure_text("Couldn't list the models.", e, ""),
                cx,
                |this, _, cx| this.load_models(cx),
            ));
        } else if m.loading || !m.loaded {
            el = el.child(kit::status_line(
                "settings-models-loading",
                StatusKind::Pending,
                "Checking models…",
                cx,
            ));
        }
        for role in roles {
            let rows: Vec<&ModelRow> = m
                .list
                .rows
                .iter()
                .filter(|r| r.roles.contains(&role))
                .collect();
            if rows.is_empty() {
                continue;
            }
            el = el.child(
                crate::ui::subheading(
                    SharedString::from(format!("settings-models-role-{role}")),
                    role_label(&role),
                )
                .mt_2(),
            );
            for r in rows {
                el = el.child(self.model_row(r, &role, narrow, cx));
            }
        }
        el = el.child(
            row().mt_2().child(
                Button::new("models-doctor")
                    .label(if m.doctor_running {
                        "Verifying…"
                    } else {
                        "Check downloaded models"
                    })
                    .map(|button| {
                        crate::ui::kit::disable_with_reason(
                            button,
                            m.doctor_running,
                            "BroScribe is checking the installed models.",
                            cx,
                        )
                    })
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.run_doctor(cx))),
            ),
        );
        match &m.doctor {
            Some(Ok(v)) => {
                let ok = v["ok"].as_bool().unwrap_or(false);
                el = el.child(if ok {
                    kit::status_line(
                        "settings-models-verified",
                        StatusKind::Done,
                        "All models in use are downloaded and intact.",
                        cx,
                    )
                } else {
                    kit::status_line(
                        "settings-models-verified",
                        StatusKind::Failed,
                        "Some models in use are missing or damaged.",
                        cx,
                    )
                });
                if let Some(rows) = v["models"].as_array() {
                    for (i, r) in rows.iter().enumerate() {
                        el = el.child(muted(
                            ("settings-models-verified-row", i),
                            format!(
                                "{}: {}",
                                self.models
                                    .list
                                    .model_name(r["id"].as_str().unwrap_or_default()),
                                model_state_label(r["state"].as_str().unwrap_or_default())
                            ),
                            cx,
                        ));
                    }
                }
            }
            Some(Err(e)) => {
                el = el.child(kit::status_line(
                    "settings-models-verified",
                    StatusKind::Failed,
                    verify_failure(e),
                    cx,
                ))
            }
            None => {}
        }
        el
    }

    fn model_row(
        &self,
        r: &ModelRow,
        role: &str,
        narrow: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = r.active_for.iter().any(|a| a == role);
        let name = self.models.list.model_name(&r.id);
        let pull = self.models.pulls.get(&r.id);
        let running = pull.is_some_and(PullState::in_progress);
        let key = format!("model-{role}-{}", r.id);
        let size = bytes(r.bytes);
        let mut state = kit::stack().w(px(160.)).flex_none();
        let mut actions = div().flex().items_center().flex_none().gap_1();
        if let Some(PullState::Running(pct)) = pull {
            if let Some(pct) = pct {
                let pct_text = crate::ui::format::percent(*pct);
                state = state
                    .child(
                        kit::progress(
                            format!("pull-{role}-{}", r.id),
                            format!("Downloading {name}: {pct_text}"),
                            f32::from(*pct) / 100.,
                        )
                        .max_w(px(112.)),
                    )
                    .child(muted(format!("{key}-percent"), pct_text, cx));
            } else {
                state = state.child(kit::status_line(
                    format!("{key}-starting"),
                    StatusKind::Pending,
                    "Starting…",
                    cx,
                ));
            }
            let id = r.id.clone();
            actions = actions.child(
                Button::new(format!("pull-cancel-{role}-{}", r.id))
                    .small()
                    .label("Cancel")
                    .on_click(cx.listener(move |this, _, _, cx| this.cancel_pull(&id, cx))),
            );
        } else if matches!(pull, Some(PullState::Cancelling)) {
            state = state.child(kit::status_line(
                format!("{key}-state"),
                StatusKind::Pending,
                "Cancelling the download…",
                cx,
            ));
        } else {
            state = state.child(muted(format!("{key}-state"), r.state_label(), cx));
            if !r.installed() || (!active && role_usable(role)) {
                let id = r.id.clone();
                let use_for = (!active && role_usable(role)).then(|| role.to_owned());
                let action = match (
                    r.installed(),
                    use_for.is_some(),
                    r.state.starts_with("corrupt:"),
                ) {
                    (true, _, _) => "Use",
                    (false, true, _) => "Download and use",
                    (false, false, true) => "Download again",
                    _ => "Download",
                };
                let opener = cx.focus_handle();
                let disabled = self.models.batch.is_some()
                    || (use_for.is_some()
                        && (self.models.using.is_some() || self.saving.contains("speech-model")));
                actions = actions.child(
                    Button::new(format!("{key}-action"))
                        .small()
                        .label(action)
                        .track_focus(&opener)
                        .disabled(disabled)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.request_model(
                                id.clone(),
                                use_for.clone(),
                                false,
                                opener.clone(),
                                window,
                                cx,
                            );
                        })),
                );
            }
        }
        if narrow {
            state = state.child(muted(format!("{key}-size"), size.clone(), cx));
        }
        let license = if r.license_verified {
            muted(
                format!("{key}-licence"),
                format!("{} licence", r.license),
                cx,
            )
        } else {
            kit::status_line(
                format!("{key}-licence"),
                StatusKind::Attention,
                format!("Licence not verified: {}", r.license),
                cx,
            )
        };
        let description = kit::stack()
            .flex_1()
            .min_w_0()
            .child(text_line(format!("{key}-name"), Role::Label, name.clone()).truncate())
            .child(
                muted(format!("{key}-id"), r.id.clone(), cx)
                    .font_family("monospace")
                    .truncate(),
            )
            .when(!r.download_hosts.is_empty(), |description| {
                description.child(muted(
                    format!("{key}-host"),
                    format!("Download host: {}", r.download_hosts.join(", ")),
                    cx,
                ))
            })
            .child(license)
            .when(active, |el| {
                el.child(kit::badge(kit::BadgeTone::Neutral, "In use", cx))
            });
        kit::stack()
            .child(
                kit::list_row(format!("{key}-row"), false, false, cx)
                    .items_center()
                    .tooltip(kit::title_tooltip(name))
                    .child(description)
                    .when(!narrow, |row| {
                        row.child(
                            muted(format!("{key}-size"), size, cx)
                                .w(px(72.))
                                .flex_none()
                                .text_right(),
                        )
                    })
                    .child(state)
                    .child(kit::row_trailing(actions).min_w(px(124.))),
            )
            .when_some(
                match pull {
                    Some(PullState::Failed(e)) => Some((StatusKind::Failed, e.clone())),
                    Some(PullState::Cancelled) => Some((
                        StatusKind::Done,
                        "Download cancelled; nothing new was installed.".into(),
                    )),
                    _ => None,
                },
                |el, (kind, text)| {
                    el.child(kit::status_line(
                        format!("{key}-pull-result"),
                        kind,
                        text,
                        cx,
                    ))
                },
            )
            .when(!running, |el| {
                el.children(
                    self.models
                        .outcomes
                        .get(&(role.to_owned(), r.id.clone()))
                        .map(|(kind, text)| {
                            kit::status_line(format!("{key}-use-result"), *kind, text.clone(), cx)
                        }),
                )
            })
    }

    fn render_your_voice(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let e = &self.enrollment;
        let bytes: u64 = e.files.iter().map(|(_, b)| b).sum();
        let mut el = kit::stack()
            .child(heading("settings-enrollment-heading", "Your voice reading").mt_2())
            .child(muted(
                "settings-enrollment-note",
                "The text you read aloud in setup is kept with your person, so BroScribe can learn your voice again if you change the speaker recognition model.",
                cx,
            ))
            .child(Button::new("settings-voice-record")
                .label(if e.saved { "Record your voice again" } else { "Record your voice" })
                .self_start()
                .on_click(cx.listener(|this, _, window, cx| this.open_setup_step(Step::Voice, window, cx))))
            .child(kit::disclosure("settings-voice-details", "More about the readings", e.details, cx,
                |this, _, cx| {
                    this.enrollment.details = !this.enrollment.details;
                    cx.notify();
                }))
            .when(e.details, |el| el.child(muted("settings-voice-retention",
                "Recording again, forgetting your voice or choosing Until transcribed removes the saved reading. BroScribe deletes the short confirmation and practice recordings after use.", cx)));
        if let Some(err) = &e.error {
            el = el.child(kit::status_line(
                "settings-enrollment-state",
                StatusKind::Failed,
                err.clone(),
                cx,
            ));
        } else if !e.loaded {
            el = el.child(kit::status_line(
                "settings-enrollment-state",
                StatusKind::Pending,
                "Looking for your voice reading…",
                cx,
            ));
        } else if e.files.is_empty() {
            el = el.child(muted(
                "settings-enrollment-state",
                if e.saved {
                    "Your voice is saved. No voice reading is kept."
                } else {
                    "Not recorded yet"
                },
                cx,
            ));
        } else {
            let count = e.files.len() as u64;
            let reading = if count == 1 {
                "Voice reading kept".to_owned()
            } else {
                format!("{count} voice readings kept")
            };
            el = el.child(text_line(
                "settings-enrollment-state",
                Role::Label,
                format!("{reading} · {}", crate::ui::format::bytes(bytes)),
            ));
            el = el.child(
                Button::new("enrollment-delete")
                    .label(if e.deleting {
                        "Deleting…"
                    } else {
                        "Delete voice reading"
                    })
                    .danger()
                    .outline()
                    .self_start()
                    .track_focus(&self.enrollment_delete_focus)
                    .map(|button| {
                        crate::ui::kit::disable_with_reason(
                            button,
                            e.deleting,
                            "BroScribe is deleting the kept reading.",
                            cx,
                        )
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.confirm_enrollment_delete(window, cx);
                    })),
            );
        }
        el.children(self.setting_notice("enrollment", cx))
            .children(self.setting_notice("setup", cx))
    }

    fn render_privacy(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use broscribe_platform::HostPlatform;
        let calendar = match NativePlatform::host_platform() {
            HostPlatform::MacOs => {
                "BroScribe reads event titles and attendees from the Apple calendars you allow and the Microsoft 365 or Google calendars you connect. Calendar services receive no recordings or transcripts."
            }
            HostPlatform::Windows | HostPlatform::Linux => {
                "BroScribe signs in to read event titles and attendees of the Microsoft 365 and Google calendars you choose. Calendar services receive no recordings or transcripts."
            }
        };
        let channels = [
            (
                "providers",
                "AI providers",
                "When you run a summary or template, BroScribe sends transcript text and related text context to the provider you enabled. A search provider receives transcript text to build the search index. Audio and voice data are never sent to AI providers.",
                SettingsPage::AiProviders,
            ),
            (
                "tools",
                "AI tools",
                "Tools you connect through MCP or the terminal command can read your meetings. Each tool governs what it sends to its own AI service.",
                SettingsPage::AiTools,
            ),
            ("calendars", "Calendars", calendar, SettingsPage::Calendars),
            (
                "updates",
                "Updates",
                "When automatic checks are on, BroScribe asks its update feed for new versions a minute after it starts and every 12 hours.",
                SettingsPage::Updates,
            ),
            (
                "downloads",
                "Model downloads",
                "Downloading a model contacts the host listed for it.",
                SettingsPage::Transcription,
            ),
        ];
        kit::stack()
            .child(kit::section("privacy-local", "Stays on this computer", true,
                div().child(muted("settings-privacy-local",
                    "Recordings, transcripts, voice data and the search index stay in your library folder on this computer. Transcription and speaker recognition run on this computer.", cx))))
            .child(kit::section("privacy-optional", "Leaves only when you turn it on", true,
                kit::stack().children(channels.into_iter().map(|(id, label, note, page)|
                    kit::field_row(format!("privacy-{id}"), label, Some((format!("privacy-{id}-note").into(), note.into())),
                        self.use_vertical_fields(cx),
                        |_| Button::new(format!("privacy-open-{id}")).label(format!("Open {}", page.title())).ghost()
                            .on_click(cx.listener(move |this, _, window, cx| this.set_page(page, window, cx))),
                        cx,
                    )
                ))))
            .child(kit::field_row("privacy-forget", "Forget a person or a voice",
                Some(("privacy-forget-note".into(), "Deletes their voice samples. Meetings keep their transcripts.".into())),
                self.use_vertical_fields(cx), |_| Button::new("privacy-open-people").label("Open People")
                    .on_click(cx.listener(|this, _, _, cx| this.state.update(cx, |state, cx| {
                        state.navigate(crate::state::Route::People { person: None }, cx);
                        cx.emit(crate::state::AppEvent::OpenMainWindow);
                    }))), cx))
    }

    fn render_providers(&self, cx: &mut Context<Self>) -> impl IntoElement {
        kit::stack().w_full()
            .child(muted("settings-providers-note",
                "An AI provider you turn on writes summaries, decisions and action items. BroScribe sends it the transcript text of those meetings, under your own plan and its terms. Audio and voice data are never sent. All providers are off until you turn one on.", cx))
            .child(self.providers.clone())
    }

    fn render_summaries(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let config = self.state.read(cx).config();
        let providers = self.providers.read(cx);
        let rows = providers.summary_rows(config);
        let current = rows
            .iter()
            .find(|row| config.profile.llm.is_empty() || config.profile.llm.contains(&row.id));
        let saving = rows.iter().any(|row| providers.is_configuring(&row.id));
        let length = self.summary_instructions.read(cx).value().chars().count();
        let dirty = self.instructions_dirty(cx);
        let invalid = length > broscribe_core::config::INSTRUCTIONS_MAX_CHARS;
        let selected = current.map_or("", |row| row.id.as_str());
        let enabled = config.auto_enrich == AutoEnrich::New;
        kit::stack().w_full()
            .child(kit::field_row("settings-auto-enrich-title", "Summarise imported recordings automatically",
                Some(("settings-auto-enrich-note".into(), "When you import a single recording, your AI provider summarises it. Meetings you record, and folder imports, are summarised when you ask.".into())),
                self.use_vertical_fields(cx), |label| Switch::new("settings-auto-enrich").small()
                    .accessibility_label(label).checked(enabled).disabled(self.saving.contains("summary-mode"))
                    .on_change(cx.listener(|this, checked, _, cx| this.set_auto_enrich(if *checked { AutoEnrich::New } else { AutoEnrich::Off }, cx))), cx))
            .children(self.setting_notice("summary-mode", cx))
            .when(rows.is_empty(), |el| el.child(kit::status_line("summary-no-provider", StatusKind::Attention,
                "No AI provider is turned on, so nothing is summarised automatically.", cx)))
            .child(kit::field_row("summary-provider-label", "Summarise with",
                Some(("summary-provider-note".into(), "Used for automatic summaries and the templates that run after them. In a meeting you can choose another provider for any run.".into())),
                self.use_vertical_fields(cx), |label| crate::ui::select::Picker::new("summary-provider", label,
                    if rows.is_empty() {
                        vec![crate::ui::select::OptionItem::new("", "Turn on an AI provider first.")]
                    } else {
                        rows.iter().map(|row| crate::ui::select::OptionItem::new(&row.id, row.name.clone())).collect()
                    }, selected, cx,
                    |this, id, _, cx| this.providers.update(cx, |view, cx| view.prefer(id.to_owned(), cx)))
                    .disabled(rows.is_empty() || saving), cx))
            .when(current.is_none(), |el| el.child(Button::new("summary-open-providers").label("Open AI providers").ghost()
                .on_click(cx.listener(|this, _, window, cx| this.set_page(SettingsPage::AiProviders, window, cx)))))
            .when(saving, |el| el.child(kit::status_line("summary-provider-pending", StatusKind::Pending, "Saving…", cx)))
            .children(rows.iter().filter_map(|row| providers.result(&row.id, super::providers::ProviderControl::Consent))
                .filter_map(|result| result.as_ref().err())
                .enumerate().map(|(i, cause)| kit::status_line(("summary-provider-error", i), StatusKind::Failed, cause.clone(), cx)))
            .child(kit::field("settings-summary-instructions-title", "What should summaries focus on?",
                Some(("settings-summary-instructions-note".into(),
                    "Used for every summary unless you change it for one run in the meeting. It can set emphasis, language and length. The summary keeps its sections.".into())),
                |label| Textarea::new(&self.summary_instructions).h(gpui::rems(6.)).aria_label(label)
                    .disabled(self.instructions_saving), cx))
            .child(div().w_full().flex().justify_between().items_center()
                .child(row().child(Button::new("summary-instructions-save").label("Save")
                    .map(|button| kit::primary(button, self.instructions_saving || !dirty || invalid, cx))
                    .disabled(self.instructions_saving || !dirty || invalid)
                    .on_click(cx.listener(|this, _, _, cx| this.save_summary_instructions(cx))))
                    .when(dirty, |row| row
                        .child(Button::new("summary-instructions-revert").label("Revert").ghost().disabled(self.instructions_saving)
                            .on_click(cx.listener(|this, _, window, cx| this.revert_instructions(window, cx))))
                        .child(muted("summary-instructions-unsaved", "Unsaved changes", cx))))
                .child(if invalid {
                    kit::status_line("summary-instructions-count", StatusKind::Failed, format!("{length} / 4,000"), cx)
                } else {
                    muted("summary-instructions-count", format!("{length} / 4,000"), cx)
                }))
            .when_some(self.instructions_notice.clone(), |el, (text, error)| el.child(kit::status_line(
                "settings-summary-instructions-notice", if error { StatusKind::Failed } else { StatusKind::Done }, text, cx)))
    }

    fn render_search_model(&self, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::ui::select::{OptionItem, Picker};
        let config = self.state.read(cx).config();
        let active = config.profile.text_embedding.as_str();
        let providers = self.providers.read(cx);
        let local: Vec<_> = self
            .models
            .list
            .rows
            .iter()
            .filter(|row| row.roles.iter().any(|role| role == "text_embedding"))
            .collect();
        let mut options: Vec<_> = local
            .iter()
            .map(|row| {
                OptionItem::new(
                    &row.id,
                    format!(
                        "On this computer ({})",
                        self.models.list.model_name(&row.id)
                    ),
                )
            })
            .collect();
        options.extend(
            providers
                .rows()
                .filter(|row| {
                    row.purpose.as_deref() == Some("text_embedding")
                        && config
                            .provider(&row.id)
                            .is_some_and(|provider| provider.is_consented())
                        && super::providers::http_configured(config.provider(&row.id))
                })
                .map(|row| OptionItem::new(&row.id, row.name.clone())),
        );
        let local_active = local.into_iter().find(|row| row.id == active);
        let remote_active = local_active.is_none() && options.iter().any(|row| row.value == active);
        kit::stack()
            .w_full()
            .child(heading("settings-search-model", "Search"))
            .child(kit::field_row(
                "settings-search-by-meaning",
                "Search by meaning",
                Some((
                    "settings-search-description".into(),
                    "Finds meetings by what was meant. Keyword search always works.".into(),
                )),
                self.use_vertical_fields(cx),
                |label| {
                    Picker::new(
                        "settings-search-picker",
                        label,
                        options,
                        active,
                        cx,
                        |this, id, window, cx| {
                            let local = this.models.list.rows.iter().any(|row| row.id == id);
                            if local {
                                this.request_model(
                                    id.into(),
                                    Some("text_embedding".into()),
                                    false,
                                    this.search_model_focus.clone(),
                                    window,
                                    cx,
                                );
                            } else {
                                this.use_model("text_embedding".into(), id.into(), false, cx);
                            }
                        },
                    )
                    .disabled(self.models.using.is_some() || self.models.batch.is_some())
                },
                cx,
            ))
            .when(remote_active, |group| {
                group.child(muted(
                    "search-provider-privacy",
                    "Transcript text is sent to that server to build the search index.",
                    cx,
                ))
            })
            .when_some(local_active.filter(|row| !row.installed()), |group, row| {
                let id = row.id.clone();
                let name = self.models.list.model_name(&id);
                let downloading = self
                    .models
                    .pulls
                    .get(&id)
                    .is_some_and(PullState::in_progress);
                let cancel = id.clone();
                group
                    .child(kit::status_line(
                        "search-model-download-needed",
                        StatusKind::Attention,
                        format!("Search by meaning needs {name} ({}).", bytes(row.bytes)),
                        cx,
                    ))
                    .child(
                        Button::new("search-model-download")
                            .label(if downloading { "Cancel" } else { "Download" })
                            .map(|button| kit::primary(button, self.models.batch.is_some(), cx))
                            .track_focus(&self.search_model_focus)
                            .disabled(self.models.batch.is_some())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if downloading {
                                    this.cancel_pull(&cancel, cx);
                                } else {
                                    this.request_model(
                                        id.clone(),
                                        Some("text_embedding".into()),
                                        false,
                                        this.search_model_focus.clone(),
                                        window,
                                        cx,
                                    );
                                }
                            })),
                    )
            })
            .when(
                self.models
                    .using
                    .as_ref()
                    .is_some_and(|(role, _)| role == "text_embedding")
                    || self.state.read(cx).jobs().active.iter().any(|job| {
                        job.kind.as_deref() == Some(broscribe_core::queue::VECTORS_KIND)
                    }),
                |group| {
                    group.child(kit::status_line(
                        "search-model-pending",
                        StatusKind::Pending,
                        "Refreshing search…",
                        cx,
                    ))
                },
            )
            .children(
                self.models
                    .outcomes
                    .iter()
                    .filter(|((role, _), _)| role == "text_embedding")
                    .map(|((_, id), (kind, words))| {
                        kit::status_line(
                            format!("search-model-result-{id}"),
                            *kind,
                            words.clone(),
                            cx,
                        )
                    }),
            )
    }

    fn render_status_item(&self, cx: &mut Context<Self>) -> impl IntoElement {
        kit::field_row(
            "settings-status-item-title",
            "Show BroScribe in the menu bar",
            None,
            self.use_vertical_fields(cx),
            |label| {
                Switch::new("settings-status-item")
                    .small()
                    .accessibility_label(label)
                    .checked(self.state.read(cx).config().show_status_item())
                    .disabled(self.saving.contains("status-item"))
                    .on_change(cx.listener(|this, checked, _, cx| {
                        let show = *checked;
                        this.apply_setting(
                            "status-item",
                            move |c| c.app.show_status_item = Some(show),
                            cx,
                        );
                    }))
            },
            cx,
        )
        .children(self.setting_notice("status-item", cx))
    }

    fn render_login(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let login = &self.login;
        let enabled = login
            .value
            .unwrap_or_else(|| self.state.read(cx).config().open_at_login());
        let pending = if login.running {
            Some("Saving…")
        } else if login.value.is_none() && login.error.is_none() {
            Some("Checking whether BroScribe starts at login…")
        } else {
            None
        };
        kit::field_row(
            "settings-login-title",
            "Start BroScribe at login",
            Some((
                "settings-login-note".into(),
                login_description(self.tray_place).into(),
            )),
            self.use_vertical_fields(cx),
            |label| {
                Switch::new("settings-login")
                    .small()
                    .accessibility_label(label)
                    .checked(enabled)
                    .disabled(login.running)
                    .on_change(
                        cx.listener(|this, checked, _, cx| this.set_open_at_login(*checked, cx)),
                    )
            },
            cx,
        )
        .children(pending.map(|message| {
            kit::status_line("settings-login-pending", StatusKind::Pending, message, cx)
        }))
        .children(login.error.as_ref().map(|e| {
            kit::status_line(
                "settings-login-error",
                StatusKind::Failed,
                failure_text("Couldn't change this setting.", e, ""),
                cx,
            )
        }))
        .when(
            self.tray_place == broscribe_platform::StatusIconPlace::NoTrayHost,
            |el| {
                el.child(kit::banner(
                    "settings-no-tray",
                    StatusKind::Info,
                    Role::Note,
                    crate::ui::NO_TRAY_TEXT,
                    cx,
                ))
            },
        )
    }

    fn render_agents(&self) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(heading("settings-agents", "AI tools").mt_3())
            .child(self.agents.clone())
    }
}

/// Storage & retention, shared with the setup assistant's Storage step.
/// `on_handling`/`on_retention`/`on_days` receive the user's choice.
#[allow(clippy::too_many_arguments)]
pub(crate) fn storage_section<V: 'static>(
    config: &Config,
    days_input: &Entity<InputState>,
    retention_guard: &RetentionGuard,
    cx: &mut Context<V>,
    on_handling: impl Fn(&mut V, Handling, &mut Context<V>) + Clone + 'static,
    on_retention: impl Fn(&mut V, Retention, RetentionTrigger, &mut Window, &mut Context<V>)
    + Clone
    + 'static,
    handling_status: Option<Stateful<Div>>,
) -> AnyElement {
    let handling = config.import_handling;
    let retention = config.retention;
    let consequence = match retention {
        Retention::KeepForever => {
            "Recordings stay in the library until you delete them; you can always replay them and \
             re-run transcription or speaker recognition with other models."
                .to_owned()
        }
        Retention::DeleteImmediately { wait_until_named } => format!(
            "The library's copy of each recording is deleted as soon as its transcript and \
             speaker recognition are done{}. Transcripts, names and voice data stay. You can \
             no longer replay the recording, transcribe it again or use it to learn voices. \
             BroScribe deletes your voice reading after learning your voice.",
            if wait_until_named {
                " and every speaker is named"
            } else {
                ""
            }
        ),
        Retention::DeleteAfterDays { days } => format!(
            "The library's copy of each recording is deleted {} after the meeting starts. \
             The same period applies from the day you record your voice reading. After that, \
             the audio is unavailable for playback, transcription and learning voices.",
            crate::ui::format::count(u64::from(days), "day", "days"),
        ),
    };
    let mut el = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(kit::section(
            "storage-handling-title",
            "Imported recordings",
            true,
            div()
                .child(handling_choices(handling, cx, on_handling))
                .child(muted(
                    "storage-handling-note",
                    recording_file_consequence(handling),
                    cx,
                ))
                .children(handling_status),
        ))
        .child(heading("storage-retention-title", "Keep recordings").mt_2())
        .child(retention_choices_with_guard(
            retention,
            Some(retention_guard),
            Some(days_input),
            cx,
            on_retention,
        ));
    if retention_guard.changed_elsewhere() {
        el = el.child(kit::status_line(
            "storage-retention-changed",
            kit::StatusKind::Attention,
            RETENTION_CHANGED_ELSEWHERE,
            cx,
        ));
    }
    if let Some(status) = retention_guard.status() {
        el = el.child(match status {
            RetentionStatus::Pending => kit::status_line(
                "storage-retention-status",
                kit::StatusKind::Pending,
                "Checking which recordings this deletes…",
                cx,
            ),
            RetentionStatus::Saving => kit::status_line(
                "storage-retention-status",
                kit::StatusKind::Pending,
                "Saving…",
                cx,
            ),
            RetentionStatus::Failed(text) => kit::status_line(
                "storage-retention-status",
                kit::StatusKind::Failed,
                text.clone(),
                cx,
            ),
        });
    }
    el.child(muted("storage-consequence", consequence, cx))
        .child(muted("storage-ssd-caveat", ssd_caveat(), cx))
        .into_any_element()
}

/// How imported recordings are handled, as one radio group.
pub(crate) fn handling_choices<V: 'static>(
    handling: Handling,
    cx: &mut Context<V>,
    on_handling: impl Fn(&mut V, Handling, &mut Context<V>) + Clone + 'static,
) -> ChoiceGroup {
    recording_file_choices(
        "storage-handling",
        "Imported recordings",
        handling,
        true,
        cx,
        on_handling,
    )
}

pub(crate) fn recording_file_choices<V: 'static>(
    id: &'static str,
    label: &'static str,
    handling: Handling,
    allow_move: bool,
    cx: &Context<V>,
    on_handling: impl Fn(&mut V, Handling, &mut Context<V>) + Clone + 'static,
) -> ChoiceGroup {
    let mut group = ChoiceGroup::new(
        div().flex().flex_col().items_start().gap(kit::CONTROL_GAP),
        id,
        label,
    );
    for (id, label, value) in [
        ("handling-copy", "Copy into the library", Handling::Copy),
        ("handling-move", "Move into the library", Handling::Move),
        ("handling-reference", "Keep in place", Handling::Reference),
    ]
    .into_iter()
    .filter(|(_, _, value)| *value != Handling::Move || allow_move)
    {
        let f = on_handling.clone();
        group = group.option(choice(
            id,
            label,
            handling == value,
            cx.listener(move |this, _: &bool, _, cx| f(this, value, cx)),
        ));
    }
    group
}

pub(crate) fn recording_file_consequence(handling: Handling) -> &'static str {
    match handling {
        Handling::Copy => {
            "The library keeps its own copy. The original stays untouched. This uses extra disk space."
        }
        Handling::Move => {
            "The file moves into the library and leaves its folder. The library then holds your only copy. Deleting the meeting deletes the recording."
        }
        Handling::Reference => {
            "The library points at the file where it is. If you move, rename or delete it, playback and reprocessing stop working. Retention never deletes files kept in place."
        }
    }
}

/// How long recordings are kept, as one radio group. Choosing "after a
/// number of days" keeps the current number (30 when there is none).
#[cfg(test)]
pub(crate) fn retention_choices<V: 'static>(
    retention: Retention,
    cx: &mut Context<V>,
    on_retention: impl Fn(&mut V, Retention, &mut Context<V>) + Clone + 'static,
) -> ChoiceGroup {
    retention_choices_with_guard(retention, None, None, cx, move |this, value, _, _, cx| {
        on_retention(this, value, cx)
    })
}

fn retention_choices_with_guard<V: 'static>(
    retention: Retention,
    guard: Option<&RetentionGuard>,
    days_input: Option<&Entity<InputState>>,
    cx: &mut Context<V>,
    on_retention: impl Fn(&mut V, Retention, RetentionTrigger, &mut Window, &mut Context<V>)
    + Clone
    + 'static,
) -> ChoiceGroup {
    let days = match retention {
        Retention::DeleteAfterDays { days } => days,
        _ => 30,
    };
    let mut group = ChoiceGroup::new(
        kit::stack().items_start(),
        "storage-retention",
        "Keep recordings",
    );
    for (id, label, value, trigger) in [
        (
            "retention-keep",
            "Forever",
            Retention::KeepForever,
            RetentionTrigger::Keep,
        ),
        (
            "retention-immediate",
            "Until transcribed",
            Retention::DeleteImmediately {
                wait_until_named: false,
            },
            RetentionTrigger::Immediate,
        ),
        (
            "retention-named",
            "Until transcribed and every speaker is named",
            Retention::DeleteImmediately {
                wait_until_named: true,
            },
            RetentionTrigger::Named,
        ),
        (
            "retention-days",
            "Delete after",
            Retention::DeleteAfterDays { days },
            RetentionTrigger::Days,
        ),
    ] {
        let f = on_retention.clone();
        let mut button = choice(
            id,
            label,
            retention == value,
            cx.listener(move |this, _: &bool, window, cx| f(this, value, trigger, window, cx)),
        );
        if let Some(guard) = guard {
            button = button.track_focus(guard.focus_for(trigger));
        }
        if let Some(input) = days_input.filter(|_| trigger == RetentionTrigger::Days) {
            use gpui::component::input::NumberInput;
            button = button
                .accessibility_label(format!("Delete after {days} days"))
                .trailing(
                    row()
                        .child(
                            NumberInput::new(input)
                                .w(px(72.))
                                .stepper(false)
                                .accessibility_label("Days to keep recordings"),
                        )
                        .child(text_line(
                            "storage-retention-days-unit",
                            Role::Label,
                            "days",
                        )),
                );
        }
        group = group.option(button);
    }
    group
}

/// The Default language field's text for `config`.
fn language_text(config: &Config) -> String {
    config.default_language.clone().unwrap_or_default()
}

/// The retention-days field's text for `config` (30 when retention is not
/// by days).
fn retention_days_text(config: &Config) -> String {
    match config.retention {
        Retention::DeleteAfterDays { days } => days,
        _ => 30,
    }
    .to_string()
}

/// The visible and accessible name of a reading-size step.
fn theme_choice_label(choice: ThemeChoice) -> &'static str {
    match choice {
        ThemeChoice::System => "System",
        ThemeChoice::Light => "Light",
        ThemeChoice::Dark => "Dark",
    }
}

fn reading_size_label(size: ReadingSize) -> &'static str {
    match size {
        ReadingSize::Small => "Small",
        ReadingSize::Standard => "Standard",
        ReadingSize::Large => "Large",
        ReadingSize::Larger => "Larger",
        ReadingSize::Largest => "Largest",
    }
}

pub(crate) use crate::ui::kit::{ChoiceGroup, choice};

/// What assistive technology reads for a [`ChoiceGroup`], as GPUI's own
/// node writers give it.
#[cfg(test)]
#[derive(Debug, PartialEq)]
pub(crate) struct ChoiceNodes {
    /// The group's ID, role and name.
    pub(crate) group: (String, Role, String),
    /// Each option's ID, role, name and toggled state, in order.
    pub(crate) options: Vec<(String, Role, String, Option<gpui::Toggled>)>,
}

/// Makes the choice groups a [`ChoiceProbe`] renders.
#[cfg(test)]
type BuildChoices = Box<dyn Fn(&mut Window, &mut Context<ChoiceProbe>) -> Vec<ChoiceGroup>>;

/// Draws the actual group and component subtrees; the SDK's opt-in paint
/// observer reads their native accessibility metadata without an AT process.
#[cfg(test)]
pub(crate) struct ChoiceProbe {
    build: BuildChoices,
}

#[cfg(test)]
impl Render for ChoiceProbe {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().flex_col().children((self.build)(window, cx))
    }
}

/// What assistive technology reads for the groups `build` makes when a
/// window draws them.
#[cfg(test)]
pub(crate) fn choice_nodes(
    cx: &mut gpui::TestAppContext,
    build: impl Fn(&mut Window, &mut Context<ChoiceProbe>) -> Vec<ChoiceGroup> + 'static,
) -> Vec<ChoiceNodes> {
    let probe = ChoiceProbe {
        build: Box::new(build),
    };
    let window = cx.add_window(move |_, _| probe);
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        let snapshots = gpui::base::test_support::snapshots(window);
        let position = |node: &gpui::base::test_support::ElementSnapshot| {
            (node.bounds().origin.y, node.bounds().origin.x)
        };
        let mut groups: Vec<_> = snapshots
            .iter()
            .filter(|node| node.role() == Some(Role::RadioGroup))
            .collect();
        groups.sort_by_key(|node| position(node));
        groups
            .into_iter()
            .map(|group| {
                let mut options: Vec<_> = snapshots
                    .iter()
                    .filter(|node| {
                        node.role() == Some(Role::RadioButton)
                            && node.path().starts_with(group.path())
                    })
                    .collect();
                options.sort_by_key(|node| position(node));
                ChoiceNodes {
                    group: (
                        group.path().last().expect("identified group").to_string(),
                        group.role().expect("radio group"),
                        group.label().unwrap_or_default().to_owned(),
                    ),
                    options: options
                        .into_iter()
                        .map(|node| {
                            (
                                node.path().last().expect("identified radio").to_string(),
                                node.role().expect("radio button"),
                                node.label().unwrap_or_default().to_owned(),
                                node.checked().map(|checked| {
                                    if checked {
                                        gpui::Toggled::True
                                    } else {
                                        gpui::Toggled::False
                                    }
                                }),
                            )
                        })
                        .collect(),
                }
            })
            .collect()
    })
    .unwrap()
}

#[cfg(test)]
impl ChoiceNodes {
    /// A radio group `id` named `label` with `options` (ID, name, whether
    /// selected) in order: what every choice group must read as.
    pub(crate) fn radios(id: &str, label: &str, options: &[(&str, &str, bool)]) -> Self {
        Self {
            group: (id.to_owned(), Role::RadioGroup, label.to_owned()),
            options: options
                .iter()
                .map(|&(id, label, selected)| {
                    let toggled = if selected {
                        gpui::Toggled::True
                    } else {
                        gpui::Toggled::False
                    };
                    (
                        id.to_owned(),
                        Role::RadioButton,
                        label.to_owned(),
                        Some(toggled),
                    )
                })
                .collect(),
        }
    }
}

pub(crate) fn role_label(role: &str) -> String {
    match role {
        "vad" => "Speech detection".into(),
        "asr" => "Transcription".into(),
        "live" => "Live preview".into(),
        "diarization" => "Speaker separation".into(),
        "speaker_embedding" => "Speaker recognition".into(),
        "text_embedding" => "Search".into(),
        "llm" => "Language model".into(),
        _ => "Other models".into(),
    }
}

/// Roles `models use` accepts in this version.
fn folder_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

fn disk_usage_text(library: Option<u64>, free: Option<u64>) -> String {
    let library = library.map_or_else(|| "Unknown".into(), bytes);
    let free = free.map_or_else(
        || "Unknown free space".into(),
        |value| format!("{} free on this disk", bytes(value)),
    );
    format!("Library: {library} · {free}")
}

pub(crate) fn parse_retention_days(text: &str) -> Option<u32> {
    text.trim()
        .parse()
        .ok()
        .filter(|days| (1..=Retention::MAX_DAYS).contains(days))
}

fn login_description(place: broscribe_platform::StatusIconPlace) -> &'static str {
    use broscribe_platform::StatusIconPlace::*;
    match place {
        MenuBar => "Opens with only its icon in the menu bar. No window opens.",
        NotificationArea => {
            "Opens with only its icon in the notification area. Windows may keep the icon under Show hidden icons (^)."
        }
        SystemTray => "Opens with only its icon in the system tray. No window opens.",
        NoTrayHost => {
            "Your desktop doesn't show tray icons, so BroScribe opens its window when you log in."
        }
    }
}

pub(crate) fn speech_model_choices<V: 'static>(
    id: &'static str,
    list: &ModelsList,
    language: Option<&str>,
    disabled: bool,
    cx: &Context<V>,
    select: fn(&mut V, String, &mut Context<V>),
) -> ChoiceGroup {
    let mut choices = ChoiceGroup::new(kit::stack().items_start(), id, "Speech model");
    for profile in &list.profiles {
        let name = list.model_name(&profile.asr);
        let multilingual = profile.languages.iter().any(|lang| lang == "*");
        let mut facts = Vec::new();
        if multilingual {
            facts.push("Many languages".to_owned());
        } else if !profile.languages.is_empty() {
            facts.push(format!("{} languages", profile.languages.len()));
        }
        if let Some(model) = list.rows.iter().find(|row| row.id == profile.asr) {
            facts.push(bytes(model.bytes));
        }
        if let Some(language) = language.filter(|_| !multilingual && !profile.languages.is_empty())
        {
            let base = language.split('-').next().unwrap_or(language);
            if !profile.languages.iter().any(|listed| {
                listed.eq_ignore_ascii_case(language) || listed.eq_ignore_ascii_case(base)
            }) {
                facts.push(format!(
                    "doesn't list {}",
                    crate::ui::format::language_name(language)
                ));
            }
        }
        let key = profile.id.clone();
        let mut option = kit::radio_option(
            format!("profile-{key}"),
            name.clone(),
            profile.active,
            !disabled,
            cx,
            move |this, _, cx| select(this, key.clone(), cx),
        );
        if !facts.is_empty() {
            let facts = facts.join(" · ");
            option = option
                .accessibility_label(format!("{name} · {facts}"))
                .details(
                    div()
                        .text_role(TextRole::Caption)
                        .text_color(cx.theme().muted_foreground)
                        .child(facts),
                );
        }
        choices = choices.option(option);
    }
    choices
}

fn role_usable(role: &str) -> bool {
    MODEL_ROLES.contains(&role)
}

pub(super) fn parse_input_devices(v: &Value) -> Vec<InputDevice> {
    v["devices"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|d| {
                    let id = d["id"].as_str().filter(|id| !id.is_empty())?.to_owned();
                    let name = d["name"]
                        .as_str()
                        .map(str::trim)
                        .filter(|n| !n.is_empty())
                        .map_or_else(|| id.clone(), str::to_owned);
                    Some(InputDevice {
                        id,
                        name,
                        is_default: d["is_default"].as_bool().unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The microphone options: "System default", then each device. A pinned
/// device missing from a loaded list is kept (selected) so the user sees it
/// is not used and can switch back to the default; `devices` is `None` when
/// no list is known.
fn mic_options(devices: Option<&[InputDevice]>, pinned: Option<&str>) -> Vec<MicOption> {
    let mut options = vec![MicOption {
        target: None,
        label: devices
            .and_then(|devices| devices.iter().find(|device| device.is_default))
            .map_or_else(
                || "System default".into(),
                |device| format!("System default ({})", device.name),
            ),
        #[cfg(test)]
        selected: pinned.is_none(),
    }];
    let mut devices_sorted: Vec<_> = devices.unwrap_or_default().iter().collect();
    devices_sorted.sort_by_cached_key(|device| device.name.to_lowercase());
    for d in devices_sorted {
        options.push(MicOption {
            target: Some(d.id.clone()),
            label: d.name.clone(),
            #[cfg(test)]
            selected: pinned == Some(d.id.as_str()),
        });
    }
    if let Some(id) = pinned
        && !devices.unwrap_or_default().iter().any(|d| d.id == id)
    {
        options.push(MicOption {
            target: Some(id.to_owned()),
            label: if devices.is_some() {
                format!("{id} (not connected)")
            } else {
                id.to_owned()
            },
            #[cfg(test)]
            selected: true,
        });
    }
    options
}

/// `audio pin -- <id>`, or `audio pin --clear` for the system default.
fn pin_mic_args(target: Option<&str>) -> Vec<OsString> {
    match target {
        Some(id) => args(["audio", "pin", "--", id]),
        None => args(["audio", "pin", "--clear"]),
    }
}

/// A settings group: a card on the settings pane's desk.
fn settings_group(cx: &App) -> Div {
    kit::as_card(div(), cx).flex().flex_col().gap_3().p_4()
}

/// The Settings page's Main landmark. Its name differs from the page title it
/// holds, so a screen reader never reads "Settings" twice on arrival.
fn settings_landmark() -> Stateful<Div> {
    div()
        .id("settings")
        .role(Role::Main)
        .aria_label("BroScribe settings")
}

/// The Settings page title, a heading.
#[cfg(test)]
fn settings_title() -> Stateful<Div> {
    title("settings-title", "Settings")
}

impl SettingsView {
    fn render_window_page(
        &self,
        page: SettingsPage,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let sidebar = div()
            .id("settings-pages")
            .role(Role::Group)
            .aria_label("Settings pages")
            .track_focus(&self.page_list_focus.clone().tab_index(0).tab_stop(true))
            .on_key_down(
                cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                    if event.keystroke.modifiers.modified() {
                        return;
                    }
                    let pages = &SettingsPage::ALL;
                    let current = pages
                        .iter()
                        .position(|item| *item == page)
                        .unwrap_or_default();
                    let next = match event.keystroke.key.as_str() {
                        "up" => (current + pages.len() - 1) % pages.len(),
                        "down" => (current + 1) % pages.len(),
                        "home" => 0,
                        "end" => pages.len() - 1,
                        _ => return,
                    };
                    this.set_page(pages[next], window, cx);
                    cx.stop_propagation();
                }),
            )
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .h_full()
            .w(gpui::rems(10.))
            .flex_none()
            .overflow_y_scroll()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .children(
                SettingsPage::ALL
                    .into_iter()
                    .enumerate()
                    .map(|(index, item)| {
                        Button::new(("settings-page", index))
                            .label(item.title())
                            .ghost()
                            .tab_stop(false)
                            .selected(item == page)
                            .w_full()
                            .justify_start()
                            .when(item == page, |button| {
                                // The current page is the sidebar's pill.
                                let colours = crate::ui::brand::brand(cx);
                                button
                                    .bg(colours.sidebar_selected)
                                    .text_color(cx.theme().foreground)
                                    .shadow(colours.pill_shadow())
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.page_list_focus.focus(window, cx);
                                this.set_page(item, window, cx);
                            }))
                    }),
            );
        let mut content = div()
            .id("settings-page-content")
            .track_focus(&self.pane_focus)
            .flex()
            .flex_col()
            .gap_4()
            .p(kit::DESK_PADDING)
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(cx.theme().background)
            .overflow_y_scroll()
            .track_scroll(&self.page_scroll)
            .when(
                page != SettingsPage::Calendars
                    && (page != SettingsPage::Updates || self.updates.is_none()),
                |el| el.child(title("settings-page-title", page.title())),
            )
            .child(muted("settings-page-description", page.description(), cx));
        // The pane is the desk; each settings group is a card on it.
        let group = settings_group;
        content = match page {
            SettingsPage::General => content
                .child(group(cx).child(self.render_login(cx)))
                .when(broscribe_platform_native::NativePlatform::host_platform() == broscribe_platform::HostPlatform::MacOs, |el| {
                    el.child(group(cx).child(self.render_status_item(cx)))
                })
                .child(group(cx).child(self.render_theme_choice(cx)))
                .child(group(cx).child(self.render_reading_size(cx))
                .child(row().children([
                    ("Larger", gpui::component::kbd::Kbd::binding_for_action(&crate::menu::LargerText, None, window)),
                    ("Smaller", gpui::component::kbd::Kbd::binding_for_action(&crate::menu::SmallerText, None, window)),
                ].into_iter().filter_map(|(label, key)| key.map(|key|
                    row().child(muted(format!("settings-text-size-{label}"), label, cx)).child(key))))))
                .child(group(cx).child(kit::field_row(
                    "settings-setup",
                    "Setup assistant",
                    Some(("settings-setup-help".into(), "Walks through languages, permissions, models, your voice and more. Progress is saved.".into())),
                    self.use_vertical_fields(cx),
                    |_| Button::new("open-setup")
                        .label("Open setup assistant")
                        .on_click(cx.listener(|this, _, window, cx| this.open_setup(window, cx))),
                    cx,
                ))
                .children(self.setting_notice("setup", cx)))
                .when(
                    !matches!(
                        NativePlatform::host_platform(),
                        broscribe_platform::HostPlatform::MacOs
                    ),
                    |content| {
                        content.child(
                            Button::new("settings-about")
                                .label("About BroScribe")
                                .self_start()
                                .on_click(|_, window, cx| {
                                    window.dispatch_action(Box::new(crate::menu::ShowAbout), cx)
                                }),
                        )
                    },
                ),
            SettingsPage::Recording => content
                .child(group(cx).child(self.render_mic(cx)))
                .child(
                    group(cx)
                        .child(self.render_live_identity(cx))
                        .child(self.render_consent_reminder(cx)),
                )
                .child(group(cx).child(self.render_meeting_apps(cx)))
                .child(group(cx).child(self.render_system_checks(cx))),
            SettingsPage::Transcription => content
                .child(
                    group(cx)
                        .child(self.render_speech_models(cx))
                        .child(self.render_default_language(cx)),
                )
                .child(group(cx).child(self.render_your_voice(cx)))
                .child(group(cx).child(self.render_speaker_grouping(cx)))
                .child(group(cx).child(self.render_word_list(cx)))
                .child(group(cx).child(self.render_search_model(cx)))
                .child(group(cx).child(self.render_advanced_models(window, cx))),
            SettingsPage::Storage => {
                let config = self.state.read(cx).config().clone();
                let handling_status = self.setting_notice("import-handling", cx);
                content
                    .child(
                        group(cx)
                            .child(self.render_library_setting(cx))
                            .child(self.render_disk_use(cx)),
                    )
                    .child(group(cx).child(storage_section(
                        &config,
                        &self.retention_days,
                        &self.retention,
                        cx,
                        |this: &mut Self, h, cx| this.set_handling(h, cx),
                        |this: &mut Self, r, trigger, window, cx| {
                            this.set_retention(r, trigger, window, cx)
                        },
                        handling_status,
                    ))
                    .children(self.setting_notice("retention-days", cx))
                    .children(self.setting_notice("retention", cx)))
            }
            SettingsPage::AiProviders => content.child(group(cx).child(self.render_providers(cx))),
            SettingsPage::Summaries => content.child(group(cx).child(self.render_summaries(cx))),
            SettingsPage::Templates => content.child(group(cx).child(self.templates.clone())),
            SettingsPage::Calendars => content.child(group(cx).child(self.integrations.clone())),
            SettingsPage::AiTools => content.child(group(cx).child(self.render_agents())),
            SettingsPage::Privacy => content
                .child(group(cx).child(self.render_privacy(cx)))
                .child(group(cx).child(self.uninstall_button(cx))),
            SettingsPage::Updates => content.children(
                self.updates
                    .iter()
                    .map(|updates| group(cx).child(updates.clone())),
            ),
        };
        let close = cx.weak_entity();
        let handle = window.window_handle();
        crate::ui::chrome::framed_with_close(
            page.title(),
            settings_landmark()
                .size_full()
                .flex()
                .child(sidebar)
                .child(content)
                .on_action(
                    cx.listener(|this, _: &crate::menu::CloseWindow, window, cx| {
                        if this.should_close(window, cx) {
                            window.remove_window();
                        }
                        cx.stop_propagation();
                    }),
                ),
            window,
            move |cx| {
                let _ = handle.update(cx, |_, window, cx| {
                    let _ = close.update(cx, |this, cx| {
                        if this.should_close(window, cx) {
                            window.remove_window();
                        }
                    });
                });
            },
        )
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.first_control_pending) {
            // This frame draws the page; the next one starts with focus on
            // its first control, unless something else took focus meanwhile.
            let anchor = self.pane_focus.clone();
            window.on_next_frame(move |window, cx| {
                if anchor.is_focused(window) {
                    window.focus_next(cx);
                    if !anchor.contains_focused(window, cx) {
                        anchor.focus(window, cx);
                    }
                }
            });
        }
        if let Some(page) = self.window_page {
            return self.render_window_page(page, window, cx);
        }
        #[cfg(not(test))]
        return self.render_window_page(SettingsPage::General, window, cx);

        #[cfg(test)]
        self.render_all_controls_for_tests(cx)
    }
}

#[cfg(test)]
impl SettingsView {
    // The existing control fixtures render several independent sections together.
    fn render_all_controls_for_tests(&self, cx: &mut Context<Self>) -> AnyElement {
        let config = self.state.read(cx).config().clone();
        let handling_status = self.setting_notice("import-handling", cx);
        let storage = storage_section(
            &config,
            &self.retention_days,
            &self.retention,
            cx,
            |this: &mut Self, h, cx| this.set_handling(h, cx),
            |this: &mut Self, r, trigger, window, cx| this.set_retention(r, trigger, window, cx),
            handling_status,
        );
        settings_landmark()
            .flex()
            .flex_col()
            .gap_2()
            .flex_1()
            .p_4()
            .overflow_y_scroll()
            .child(row().child(settings_title()).child(
                Button::new("open-setup").label("Setup assistant").on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| this.open_setup(window, cx)),
                ),
            ))
            .children(self.setting_notice("retention", cx))
            .child(self.render_general(cx))
            .child(heading("settings-storage", "Storage & retention").mt_3())
            .child(storage)
            .child(self.render_models(false, cx))
            .child(heading("settings-privacy", "Privacy"))
            .child(self.render_privacy(cx))
            .child(self.render_your_voice(cx))
            .child(self.render_providers(cx))
            .child(heading("settings-summaries", "Summaries"))
            .child(self.render_summaries(cx))
            .child(heading("settings-templates-heading", "Templates").mt_3())
            .child(self.templates.clone())
            .child(muted(
                "settings-integrations-hint",
                crate::views::integrations::calendar_settings_hint(),
                cx,
            ))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::component::WindowExt as _;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use broscribe_platform::fakes::{FakeMicCapture, FakePaths, FakeSystemInfo};
    use broscribe_platform::{AppPaths as _, resolve_paths};
    use gpui::TestAppContext;

    use crate::state::Route;

    /// A synthetic `broscribe` that logs its argv and lists one HTTP provider
    /// without consent text and two input devices.
    pub(super) fn synthetic_cli(dir: &Path) -> PathBuf {
        use crate::test_support::{ps_path, ps_str, write_cli};
        let log = dir.join("args.log");
        let reply = "{\"providers\":[{\"id\":\"remote\",\"runtime\":\"openai-compatible-http\",\"has_key\":false}],\
                     \"devices\":[{\"id\":\"synthetic-builtin\",\"name\":\"Synthetic Built-in Mic\",\"is_default\":true},\
                     {\"id\":\"synthetic-usb\",\"name\":\"Synthetic USB Mic\",\"is_default\":false}]}";
        write_cli(
            &dir.join("synthetic-cli"),
            &format!(
                "printf '%s\\n' \"$*\" >> '{}'\n\
                 echo '{reply}'\n",
                log.display(),
            ),
            &format!("fx_log_args {};fx_echo {}", ps_path(&log), ps_str(reply)),
        )
    }

    #[test]
    fn storage_recovery_names_only_the_hosts_backup_service() {
        use broscribe_platform::HostPlatform;
        for (host, backup) in [
            (HostPlatform::MacOs, "Time Machine or synced backups"),
            (HostPlatform::Windows, "File History or synced backups"),
            (HostPlatform::Linux, "your backups"),
        ] {
            let words = ssd_caveat_for(host);
            assert!(
                words.ends_with(&format!("Copies in {backup} stay.")),
                "{words}"
            );
        }
    }

    #[test]
    fn input_devices_parsing_and_options() {
        let v = serde_json::json!({"devices":[
            {"id":"synthetic-builtin","name":"Synthetic Built-in Mic","is_default":true,
             "sample_rates":[48000]},
            {"id":"synthetic-usb","name":"  "},
            {"name":"no id"}
        ]});
        let devices = parse_input_devices(&v);
        assert_eq!(
            devices,
            [
                InputDevice {
                    id: "synthetic-builtin".into(),
                    name: "Synthetic Built-in Mic".into(),
                    is_default: true,
                },
                InputDevice {
                    id: "synthetic-usb".into(),
                    name: "synthetic-usb".into(),
                    is_default: false,
                },
            ]
        );
        assert!(parse_input_devices(&serde_json::json!({"providers": []})).is_empty());

        let labels = |options: &[MicOption]| -> Vec<(String, bool)> {
            options
                .iter()
                .map(|o| (o.label.clone(), o.selected))
                .collect()
        };
        // Nothing pinned: the system default is selected.
        let options = mic_options(Some(&devices), None);
        assert_eq!(
            labels(&options),
            [
                ("System default (Synthetic Built-in Mic)".into(), true),
                ("Synthetic Built-in Mic".into(), false),
                ("synthetic-usb".into(), false),
            ]
        );
        assert_eq!(options[0].target, None);
        assert_eq!(options[2].target.as_deref(), Some("synthetic-usb"));
        // A connected pinned device is selected in place.
        let options = mic_options(Some(&devices), Some("synthetic-usb"));
        assert_eq!(options.len(), 3);
        assert!(!options[0].selected && options[2].selected);
        // A pinned device that is not connected stays listed and selected,
        // so switching back to the default can clear it.
        let options = mic_options(Some(&devices), Some("gone-mic"));
        assert_eq!(
            labels(&options[3..]),
            [("gone-mic (not connected)".into(), true)]
        );
        assert_eq!(options[3].target.as_deref(), Some("gone-mic"));
        assert!(!options[0].selected);
        // Without a list, connectivity is unknown: no "not connected" claim.
        assert_eq!(
            labels(&mic_options(None, Some("gone-mic"))),
            [("System default".into(), false), ("gone-mic".into(), true)]
        );

        assert_eq!(
            pin_mic_args(Some("synthetic-usb")),
            args(["audio", "pin", "--", "synthetic-usb"])
        );
        assert_eq!(pin_mic_args(None), args(["audio", "pin", "--clear"]));
    }

    #[gpui::test]
    fn mic_choice_goes_through_the_cli(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (state, window) =
            settings_window(cx, dir.path(), Arc::new(FakeMicCapture::new(16_000)));
        let log = || std::fs::read_to_string(dir.path().join("args.log")).unwrap_or_default();
        let listings = || {
            log()
                .lines()
                .filter(|l| l.ends_with("audio devices"))
                .count()
        };
        assert_eq!(listings(), 1);
        window
            .update(cx, |this, _, _| {
                let ids: Vec<_> = this.mics.devices.iter().flatten().map(|d| &d.id).collect();
                assert_eq!(ids, ["synthetic-builtin", "synthetic-usb"]);
            })
            .unwrap();
        // Reopening Settings reloads the list and the config (a pin made from
        // a terminal) and drops the last message; other changes do not reload.
        let data_dir = state.read_with(cx, |s, _| s.paths().data_dir.clone());
        let mut pinned = Config::default();
        pinned.audio.pinned_mic = Some("synthetic-usb".into());
        pinned.save(&data_dir).unwrap();
        window
            .update(cx, |this, _, _| {
                this.mics.result = Some(Ok("An earlier message.".into()))
            })
            .unwrap();
        assert_eq!(listings(), 1);
        window
            .update(cx, |this, _, cx| this.entered_settings(cx))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(listings(), 2);
        let pinned_mic = |cx: &mut TestAppContext| {
            state.read_with(cx, |s, _| s.config().audio.pinned_mic.clone())
        };
        assert_eq!(pinned_mic(cx).as_deref(), Some("synthetic-usb"));
        window
            .update(cx, |this, _, _| assert_eq!(this.mics.result, None))
            .unwrap();
        // A snapshot at the file's revision that says "System default": the
        // app applies it (only older revisions are refused).
        let forget_pin = |cx: &mut TestAppContext| {
            state.update(cx, |s, cx| {
                let mut stale = s.config().clone();
                stale.audio.pinned_mic = None;
                s.set_config(stale, cx)
            });
            cx.run_until_parked();
        };
        forget_pin(cx);
        assert_eq!(listings(), 2);

        // The in-memory config says "System default" (stale: the file pins
        // synthetic-usb); choosing it still clears the pin.
        assert_eq!(pinned_mic(cx), None);
        window
            .update(cx, |this, _, cx| this.pin_mic(None, cx))
            .unwrap();
        cx.run_until_parked();
        assert!(log().contains("--json audio pin --clear"), "{}", log());
        window
            .update(cx, |this, _, _| {
                assert_eq!(
                    this.mics.result,
                    Some(Ok("Recording with the system default microphone.".into()))
                );
            })
            .unwrap();

        // Pinning keeps the options disabled until the changed config is in:
        // memory says "System default" again, the file still pins synthetic-usb
        // (the reload after the click brings it in).
        forget_pin(cx);
        let view = window.root(cx).unwrap();
        let pinning_at_reload = Rc::new(RefCell::new(Vec::new()));
        let _observer = cx.update(|cx| {
            let seen = pinning_at_reload.clone();
            let was_pinned = Cell::new(false);
            cx.observe(&state, move |state, cx| {
                let pinned = state.read(cx).config().audio.pinned_mic.is_some();
                if pinned && !was_pinned.replace(pinned) {
                    seen.borrow_mut().push(view.read(cx).mics.pinning);
                }
            })
        });
        window
            .update(cx, |this, _, cx| {
                this.pin_mic(Some("synthetic-usb".into()), cx);
                assert!(this.mics.pinning);
                // A second click while it runs is ignored.
                this.pin_mic(None, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert!(
            log().contains("--json audio pin -- synthetic-usb"),
            "{}",
            log()
        );
        assert_eq!(log().matches("--clear").count(), 1, "{}", log());
        assert_eq!(*pinning_at_reload.borrow(), [true]);
        assert_eq!(pinned_mic(cx).as_deref(), Some("synthetic-usb"));
        window
            .update(cx, |this, _, _| {
                assert!(!this.mics.pinning);
                assert_eq!(
                    this.mics.result,
                    Some(Ok(
                        "Recording with Synthetic USB Mic while it is connected.".into()
                    ))
                );
            })
            .unwrap();
    }

    fn settings_window(
        cx: &mut TestAppContext,
        dir: &Path,
        mic: Arc<FakeMicCapture>,
    ) -> (Entity<AppState>, gpui::WindowHandle<SettingsView>) {
        let cli = synthetic_cli(dir);
        settings_window_with(cx, dir, mic, cli)
    }

    fn settings_window_with(
        cx: &mut TestAppContext,
        dir: &Path,
        mic: Arc<FakeMicCapture>,
        cli: PathBuf,
    ) -> (Entity<AppState>, gpui::WindowHandle<SettingsView>) {
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir)).unwrap(),
                    Config::default(),
                    Library::open(dir.join("lib")).unwrap(),
                    cli,
                ),
                cx,
            )
        });
        let deps = SetupDeps {
            mic,
            system: Arc::new(FakeSystemInfo::default()),
        };
        cx.update(gpui::init);
        let window = cx.add_window(|window, cx| SettingsView::new(state.clone(), deps, window, cx));
        cx.run_until_parked();
        (state, window)
    }

    #[gpui::test]
    fn open_at_login_and_agent_access_go_through_the_cli(cx: &mut TestAppContext) {
        use crate::views::agents::tests::{commands, synthetic_agents_cli};
        let dir = tempfile::tempdir().unwrap();
        let cli = synthetic_agents_cli(dir.path());
        let (_state, window) =
            settings_window_with(cx, dir.path(), Arc::new(FakeMicCapture::new(16_000)), cli);
        let count = |c: &str| commands(dir.path()).iter().filter(|l| *l == c).count();
        assert_eq!(count("agents status"), 1);
        assert_eq!(count("app open-at-login status"), 1);
        window
            .update(cx, |this, _, cx| {
                assert_eq!(this.login.value, Some(true));
                assert_eq!(this.login.error, None);
                assert!(matches!(this.agents.read(cx).status(), Some(Ok(_))));
                // Already on: nothing runs.
                this.set_open_at_login(true, cx);
                assert!(!this.login.running);
                this.set_open_at_login(false, cx);
                assert!(this.login.running, "the switch is disabled while it runs");
                // A second change waits for the first.
                this.set_open_at_login(true, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(count("app open-at-login off"), 1);
        assert_eq!(count("app open-at-login on"), 0);
        window
            .update(cx, |this, _, cx| {
                assert!(!this.login.running);
                assert_eq!(this.login.value, Some(false));
                this.set_open_at_login(true, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(count("app open-at-login on"), 1);
        window
            .update(cx, |this, _, cx| {
                assert_eq!(this.login.value, Some(true));
                // Agent actions in Settings run the same commands.
                this.agents.update(cx, |agents, cx| agents.install(cx));
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(count("agents install"), 1);
        // A failed command is shown.
        std::fs::remove_file(crate::test_support::native_cli_path(
            &dir.path().join("synthetic-cli"),
        ))
        .unwrap();
        window
            .update(cx, |this, _, cx| this.set_open_at_login(false, cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert!(!this.login.running);
                assert_eq!(this.login.value, Some(true));
                assert!(this.login.error.is_some());
            })
            .unwrap();
    }

    /// Settings' microphone, automatic-summary and storage choices read as
    /// labelled radio groups whose options say which one is selected, and
    /// the selection follows the configuration (UI26).
    #[gpui::test]
    fn settings_choices_read_as_radio_groups_with_their_selection(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (state, window) =
            settings_window(cx, dir.path(), Arc::new(FakeMicCapture::new(16_000)));
        let view = window.root(cx).unwrap();
        let read = |cx: &mut TestAppContext| {
            let view = view.clone();
            choice_nodes(cx, move |_, cx| {
                view.update(cx, |this, cx| {
                    let config = this.state.read(cx).config().clone();
                    vec![
                        handling_choices(config.import_handling, cx, |_, _, _| {}),
                        retention_choices(config.retention, cx, |_, _, _| {}),
                    ]
                })
            })
        };
        let expected = |handling: [bool; 3], keep: [bool; 4]| {
            vec![
                ChoiceNodes::radios(
                    "storage-handling",
                    "Imported recordings",
                    &[
                        ("handling-copy", "Copy into the library", handling[0]),
                        ("handling-move", "Move into the library", handling[1]),
                        ("handling-reference", "Keep in place", handling[2]),
                    ],
                ),
                ChoiceNodes::radios(
                    "storage-retention",
                    "Keep recordings",
                    &[
                        ("retention-keep", "Forever", keep[0]),
                        ("retention-immediate", "Until transcribed", keep[1]),
                        (
                            "retention-named",
                            "Until transcribed and every speaker is named",
                            keep[2],
                        ),
                        ("retention-days", "Delete after", keep[3]),
                    ],
                ),
            ]
        };
        assert_eq!(
            read(cx),
            expected([true, false, false], [true, false, false, false])
        );

        state.update(cx, |s, cx| {
            let mut config = s.config().clone();
            config.audio.pinned_mic = Some("synthetic-usb".into());
            config.auto_enrich = AutoEnrich::Off;
            config.import_handling = Handling::Reference;
            config.retention = Retention::DeleteImmediately {
                wait_until_named: true,
            };
            s.set_config(config, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            read(cx),
            expected([false, false, true], [false, false, true, false])
        );
    }

    #[gpui::test]
    fn open_at_login_switch_changes_the_cli_setting_and_exposes_its_value(cx: &mut TestAppContext) {
        use crate::views::agents::tests::synthetic_agents_cli;
        let dir = tempfile::tempdir().unwrap();
        let cli = synthetic_agents_cli(dir.path());
        let _tray = broscribe_platform::fakes::FakeStatusIconPlace::new(
            broscribe_platform::StatusIconPlace::NoTrayHost,
        );
        let (_state, window) =
            settings_window_with(cx, dir.path(), Arc::new(FakeMicCapture::new(16_000)), cli);
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let inspect = |visual: &mut gpui::VisualTestContext| {
            visual.update(|window, cx| {
                let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
                assert_eq!(
                    tree.read(tree.find("settings-login")),
                    ("Switch", "Start BroScribe at login")
                );
                assert_eq!(tree.read(tree.find("settings-login-note")).1, "Your desktop doesn't show tray icons, so BroScribe opens its window when you log in.");
                assert_eq!(tree.read(tree.find("settings-no-tray")), ("Note", crate::ui::NO_TRAY_TEXT));
                let controls = gpui::base::test_support::snapshots(window);
                let login = controls
                    .iter()
                    .find(|node| {
                        node.role() == Some(Role::Switch)
                            && node.label() == Some("Start BroScribe at login")
                    })
                    .expect("login switch");
                (login.checked(), login.bounds().center())
            })
        };
        let (on, point) = inspect(&mut visual);
        assert_eq!(on, Some(true));
        visual.simulate_click(point, Default::default());
        visual.run_until_parked();
        assert_eq!(inspect(&mut visual).0, Some(false));
    }

    /// SET-05: the Main landmark and the page title it holds are named
    /// differently, read through the names GPUI writes for them (the
    /// landmark is named by `aria_label`, which `spoken_log` doesn't record).
    #[test]
    fn the_settings_landmark_is_not_named_like_its_title() {
        use crate::ui::a11y_name;
        let (landmark_id, landmark_role, landmark) =
            a11y_name(&settings_landmark()).expect("the landmark is a node");
        let (title_id, title_role, title) =
            a11y_name(&settings_title()).expect("the title is a node");
        assert_eq!(
            (landmark_id.as_str(), landmark_role, landmark.as_str()),
            ("settings", Role::Main, "BroScribe settings")
        );
        assert_eq!(
            (title_id.as_str(), title_role, title.as_str()),
            ("settings-title", Role::Heading, "Settings")
        );
        assert_ne!(landmark.to_lowercase(), title.to_lowercase());
    }

    #[gpui::test]
    fn settings_fields_commit_on_enter_and_blur_and_keep_independent_feedback(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let cli = retention_cli(dir.path(), &dry_run_reply(deletes(0, 0), 0), false);
        let mut initial = Config::default();
        initial.retention = Retention::DeleteAfterDays { days: 30 };
        let (state, view, visual) = settings_root_window_with(cx, dir.path(), initial, cli);
        let data = state.read_with(visual, |state, _| state.paths().data_dir.clone());
        let language = view.read_with(visual, |view, _| view.language.clone());
        visual.update(|window, cx| {
            language.update(cx, |input, cx| {
                input.set_value("pt-BR", window, cx);
                cx.emit(InputEvent::PressEnter {
                    secondary: false,
                    shift: false,
                });
            });
        });
        visual.run_until_parked();
        assert_eq!(
            Config::load(&data).unwrap().default_language.as_deref(),
            Some("pt-BR")
        );

        let days = view.read_with(visual, |view, _| view.retention_days.clone());
        visual.update(|window, cx| {
            days.update(cx, |input, cx| {
                input.set_value("45", window, cx);
                cx.emit(InputEvent::Blur);
            });
        });
        visual.run_until_parked();
        assert_eq!(
            Config::load(&data).unwrap().retention,
            Retention::DeleteAfterDays { days: 45 }
        );

        std::fs::write(Config::path(&data), "[invalid").unwrap();
        visual.update(|window, cx| {
            language.update(cx, |input, cx| {
                input.set_value("en", window, cx);
                cx.emit(InputEvent::Blur);
            });
        });
        visual.run_until_parked();
        assert_eq!(
            language.read_with(visual, |input, _| input.value().to_string()),
            "pt-BR"
        );
        view.update(visual, |this, cx| {
            this.set_reading_size(ReadingSize::Large, cx)
        });
        visual.run_until_parked();
        view.read_with(visual, |view, _| {
            assert!(view.setting_notices["language"].error);
            assert!(view.setting_notices["reading-size"].error);
            assert!(view.saving.is_empty());
        });
    }

    #[gpui::test]
    fn settings_panes_have_one_title_and_named_subsections(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (_, window) = settings_window(cx, dir.path(), Arc::new(FakeMicCapture::new(16_000)));
        for page in SettingsPage::ALL {
            window
                .update(cx, |view, window, cx| {
                    view.set_page(page, window, cx);
                    if page == SettingsPage::Transcription {
                        view.models.advanced = true;
                        view.models.list = parse_models_list(&serde_json::json!({
                            "models": [{
                                "id": "synthetic-model-id",
                                "display_name": "Synthetic model",
                                "roles": ["asr"],
                                "state": "installed"
                            }]
                        }));
                    }
                })
                .unwrap();
            cx.update_window(window.into(), |_, window, cx| {
                let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
                let expected = [
                    "General",
                    "Recording",
                    "Transcription",
                    "Storage",
                    "AI providers",
                    "Summaries",
                    "Templates",
                    "Calendars",
                    "AI tools",
                    "Privacy",
                    "Updates",
                ];
                let pane_list = &tree.nodes[tree.find("settings-pages")];
                let names: Vec<_> = pane_list
                    .children
                    .iter()
                    .map(|index| tree.nodes[*index].name.as_str())
                    .collect();
                assert_eq!(names, expected);
                tree.assert_title(match page {
                    SettingsPage::Calendars => "calendars-heading",
                    SettingsPage::Updates => "updates-title",
                    _ => "settings-page-title",
                });
                if page == SettingsPage::General {
                    tree.assert_heading("settings-reading-size-title", 3);
                    assert_eq!(
                        tree.read(tree.find("open-setup")),
                        ("Button", "Open setup assistant")
                    );
                }
                if page == SettingsPage::AiProviders {
                    assert_eq!(
                        tree.read(tree.find("settings-providers-note")),
                        ("Label", "An AI provider you turn on writes summaries, decisions and action items. BroScribe sends it the transcript text of those meetings, under your own plan and its terms. Audio and voice data are never sent. All providers are off until you turn one on."),
                    );
                }
                if page == SettingsPage::Recording {
                    tree.assert_heading("settings-mic-title", 3);
                }
                if page == SettingsPage::Transcription {
                    tree.assert_heading("settings-speaker-grouping-title", 3);
                    let choices = &tree.nodes[tree.find("speaker-grouping")];
                    assert_eq!(choices.name, "Speaker grouping");
                    assert!(tree.nodes.iter().any(|node| node.name == "Suggest duplicates (%)"));
                    assert!(tree.nodes.iter().any(|node| node.name == "Merge automatically (%)"));
                    tree.assert_heading("settings-models", 2);
                    tree.assert_heading("settings-models-role-asr", 3);
                    assert_eq!(
                        tree.read(tree.find("model-asr-synthetic-model-id-name")),
                        ("Label", "Synthetic model")
                    );
                    assert!(
                        tree.nodes
                            .iter()
                            .any(|node| node.name == "synthetic-model-id")
                    );
                }
            })
            .unwrap();
        }
    }

    /// SET-05: every text of the Settings page is its own named node with a
    /// stable ID; waits are Pending lines (`Status`) and failures Failed
    /// lines (`Alert`), never colour alone.
    #[gpui::test]
    fn settings_texts_are_named_nodes_with_waits_and_failures_by_role(cx: &mut TestAppContext) {
        use crate::ui::spoken_log::{self, assert_distinct, node};

        let dir = tempfile::tempdir().unwrap();
        let (_state, window) =
            settings_window(cx, dir.path(), Arc::new(FakeMicCapture::new(16_000)));
        let named = |cx: &mut TestAppContext, change: &dyn Fn(&mut SettingsView)| {
            window
                .update(cx, |this, window, cx| {
                    change(this);
                    spoken_log::while_building(|| Render::render(this, window, cx)).1
                })
                .unwrap()
        };

        // Loaded: titles, headings and values are named; the synthetic CLI's
        // open-at-login answer is unexpected, so that line is an alert.
        let loaded = named(cx, &|_| {});
        assert_distinct(&loaded, &[]);
        for (id, role, text) in [
            ("settings-title", Role::Heading, "Settings"),
            ("settings-general", Role::Heading, "General"),
            ("settings-storage", Role::Heading, "Storage & retention"),
            ("settings-models", Role::Heading, "Models"),
            ("settings-privacy", Role::Heading, "Privacy"),
            ("settings-summaries", Role::Heading, "Summaries"),
            ("settings-mic-title", Role::Heading, "Microphone"),
            ("settings-language-title", Role::Label, "Meeting language"),
            ("settings-reading-size-title", Role::Heading, "Text size"),
            (
                "settings-live-identity-title",
                Role::Label,
                "Live speaker recognition",
            ),
            (
                "settings-login-title",
                Role::Label,
                "Start BroScribe at login",
            ),
            (
                "settings-login-error",
                Role::Alert,
                "Couldn't change this setting. BroScribe got an answer it didn't expect.",
            ),
            (
                "storage-handling-title",
                Role::Heading,
                "Imported recordings",
            ),
            ("storage-retention-title", Role::Heading, "Keep recordings"),
            (
                "settings-enrollment-heading",
                Role::Heading,
                "Your voice reading",
            ),
            ("settings-enrollment-state", Role::Label, "Not recorded yet"),
            (
                "settings-auto-enrich-title",
                Role::Label,
                "Summarise imported recordings automatically",
            ),
            (
                "settings-summary-instructions-title",
                Role::Label,
                "What should summaries focus on?",
            ),
            ("settings-templates-heading", Role::Heading, "Templates"),
        ] {
            assert_eq!(node(&loaded, id), (role, text), "{id}");
        }
        let (role, folder) = node(&loaded, "settings-library-folder");
        assert_eq!(role, Role::Label);
        assert!(folder.starts_with("Library folder: "), "{folder}");

        // Waits read as statuses.
        let waiting = named(cx, &|this| {
            this.mics.loading = true;
            this.mics.devices = None;
            this.models.loaded = false;
            this.enrollment.loaded = false;
            this.library_switch = LibrarySwitch::Waiting(PathBuf::from("/synthetic/library"));
            this.models.list = parse_models_list(&serde_json::json!({"models": [
                {"id": "synthetic-asr", "display_name": "Synthetic speech model", "roles": ["asr"], "license": "MIT",
                 "license_verified": true, "bytes": 1024, "state": "missing",
                 "active_for": ["asr"]}
            ]}));
            this.models
                .pulls
                .insert("synthetic-asr".into(), PullState::Cancelling);
        });
        assert_distinct(&waiting, &[]);
        for (id, text) in [
            ("settings-mic-loading", "Looking for microphones…"),
            ("settings-models-loading", "Checking models…"),
            (
                "settings-enrollment-state",
                "Looking for your voice reading…",
            ),
            ("model-asr-synthetic-asr-state", "Cancelling the download…"),
        ] {
            assert_eq!(node(&waiting, id), (Role::Status, text), "{id}");
        }
        let (role, switching) = node(&waiting, "settings-library-switching");
        assert_eq!(role, Role::Status);
        assert!(switching.starts_with("Switches to "), "{switching}");
        // Model rows: the role is a heading, the model and its state labels.
        assert_eq!(
            node(&waiting, "settings-models-role-asr"),
            (Role::Heading, "Transcription")
        );
        assert_eq!(
            node(&waiting, "model-asr-synthetic-asr-name"),
            (Role::Label, "Synthetic speech model")
        );

        // Failures read as alerts, each beside its control.
        let failed = named(cx, &|this| {
            this.mics.loading = false;
            this.mics.error = Some("Couldn't list the microphones. Synthetic cause.".into());
            this.mics.result = Some(Err(
                "Couldn't change the microphone. Synthetic cause.".into()
            ));
            this.models.loaded = true;
            this.models.error = Some("Synthetic cause.".into());
            this.models.doctor = Some(Err("Synthetic cause.".into()));
            this.models.pulls.insert(
                "synthetic-asr".into(),
                PullState::Failed(download_failure(
                    "Synthetic speech model",
                    &CliError::answered("io", "/synthetic/private/partial: disk full"),
                )),
            );
            this.enrollment.loaded = true;
            this.enrollment.error =
                Some("Couldn't check for your voice reading. Synthetic cause.".into());
            this.setting_notices.insert(
                "reading-size",
                Notice {
                    text: "Couldn't change the reading size. Synthetic cause.".into(),
                    error: true,
                },
            );
            this.instructions_notice = Some((
                "Couldn't save the instructions. Synthetic cause.".into(),
                true,
            ));
        });
        assert_distinct(&failed, &[]);
        for (id, text) in [
            (
                "settings-mic-error",
                "Couldn't list the microphones. Synthetic cause.",
            ),
            (
                "settings-mic-result",
                "Couldn't change the microphone. Synthetic cause.",
            ),
            (
                "settings-models-error",
                "Couldn't list the models. Synthetic cause.",
            ),
            (
                "settings-models-verified",
                "Couldn't check the models. Synthetic cause.",
            ),
            (
                "model-asr-synthetic-asr-pull-result",
                "Couldn't download Synthetic speech model. The download didn't finish. Check your \
                 connection, then try again.",
            ),
            (
                "settings-enrollment-state",
                "Couldn't check for your voice reading. Synthetic cause.",
            ),
            (
                "status-reading-size",
                "Couldn't change the reading size. Synthetic cause.",
            ),
            (
                "settings-summary-instructions-notice",
                "Couldn't save the instructions. Synthetic cause.",
            ),
        ] {
            assert_eq!(node(&failed, id), (Role::Alert, text), "{id}");
        }

        // Successes are statuses with their words.
        let done = named(cx, &|this| {
            this.mics.result = Some(Ok("Recording with the system default microphone.".into()));
            this.models.doctor = Some(Ok(serde_json::json!({"ok": true, "models": []})));
            this.setting_notices.insert(
                "retention",
                Notice {
                    text: "Retention saved.".into(),
                    error: false,
                },
            );
            this.instructions_notice = Some(("Saved.".into(), false));
        });
        for (id, text) in [
            (
                "settings-mic-result",
                "Recording with the system default microphone.",
            ),
            (
                "settings-models-verified",
                "All models in use are downloaded and intact.",
            ),
            ("status-retention", "Retention saved."),
            ("settings-summary-instructions-notice", "Saved."),
        ] {
            assert_eq!(node(&done, id), (Role::Status, text), "{id}");
        }
    }

    #[test]
    fn model_names_and_missing_requirements_come_from_the_registry() {
        let registry = broscribe_models::Registry::builtin().unwrap();
        assert!(!registry.models.is_empty());
        assert!(registry.models.iter().all(|entry| {
            entry
                .display_name
                .as_deref()
                .is_some_and(|name| name != entry.id)
        }));

        let list = parse_models_list(&serde_json::json!({
            "profile_models": ["required", "installed"],
            "models": [
                {"id": "required", "display_name": "Acorn", "bytes": 1_000_000, "state": "missing"},
                {"id": "installed", "display_name": "Beech", "state": "installed"},
                {"id": "optional", "display_name": "Clover", "state": "missing"}
            ],
        }));
        let required = list.missing_profile_models();
        assert_eq!(required.len(), 1);
        assert_eq!(list.model_name(&required[0].id), "Acorn");
        assert_eq!(
            transcription_block(&required).as_deref(),
            Some("Transcription can’t run until 1 model is downloaded (1.0 MB).")
        );
        assert_eq!(
            model_state_label("corrupt: checksum"),
            "Damaged. Download again."
        );
        assert!(transcription_block(&[]).is_none());
    }

    #[test]
    fn login_description_names_the_platform_surface() {
        use broscribe_platform::StatusIconPlace::*;
        assert!(login_description(MenuBar).contains("menu bar"));
        assert!(login_description(NotificationArea).contains("Show hidden icons"));
        assert!(login_description(SystemTray).contains("system tray"));
        assert!(login_description(NoTrayHost).contains("opens its window"));
    }

    #[gpui::test]
    fn recording_and_transcription_settings_expose_pane_controls(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (_, window) = settings_window(cx, dir.path(), Arc::new(FakeMicCapture::new(16_000)));
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        window.update(&mut visual, |view, window, cx| {
            view.set_page(SettingsPage::Recording, window, cx);
            view.meeting_apps = Some(Ok(setup::parse_meeting_apps(&serde_json::json!({
                "apps": [{"app_id": "acorn", "name": "Acorn Call", "policy": "ask", "installed": true}]
            }))));
        }).unwrap();
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            for id in ["live-identity", "consent-reminder"] {
                assert_eq!(tree.read(tree.find(id)).0, "Switch");
            }
            for id in [
                "recording-meeting-apps",
                "recording-system-checks",
                "recording-run-audio-check",
            ] {
                tree.find(id);
            }
        });
        window.update(&mut visual, |view, window, cx| {
            view.set_page(SettingsPage::Transcription, window, cx);
            view.models.list = parse_models_list(&serde_json::json!({
                "profile_models": ["acorn"],
                "profiles": [{"id": "example", "asr": "acorn", "languages": ["*"], "active": true}],
                "models": [{"id": "acorn", "display_name": "Acorn", "roles": ["asr"], "state": "missing"}]
            }));
        }).unwrap();
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            for id in [
                "settings-speech-models",
                "transcription-download-all",
                "settings-enrollment-heading",
                "settings-voice-record",
                "settings-word-list",
                "settings-words-save",
                "settings-models-advanced",
            ] {
                tree.find(id);
            }
            assert!(tree.nodes.iter().all(|node| node.name != "Models"));
        });
    }

    #[gpui::test]
    fn advanced_model_progress_has_a_named_percentage(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (_, window) = settings_window(cx, dir.path(), Arc::new(FakeMicCapture::new(16_000)));
        window.update(cx, |this, window, cx| {
            this.set_page(SettingsPage::Transcription, window, cx);
            this.models.advanced = true;
            this.models.list = parse_models_list(&serde_json::json!({"models":[
                {"id":"acorn", "display_name":"Acorn", "roles":["asr"], "state":"missing", "bytes":3456789}
            ]}));
            this.models.pulls.insert("acorn".into(), PullState::Running(Some(42)));
        }).unwrap();
        cx.update_window(window.into(), |_, window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("pull-asr-acorn")).1,
                "Downloading Acorn: 42%"
            );
            assert_eq!(tree.read(tree.find("pull-cancel-asr-acorn")).1, "Cancel");
            assert_eq!(
                tree.read(tree.find("model-asr-acorn-size")).1,
                bytes(3_456_789)
            );
        })
        .unwrap();
    }

    #[gpui::test]
    fn download_and_use_waits_for_a_successful_model_pull(cx: &mut TestAppContext) {
        use crate::test_support::{ps_path, ps_str, write_cli};
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("model-commands.log");
        let fail = dir.path().join("fail");
        let reply = r#"{"installed":true}"#;
        let error = r#"{"error":{"code":"cancelled","message":"synthetic cancellation"}}"#;
        let cli = write_cli(
            &dir.path().join("synthetic-cli"),
            &format!(
                "printf '%s\n' \"$*\" >> '{}'
case \"$*\" in
 *'models pull acorn'*) if test -f '{}'; then echo '{error}'; exit 1; fi;;
esac
echo '{reply}'
",
                log.display(),
                fail.display()
            ),
            &format!(
                "fx_log_args {};if ($all -clike '*models pull acorn*' -and (Test-Path {})) {{fx_echo {};exit 1}};fx_echo {}",
                ps_path(&log),
                ps_path(&fail),
                ps_str(error),
                ps_str(reply)
            ),
        );
        std::fs::write(&fail, "").unwrap();
        let (_, window) =
            settings_window_with(cx, dir.path(), Arc::new(FakeMicCapture::new(16_000)), cli);
        let act = |cx: &mut TestAppContext| {
            window.update(cx, |this, window, cx| {
            this.models.list = parse_models_list(&serde_json::json!({"models": [
                {"id": "acorn", "display_name": "Acorn", "roles": ["asr"], "state": "missing", "license_verified":true},
                {"id": "clover", "display_name": "Clover", "active_for":["asr"]}
            ]}));
            this.request_model("acorn".into(), Some("asr".into()), false, cx.focus_handle(), window, cx);
        }).unwrap()
        };
        act(cx);
        cx.run_until_parked();
        assert!(
            !std::fs::read_to_string(&log)
                .unwrap()
                .contains("models use asr acorn")
        );
        window
            .update(cx, |this, _, _| {
                assert_eq!(this.models.pulls.get("acorn"), Some(&PullState::Cancelled));
                assert_eq!(
                    this.models
                        .outcomes
                        .get(&("asr".into(), "acorn".into()))
                        .unwrap()
                        .1,
                    "Still using Clover."
                );
            })
            .unwrap();
        std::fs::remove_file(fail).unwrap();
        act(cx);
        cx.run_until_parked();
        let lines = std::fs::read_to_string(&log).unwrap();
        assert!(
            lines.rfind("models pull acorn").unwrap()
                < lines.rfind("models use asr acorn").unwrap()
        );
        window
            .update(cx, |this, _, _| {
                assert_eq!(this.models.pulls.get("acorn"), Some(&PullState::Done))
            })
            .unwrap();
    }

    #[gpui::test]
    fn download_all_stops_on_failure_and_keeps_completed_models(cx: &mut TestAppContext) {
        use crate::test_support::{ps_path, ps_str, write_cli};
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("download-commands.log");
        let models = serde_json::json!({
            "profile_models": ["acorn", "beech", "clover"],
            "models": [
                {"id":"acorn", "display_name":"Acorn", "state":"missing", "license_verified":true},
                {"id":"beech", "display_name":"Beech", "state":"missing", "license_verified":true},
                {"id":"clover", "display_name":"Clover", "state":"missing", "license_verified":true}
            ]
        })
        .to_string();
        let failure = r#"{"error":{"code":"io","message":"download failed"}}"#;
        let cli = write_cli(
            &dir.path().join("synthetic-cli"),
            &format!(
                "printf '%s\n' \"$*\" >> '{}'
case \"$*\" in
 *'models pull beech'*) echo '{failure}'; exit 1;;
esac
echo '{models}'
",
                log.display()
            ),
            &format!(
                "fx_log_args {};if ($all -clike '*models pull beech*') {{fx_echo {};exit 1}};fx_echo {}",
                ps_path(&log),
                ps_str(failure),
                ps_str(&models)
            ),
        );
        let (_, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            view.update(cx, |this, cx| {
                this.set_page(SettingsPage::Transcription, window, cx)
            })
        });
        crate::ui::kit::dialog_gate::click(vcx, "transcription-download-all", Role::Button);
        let calls = std::fs::read_to_string(log).unwrap();
        assert!(
            calls.find("models pull acorn").unwrap() < calls.find("models pull beech").unwrap()
        );
        assert!(!calls.contains("models pull clover"));
        view.read_with(vcx, |this, _| {
            assert_eq!(this.models.pulls["acorn"], PullState::Done);
            assert!(matches!(&this.models.pulls["beech"], PullState::Failed(_)));
            let (kind, words) = this.models.batch_status.as_ref().unwrap();
            assert_eq!(*kind, StatusKind::Failed);
            assert!(words.contains("Models already downloaded are kept."));
        });
    }

    #[gpui::test]
    fn unverified_model_opens_cancel_focused_without_downloading(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = synthetic_cli(dir.path());
        let (_, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        vcx.update(|window, cx| {
            view.update(cx, |this, cx| {
                this.set_page(SettingsPage::Transcription, window, cx);
                this.models.advanced = true;
                this.models.list = parse_models_list(&serde_json::json!({"models": [
                    {"id":"acorn", "display_name":"Acorn", "roles":["asr"], "state":"missing",
                     "license":"Unknown licence", "license_verified": false}
                ]}));
                this.request_model(
                    "acorn".into(),
                    Some("asr".into()),
                    false,
                    cx.focus_handle(),
                    window,
                    cx,
                );
            })
        });
        draw_twice(vcx);
        vcx.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert!(tree.nodes.iter().any(|node| node.role == "AlertDialog"
                && node.name == "Use a model with an unverified licence?"));
            assert!(
                gpui::base::test_support::snapshots(window)
                    .iter()
                    .any(|node| node.label() == Some("Cancel") && node.focused() == Some(true))
            );
        });
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(!cli_log(dir.path()).contains("models pull acorn"));
    }

    #[gpui::test]
    fn storage_shows_disk_use_and_cancelling_folder_change_creates_nothing(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let cli = synthetic_cli(dir.path());
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        let root = state.read_with(vcx, |state, _| state.library().root().to_owned());
        std::fs::write(root.join("synthetic-file"), vec![0; 128]).unwrap();
        let other = dir.path().join("New library");
        vcx.update(|window, cx| {
            view.update(cx, |this, cx| {
                this.set_page(SettingsPage::Storage, window, cx);
                this.measure_disk_use(cx);
            })
        });
        vcx.run_until_parked();
        assert_eq!(directory_bytes(&root).unwrap(), 128);
        assert_eq!(
            disk_usage_text(None, None),
            "Library: Unknown · Unknown free space"
        );
        assert!(disk_usage_text(Some(3_100_000_000), Some(120_000_000_000)).contains("3.1 GB"));
        vcx.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert!(
                tree.read(tree.find("storage-disk-use-value"))
                    .1
                    .contains("128 B")
            );
            assert_eq!(
                tree.read(tree.find("reveal-library")).1,
                NativePlatform::host_platform().reveal_label()
            );
            assert!(tree.nodes.iter().all(|node| node.name != "Save"));
            view.update(cx, |this, cx| {
                this.confirm_library(other.clone(), window, cx)
            });
        });
        assert!(has_dialog(vcx));
        assert!(
            spoken_now(vcx)
                .iter()
                .any(|(id, _, name)| id == "library-switch-change"
                    && name.contains("Keep recordings policy"))
        );
        assert!(!other.exists());
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(!has_dialog(vcx));
        assert!(!other.exists());
        assert!(state.read_with(vcx, |state, _| state.config().library.is_none()));
    }

    #[gpui::test]
    fn uninstall_confirmation_previews_and_escape_returns_to_the_opener(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        use crate::test_support::{ps_path, ps_str, write_cli};
        let log = dir.path().join("args.log");
        let preview = serde_json::json!({
            "ready": true, "manual_steps": [], "steps": []
        })
        .to_string();
        let cli = write_cli(
            &dir.path().join("uninstall-cli"),
            &format!(
                "printf '%s\n' \"$*\" >> '{}'\necho '{}'",
                log.display(),
                preview
            ),
            &format!("fx_log_args {};fx_echo {}", ps_path(&log), ps_str(&preview)),
        );
        let (_, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        vcx.update(|window, cx| {
            view.update(cx, |this, cx| {
                this.set_page(SettingsPage::Privacy, window, cx)
            });
        });
        draw_twice(vcx);
        let opener = vcx.update(|window, cx| {
            view.update(cx, |this, cx| {
                let opener = this.uninstall_focus.clone();
                opener.focus(window, cx);
                this.open_uninstall(window, cx);
                opener
            })
        });
        vcx.run_until_parked();
        assert!(has_dialog(vcx));
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(!has_dialog(vcx));
        vcx.update(|window, _| assert!(opener.is_focused(window)));
        assert!(cli_log(dir.path()).contains("uninstall --dry-run"));
        assert!(!cli_log(dir.path()).contains("uninstall --yes"));
    }

    #[gpui::test]
    fn privacy_names_each_channel_and_opens_people(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = synthetic_cli(dir.path());
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        vcx.update(|window, cx| {
            view.update(cx, |this, cx| {
                this.set_page(SettingsPage::Privacy, window, cx)
            })
        });
        vcx.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("privacy-local")).1,
                "Stays on this computer"
            );
            assert_eq!(
                tree.read(tree.find("privacy-optional")).1,
                "Leaves only when you turn it on"
            );
            for id in ["providers", "tools", "calendars", "updates", "downloads"] {
                assert!(
                    tree.read(tree.find(&format!("privacy-open-{id}")))
                        .1
                        .starts_with("Open ")
                );
            }
        });
        crate::ui::kit::dialog_gate::click(vcx, "privacy-open-people", Role::Button);
        assert_eq!(
            state.read_with(vcx, |state, _| state.route().clone()),
            Route::People { person: None }
        );
    }

    #[gpui::test]
    fn summary_switch_and_draft_revert_have_independent_state(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = synthetic_cli(dir.path());
        let (_, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        vcx.update(|window, cx| {
            view.update(cx, |this, cx| {
                this.set_page(SettingsPage::Summaries, window, cx);
                this.summary_instructions
                    .update(cx, |input, cx| input.set_value("Éco", window, cx));
            });
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(tree.read(tree.find("settings-auto-enrich")).0, "Switch");
            assert!(
                tree.read(tree.find("settings-auto-enrich-note"))
                    .1
                    .contains("single recording")
            );
            assert_eq!(
                tree.read(tree.find("summary-instructions-count")).1,
                "3 / 4,000"
            );
            assert_eq!(
                tree.read(tree.find("summary-instructions-unsaved")).1,
                "Unsaved changes"
            );
        });
        vcx.update(|window, cx| {
            view.update(cx, |this, cx| {
                assert!(this.instructions_dirty(cx));
                assert!(!this.should_close(window, cx));
            })
        });
        assert!(has_dialog(vcx));
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            view.update(cx, |this, cx| {
                assert!(this.instructions_dirty(cx));
                this.revert_instructions(window, cx);
                assert!(!this.instructions_dirty(cx));
            })
        });
    }

    #[test]
    fn hotword_note_follows_the_models_list() {
        let v = |plain_hints: Value| {
            parse_models_list(&serde_json::json!({
                "models": [
                    {"id": "synthetic-asr-plain", "display_name": "Acorn Voice", "accepts_hotwords": plain_hints,
                     "active_for": ["asr"]},
                    {"id": "synthetic-asr-hints", "display_name": "Clover Voice", "accepts_hotwords": true, "active_for": []}
                ],
                "profiles": [
                    {"id": "synthetic-plain", "asr": "synthetic-asr-plain", "active": true},
                    {"id": "synthetic-hints", "asr": "synthetic-asr-hints"},
                    {"id": "synthetic-gone", "asr": "synthetic-asr-missing"}
                ]
            }))
        };
        let note = hotword_note(&v(Value::Bool(false)), "ignored").unwrap();
        assert_eq!(note.model, "Acorn Voice");
        assert_eq!(note.profiles, ["Clover Voice"]);
        assert_eq!(
            note.text(),
            "Acorn Voice doesn’t use your word list. Clover Voice does."
        );
        // Takes hints, or the list does not say: no note.
        assert_eq!(hotword_note(&v(Value::Bool(true)), "ignored"), None);
        assert_eq!(hotword_note(&v(Value::Null), "ignored"), None);
        // No model marked active: the configured one.
        let mut list = v(Value::Bool(false));
        list.rows[0].active_for.clear();
        assert_eq!(hotword_note(&list, "synthetic-asr-hints"), None);
        let note = hotword_note(&list, "synthetic-asr-plain").unwrap();
        assert_eq!(note.model, "Acorn Voice");
        list.rows.retain(|r| r.id != "synthetic-asr-hints");
        assert!(
            hotword_note(&list, "synthetic-asr-plain")
                .unwrap()
                .profiles
                .is_empty()
        );
    }

    #[gpui::test]
    fn calendar_update_and_ai_tool_panes_scroll_at_largest_text(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (state, window) =
            settings_window(cx, dir.path(), Arc::new(FakeMicCapture::new(16_000)));
        state.update(cx, |state, cx| {
            let mut config = state.config().clone();
            config.appearance.reading_size = ReadingSize::Largest;
            state.set_config(config, cx);
        });
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_resize(gpui::size(px(640.), px(440.)));
        for page in [
            SettingsPage::Calendars,
            SettingsPage::Updates,
            SettingsPage::AiTools,
        ] {
            window
                .update(&mut visual, |view, window, cx| {
                    view.set_page(page, window, cx)
                })
                .unwrap();
            visual.run_until_parked();
            let scroll = window
                .update(&mut visual, |view, _, _| view.page_scroll.clone())
                .unwrap();
            visual.update(|window, cx| {
                window.set_rem_size(px(21.));
                window.draw(cx).clear(cx);
                let viewport = scroll.bounds();
                assert!(viewport.size.width < px(480.), "{page:?}: {viewport:?}");
                assert_eq!(scroll.offset().y, px(0.));
                assert!(scroll.max_offset().y > px(0.), "{page:?} must scroll");
                scroll.set_offset(gpui::point(px(0.), -scroll.max_offset().y));
                window.refresh();
                window.draw(cx).clear(cx);
                assert!(scroll.offset().y < px(0.));
            });
        }
    }

    #[gpui::test]
    fn providers_refresh_on_entry_and_after_the_assistant_drafts_go_on_leave(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (_state, window) =
            settings_window(cx, dir.path(), Arc::new(FakeMicCapture::new(16_000)));
        let listings = || {
            std::fs::read_to_string(dir.path().join("args.log"))
                .unwrap_or_default()
                .lines()
                .filter(|l| l.ends_with("providers list"))
                .count()
        };
        assert_eq!(listings(), 1);
        let pending = |window: gpui::WindowHandle<SettingsView>, cx: &mut TestAppContext| {
            window
                .update(cx, |this, _, cx| {
                    let p = this.providers.read(cx);
                    (
                        p.key_form_provider().map(str::to_owned),
                        p.pending_consent().map(str::to_owned),
                    )
                })
                .unwrap()
        };
        let open_drafts = |window: gpui::WindowHandle<SettingsView>, cx: &mut TestAppContext| {
            window
                .update(cx, |this, window, cx| {
                    this.providers.update(cx, |p, cx| {
                        p.open_key_form("remote".into(), window, cx);
                        p.request_consent("remote".into(), cx);
                    })
                })
                .unwrap();
        };
        open_drafts(window, cx);
        assert_eq!(
            pending(window, cx),
            (Some("remote".into()), Some("remote".into()))
        );
        window
            .update(cx, |this, window, cx| {
                this.set_page(SettingsPage::AiProviders, window, cx);
                this.set_page(SettingsPage::General, window, cx);
            })
            .unwrap();
        assert_eq!(pending(window, cx), (None, None));
        assert_eq!(listings(), 1);
        // Entering the provider pane refreshes detection and key state.
        cx.run_until_parked();
        assert_eq!(listings(), 2);
        // The assistant replaces the page (Settings' drafts go) and its
        // close relists Settings' providers.
        open_drafts(window, cx);
        window
            .update(cx, |this, window, cx| this.open_setup(window, cx))
            .unwrap();
        assert_eq!(
            pending(window, cx),
            (Some("remote".into()), Some("remote".into()))
        );
        cx.run_until_parked();
        let before_close = listings();
        window
            .update(cx, |this, _, cx| {
                let setup = this.setup.clone().unwrap();
                setup.update(cx, |setup, cx| setup.close(cx));
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| assert!(this.setup.is_none()))
            .unwrap();
        assert_eq!(listings(), before_close + 1);
    }

    #[gpui::test]
    fn library_switch_waits_for_sheet_imports(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (state, window) =
            settings_window(cx, dir.path(), Arc::new(FakeMicCapture::new(16_000)));
        let runs = cx.update(ImportRuns::global);
        let request = crate::views::import_sheet::ImportRequest {
            path: dir.path().join("synthetic.wav"),
            handling: Handling::Copy,
            language: None,
            title: None,
            date: None,
            participants: Vec::new(),
            manifest: false,
        };
        state.update(cx, |s, cx| s.start_imports(vec![request], &runs, cx));
        let other = dir.path().join("other-lib");
        window
            .update(cx, |this, _, cx| {
                assert!(jobs_busy(this.state.read(cx), this.imports.read(cx)));
                this.library_switch = LibrarySwitch::Waiting(other.clone());
                this.maybe_switch_library(cx);
                assert_eq!(this.library_switch, LibrarySwitch::Waiting(other.clone()));
            })
            .unwrap();
        cx.run_until_parked();
        // The import finished: the deferred switch went ahead on its own.
        window
            .update(cx, |this, _, _| {
                assert_eq!(this.library_switch, LibrarySwitch::Idle)
            })
            .unwrap();
    }

    #[gpui::test]
    fn setup_stays_open_across_main_navigation_and_closing_it_stops_its_capture(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mic = Arc::new(FakeMicCapture::new(16_000));
        let (state, window) = settings_window(cx, dir.path(), mic.clone());
        let data = state.read_with(cx, |state, _| state.paths().data_dir.clone());
        let config = Config::update(&data, |config| {
            config.setup.defer_assistant = true;
            config.setup.completed_steps = vec!["languages".into(), "permissions".into()];
            Ok(())
        })
        .unwrap()
        .0;
        state.update(cx, |state, cx| state.set_config(config, cx));
        window
            .update(cx, |this, window, cx| {
                this.open_setup(window, cx);
                let setup = this.setup.clone().unwrap();
                this.open_setup(window, cx);
                assert_eq!(cx.windows().len(), 2);
                assert_eq!(this.setup.as_ref(), Some(&setup));
                assert_eq!(setup.read(cx).step(), crate::views::setup::Step::Models);
                setup.update(cx, |setup, cx| setup.start_native_capture_for_test(cx));
            })
            .unwrap();
        cx.run_until_parked();
        let saved = Config::load(&data).unwrap();
        assert!(!saved.setup.defer_assistant);
        assert_eq!(saved.setup.completed_steps, ["languages", "permissions"]);
        assert_eq!(mic.stopped.load(Ordering::SeqCst), 0);
        state.update(cx, |s, cx| s.navigate(Route::library(), cx));
        cx.run_until_parked();
        assert_eq!(mic.stopped.load(Ordering::SeqCst), 0);
        window
            .update(cx, |this, _, cx| {
                let setup = this.setup.clone().unwrap();
                setup.update(cx, |setup, cx| setup.close(cx));
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                assert!(this.setup.is_none());
                assert_eq!(cx.windows().len(), 1);
            })
            .unwrap();
        assert_eq!(mic.stopped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn pull_progress_lines() {
        assert_eq!(
            parse_pull_line("silero-vad: 45% of 1.7 MiB", "silero-vad"),
            Some(45)
        );
        assert_eq!(
            parse_pull_line("silero-vad: 100% of 1.7 MiB", "silero-vad"),
            Some(100)
        );
        assert_eq!(parse_pull_line("other: 45% of 1 MiB", "silero-vad"), None);
        assert_eq!(parse_pull_line("silero-vad: verifying", "silero-vad"), None);
    }

    #[test]
    fn models_list_parsing_reads_profiles() {
        let v = serde_json::json!({"models":[{"id":"silero-vad","roles":["vad"],"license":"MIT",
            "license_verified":true,"bytes":2048,"state":"installed","active_for":["vad"]}]});
        let list = parse_models_list(&v);
        assert!(list.profiles.is_empty());
        assert_eq!(list.rows.len(), 1);
        assert!(list.rows[0].installed());
        let v = serde_json::json!({"models":[], "profiles":[
            {"id":"synthetic-a","description":"Model A","asr":"asr-a","languages":["en","pt"],"active":true},
            {"id":"synthetic-b","description":"Model B","asr":"asr-b","languages":["*"]},
            {"description":"no id"}
        ]});
        let profiles = parse_models_list(&v).profiles;
        assert_eq!(
            profiles,
            [
                ProfileChoice {
                    id: "synthetic-a".into(),
                    description: "Model A".into(),
                    asr: "asr-a".into(),
                    languages: vec!["en".into(), "pt".into()],
                    active: true,
                },
                ProfileChoice {
                    id: "synthetic-b".into(),
                    description: "Model B".into(),
                    asr: "asr-b".into(),
                    languages: vec!["*".into()],
                    active: false,
                },
            ]
        );
    }

    #[test]
    fn models_list_takes_the_profile_models_the_registry_resolved_and_never_guesses() {
        let list = parse_models_list(&serde_json::json!({"models": [],
            "profile_models": ["synthetic-vad", "synthetic-asr", "synthetic-live",
                               "synthetic-diar", "synthetic-emb"]}));
        assert_eq!(
            list.profile_models.unwrap(),
            [
                "synthetic-vad",
                "synthetic-asr",
                "synthetic-live",
                "synthetic-diar",
                "synthetic-emb"
            ]
        );
        let list = parse_models_list(&serde_json::json!({"models": [],
            "profile_models_error": "ASR model synthetic-asr declares no live_model"}));
        assert_eq!(
            list.profile_models.unwrap_err(),
            "ASR model synthetic-asr declares no live_model"
        );
        // Not reported at all: an error, never a list rebuilt from the config.
        assert!(
            parse_models_list(&serde_json::json!({"models": []}))
                .profile_models
                .is_err()
        );
    }

    #[test]
    fn language_tags() {
        for ok in ["en", "pt-BR", "pt-PT", "zh-Hant-TW", "yue"] {
            assert!(valid_language_tag(ok), "{ok}");
        }
        for bad in ["", "e", "english", "pt_BR", "pt-", "-BR"] {
            assert!(!valid_language_tag(bad), "{bad}");
        }
    }

    #[test]
    fn config_edits_apply_to_the_fresh_file_and_preserve_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let mut c = Config::default();
        c.hotwords.push("Kubernetes".into());
        c.save(&data).unwrap();
        let read_before = Config::load(&data).unwrap();
        // Another writer (a terminal `setup`) changes a field meanwhile.
        edit_config(&data, |c| c.default_language = Some("pt-BR".into())).unwrap();
        let saved = edit_config(&data, |c| {
            c.retention = Retention::DeleteAfterDays { days: 7 };
        })
        .unwrap();
        assert_eq!(saved.hotwords, ["Kubernetes"]);
        assert_eq!(saved.default_language.as_deref(), Some("pt-BR"));
        assert!(saved.revision() > read_before.revision() + 1);
        assert_eq!(Config::load(&data).unwrap(), saved);
    }

    /// Settings › General › Appearance: System is the default; Dark and
    /// Light save to `[appearance] theme` and every window follows.
    #[gpui::test]
    fn appearance_saves_and_the_theme_follows(cx: &mut TestAppContext) {
        use gpui::component::ThemeMode;
        let dir = tempfile::tempdir().unwrap();
        let (state, window) =
            settings_window(cx, dir.path(), Arc::new(FakeMicCapture::new(16_000)));
        let data = state.read_with(cx, |s, _| s.paths().data_dir.clone());
        state.read_with(cx, |s, _| {
            assert_eq!(s.config().appearance.theme, ThemeChoice::System)
        });
        cx.update(|cx| crate::ui::follow_reading_size(&state, cx));
        for (choice, mode) in [
            (ThemeChoice::Dark, ThemeMode::Dark),
            (ThemeChoice::Light, ThemeMode::Light),
        ] {
            window
                .update(cx, |this, _, cx| this.set_theme_choice(choice, cx))
                .unwrap();
            cx.run_until_parked();
            assert_eq!(Config::load(&data).unwrap().appearance.theme, choice);
            cx.update(|cx| {
                assert_eq!(crate::ui::theme::theme_choice(cx), choice);
                assert_eq!(cx.theme().mode, mode, "{choice:?}");
            });
        }
        window
            .update(cx, |this, _, cx| {
                this.set_theme_choice(ThemeChoice::System, cx)
            })
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(crate::ui::theme::theme_choice(cx), ThemeChoice::System);
            assert_eq!(cx.theme().mode, ThemeMode::from(cx.window_appearance()));
        });
        // The three choices are named radios under one Appearance group.
        cx.update_window(window.into(), |_, window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            for name in ["System", "Light", "Dark"] {
                assert!(
                    tree.nodes
                        .iter()
                        .any(|node| node.role == "RadioButton" && node.name == name),
                    "{name}"
                );
            }
        })
        .unwrap();
    }

    #[gpui::test]
    fn reading_size_saves_and_untouched_fields_follow_newer_saves(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (state, window) =
            settings_window(cx, dir.path(), Arc::new(FakeMicCapture::new(16_000)));
        let data = state.read_with(cx, |s, _| s.paths().data_dir.clone());
        window
            .update(cx, |this, window, cx| {
                // A language draft is being typed; retention days are untouched.
                this.language
                    .update(cx, |input, cx| input.set_value("pt-PT", window, cx));
                this.set_reading_size(ReadingSize::Larger, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            Config::load(&data).unwrap().appearance.reading_size,
            ReadingSize::Larger
        );
        state.read_with(cx, |s, _| {
            assert_eq!(s.config().appearance.reading_size, ReadingSize::Larger)
        });
        // A terminal saves other values as a newer revision.
        let saved = edit_config(&data, |c| {
            c.default_language = Some("en".into());
            c.retention = Retention::DeleteAfterDays { days: 9 };
        })
        .unwrap();
        state.update(cx, |s, cx| s.set_config(saved, cx));
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                assert_eq!(this.language.read(cx).value().to_string(), "pt-PT");
                assert_eq!(this.retention_days.read(cx).value().to_string(), "9");
            })
            .unwrap();
        let config = Config::load(&data).unwrap();
        assert_eq!(config.appearance.reading_size, ReadingSize::Larger);
        assert_eq!(config.default_language.as_deref(), Some("en"));
    }

    #[gpui::test]
    fn cancelling_a_download_closes_stdin_and_shows_the_cli_answer(cx: &mut TestAppContext) {
        use crate::test_support::{ps_str, write_cli};
        let dir = tempfile::tempdir().unwrap();
        let cancelled = r#"{"error":{"code":"cancelled","message":"the pull was cancelled"}}"#;
        // The synthetic CLI cancels only through the published flag: it
        // waits for stdin to close, then answers `cancelled`.
        let cli = write_cli(
            &dir.path().join("synthetic-cli"),
            "case \"$*\" in\n\
             *'models pull synthetic-model --cancel-on-stdin-eof'*)\n\
             cat >/dev/null\n\
             echo '{\"error\":{\"code\":\"cancelled\",\"message\":\"the pull was cancelled\"}}'\n\
             exit 1;;\n\
             *) echo '{}';;\nesac\n",
            &format!(
                "if($all -clike '*models pull synthetic-model --cancel-on-stdin-eof*')\
                 {{fx_drain_stdin;fx_echo {};exit 1}}else{{fx_echo '{{}}'}}",
                ps_str(cancelled)
            ),
        );
        let (_state, window) =
            settings_window_with(cx, dir.path(), Arc::new(FakeMicCapture::new(16_000)), cli);
        window
            .update(cx, |this, _, cx| {
                this.pull("synthetic-model".into(), false, cx);
                this.cancel_pull("synthetic-model", cx);
                assert_eq!(
                    this.models.pulls.get("synthetic-model"),
                    Some(&PullState::Cancelling)
                );
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert_eq!(
                    this.models.pulls.get("synthetic-model"),
                    Some(&PullState::Cancelled)
                );
                assert!(this.models.pull_cancels.is_empty());
            })
            .unwrap();
    }

    /// A synthetic `broscribe` that runs `sh` (Unix) or the PowerShell
    /// fixture body `ps` (Windows) for any command, through a runner on a
    /// synthetic library folder.
    fn pull_cli(dir: &Path, sh: &str, ps: &str) -> CliRunner {
        let cli =
            crate::test_support::write_cli(&dir.join("synthetic-pull-cli"), &format!("{sh}\n"), ps);
        crate::test_support::runner(cli, dir.join("lib"))
    }

    /// `{"installed":"synthetic-model"}` for a PowerShell fixture body.
    fn installed_ps() -> String {
        crate::test_support::ps_str(r#"{"installed":"synthetic-model"}"#)
    }

    #[gpui::test]
    async fn a_progress_flood_while_the_ui_is_busy_holds_only_the_newest_percent(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let cli = pull_cli(
            dir.path(),
            r#"i=0
while [ $i -lt 5000 ]; do echo "synthetic-model: $((i % 100))% of 1 MB" >&2; i=$((i + 1)); done
echo 'synthetic-model: 100% of 1 MB' >&2
echo '{"installed":"synthetic-model"}'"#,
            &format!(
                "for($i=0;$i -lt 5000;$i++){{fx_eecho ('synthetic-model: '+($i % 100)+'% of 1 MB')}};\
                 fx_eecho 'synthetic-model: 100% of 1 MB';fx_echo {}",
                installed_ps()
            ),
        );
        let (task, mut progress, _cancel) =
            spawn_pull(cli, "synthetic-model".into(), false, &cx.executor());
        // The UI reads no progress until the pull has ended.
        let result = task.await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(progress.next().await, Some(100));
        assert_eq!(progress.next().await, None, "nothing else was queued");
    }

    #[gpui::test]
    async fn malformed_and_over_long_stderr_never_stops_the_pull_or_its_progress(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        // Invalid UTF-8, one line far past the limit, then more stderr than
        // a pipe holds, then progress and the answer.
        let cli = pull_cli(
            dir.path(),
            r#"printf '\377\376 synthetic\n' >&2
head -c 200000 /dev/zero | tr '\0' 'y' >&2
printf '\n' >&2
head -c 300000 /dev/zero | tr '\0' 'z' | fold -w 100 >&2
printf '\n' >&2
echo 'synthetic-model: 42% of 1 MB' >&2
echo '{"installed":"synthetic-model"}'"#,
            &format!(
                "fx_ebytes 255,254;fx_eprint (' synthetic'+[char]10);\
                 fx_eprint ('y'*200000);fx_eprint ([string][char]10);\
                 fx_eprint ((@('z'*100)*3000) -join [string][char]10);fx_eprint ([string][char]10);\
                 fx_eecho 'synthetic-model: 42% of 1 MB';fx_echo {}",
                installed_ps()
            ),
        );
        let (task, mut progress, _cancel) =
            spawn_pull(cli, "synthetic-model".into(), false, &cx.executor());
        let result = task.await;
        assert_eq!(result.unwrap()["installed"], "synthetic-model");
        assert_eq!(progress.next().await, Some(42));
        assert_eq!(progress.next().await, None);
    }

    #[gpui::test]
    async fn stdout_filling_its_pipe_before_stderr_closes_is_read_at_once(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        // More stdout than a pipe holds while stderr is still open: read
        // one after the other, neither would ever end.
        let mut cli = pull_cli(
            dir.path(),
            r#"head -c 300000 /dev/zero | tr '\0' 'x'
echo 'synthetic-model: 50% of 1 MB' >&2"#,
            "fx_print ('x'*300000);fx_eecho 'synthetic-model: 50% of 1 MB'",
        );
        cli.set_output_limit(1024);
        let (task, mut progress, _cancel) =
            spawn_pull(cli, "synthetic-model".into(), false, &cx.executor());
        let error = task.await.unwrap_err();
        assert_eq!(error.code, "output_too_large");
        assert_eq!(progress.next().await, Some(50));
    }

    /// Cancel closes stdin while the pull runs; the result is the CLI's own
    /// answer, whichever it gives: `cancelled`, or an install it had
    /// already committed (never relabelled cancelled).
    #[test]
    fn a_running_pull_ends_with_the_clis_own_answer_to_its_cancellation() {
        let refusal = r#"{"error":{"code":"cancelled","message":"the pull was cancelled"}}"#;
        for (answer, answer_ps, cancelled) in [
            (
                format!("echo '{refusal}'; exit 1"),
                format!("fx_echo {};exit 1", crate::test_support::ps_str(refusal)),
                true,
            ),
            (
                r#"echo '{"installed":"synthetic-model"}'"#.to_owned(),
                format!("fx_echo {}", installed_ps()),
                false,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let cli = pull_cli(
                dir.path(),
                &format!("echo 'synthetic-model: 10% of 1 MB' >&2\ncat >/dev/null\n{answer}"),
                &format!("fx_eecho 'synthetic-model: 10% of 1 MB';fx_drain_stdin;{answer_ps}"),
            );
            let mut progress = PullProgress::new();
            let slot = Arc::clone(&progress.shared);
            let cancel = PullCancel::default();
            let run = std::thread::spawn({
                let cancel = cancel.clone();
                move || {
                    let result = run_pull(&cli, "synthetic-model", false, &cancel, &slot);
                    slot.close();
                    result
                }
            });
            // The child runs and reported progress; only stdin closing ends it.
            assert_eq!(futures::executor::block_on(progress.next()), Some(10));
            cancel.cancel();
            let result = run.join().unwrap();
            let state = PullState::finished("synthetic-model", &result);
            if cancelled {
                assert_eq!(state, PullState::Cancelled, "{result:?}");
            } else {
                assert_eq!(state, PullState::Done, "{result:?}");
            }
        }
    }

    #[gpui::test]
    fn summary_instructions_are_saved_through_stdin(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display();
        let cli = crate::test_support::write_cli(
            &dir.path().join("synthetic-cli"),
            &format!(
                "printf '%s\\n' \"$*\" >> '{d}/args.log'\n\
                 case \"$*\" in *--stdin*) cat > '{d}/stdin.log' ;; esac\n\
                 echo '{{}}'\n"
            ),
            &format!(
                "fx_log_args {};if($all -clike '*--stdin*'){{fx_save_stdin {}}};fx_echo '{{}}'",
                crate::test_support::ps_path(&dir.path().join("args.log")),
                crate::test_support::ps_path(&dir.path().join("stdin.log")),
            ),
        );
        let (_state, window) =
            settings_window_with(cx, dir.path(), Arc::new(FakeMicCapture::new(16_000)), cli);
        let text = "Focus on the synthetic risks, in three bullet points.";
        window
            .update(cx, |this, window, cx| {
                this.summary_instructions
                    .update(cx, |i, cx| i.set_value(text.to_owned(), window, cx));
                this.save_summary_instructions(cx);
            })
            .unwrap();
        cx.run_until_parked();
        let log = std::fs::read_to_string(dir.path().join("args.log")).unwrap();
        assert!(log.contains(" enrich instructions set --stdin\n"), "{log}");
        assert!(
            !log.contains("synthetic risks"),
            "instructions leaked into argv"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("stdin.log")).unwrap(),
            text
        );
        window
            .update(cx, |this, window, cx| {
                assert!(!this.instructions_notice.as_ref().unwrap().1);
                this.summary_instructions
                    .update(cx, |i, cx| i.set_value(String::new(), window, cx));
                this.save_summary_instructions(cx);
            })
            .unwrap();
        cx.run_until_parked();
        let log = std::fs::read_to_string(dir.path().join("args.log")).unwrap();
        assert!(log.contains(" enrich instructions clear\n"), "{log}");
    }

    /// Opens `SettingsView` inside a `gpui::component::Root` window so modal
    /// `AlertDialog`s mount, paint and restore focus identically to the app,
    /// on the library `initial` names (the default one when unset).
    pub(super) fn settings_root_window_with<'a>(
        cx: &'a mut TestAppContext,
        dir: &Path,
        initial: Config,
        cli: PathBuf,
    ) -> (
        Entity<AppState>,
        Entity<SettingsView>,
        &'a mut gpui::VisualTestContext,
    ) {
        let library = initial
            .library
            .clone()
            .unwrap_or_else(|| FakePaths::new(dir).default_library_dir());
        settings_root_window_on(cx, dir, initial, cli, library, false)
    }

    /// [`settings_root_window_with`] with the app started on `library`
    /// whatever `initial` names (as `--library` does, or
    /// `BROSCRIBE_LIBRARY` when `from_env`).
    fn settings_root_window_on<'a>(
        cx: &'a mut TestAppContext,
        dir: &Path,
        mut initial: Config,
        cli: PathBuf,
        library: PathBuf,
        from_env: bool,
    ) -> (
        Entity<AppState>,
        Entity<SettingsView>,
        &'a mut gpui::VisualTestContext,
    ) {
        let mut paths = resolve_paths(&FakePaths::new(dir)).unwrap();
        paths.library_from_env = from_env.then(|| library.clone());
        initial.save(&paths.data_dir).unwrap();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(paths, initial, Library::open(library).unwrap(), cli),
                cx,
            )
        });
        let deps = SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        cx.update(crate::ui::theme::init);
        let view_slot: Rc<RefCell<Option<Entity<SettingsView>>>> = Rc::default();
        let slot = view_slot.clone();
        let state_for_win = state.clone();
        let (_, vcx) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|cx| SettingsView::new(state_for_win, deps, window, cx));
            *slot.borrow_mut() = Some(view.clone());
            gpui::component::Root::new(view, window, cx)
        });
        vcx.simulate_resize(gpui::size(px(960.), px(720.)));
        vcx.run_until_parked();
        vcx.update(|window, cx| window.draw(cx).clear(cx));
        let view = view_slot.borrow().clone().unwrap();
        (state, view, vcx)
    }

    /// A complete `retention apply --dry-run --policy` answer: `@POLICY@` is
    /// replaced by the policy the synthetic CLI was asked about.
    fn dry_run_reply(preview: RetentionPreview, unreadable: u64) -> String {
        let RetentionPreview {
            recordings,
            bytes,
            held,
            held_bytes,
            voice_readings,
        } = preview;
        format!(
            r#"{{"dry_run":true,"policy":"@POLICY@","checked":3,"recordings":{recordings},"bytes":{bytes},"kept_for_live_jobs":{held},"kept_for_live_jobs_bytes":{held_bytes},"due_dates_updated":0,"leftovers_removed":0,"enrollment_files_removed":{voice_readings},"unreadable":{unreadable},"unreadable_people":0,"unreadable_jobs":0}}"#
        )
    }

    fn deletes(recordings: u64, bytes: u64) -> RetentionPreview {
        RetentionPreview {
            recordings,
            bytes,
            ..RetentionPreview::default()
        }
    }

    /// A synthetic CLI that logs argv and answers `retention apply --dry-run`
    /// with `reply_json`, its `@POLICY@` replaced by the `--policy` value (or
    /// fails with exit 1 when `fail` is true).
    fn retention_cli(dir: &Path, reply_json: &str, fail: bool) -> PathBuf {
        use crate::test_support::{ps_path, ps_str, write_cli};
        let log = dir.join("args.log");
        let (sh_exit, ps_exit) = if fail {
            ("\nexit 1", ";exit 1")
        } else {
            ("", "")
        };
        write_cli(
            &dir.join("synthetic-retention-cli"),
            &format!(
                "printf '%s\\n' \"$*\" >> '{log}'\n\
                 case \"$*\" in\n\
                 *'retention apply --dry-run'*)\n\
                 policy=$(printf '%s\\n' \"$*\" | sed -n 's/.*--policy \\([^ ]*\\).*/\\1/p')\n\
                 printf '%s\\n' '{reply_json}' | sed \"s/@POLICY@/$policy/g\"{sh_exit};;\n\
                 *) echo '{{}}';;\n\
                 esac\n",
                log = log.display()
            ),
            &format!(
                "fx_log_args {};if($all -clike '*retention apply --dry-run*'){{$p='';\
                 if($all -cmatch '--policy (\\S+)'){{$p=$Matches[1]}};\
                 fx_echo ({} -replace '@POLICY@',$p){ps_exit}}}else{{fx_echo '{{}}'}}",
                ps_path(&log),
                ps_str(reply_json)
            ),
        )
    }

    fn cli_log(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("args.log")).unwrap_or_default()
    }

    fn dry_runs(dir: &Path) -> usize {
        cli_log(dir).matches("retention apply --dry-run").count()
    }

    fn draw_twice(vcx: &mut gpui::VisualTestContext) {
        vcx.update(|window, cx| window.draw(cx).clear(cx));
        vcx.update(|window, cx| window.draw(cx).clear(cx));
    }

    /// What one frame says, after a frame that lets a new dialog settle.
    fn spoken_now(vcx: &mut gpui::VisualTestContext) -> Vec<(String, Role, String)> {
        vcx.update(|window, cx| window.draw(cx).clear(cx));
        crate::ui::spoken_log::while_building(|| {
            vcx.update(|window, cx| window.draw(cx).clear(cx));
        })
        .1
    }

    fn has_dialog(vcx: &mut gpui::VisualTestContext) -> bool {
        vcx.update(|window, cx| window.has_active_dialog(cx))
    }

    fn saved_retention(data_dir: &Path) -> Retention {
        Config::load(data_dir).unwrap().retention
    }

    fn choose(
        vcx: &mut gpui::VisualTestContext,
        view: &Entity<SettingsView>,
        retention: Retention,
        trigger: RetentionTrigger,
    ) {
        vcx.update(|window, cx| {
            view.update(cx, |this, cx| {
                this.set_retention(retention, trigger, window, cx)
            });
        });
    }

    fn retention_button_center(
        vcx: &mut gpui::VisualTestContext,
        id: &str,
    ) -> gpui::Point<gpui::Pixels> {
        vcx.update(|window, _| {
            gpui::base::test_support::snapshots(window)
                .into_iter()
                .find(|node| {
                    node.role() == Some(Role::Button)
                        && node
                            .path()
                            .last()
                            .is_some_and(|last| last.to_string() == id)
                })
                .unwrap_or_else(|| panic!("button {id} not painted"))
                .bounds()
                .center()
        })
    }

    fn click_button(vcx: &mut gpui::VisualTestContext, id: &str) {
        draw_twice(vcx);
        let center = retention_button_center(vcx, id);
        vcx.simulate_click(center, gpui::Modifiers::none());
        vcx.run_until_parked();
        draw_twice(vcx);
    }

    /// The spoken name of button `id` as painted.
    fn button_label(vcx: &mut gpui::VisualTestContext, id: &str) -> Option<String> {
        draw_twice(vcx);
        vcx.update(|window, _| {
            gpui::base::test_support::snapshots(window)
                .into_iter()
                .find(|node| {
                    node.role() == Some(Role::Button)
                        && node
                            .path()
                            .last()
                            .is_some_and(|last| last.to_string() == id)
                })
                .and_then(|node| node.label().map(str::to_owned))
        })
    }

    /// Whether radio `id` is painted as selected.
    fn radio_checked(vcx: &mut gpui::VisualTestContext, id: &str) -> Option<bool> {
        vcx.update(|window, cx| {
            window.draw(cx).clear(cx);
            gpui::base::test_support::snapshots(window)
                .into_iter()
                .find(|node| {
                    node.role() == Some(Role::RadioButton)
                        && node
                            .path()
                            .last()
                            .is_some_and(|last| last.to_string() == id)
                })
                .and_then(|node| node.checked())
        })
    }

    const PROCESSED: Retention = Retention::DeleteImmediately {
        wait_until_named: false,
    };
    const PROCESSED_AND_NAMED: Retention = Retention::DeleteImmediately {
        wait_until_named: true,
    };

    #[test]
    fn retention_dry_run_answers_must_be_complete_and_for_the_proposed_policy() {
        let full = |policy: &str| {
            serde_json::from_str::<Value>(
                &dry_run_reply(
                    RetentionPreview {
                        recordings: 2,
                        bytes: 20,
                        held: 1,
                        held_bytes: 5,
                        voice_readings: 1,
                    },
                    0,
                )
                .replace("@POLICY@", policy),
            )
            .unwrap()
        };
        assert_eq!(
            parse_retention_dry_run(&full("processed"), PROCESSED),
            Ok(RetentionPreview {
                recordings: 2,
                bytes: 20,
                held: 1,
                held_bytes: 5,
                voice_readings: 1,
            })
        );
        let unexpected = Err(UNEXPECTED_ANSWER.to_owned());
        // An answer about another policy proves nothing about this one.
        assert_eq!(
            parse_retention_dry_run(&full("keep"), PROCESSED),
            unexpected
        );
        // Neither does the legacy `deleted` list, a real run or a missing count.
        assert_eq!(
            parse_retention_dry_run(
                &serde_json::json!({"dry_run": true, "policy": "processed", "deleted": []}),
                PROCESSED
            ),
            unexpected
        );
        let mut not_dry = full("processed");
        not_dry["dry_run"] = false.into();
        assert_eq!(parse_retention_dry_run(&not_dry, PROCESSED), unexpected);
        for key in [
            "recordings",
            "bytes",
            "kept_for_live_jobs",
            "kept_for_live_jobs_bytes",
            "enrollment_files_removed",
            "unreadable",
            "unreadable_people",
        ] {
            let mut partial = full("processed");
            partial.as_object_mut().unwrap().remove(key);
            assert_eq!(
                parse_retention_dry_run(&partial, PROCESSED),
                unexpected,
                "{key}"
            );
        }
        // A meeting it couldn't read leaves what goes unknown.
        let mut unreadable = full("processed");
        unreadable["unreadable"] = 2.into();
        assert_eq!(
            parse_retention_dry_run(&unreadable, PROCESSED),
            Err("2 meetings couldn't be read".to_owned())
        );
        // So does a profile holding a kept voice reading: whether the
        // reading goes is unknown.
        let mut profile = full("processed");
        profile["unreadable_people"] = 1.into();
        assert_eq!(
            parse_retention_dry_run(&profile, PROCESSED),
            Err("1 person's profile couldn't be read".to_owned())
        );
        profile["unreadable_people"] = 2.into();
        assert_eq!(
            parse_retention_dry_run(&profile, PROCESSED),
            Err("2 people's profiles couldn't be read".to_owned())
        );
        profile["unreadable"] = 1.into();
        assert_eq!(
            parse_retention_dry_run(&profile, PROCESSED),
            Err("1 meeting and 2 people's profiles couldn't be read".to_owned())
        );
    }

    #[test]
    fn retention_check_causes_are_path_free() {
        let err = |code: &str, message: &str| CliError::new(code, message);
        assert_eq!(
            retention_check_cause(&err("io", "Library folder is unreadable")),
            "Library folder is unreadable"
        );
        assert_eq!(
            retention_check_cause(&err(
                "schema",
                "/synthetic/library/people/p-c3c3c3/voiceprints.json: expected value"
            )),
            "A file in the library folder is damaged"
        );
        assert_eq!(
            retention_check_cause(&err("spawn", "/synthetic/bin/broscribe: not found")),
            "BroScribe's command-line tool didn't start"
        );
        assert_eq!(
            retention_check_cause(&err("io", "waiting for the CLI: /synthetic: gone")),
            "The check didn't finish"
        );
        assert_eq!(
            retention_check_cause(&err("maintenance", "maintenance is enabled")),
            "BroScribe is in maintenance mode"
        );
        assert_eq!(
            retention_check_cause(&err("bad_output", "the CLI printed no JSON")),
            UNEXPECTED_ANSWER
        );
        // A failure before the check ran, as the CLI words it.
        assert_eq!(
            retention_check_cause(&err("io", "BroScribe's data folder is unreadable")),
            "BroScribe's data folder is unreadable"
        );
    }

    #[test]
    fn retention_confirmation_words_count_held_recordings_and_the_voice_reading() {
        let text = retention_confirm_text(RetentionPreview {
            recordings: 2,
            bytes: 2_000_000,
            held: 1,
            held_bytes: 1_000_000,
            voice_readings: 1,
        });
        assert_eq!(text.title, "Delete 3 recordings now?");
        assert_eq!(text.action, "Delete 3 recordings");
        assert_eq!(
            text.goes,
            [
                "The recordings of 3 meetings already in your library (3.0 MB)",
                "1 of them goes when its processing finishes",
                "Later recordings, once they qualify",
                "Your kept voice reading",
            ]
        );
        assert!(text.warning.starts_with("Without the audio"));

        let held_only = retention_confirm_text(RetentionPreview {
            held: 1,
            held_bytes: 9,
            ..RetentionPreview::default()
        });
        assert_eq!(held_only.title, "Delete 1 recording now?");
        assert_eq!(
            held_only.goes,
            [
                "The recordings of 1 meeting already in your library (9 B)",
                "It goes when its processing finishes",
                "Later recordings, once they qualify",
            ]
        );
        let all_held = retention_confirm_text(RetentionPreview {
            held: 2,
            ..RetentionPreview::default()
        });
        assert_eq!(all_held.goes[1], "They go when their processing finishes");
        let some_held = retention_confirm_text(RetentionPreview {
            recordings: 1,
            held: 2,
            ..RetentionPreview::default()
        });
        assert_eq!(
            some_held.goes[1],
            "2 of them go when their processing finishes"
        );

        let reading = retention_confirm_text(RetentionPreview {
            voice_readings: 1,
            ..RetentionPreview::default()
        });
        assert_eq!(reading.title, "Delete your kept voice reading now?");
        assert_eq!(reading.action, "Delete voice reading");
        assert_eq!(
            reading.goes,
            [
                "Later recordings, once they qualify",
                "Your kept voice reading"
            ]
        );
        assert!(reading.warning.starts_with("Without it"));
        let readings = retention_confirm_text(RetentionPreview {
            voice_readings: 2,
            ..RetentionPreview::default()
        });
        assert_eq!(readings.title, "Delete your kept voice readings now?");
        assert_eq!(readings.goes[1], "Your 2 kept voice readings");
    }

    #[gpui::test]
    fn retention_looser_or_equal_policy_saves_without_dry_run(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let mut initial = Config::default();
        initial.retention = PROCESSED;
        let cli = retention_cli(dir.path(), &dry_run_reply(deletes(99, 9999), 0), false);
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), initial, cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());

        // Equal policy (`processed` -> `processed`), then looser
        // (`processed` -> `processed-and-named` -> `keep`).
        choose(vcx, &view, PROCESSED, RetentionTrigger::Immediate);
        vcx.run_until_parked();
        choose(vcx, &view, PROCESSED_AND_NAMED, RetentionTrigger::Named);
        vcx.run_until_parked();
        assert_eq!(saved_retention(&data_dir), PROCESSED_AND_NAMED);
        choose(vcx, &view, Retention::KeepForever, RetentionTrigger::Keep);
        vcx.run_until_parked();

        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
        assert_eq!(
            dry_runs(dir.path()),
            0,
            "looser/equal policy must not run a dry run: {}",
            cli_log(dir.path())
        );
        assert!(!has_dialog(vcx));
    }

    /// WRT-04, WRT-08: a choice whose save fails says so in words, on the
    /// retention group's own line (a Failed `Alert` beside the choice, as a
    /// failed check is), never beside the page's title: an unreadable
    /// `config.toml` (a folder in its place) and a malformed one whose
    /// parser error quotes a private value. Neither the file's absolute path
    /// nor the parser's words reach the line, and nothing is written.
    #[gpui::test]
    fn retention_save_failure_is_said_in_words(cx: &mut TestAppContext) {
        for malformed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut initial = Config::default();
            initial.retention = PROCESSED;
            let cli = retention_cli(dir.path(), &dry_run_reply(deletes(0, 0), 0), false);
            let (state, view, vcx) = settings_root_window_with(cx, dir.path(), initial, cli);
            let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());
            let config = Config::path(&data_dir);
            let cause = if malformed {
                std::fs::write(
                    &config,
                    "[user]\nemails = [\"zq7-private@example.invalid\"\nretention = \"\n",
                )
                .unwrap();
                crate::jobs::SETTINGS_FILE_UNREADABLE
            } else {
                std::fs::remove_file(&config).unwrap();
                std::fs::create_dir(&config).unwrap();
                match std::fs::read_to_string(&config).unwrap_err().kind() {
                    std::io::ErrorKind::PermissionDenied => "BroScribe isn't allowed to open it.",
                    std::io::ErrorKind::IsADirectory => "BroScribe couldn't read or write a file.",
                    kind => panic!("unexpected error reading a settings directory: {kind:?}"),
                }
            };

            choose(vcx, &view, Retention::KeepForever, RetentionTrigger::Keep);
            vcx.run_until_parked();

            let line = format!("Couldn't save the retention choice. {cause}");
            assert_eq!(
                view.read_with(vcx, |view, _| view.retention.status().cloned()),
                Some(RetentionStatus::Failed(line.clone()))
            );
            let spoken = spoken_now(vcx);
            assert!(
                spoken
                    .iter()
                    .any(|(id, role, text)| id == "storage-retention-status"
                        && *role == Role::Alert
                        && *text == line),
                "the failure is the retention group's own alert: {spoken:?}"
            );
            assert!(
                !spoken.iter().any(|(id, ..)| id == "settings-notice"),
                "nothing beside the title: {spoken:?}"
            );
            let data = data_dir.to_string_lossy().into_owned();
            for (_, _, text) in &spoken {
                for raw in [
                    data.as_str(),
                    "zq7-private",
                    "TOML",
                    "line 2",
                    "config.toml:",
                ] {
                    assert!(!text.contains(raw), "{raw} rendered: {text}");
                }
            }
            if malformed {
                assert!(
                    std::fs::read_to_string(&config)
                        .unwrap()
                        .contains("zq7-private"),
                    "the file is left as it was"
                );
            }
        }
    }

    /// WRT-04: any settings change ([`update_config`], here the reading
    /// size) over a malformed `config.toml` says so in words: neither the
    /// file's absolute path nor its parser's excerpt (a private address)
    /// reaches the line, and the file is left as it was.
    #[gpui::test]
    fn a_settings_change_over_a_malformed_config_shows_no_path(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = retention_cli(dir.path(), &dry_run_reply(deletes(0, 0), 0), false);
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());
        let config = Config::path(&data_dir);
        let malformed = "[user]\nemails = [\"zq7-private@example.invalid\"\n";
        std::fs::write(&config, malformed).unwrap();

        view.update(vcx, |view, cx| {
            view.set_reading_size(ReadingSize::Large, cx)
        });
        vcx.run_until_parked();

        let line = format!(
            "Couldn't save the settings. {}",
            crate::jobs::SETTINGS_FILE_UNREADABLE
        );
        let spoken = spoken_now(vcx);
        assert!(
            spoken
                .iter()
                .any(|(id, role, text)| id == "status-reading-size"
                    && *role == Role::Alert
                    && *text == line),
            "{spoken:?}"
        );
        let data = data_dir.to_string_lossy().into_owned();
        for (_, _, text) in &spoken {
            for raw in [
                data.as_str(),
                "zq7-private",
                "TOML",
                "line 2",
                "config.toml:",
            ] {
                assert!(!text.contains(raw), "{raw} rendered: {text}");
            }
        }
        assert_eq!(std::fs::read_to_string(&config).unwrap(), malformed);
    }

    #[gpui::test]
    fn retention_stricter_policy_stays_pending_then_saves_when_nothing_goes(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let cli = retention_cli(dir.path(), &dry_run_reply(deletes(0, 0), 0), false);
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());

        // Immediately after clicking a stricter option, nothing is saved yet,
        // the saved radio stays selected and the Pending status line renders
        // under the retention group.
        vcx.update(|window, cx| {
            view.update(cx, |this, cx| {
                this.set_retention(PROCESSED, RetentionTrigger::Immediate, window, cx);
                assert_eq!(this.retention.status(), Some(&RetentionStatus::Pending));
            });
        });
        assert_eq!(
            saved_retention(&data_dir),
            Retention::KeepForever,
            "config must stay untouched while dry run is pending"
        );
        assert_eq!(radio_checked(vcx, "retention-keep"), Some(true));
        assert_eq!(radio_checked(vcx, "retention-immediate"), Some(false));
        let pending_spoken = spoken_now(vcx);
        assert!(
            pending_spoken.iter().any(|(id, role, text)| {
                id == "storage-retention-status"
                    && *role == Role::Status
                    && text == "Checking which recordings this deletes…"
            }),
            "expected Pending status line: {pending_spoken:?}"
        );

        // Once the dry run finds nothing to delete, it saves at once with no
        // dialog.
        vcx.run_until_parked();
        assert_eq!(saved_retention(&data_dir), PROCESSED);
        assert!(!has_dialog(vcx));
        assert_eq!(radio_checked(vcx, "retention-immediate"), Some(true));
        let log = cli_log(dir.path());
        assert!(
            log.contains("--json") && log.contains("retention apply --dry-run --policy processed"),
            "{log}"
        );
    }

    /// Opens Settings on a synthetic CLI answering every dry run with
    /// `reply` (exit 1 when `fail`), chooses `processed-and-named` and
    /// returns what the retention status line says once the answer is in.
    fn failed_check_line(cx: &mut TestAppContext, dir: &Path, reply: &str, fail: bool) -> String {
        let cli = retention_cli(dir, reply, fail);
        let (state, view, vcx) = settings_root_window_with(cx, dir, Config::default(), cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());
        choose(vcx, &view, PROCESSED_AND_NAMED, RetentionTrigger::Named);
        vcx.run_until_parked();

        assert_eq!(dry_runs(dir), 1);
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
        assert!(!has_dialog(vcx));
        let spoken = spoken_now(vcx);
        let lines: Vec<_> = spoken
            .iter()
            .filter(|(id, role, _)| id == "storage-retention-status" && *role == Role::Alert)
            .map(|(_, _, text)| text.clone())
            .collect();
        assert_eq!(lines.len(), 1, "expected one Failed Alert line: {spoken:?}");
        lines[0].clone()
    }

    #[gpui::test]
    fn retention_dry_run_failure_shows_the_cli_cause_and_changes_nothing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            failed_check_line(
                cx,
                dir.path(),
                r#"{"error":{"code":"io","message":"Library folder is unreadable"}}"#,
                true
            ),
            "Couldn't check which recordings this deletes. Library folder is unreadable. \
             Nothing changed."
        );
    }

    #[gpui::test]
    fn retention_dry_run_failure_never_shows_a_path_or_an_id(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let line = failed_check_line(
            cx,
            dir.path(),
            r#"{"error":{"code":"schema","message":"/synthetic/library/people/p-c3c3c3/voiceprints.json: expected value"}}"#,
            true,
        );
        assert_eq!(
            line,
            "Couldn't check which recordings this deletes. A file in the library folder is \
             damaged. Nothing changed."
        );
        assert!(!line.contains("p-c3c3c3") && !line.contains('/'), "{line}");
    }

    #[gpui::test]
    fn retention_check_with_unreadable_metadata_fails(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            failed_check_line(cx, dir.path(), &dry_run_reply(deletes(0, 0), 1), false),
            "Couldn't check which recordings this deletes. 1 meeting couldn't be read. \
             Nothing changed."
        );
    }

    #[gpui::test]
    fn retention_check_answering_another_policy_fails(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            failed_check_line(
                cx,
                dir.path(),
                &dry_run_reply(deletes(0, 0), 0).replace("@POLICY@", "keep"),
                false
            ),
            "Couldn't check which recordings this deletes. The check gave an unexpected \
             answer. Nothing changed."
        );
    }

    #[gpui::test]
    fn retention_check_with_a_legacy_answer_fails(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            failed_check_line(cx, dir.path(), r#"{"dry_run":true,"deleted":[]}"#, false),
            "Couldn't check which recordings this deletes. The check gave an unexpected \
             answer. Nothing changed."
        );
    }

    #[gpui::test]
    fn retention_voice_reading_alone_asks_first_whatever_the_policy(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let reply = dry_run_reply(
            RetentionPreview {
                voice_readings: 1,
                ..RetentionPreview::default()
            },
            0,
        );
        let cli = retention_cli(dir.path(), &reply, false);
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());

        for (policy, trigger) in [
            (PROCESSED_AND_NAMED, RetentionTrigger::Named),
            (
                Retention::DeleteAfterDays { days: 30 },
                RetentionTrigger::Days,
            ),
        ] {
            choose(vcx, &view, policy, trigger);
            vcx.run_until_parked();
            assert!(has_dialog(vcx), "{policy:?}: the voice reading alone asks");
            assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
            let spoken = spoken_now(vcx);
            let said = |role: Role, needle: &str| {
                spoken
                    .iter()
                    .any(|(_, r, text)| *r == role && text == needle)
            };
            assert!(
                said(Role::Heading, "Delete your kept voice reading now?"),
                "{policy:?}: {spoken:?}"
            );
            assert!(
                said(Role::Label, "Your kept voice reading"),
                "{policy:?}: {spoken:?}"
            );
            assert!(
                !spoken
                    .iter()
                    .any(|(_, _, text)| text.starts_with("The recordings of")),
                "{policy:?}: no meeting goes: {spoken:?}"
            );
            assert_eq!(
                button_label(vcx, "retention-confirm-delete").as_deref(),
                Some("Delete voice reading"),
                "{policy:?}"
            );
            click_button(vcx, "retention-confirm-cancel");
            assert!(!has_dialog(vcx));
            assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
        }

        // Confirming saves.
        choose(vcx, &view, PROCESSED_AND_NAMED, RetentionTrigger::Named);
        vcx.run_until_parked();
        click_button(vcx, "retention-confirm-delete");
        assert!(!has_dialog(vcx));
        assert_eq!(saved_retention(&data_dir), PROCESSED_AND_NAMED);
    }

    #[gpui::test]
    fn retention_recordings_held_for_a_job_ask_first(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let reply = dry_run_reply(
            RetentionPreview {
                recordings: 2,
                bytes: 2_000_000,
                held: 1,
                held_bytes: 1_000_000,
                voice_readings: 0,
            },
            0,
        );
        let cli = retention_cli(dir.path(), &reply, false);
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());

        choose(vcx, &view, PROCESSED, RetentionTrigger::Immediate);
        vcx.run_until_parked();
        assert!(has_dialog(vcx));
        let spoken = spoken_now(vcx);
        let said = |role: Role, needle: &str| {
            spoken
                .iter()
                .any(|(_, r, text)| *r == role && text == needle)
        };
        assert!(
            said(Role::Heading, "Delete 3 recordings now?"),
            "{spoken:?}"
        );
        assert_eq!(
            button_label(vcx, "retention-confirm-delete").as_deref(),
            Some("Delete 3 recordings")
        );
        assert!(
            said(
                Role::Label,
                "The recordings of 3 meetings already in your library (3.0 MB)"
            ),
            "{spoken:?}"
        );
        assert!(
            said(Role::Label, "1 of them goes when its processing finishes"),
            "{spoken:?}"
        );
        assert!(
            !spoken
                .iter()
                .any(|(_, _, text)| text == "Your kept voice reading"),
            "no voice reading goes: {spoken:?}"
        );
        click_button(vcx, "retention-confirm-cancel");
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
    }

    #[gpui::test]
    fn retention_confirmation_dialog_content_order_cancel_escape_secondary_dot_and_confirm(
        cx: &mut TestAppContext,
    ) {
        use broscribe_platform::DialogButtonOrder;
        use broscribe_platform::fakes::FakeDialogButtonOrder;

        let dir = tempfile::tempdir().unwrap();
        let reply = dry_run_reply(
            RetentionPreview {
                recordings: 3,
                bytes: 3_100_000_000,
                voice_readings: 1,
                ..RetentionPreview::default()
            },
            0,
        );
        let cli = retention_cli(dir.path(), &reply, false);
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());

        // 1. `processed` (Until transcribed) opens the AlertDialog.
        let trigger_immediate = view.read_with(vcx, |this, _| {
            this.retention
                .focus_for(RetentionTrigger::Immediate)
                .clone()
        });
        choose(vcx, &view, PROCESSED, RetentionTrigger::Immediate);
        vcx.run_until_parked();
        assert!(has_dialog(vcx));
        assert_eq!(
            saved_retention(&data_dir),
            Retention::KeepForever,
            "config must stay untouched before confirmation"
        );

        vcx.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            let dialogs: Vec<_> = tree
                .nodes
                .iter()
                .filter(|node| node.role == "AlertDialog")
                .collect();
            let [dialog] = dialogs.as_slice() else {
                panic!("one retention alert: {dialogs:?}")
            };
            assert_eq!(dialog.name, "Delete 3 recordings now?");
            for line in [
                "Later recordings, once they qualify",
                "Your kept voice reading",
                "Files left in place at import",
            ] {
                assert!(dialog.description.contains(line));
            }
            assert_eq!(
                tree.read(tree.find("retention-confirm-goes-0")),
                (
                    "Label",
                    "The recordings of 3 meetings already in your library (3.1 GB)"
                )
            );
        });

        let spoken = {
            let _order = FakeDialogButtonOrder::new(DialogButtonOrder::ActionFirst);
            spoken_now(vcx)
        };
        let has_spoken = |role: Role, needle: &str| {
            spoken
                .iter()
                .any(|(_, r, text)| *r == role && text == needle)
        };
        assert!(
            has_spoken(Role::Heading, "Delete 3 recordings now?"),
            "{spoken:?}"
        );
        assert!(
            has_spoken(
                Role::Alert,
                "Without the audio you can't replay these meetings or transcribe them again with another model."
            ),
            "{spoken:?}"
        );
        assert!(has_spoken(Role::Heading, "What goes"), "{spoken:?}");
        assert!(
            has_spoken(
                Role::Label,
                "The recordings of 3 meetings already in your library (3.1 GB)"
            ),
            "{spoken:?}"
        );
        assert!(
            has_spoken(Role::Label, "Later recordings, once they qualify"),
            "{spoken:?}"
        );
        assert!(
            has_spoken(Role::Label, "Your kept voice reading"),
            "{spoken:?}"
        );
        assert!(has_spoken(Role::Heading, "What stays"), "{spoken:?}");
        assert!(
            has_spoken(Role::Label, "Transcripts, summaries, names and voice data"),
            "{spoken:?}"
        );
        assert!(
            has_spoken(Role::Label, "Files left in place at import"),
            "{spoken:?}"
        );
        assert!(has_spoken(Role::Label, &ssd_caveat()), "{spoken:?}");
        assert!(
            has_spoken(Role::Label, "This can't be undone."),
            "{spoken:?}"
        );

        // ActionFirst places delete before Cancel, and Cancel holds focus on open.
        let (delete_x, cancel_x, cancel_focused) = vcx.update(|window, _| {
            let snapshots = gpui::base::test_support::snapshots(window);
            let find = |id: &str| {
                snapshots
                    .iter()
                    .find(|n| {
                        n.role() == Some(Role::Button)
                            && n.path().last().is_some_and(|l| l.to_string() == id)
                    })
                    .unwrap_or_else(|| panic!("missing {id}"))
            };
            let delete = find("retention-confirm-delete");
            let cancel = find("retention-confirm-cancel");
            (
                delete.bounds().origin.x,
                cancel.bounds().origin.x,
                cancel.focused(),
            )
        });
        assert!(
            delete_x < cancel_x,
            "ActionFirst must place Delete before Cancel: delete_x={delete_x:?} cancel_x={cancel_x:?}"
        );
        assert_eq!(
            cancel_focused,
            Some(true),
            "Cancel must be focused when the dialog opens"
        );

        // Pressing Enter does NOT confirm deletion (no Return default).
        vcx.simulate_keystrokes("enter");
        vcx.run_until_parked();
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);

        // Pressing Escape cancels without saving and returns focus to the
        // clicked radio; the dialog is still open until then.
        draw_twice(vcx);
        assert!(has_dialog(vcx), "Enter must leave the dialog open");
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        draw_twice(vcx);
        assert!(!has_dialog(vcx));
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
        assert!(
            vcx.update(|window, _| trigger_immediate.is_focused(window)),
            "Escape must return focus to the clicked retention radio"
        );

        // 2. Re-open for `days:30`: the voice reading the check counted is
        // listed there too, and ⌘. (Ctrl+. on Windows and Linux) cancels.
        let trigger_days = view.read_with(vcx, |this, _| {
            this.retention.focus_for(RetentionTrigger::Days).clone()
        });
        choose(
            vcx,
            &view,
            Retention::DeleteAfterDays { days: 30 },
            RetentionTrigger::Days,
        );
        vcx.run_until_parked();
        let spoken_days = {
            let _order = FakeDialogButtonOrder::new(DialogButtonOrder::CancelFirst);
            spoken_now(vcx)
        };
        assert!(
            spoken_days
                .iter()
                .any(|(_, _, text)| text == "Your kept voice reading"),
            "days policy lists the voice reading the check counted: {spoken_days:?}"
        );
        let (delete_x, cancel_x) = vcx.update(|window, _| {
            let snapshots = gpui::base::test_support::snapshots(window);
            let find_x = |id: &str| {
                snapshots
                    .iter()
                    .find(|n| {
                        n.role() == Some(Role::Button)
                            && n.path().last().is_some_and(|l| l.to_string() == id)
                    })
                    .unwrap_or_else(|| panic!("missing {id}"))
                    .bounds()
                    .origin
                    .x
            };
            (
                find_x("retention-confirm-delete"),
                find_x("retention-confirm-cancel"),
            )
        });
        assert!(
            cancel_x < delete_x,
            "CancelFirst must place Cancel before Delete: cancel_x={cancel_x:?} delete_x={delete_x:?}"
        );
        vcx.simulate_keystrokes("secondary-.");
        vcx.run_until_parked();
        draw_twice(vcx);
        assert!(!has_dialog(vcx));
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
        assert!(
            vcx.update(|window, _| trigger_days.is_focused(window)),
            "⌘. must return focus to the clicked retention radio"
        );

        // 3. Re-open and click the danger confirm button: saves ONLY after confirm.
        choose(vcx, &view, PROCESSED, RetentionTrigger::Immediate);
        vcx.run_until_parked();
        click_button(vcx, "retention-confirm-delete");
        assert!(!has_dialog(vcx));
        assert_eq!(saved_retention(&data_dir), PROCESSED);
    }

    /// ⌘. belongs to the retention confirmation only (U03): any other
    /// dialog still closes with Escape alone until U18.
    #[gpui::test]
    fn secondary_dot_leaves_other_dialogs_open(cx: &mut TestAppContext) {
        use gpui::component::WindowExt as _;

        let dir = tempfile::tempdir().unwrap();
        let cli = retention_cli(dir.path(), "{}", false);
        let (_, _, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        let focus = vcx.update(|_, cx| cx.focus_handle());
        let anchor = focus.clone();
        vcx.update(|window, cx| {
            window.open_alert_dialog(cx, move |alert, _, _| {
                alert.keyboard(true).title("Synthetic dialog").footer(
                    div()
                        .id("synthetic-footer")
                        .track_focus(&anchor)
                        .child("Synthetic"),
                )
            });
        });
        draw_twice(vcx);
        vcx.update(|window, cx| focus.focus(window, cx));
        draw_twice(vcx);
        vcx.simulate_keystrokes("secondary-.");
        vcx.run_until_parked();
        assert!(has_dialog(vcx), "⌘. must not close another dialog");
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(!has_dialog(vcx));
    }

    #[gpui::test]
    fn retention_singular_recording_copy_and_stale_dry_run_guard(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = retention_cli(dir.path(), &dry_run_reply(deletes(1, 512_000), 0), false);
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());

        // Start a stricter dry run, then immediately supersede with KeepForever:
        // the stale dry run must not open a dialog.
        vcx.update(|window, cx| {
            view.update(cx, |this, cx| {
                this.set_retention(PROCESSED, RetentionTrigger::Immediate, window, cx);
                this.set_retention(Retention::KeepForever, RetentionTrigger::Keep, window, cx);
            });
        });
        vcx.run_until_parked();
        assert!(!has_dialog(vcx));
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);

        // Singular wording check ("1 recording", "1 meeting", "Delete 1 recording").
        choose(vcx, &view, PROCESSED, RetentionTrigger::Immediate);
        vcx.run_until_parked();
        let spoken = spoken_now(vcx);
        assert!(
            spoken
                .iter()
                .any(|(_, role, text)| *role == Role::Heading && text == "Delete 1 recording now?"),
            "{spoken:?}"
        );
        assert!(
            spoken.iter().any(|(_, role, text)| {
                *role == Role::Label
                    && text == "The recordings of 1 meeting already in your library (512 KB)"
            }),
            "{spoken:?}"
        );
        click_button(vcx, "retention-confirm-cancel");
        assert!(!has_dialog(vcx));
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
    }

    #[gpui::test]
    fn retention_policy_changed_elsewhere_is_never_overwritten_unchecked(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = retention_cli(dir.path(), &dry_run_reply(deletes(2, 18), 0), false);
        let mut initial = Config::default();
        initial.retention = PROCESSED;
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), initial, cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());

        // A terminal (or another window) saves `keep`; this view still shows
        // `processed`, so `processed-and-named` looks looser here.
        edit_config(&data_dir, |c| c.retention = Retention::KeepForever).unwrap();
        assert_eq!(state.read_with(vcx, |s, _| s.config().retention), PROCESSED);
        choose(vcx, &view, PROCESSED_AND_NAMED, RetentionTrigger::Named);
        vcx.run_until_parked();

        // Nothing was written over `keep`: the view took the saved policy,
        // said so and checked the choice against it, which asks first.
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
        assert_eq!(
            state.read_with(vcx, |s, _| s.config().retention),
            Retention::KeepForever
        );
        assert!(has_dialog(vcx));
        assert!(
            cli_log(dir.path()).contains("retention apply --dry-run --policy processed-and-named")
        );
        let spoken = spoken_now(vcx);
        assert!(
            spoken.iter().any(|(id, _, text)| {
                id == "storage-retention-changed" && text == RETENTION_CHANGED_ELSEWHERE
            }),
            "{spoken:?}"
        );

        // The policy changes again while the confirmation is open: Delete
        // writes nothing and the choice is checked once more.
        edit_config(&data_dir, |c| {
            c.retention = Retention::DeleteAfterDays { days: 7 }
        })
        .unwrap();
        click_button(vcx, "retention-confirm-delete");
        assert_eq!(
            saved_retention(&data_dir),
            Retention::DeleteAfterDays { days: 7 }
        );
        assert!(has_dialog(vcx), "checked again against `days:7`, it asks");
        assert_eq!(dry_runs(dir.path()), 2);
        click_button(vcx, "retention-confirm-delete");
        assert!(!has_dialog(vcx));
        assert_eq!(saved_retention(&data_dir), PROCESSED_AND_NAMED);
    }

    #[gpui::test]
    fn retention_library_changed_elsewhere_saves_nothing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = retention_cli(dir.path(), &dry_run_reply(deletes(2, 18), 0), false);
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());

        choose(vcx, &view, PROCESSED, RetentionTrigger::Immediate);
        vcx.run_until_parked();
        assert!(has_dialog(vcx));
        // Another library is saved while the confirmation, which counted the
        // open one, is still on screen.
        let other = dir.path().join("other-lib");
        edit_config(&data_dir, |c| c.library = Some(other.clone())).unwrap();
        click_button(vcx, "retention-confirm-delete");

        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
        assert!(!has_dialog(vcx));
        assert_eq!(
            state.read_with(vcx, |s, _| s.settings_library_mismatch()),
            Some(OtherLibrary::SwitchPending)
        );
        assert_eq!(dry_runs(dir.path()), 1, "no check runs on the old library");
        let spoken = spoken_now(vcx);
        assert!(
            spoken.iter().any(|(id, role, text)| {
                id == "storage-retention-status" && *role == Role::Alert && text == SWITCH_PENDING
            }),
            "{spoken:?}"
        );
    }

    #[gpui::test]
    fn retention_deferred_library_switch_voids_the_check(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = retention_cli(dir.path(), &dry_run_reply(deletes(2, 18), 0), false);
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());
        // A running worker defers the switch.
        state.update(vcx, |s, _| s.set_worker_state(WorkerState::Running));

        let other = dir.path().join("other-lib");
        let switched = edit_config(&data_dir, |c| c.library = Some(other.clone())).unwrap();
        vcx.update(|window, cx| {
            view.update(cx, |this, cx| {
                this.set_retention(PROCESSED, RetentionTrigger::Immediate, window, cx);
                // The library choice is saved while the check runs.
                this.library_chosen(switched, other.clone(), cx);
                assert_eq!(this.library_switch, LibrarySwitch::Waiting(other.clone()));
                assert_eq!(this.retention.status(), None);
            });
        });
        vcx.run_until_parked();

        // The answer about the old library opens nothing and saves nothing.
        assert!(!has_dialog(vcx));
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
        assert_eq!(dry_runs(dir.path()), 1);

        // While the switch waits, a stricter choice isn't checked on the old
        // library at all.
        choose(vcx, &view, PROCESSED, RetentionTrigger::Immediate);
        vcx.run_until_parked();
        assert!(!has_dialog(vcx));
        assert_eq!(dry_runs(dir.path()), 1);
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
        let status = view.read_with(vcx, |this, _| this.retention.status().cloned());
        assert_eq!(status, Some(RetentionStatus::Failed(SWITCH_PENDING.into())));
    }

    #[gpui::test]
    fn opening_setup_voids_a_pending_settings_check(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = retention_cli(dir.path(), &dry_run_reply(deletes(2, 18), 0), false);
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());

        // Setup opens before the Settings check answers.
        vcx.update(|window, cx| {
            view.update(cx, |this, cx| {
                this.set_retention(PROCESSED, RetentionTrigger::Immediate, window, cx);
                this.open_setup(window, cx);
                assert_eq!(this.retention.status(), None);
            });
        });
        vcx.run_until_parked();
        assert_eq!(dry_runs(dir.path()), 1);
        assert!(!has_dialog(vcx), "the late answer opens no dialog");
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
    }

    #[gpui::test]
    fn retention_delete_in_an_outlived_confirmation_saves_nothing(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = retention_cli(dir.path(), &dry_run_reply(deletes(2, 18), 0), false);
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());

        choose(vcx, &view, PROCESSED, RetentionTrigger::Immediate);
        vcx.run_until_parked();
        assert!(has_dialog(vcx));
        // The view moved on (another route, Setup) under the dialog.
        view.update(vcx, |this, _| this.retention.invalidate());
        click_button(vcx, "retention-confirm-delete");
        assert!(!has_dialog(vcx));
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
    }

    const SWITCH_PENDING: &str = "Couldn't check which recordings this deletes. BroScribe opens \
        the other library when it restarts. Nothing changed. Restart BroScribe, then choose again.";
    const LAUNCH_OVERRIDE: &str = "Couldn't check which recordings this deletes. BroScribe opened \
        a different library at startup. Nothing changed. Restart BroScribe normally, then choose again.";
    const AGENT_ELSEWHERE: &str = "Couldn't check which recordings this deletes. BroScribe Agent \
        is running on another library. Nothing changed. Restart BroScribe Agent, then choose again.";

    /// Starts the app on `override-lib` while its settings name the default
    /// library (what `--library` does, or `BROSCRIBE_LIBRARY` when
    /// `from_env`), the synthetic check answering `open_reply` for the open
    /// library; a stricter choice from `saved` runs no check, saves nothing
    /// and says what to do.
    fn launch_override_blocks_a_stricter_choice(
        cx: &mut TestAppContext,
        saved: Retention,
        open_reply: &str,
        from_env: bool,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let cli = retention_cli(dir.path(), open_reply, false);
        let mut initial = Config::default();
        initial.retention = saved;
        let (state, view, vcx) = settings_root_window_on(
            cx,
            dir.path(),
            initial,
            cli,
            dir.path().join("override-lib"),
            from_env,
        );
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());
        let (other, line) = if from_env {
            (OtherLibrary::LaunchEnv, LAUNCH_OVERRIDE)
        } else {
            (OtherLibrary::LaunchFlag, LAUNCH_OVERRIDE)
        };
        assert_eq!(
            state.read_with(vcx, |s, _| s.settings_library_mismatch()),
            Some(other)
        );

        choose(vcx, &view, PROCESSED, RetentionTrigger::Immediate);
        vcx.run_until_parked();
        assert_eq!(dry_runs(dir.path()), 0, "{}", cli_log(dir.path()));
        assert!(!has_dialog(vcx));
        assert_eq!(saved_retention(&data_dir), saved);
        let spoken = spoken_now(vcx);
        assert!(
            spoken.iter().any(|(id, role, text)| {
                id == "storage-retention-status" && *role == Role::Alert && text == line
            }),
            "{spoken:?}"
        );

        // A looser choice deletes nothing more anywhere: it still saves.
        choose(vcx, &view, Retention::KeepForever, RetentionTrigger::Keep);
        vcx.run_until_parked();
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
        assert_eq!(dry_runs(dir.path()), 0);
    }

    #[gpui::test]
    fn retention_launch_on_an_empty_library_never_vouches_for_the_configured_one(
        cx: &mut TestAppContext,
    ) {
        // The open library has nothing to delete; the configured one, where
        // the policy applies at the next normal launch, may have plenty.
        launch_override_blocks_a_stricter_choice(
            cx,
            PROCESSED_AND_NAMED,
            &dry_run_reply(deletes(0, 0), 0),
            false,
        );
    }

    #[gpui::test]
    fn retention_launch_on_a_populated_library_never_asks_about_the_wrong_one(
        cx: &mut TestAppContext,
    ) {
        // The open library has recordings to delete; the configured one may
        // have none, or others: a dialog here would count the wrong ones.
        launch_override_blocks_a_stricter_choice(
            cx,
            PROCESSED_AND_NAMED,
            &dry_run_reply(deletes(2, 18), 0),
            false,
        );
    }

    #[gpui::test]
    fn retention_launch_with_the_library_variable_says_so(cx: &mut TestAppContext) {
        launch_override_blocks_a_stricter_choice(
            cx,
            PROCESSED_AND_NAMED,
            &dry_run_reply(deletes(0, 0), 0),
            true,
        );
    }

    /// A transport for an agent whose events the test delivers itself.
    struct SilentAgent;
    impl crate::daemon::Transport for SilentAgent {
        fn subscribe(
            &self,
        ) -> Result<Box<dyn crate::daemon::EventStream>, broscribe_ipc::IpcError> {
            Err(broscribe_ipc::IpcError::new(
                "unused",
                "events are delivered by the test",
            ))
        }
        fn call(
            &self,
            _: broscribe_ipc::Call,
        ) -> Result<serde_json::Value, broscribe_ipc::IpcError> {
            Ok(serde_json::Value::Null)
        }
    }

    /// The idle agent's state, as it reports running on `library`.
    fn agent_state(library: &Path) -> broscribe_ipc::Event {
        let state = serde_json::from_value(serde_json::json!({
            "pid": 123,
            "version": "synthetic",
            "protocol": broscribe_ipc::PROTOCOL_VERSION,
            "library": library.to_string_lossy(),
            "recording": null,
            "jobs": { "running": null, "waiting": 0, "deferred": 0 },
            "preview": { "state": "off" }
        }))
        .unwrap();
        broscribe_ipc::Event::State(Box::new(state))
    }

    /// Attaches a recording agent to `state` and has it report `library`.
    fn agent_on(
        vcx: &mut gpui::VisualTestContext,
        state: &Entity<AppState>,
        library: &Path,
    ) -> Entity<crate::daemon::DaemonClient> {
        let daemon =
            vcx.update(|_, cx| crate::daemon::DaemonClient::detached(Arc::new(SilentAgent), cx));
        state.update(vcx, |s, cx| s.attach_daemon(daemon.clone(), cx));
        let event = agent_state(library);
        daemon.update(vcx, |d, cx| d.received(event, cx));
        daemon
    }

    #[gpui::test]
    fn retention_an_agent_on_another_library_blocks_a_stricter_choice(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        // The open library (and the one Settings names) has nothing to
        // delete: a check there would save `processed` at once, and the
        // agent would apply it to its own library.
        let cli = retention_cli(dir.path(), &dry_run_reply(deletes(0, 0), 0), false);
        let mut initial = Config::default();
        initial.retention = PROCESSED_AND_NAMED;
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), initial, cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());
        let open = state.read_with(vcx, |s, _| s.library().root().to_path_buf());
        let daemon = agent_on(vcx, &state, &dir.path().join("agent-lib"));
        assert_eq!(
            state.read_with(vcx, |s, _| s.settings_library_mismatch()),
            Some(OtherLibrary::AgentElsewhere)
        );

        choose(vcx, &view, PROCESSED, RetentionTrigger::Immediate);
        vcx.run_until_parked();
        assert_eq!(dry_runs(dir.path()), 0, "{}", cli_log(dir.path()));
        assert!(!has_dialog(vcx));
        assert_eq!(saved_retention(&data_dir), PROCESSED_AND_NAMED);
        let spoken = spoken_now(vcx);
        assert!(
            spoken.iter().any(|(id, role, text)| {
                id == "storage-retention-status" && *role == Role::Alert && text == AGENT_ELSEWHERE
            }),
            "{spoken:?}"
        );

        // A looser choice deletes nothing more anywhere: it still saves.
        choose(vcx, &view, Retention::KeepForever, RetentionTrigger::Keep);
        vcx.run_until_parked();
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
        assert_eq!(dry_runs(dir.path()), 0);

        // The agent on the open library: the choice is checked as usual.
        let event = agent_state(&open);
        daemon.update(vcx, |d, cx| d.received(event, cx));
        assert_eq!(
            state.read_with(vcx, |s, _| s.settings_library_mismatch()),
            None
        );
        choose(vcx, &view, PROCESSED, RetentionTrigger::Immediate);
        vcx.run_until_parked();
        assert_eq!(dry_runs(dir.path()), 1, "{}", cli_log(dir.path()));
        assert_eq!(saved_retention(&data_dir), PROCESSED);

        // The agent moves to another library again, then disconnects: the
        // next one opens the library in Settings, so a stricter choice is
        // checked on the open one and saved.
        let event = agent_state(&dir.path().join("agent-lib"));
        daemon.update(vcx, |d, cx| d.received(event, cx));
        assert_eq!(
            state.read_with(vcx, |s, _| s.settings_library_mismatch()),
            Some(OtherLibrary::AgentElsewhere)
        );
        daemon.update(vcx, |d, cx| d.lost(None, cx));
        assert_eq!(
            state.read_with(vcx, |s, _| s.settings_library_mismatch()),
            None
        );
        let days = Retention::DeleteAfterDays { days: 30 };
        assert!(days.may_delete_more_than(PROCESSED));
        choose(vcx, &view, days, RetentionTrigger::DaysSave);
        vcx.run_until_parked();
        assert_eq!(dry_runs(dir.path()), 2, "{}", cli_log(dir.path()));
        assert!(cli_log(dir.path()).contains("--policy days:30"));
        assert_eq!(saved_retention(&data_dir), days);
    }

    #[gpui::test]
    fn retention_an_agent_reconnecting_on_another_library_voids_the_confirmation(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let cli = retention_cli(dir.path(), &dry_run_reply(deletes(2, 18), 0), false);
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());
        let open = state.read_with(vcx, |s, _| s.library().root().to_path_buf());
        let daemon = agent_on(vcx, &state, &open);

        choose(vcx, &view, PROCESSED, RetentionTrigger::Immediate);
        vcx.run_until_parked();
        assert!(has_dialog(vcx));
        // The agent restarts on another library while the confirmation,
        // which counted the open one, is on screen.
        daemon.update(vcx, |d, cx| d.lost(None, cx));
        let event = agent_state(&dir.path().join("agent-lib"));
        daemon.update(vcx, |d, cx| d.received(event, cx));
        click_button(vcx, "retention-confirm-delete");

        assert!(!has_dialog(vcx));
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
        assert_eq!(dry_runs(dir.path()), 1);
        let status = view.read_with(vcx, |this, _| this.retention.status().cloned());
        assert_eq!(
            status,
            Some(RetentionStatus::Failed(AGENT_ELSEWHERE.into()))
        );
    }

    #[gpui::test]
    fn retention_an_agent_reconnecting_on_another_library_during_the_write_saves_nothing(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let cli = retention_cli(dir.path(), &dry_run_reply(deletes(2, 18), 0), false);
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());
        let open = state.read_with(vcx, |s, _| s.library().root().to_path_buf());
        let daemon = agent_on(vcx, &state, &open);

        choose(vcx, &view, PROCESSED, RetentionTrigger::Immediate);
        vcx.run_until_parked();
        assert!(has_dialog(vcx));
        // Delete is confirmed while the agent still runs on the open
        // library; the write waits before it takes the config lock.
        let (release, hold) = futures::channel::oneshot::channel();
        HOLD_NEXT_RETENTION_WRITE.with(|next| *next.borrow_mut() = Some(hold));
        click_button(vcx, "retention-confirm-delete");
        vcx.run_until_parked();
        assert!(!has_dialog(vcx));
        assert!(
            HOLD_NEXT_RETENTION_WRITE.with(|next| next.borrow().is_none()),
            "the confirmed save is the one held"
        );
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);

        // Meanwhile the agent restarts on another library.
        daemon.update(vcx, |d, cx| d.lost(None, cx));
        let event = agent_state(&dir.path().join("agent-lib"));
        daemon.update(vcx, |d, cx| d.received(event, cx));
        release.send(()).unwrap();
        vcx.run_until_parked();

        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
        assert_eq!(
            state.read_with(vcx, |s, _| s.config().retention),
            Retention::KeepForever
        );
        assert_eq!(radio_checked(vcx, "retention-keep"), Some(true));
        assert_eq!(dry_runs(dir.path()), 1);
        assert_eq!(
            view.read_with(vcx, |this, _| this.retention.status().cloned()),
            Some(RetentionStatus::Failed(AGENT_ELSEWHERE.into()))
        );
        let spoken = spoken_now(vcx);
        assert!(
            spoken.iter().any(|(id, role, text)| {
                id == "storage-retention-status" && *role == Role::Alert && text == AGENT_ELSEWHERE
            }),
            "{spoken:?}"
        );
    }

    #[gpui::test]
    fn retention_the_default_library_named_explicitly_is_the_open_one(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = retention_cli(dir.path(), &dry_run_reply(deletes(0, 0), 0), false);
        // Started on the default library with no `library` setting.
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());

        // The setting now names the same folder explicitly (as setup saves
        // it): no switch is pending.
        let default = FakePaths::new(dir.path()).default_library_dir();
        let named = edit_config(&data_dir, |c| c.library = Some(default.clone())).unwrap();
        state.update(vcx, |s, cx| s.set_config(named, cx));
        assert_eq!(
            state.read_with(vcx, |s, _| s.settings_library_mismatch()),
            None
        );
        choose(vcx, &view, PROCESSED, RetentionTrigger::Immediate);
        vcx.run_until_parked();
        assert_eq!(dry_runs(dir.path()), 1, "{}", cli_log(dir.path()));
        assert_eq!(saved_retention(&data_dir), PROCESSED);

        // Started with that explicit setting, the same holds.
        let dir = tempfile::tempdir().unwrap();
        let cli = retention_cli(dir.path(), &dry_run_reply(deletes(0, 0), 0), false);
        let mut initial = Config::default();
        initial.library = Some(FakePaths::new(dir.path()).default_library_dir());
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), initial, cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());
        assert_eq!(
            state.read_with(vcx, |s, _| s.settings_library_mismatch()),
            None
        );
        choose(vcx, &view, PROCESSED, RetentionTrigger::Immediate);
        vcx.run_until_parked();
        assert_eq!(dry_runs(dir.path()), 1, "{}", cli_log(dir.path()));
        assert_eq!(saved_retention(&data_dir), PROCESSED);
    }

    #[gpui::test]
    fn retention_check_with_an_unreadable_profile_fails(cx: &mut TestAppContext) {
        // What the CLI answers for a damaged `person.md` beside a kept
        // reading and valid voiceprints: the reading isn't counted, and the
        // profile is.
        let dir = tempfile::tempdir().unwrap();
        let reply = dry_run_reply(deletes(0, 0), 0)
            .replace(r#""unreadable_people":0"#, r#""unreadable_people":1"#);
        let line = failed_check_line(cx, dir.path(), &reply, false);
        assert_eq!(
            line,
            "Couldn't check which recordings this deletes. 1 person's profile couldn't be read. \
             Nothing changed."
        );
        assert!(!line.contains('/') && !line.contains("p-"), "{line}");
    }

    #[gpui::test]
    fn retention_a_newer_choice_wins_over_an_older_save_still_waiting(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = retention_cli(dir.path(), &dry_run_reply(deletes(0, 0), 0), false);
        let (state, view, vcx) = settings_root_window_with(cx, dir.path(), Config::default(), cli);
        let data_dir = state.read_with(vcx, |s, _| s.paths().data_dir.clone());

        // `processed` finds nothing to delete, so it saves at once; its write
        // is held back before it takes the config lock.
        let (release, hold) = futures::channel::oneshot::channel();
        HOLD_NEXT_RETENTION_WRITE.with(|next| *next.borrow_mut() = Some(hold));
        choose(vcx, &view, PROCESSED, RetentionTrigger::Immediate);
        vcx.run_until_parked();
        assert_eq!(dry_runs(dir.path()), 1);
        assert!(
            HOLD_NEXT_RETENTION_WRITE.with(|next| next.borrow().is_none()),
            "the `processed` save is the one held"
        );
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);

        // Forever, still the saved policy, is chosen and its save finishes.
        choose(vcx, &view, Retention::KeepForever, RetentionTrigger::Keep);
        vcx.run_until_parked();
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);

        // Then the older save goes on: it writes nothing and says nothing.
        release.send(()).unwrap();
        vcx.run_until_parked();
        assert_eq!(saved_retention(&data_dir), Retention::KeepForever);
        assert_eq!(
            state.read_with(vcx, |s, _| s.config().retention),
            Retention::KeepForever
        );
        assert_eq!(radio_checked(vcx, "retention-keep"), Some(true));
        assert!(!view.read_with(vcx, |this, _| this.retention.changed_elsewhere()));
        assert_eq!(
            view.read_with(vcx, |this, _| this.retention.status().cloned()),
            None
        );
        assert!(!has_dialog(vcx));
    }
}
