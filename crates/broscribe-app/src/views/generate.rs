//! Generate / Regenerate inside a meeting (design §4 meeting view, §8.4):
//! the Overview hosts one [`GeneratePanel`] for the summary;
//! the Templates tab has one per template
//! ([`GenerationTarget`]).
//!
//! - Providers come from `broscribe providers list --json`, filtered to LLM
//!   providers that are enabled and consented; consent is never given here
//!   (with none, a button opens Settings → AI providers).
//! - The model picker offers "Configured model (X)" (the provider's `model`
//!   in config) or, for CLI providers without one, "Provider default"; then
//!   the list `broscribe providers models <id> --json` returned, fetched when
//!   the provider is chosen and cached in [`AppState`] for the app session,
//!   keyed by the provider and its config (consent, endpoint, model), so a
//!   config change refetches; Refresh drops the entry. Choosing another
//!   provider invalidates a list still being fetched for the previous one,
//!   so its late answer never changes the new choice. "Other…" takes free
//!   text checked with [`validate_model_name`]. Options show what an alias
//!   or the provider default resolved to last time (`resolved` and
//!   `default_resolved` in the list, read from the index); after a finished
//!   generation the list is fetched again, keeping the old options shown
//!   until the new one arrives.
//! - Generate hands the run to the app ([`AppState::submit_generation`]),
//!   which reserves it, reads the target's current result from the meeting
//!   file (the run's baseline, returned in its receipt) and runs `broscribe
//!   --json enrich <m> --queue --provider <id> [--model <m>]
//!   [--instructions-stdin]` (a template: `templates run <m> <template> …`)
//!   on the background executor, one at a time; per-run instructions go on
//!   stdin, never in argv. The app's [`crate::generation::GenerationWatcher`]
//!   follows exactly that run (and notifies
//!   when it ends if the user is elsewhere), and the panel follows it through
//!   the app's job events, reading the job from the queue when they move:
//!   "Queued · N jobs ahead", then "Generating with <provider> · <model> ·
//!   m:ss" counted from the job's `updated_at`, repainted by a 1 Hz timer that
//!   runs only while the job is queued or running. A job started elsewhere
//!   (another panel before the view was reopened, the CLI, auto-summaries) is
//!   followed the same way.
//! - A followed run is bound to its own follow token and baseline, not to
//!   the job ID (one per meeting and target, reused by every run): a read
//!   started for an earlier run is dropped, and a result newer than the
//!   baseline ends the run as generated even when the job was queued again
//!   meanwhile. Events can be lost (a dropped agent connection, a lost CLI
//!   answer), so while a run is shown the queue and the meeting file are
//!   read again every few seconds, and when the panel is shown again: the
//!   durable outcome ends it, never an endless "Generating". The outcome
//!   says only what the files show; the model that actually ran is the one
//!   the result records (see the meeting's credit line).
//!   [`GenerateEvent::Finished`] makes the meeting reload.
//! - The summary panel's "Instructions" disclosure is prefilled with the
//!   saved default (`[enrichment] instructions`) the first time it opens; a
//!   changed text applies to that run only (also while the disclosure is
//!   collapsed, which then says so), or is saved with "Save as default"
//!   (`enrich instructions set --stdin`). Collapsing never drops the draft.
//!   A template panel takes optional extra instructions for the run.
//!
//! Mounting: [`crate::views::meeting::MeetingView`] owns the summary panel
//! and renders it at the top of the enrichment tabs; the Templates tab owns
//! the template panels.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
#[cfg(test)]
use std::rc::Rc;
use std::time::Duration;

use broscribe_core::config::{
    Config, INSTRUCTIONS_MAX_CHARS, validate_instructions, validate_model_name,
};
use broscribe_core::queue::{JobId, JobState, Queue};
use broscribe_core::{Error, MeetingId, Result};
use chrono::{DateTime, FixedOffset};
use futures::FutureExt as _;
use futures::future::Shared;
use gpui::base::SelectableText;
use gpui::component::ActiveTheme as _;
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui::component::popover::Popover;
use gpui::component::{Sizable as _, WindowExt as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Div, Entity, EventEmitter, Focusable as _,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, Role, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div, rems,
};
use serde_json::Value;

use crate::generation::{
    GenerationTarget, JobRead, Outcome, PendingJob, WatchedJob, finished_outcome, generation_cause,
    read_job,
};
use crate::jobs::{ChildKind, CliError, core_user_text, failure_text, sentence};
use crate::state::AppState;
#[cfg(test)]
use crate::state::Route;
use crate::ui::kit::{self, StatusKind, button, status_line};
use crate::ui::select::{OptionItem, Picker};
use crate::ui::type_scale::{TextRole, TextRoleExt as _};
use crate::ui::{heading, spoken_duration, text_line, timer_line};
use crate::views::providers::{ProviderLocation, ProviderRow, parse_providers};
use crate::views::settings::{args, reload_config};

/// How often the elapsed time of a queued or running job is repainted.
pub const GENERATION_TICK: Duration = Duration::from_secs(1);

/// Shown under a queued or running generation.
pub const GENERATION_HINT: &str =
    "Long meetings can take several minutes. You can leave this meeting; you will be notified.";

// ---- pure helpers (tested) ----

/// The meeting's enrichment job (one per meeting, keyed by its ID).
pub fn enrich_job_id(meeting: &MeetingId) -> JobId {
    GenerationTarget::Summary.job_id(meeting)
}

/// State and error of job `id` in the queue; `None` when there is no such
/// job. Reads `job.json`: background executor only.
pub fn job_state(lib_root: &Path, id: &JobId) -> Result<Option<(JobState, Option<String>)>> {
    match Queue::open(lib_root)?.get(id) {
        Ok(job) => Ok(Some((job.state, job.error))),
        Err(Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// `enrich <meeting> --queue --provider <id> [--model <m>]
/// [--instructions-stdin]`, or for a template `templates run <meeting>
/// <template> --queue …` (the runner adds `--library <lib> --json`). Only
/// IDs and a validated model name: no meeting text or instructions in argv.
pub fn generate_args(
    meeting: &MeetingId,
    target: &GenerationTarget,
    provider: &str,
    model: Option<&str>,
    instructions_stdin: bool,
) -> Vec<OsString> {
    let mut out = match target {
        GenerationTarget::Summary => args(["enrich", meeting.as_str()]),
        GenerationTarget::Template { id, .. } => {
            args(["templates", "run", meeting.as_str(), id.as_str()])
        }
    };
    out.extend(args(["--queue", "--provider", provider]));
    if let Some(model) = model {
        out.extend(args(["--model", model]));
    }
    if instructions_stdin {
        out.push("--instructions-stdin".into());
    }
    out
}

/// The command that saves `text` as the default summary instructions
/// (`enrich instructions set --stdin`, the text on stdin), or clears them
/// when it is blank (`enrich instructions clear`).
pub fn save_instructions_command(text: &str) -> (Vec<OsString>, Option<Vec<u8>>) {
    if text.trim().is_empty() {
        (args(["enrich", "instructions", "clear"]), None)
    } else {
        (
            args(["enrich", "instructions", "set", "--stdin"]),
            Some(text.as_bytes().to_vec()),
        )
    }
}

/// The saved default summary instructions, if any.
pub fn saved_instructions(config: &Config) -> &str {
    config
        .enrichment
        .instructions
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
}

/// `m:ss`, or `h:mm:ss` from an hour on.
pub fn elapsed_text(seconds: i64) -> String {
    let s = seconds.max(0);
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// "Queued · N jobs ahead".
pub fn queued_text(ahead: usize) -> String {
    match ahead {
        0 => "Queued · next to run".to_owned(),
        1 => "Queued · 1 job ahead".to_owned(),
        n => format!("Queued · {n} jobs ahead"),
    }
}

/// "Generating with <provider> · <model>" (live status, without the 1 Hz
/// elapsed timer so screen readers are not interrupted every second).
pub fn running_status_text(provider: &str, model: &str) -> String {
    format!("Generating with {provider} · {model}")
}

/// "Generating with <provider> · <model> · m:ss".
pub fn running_text(provider: &str, model: &str, elapsed_s: i64) -> String {
    format!(
        "{} · {}",
        running_status_text(provider, model),
        elapsed_text(elapsed_s)
    )
}

/// The model a run uses as shown: the requested one, else the provider's
/// configured model, else "provider default".
pub fn run_model_label(config: &Config, provider: &str, requested: Option<&str>) -> String {
    requested
        .or_else(|| configured_model(config, provider))
        .unwrap_or("provider default")
        .to_owned()
}

/// LLM providers generation may use: enabled and consented.
pub fn usable_providers(rows: Vec<ProviderRow>) -> Vec<ProviderRow> {
    rows.into_iter().filter(is_usable_provider).collect()
}

pub(crate) fn is_usable_provider(row: &ProviderRow) -> bool {
    row.purpose.as_deref() == Some("llm") && row.enabled && row.consented
}

/// A model the provider listed.
#[derive(Clone, Debug, PartialEq)]
pub struct ListedModel {
    pub id: String,
    pub name: Option<String>,
    /// The provider marks it as its own default.
    pub default: bool,
    /// The model the provider reported when this one was last requested
    /// (from the index; `None` when unknown).
    pub resolved: Option<String>,
}

impl ListedModel {
    /// The name without the last-run hint.
    pub fn label(&self) -> String {
        let mut label = match &self.name {
            Some(name) if name != &self.id => format!("{name} ({})", self.id),
            _ => self.id.clone(),
        };
        if self.default {
            label.push_str(" — provider default");
        }
        label
    }

    /// Visible text and accessible name, with the last-run hint (unless it
    /// only repeats the ID).
    pub fn option_text(&self) -> (String, String) {
        let hint = self.resolved.as_deref().filter(|r| *r != self.id);
        last_run_text(&self.label(), hint)
    }
}

/// `label`, plus `— <resolved> (last run)` when known; the accessible name
/// spells the hint out.
pub fn last_run_text(label: &str, resolved: Option<&str>) -> (String, String) {
    match resolved {
        Some(model) => (
            format!("{label} — {model} (last run)"),
            format!("{label}; the last summary generated with it used model {model}"),
        ),
        None => (label.to_owned(), label.to_owned()),
    }
}

/// `broscribe providers models <id> --json`.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelList {
    /// `cli`, `api`, `help` or `none` (the provider cannot list).
    pub source: String,
    pub models: Vec<ListedModel>,
    /// The model the provider reported when none was requested, if known.
    pub default_resolved: Option<String>,
}

/// A non-empty trimmed string field.
fn text_field(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Parses a model list; entries without an `id` are skipped.
pub fn parse_model_list(v: &Value) -> std::result::Result<ModelList, String> {
    let models = v["models"]
        .as_array()
        .ok_or_else(|| "The provider's model list was not understood.".to_owned())?;
    Ok(ModelList {
        source: v["source"].as_str().unwrap_or("none").to_owned(),
        models: models
            .iter()
            .filter_map(|m| {
                let id = text_field(&m["id"])?;
                Some(ListedModel {
                    id,
                    name: text_field(&m["name"]),
                    default: m["default"].as_bool().unwrap_or(false),
                    resolved: text_field(&m["resolved"]),
                })
            })
            .collect(),
        default_resolved: text_field(&v["default_resolved"]),
    })
}

/// Cache key of a provider's model list: its ID and its config, so enabling,
/// consent, endpoint or model changes fetch again.
pub type ModelCacheKey = (String, String);

pub fn model_cache_key(provider: &str, config: &Config) -> ModelCacheKey {
    let entry = config
        .providers
        .get(provider)
        .and_then(|pc| serde_json::to_string(pc).ok())
        .unwrap_or_default();
    (provider.to_owned(), entry)
}

/// Fingerprint of every provider's config (a change re-lists providers).
pub(crate) fn providers_fingerprint(config: &Config) -> String {
    serde_json::to_string(&config.providers).unwrap_or_default()
}

/// The provider's configured model, if any.
fn configured_model<'a>(config: &'a Config, provider: &str) -> Option<&'a str> {
    config
        .providers
        .get(provider)
        .and_then(|pc| pc.model.as_deref())
        .map(str::trim)
        .filter(|m| !m.is_empty())
}

/// The first model option: the configured model, else the provider's own
/// default for CLI providers; `None` for HTTP providers without a model
/// (they need one chosen).
pub fn default_option(config: &Config, provider: &ProviderRow) -> Option<String> {
    match configured_model(config, &provider.id) {
        Some(model) => Some(format!("Configured model ({model})")),
        None if !provider.http => Some("Provider default".to_owned()),
        None => None,
    }
}

/// What the first model option resolved to last time: with a configured
/// model, the listed entry's hint for it; else the provider default's.
pub fn default_resolved<'a>(
    config: &Config,
    provider: &ProviderRow,
    list: &'a ModelList,
) -> Option<&'a str> {
    match configured_model(config, &provider.id) {
        Some(model) => list
            .models
            .iter()
            .find(|m| m.id == model)
            .and_then(|m| m.resolved.as_deref())
            .filter(|r| *r != model),
        None => list.default_resolved.as_deref(),
    }
}

/// An option's visible text and accessible name.
pub type OptionText = (String, String);

/// The model options before "Other…": the first option (if any), then each
/// listed model, with last-run hints.
pub fn model_option_texts(
    config: &Config,
    provider: &ProviderRow,
    list: Option<&ModelList>,
) -> (Option<OptionText>, Vec<OptionText>) {
    let first = default_option(config, provider).map(|label| {
        last_run_text(
            &label,
            list.and_then(|l| default_resolved(config, provider, l)),
        )
    });
    let listed = list
        .map(|l| l.models.iter().map(ListedModel::option_text).collect())
        .unwrap_or_default();
    (first, listed)
}

/// The model the run uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelChoice {
    /// No `--model`: configured model or the provider's default.
    Default,
    Listed(String),
    /// Free text from the "Other…" field.
    Other,
}

/// Where a generation stands.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Idle,
    /// `enrich --queue` is running.
    Submitting,
    Queued(JobId),
    Running(JobId),
    Done,
    Cancelled,
    Restored,
    Failed(String),
}

impl Phase {
    pub fn in_flight(&self) -> bool {
        matches!(self, Self::Submitting | Self::Queued(_) | Self::Running(_))
    }

    fn job(&self) -> Option<&JobId> {
        match self {
            Self::Queued(job) | Self::Running(job) => Some(job),
            _ => None,
        }
    }
}

/// What a refusal of the queueing command means to the user.
pub fn refusal_text(e: &CliError, target: &GenerationTarget) -> String {
    match e.code.as_str() {
        "library_busy" => match target {
            GenerationTarget::Summary => {
                "A summary is already being generated for this meeting; wait for it to finish."
                    .to_owned()
            }
            GenerationTarget::Template { name, .. } => format!(
                "{name} is already being generated for this meeting; wait for it to finish."
            ),
        },
        "consent_required" => {
            "Turn on this provider and agree to its terms before using it.".to_owned()
        }
        _ => generation_failure(target, &e.user_text()),
    }
}

/// “Couldn't generate the summary. {Cause}.” or “Couldn't run Meeting
/// notes. {Cause}.”: the run's failure, its cause as a sentence (left
/// out when unknown).
pub fn generation_failure(target: &GenerationTarget, cause: &str) -> String {
    let what = match target {
        GenerationTarget::Summary => "Couldn't generate the summary.".to_owned(),
        GenerationTarget::Template { name, .. } => format!("Couldn't run {name}."),
    };
    failure_text(&what, &sentence(cause), "")
}

// ---- view ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GenerateEvent {
    /// The generation job finished; the meeting should reload.
    Finished,
    CopySummary,
    CatalogChanged,
}

/// The fetch of one provider's model list.
#[derive(Clone, Debug, Default)]
struct ModelFetch {
    key: Option<ModelCacheKey>,
    loading: bool,
    error: Option<String>,
    generation: u64,
    /// The list shown while a re-fetch after a generation is in flight (its
    /// cache entry is gone), so the options do not disappear meanwhile.
    previous: Option<ModelList>,
}

impl ModelFetch {
    /// Drops whatever fetch is in flight (its answer is ignored) and the
    /// state shown for it; the generation counter keeps counting.
    fn invalidate(&mut self) {
        self.generation += 1;
        self.key = None;
        self.loading = false;
        self.error = None;
        self.previous = None;
    }
}

/// How [`GeneratePanel::fetch_models`] treats the cached list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fetch {
    /// Only when nothing is cached (or already loading) for the key.
    IfMissing,
    /// Drops the cached list first (the Refresh button).
    Refresh,
    /// After a generation: drops the cached list but keeps showing it, and
    /// the chosen model, until the new one arrives (with fresh hints).
    Revalidate,
}

/// Job-event signature watched while a generation is followed.
type JobsSignature = (u64, usize, Option<String>, bool);

/// One catalog lookup shared by every generation panel with this provider configuration.
pub(crate) type ProviderLookup = Shared<Task<Result<Vec<ProviderRow>, String>>>;

pub(crate) fn list_providers(state: &AppState, cx: &App) -> ProviderLookup {
    let task = state.run_cli(args(["providers", "list"]), ChildKind::Command, cx);
    cx.background_executor()
        .spawn(async move {
            task.await
                .map(|v| parse_providers(&v))
                .map_err(|e| failure_text("Couldn't list the AI providers.", &e.user_text(), ""))
        })
        .shared()
}

#[derive(Clone)]
pub(crate) struct GenerationSelection {
    provider: String,
    choice: ModelChoice,
    model: Option<String>,
}

#[derive(Clone)]
struct Submission {
    selection: GenerationSelection,
    instructions: Option<String>,
}

#[derive(Clone)]
struct SummaryCredit {
    provider: String,
    model: Option<String>,
    requested: Option<String>,
    reported: bool,
    created: DateTime<FixedOffset>,
    instructions: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OptionsAnchor {
    Header,
    Failure,
    Stale,
}

/// What the meeting looked like when the followed run began: the worker
/// removes a finished job before reporting it, so the outcome is read from
/// the meeting (a new result) and from failures reported since.
#[derive(Clone, Debug, PartialEq)]
struct Request {
    generation_run: Option<String>,
    /// The provider the generation runs with (`None`: a job this panel
    /// did not start; its input names it).
    provider: Option<String>,
    /// The model requested, if any.
    model: Option<String>,
    /// `created_at` of the result then, if any, read from the meeting file.
    baseline: Option<DateTime<FixedOffset>>,
    /// Failures already reported for the job then.
    failures_before: usize,
    /// The queueing command's answer was lost: whether it queued anything
    /// is known only from the queue and the meeting.
    lost_answer: bool,
}

/// The phase a finished job of `target` leaves.
fn finished_phase(target: &GenerationTarget, outcome: Outcome) -> Phase {
    match outcome {
        Outcome::Ready => Phase::Done,
        Outcome::Failed(cause) => Phase::Failed(generation_failure(target, &cause)),
    }
}

/// While a run is shown, the queue and the meeting are read again every this
/// many repaints, whether or not job events arrived.
const RECONCILE_TICKS: u32 = 5;

/// The instructions disclosure.
struct Instructions {
    open: bool,
    input: Entity<TextareaState>,
    /// The saved default the field was last filled with; `None` until the
    /// disclosure first opens (the field holds no draft yet).
    seeded: Option<String>,
    /// The result of "Save as default".
    notice: Option<(String, bool)>,
    saving: bool,
}

pub struct GeneratePanel {
    state: Entity<AppState>,
    meeting_id: MeetingId,
    target: GenerationTarget,
    /// `None` until listed.
    providers: Option<std::result::Result<Vec<ProviderRow>, String>>,
    providers_loading: bool,
    providers_generation: u64,
    seen_providers: String,
    provider: Option<String>,
    choice: Option<ModelChoice>,
    pending_choice: Option<ModelChoice>,
    refreshed_choice: bool,
    last_submission: Option<Submission>,
    consent_refusal: bool,
    result_selection: Option<(String, Option<String>)>,
    selection_edited: bool,
    summary_credit: Option<SummaryCredit>,
    options_width: gpui::Pixels,
    options: Option<OptionsAnchor>,
    options_dialog: bool,
    focus_provider: bool,
    header_focus: gpui::FocusHandle,
    options_opener: Option<gpui::FocusHandle>,
    options_focus_out: Option<Subscription>,
    copied: bool,
    other: Entity<InputState>,
    other_error: Option<String>,
    preselected_other: Option<String>,
    models: ModelFetch,
    phase: Phase,
    /// The followed job as last read (queue position, start, provider).
    pending: Option<PendingJob>,
    /// Repaints the elapsed time while the job is queued or running.
    ticker: Option<Task<()>>,
    /// `created_at` of the meeting's result for the target as last loaded.
    result_created: Option<DateTime<FixedOffset>>,
    can_restore: bool,
    editing_generation: bool,
    edit_error: Option<String>,
    /// The run this panel follows: what the meeting held when it began.
    request: Option<Request>,
    /// Bumped whenever a new run is submitted or followed; answers and
    /// reads started for another run are dropped (the job ID is reused by
    /// every run of the target, so it cannot tell runs apart).
    run: u64,
    stale: bool,
    /// A job for the target is queued or running (not ours).
    meeting_busy: bool,
    /// The template's definition is gone: its result stays, Run does not.
    definition_missing: bool,
    seen_jobs: Option<JobsSignature>,
    /// On screen (the owning tab is shown): gates the timer.
    visible: bool,
    /// The last attempt failed and the scheduler will run it again.
    retrying: Option<String>,
    /// A job read is in flight; `recheck` asks for another after it.
    checking: bool,
    recheck: bool,
    instructions: Instructions,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<GenerateEvent> for GeneratePanel {}

impl GeneratePanel {
    pub fn new(
        state: Entity<AppState>,
        meeting_id: MeetingId,
        target: GenerationTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let other = cx.new(|cx| InputState::new(window, cx).placeholder("Model name"));
        let placeholder = match target {
            GenerationTarget::Summary => "Keep it short and focus on budget decisions",
            GenerationTarget::Template { .. } => "Leave out the small talk",
        };
        let instructions = cx.new(|cx| TextareaState::new(window, cx).placeholder(placeholder));
        let subscriptions = vec![
            cx.observe_in(&state, window, |this, _, window, cx| {
                this.state_changed(window, cx)
            }),
            cx.subscribe(&other, |this, _, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.other_error = None;
                    cx.notify();
                }
            }),
            cx.subscribe(&instructions, |this, _, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.instructions.notice = None;
                    cx.notify();
                }
            }),
        ];
        let seen_providers = providers_fingerprint(state.read(cx).config());
        Self {
            state,
            meeting_id,
            target,
            providers: None,
            providers_loading: false,
            providers_generation: 0,
            seen_providers,
            provider: None,
            choice: None,
            pending_choice: None,
            refreshed_choice: false,
            last_submission: None,
            consent_refusal: false,
            result_selection: None,
            selection_edited: false,
            summary_credit: None,
            options_width: gpui::px(0.),
            options: None,
            options_dialog: false,
            focus_provider: false,
            header_focus: cx.focus_handle(),
            options_opener: None,
            options_focus_out: None,
            copied: false,
            other,
            other_error: None,
            preselected_other: None,
            models: ModelFetch::default(),
            phase: Phase::Idle,
            pending: None,
            ticker: None,
            result_created: None,
            can_restore: false,
            editing_generation: false,
            edit_error: None,
            request: None,
            run: 0,
            stale: false,
            meeting_busy: false,
            definition_missing: false,
            seen_jobs: None,
            visible: false,
            retrying: None,
            checking: false,
            recheck: false,
            instructions: Instructions {
                open: false,
                input: instructions,
                seeded: None,
                notice: None,
                saving: false,
            },
            _subscriptions: subscriptions,
        }
    }

    pub(crate) fn set_summary(
        &mut self,
        result: Option<&broscribe_core::schema::Enrichment>,
        cx: &mut Context<Self>,
    ) {
        self.summary_credit = result.map(|result| SummaryCredit {
            provider: result.provider.clone(),
            model: result.model.clone(),
            requested: result.requested_model.clone(),
            reported: result.model_reported,
            created: result.created_at,
            instructions: result.instructions.clone(),
        });
        self.set_result_selection(
            result.map(|result| (result.provider.as_str(), result.requested_model.as_deref())),
            cx,
        );
    }

    pub(crate) fn set_result_selection(
        &mut self,
        result: Option<(&str, Option<&str>)>,
        cx: &mut Context<Self>,
    ) {
        let result_just_loaded = self.result_selection.is_none() && result.is_some();
        self.result_selection = result.map(|(id, model)| (id.to_owned(), model.map(str::to_owned)));
        if result_just_loaded
            && !self.selection_edited
            && self.last_submission.is_none()
            && let Some((id, model)) = self.result_selection.clone()
            && self.row(&id).is_some()
        {
            self.select_provider(id, model.map(ModelChoice::Listed), cx);
        } else if self.provider.is_none() {
            self.preselect(cx);
        }
    }

    pub(crate) fn set_options_width(&mut self, width: gpui::Pixels, cx: &mut Context<Self>) {
        if self.options_width != width {
            self.options_width = width;
            cx.notify();
        }
    }

    pub(crate) fn summary_copied(&mut self, cx: &mut Context<Self>) {
        self.copied = true;
        cx.notify();
    }

    /// Whether the 1 Hz repaint of the elapsed time runs (tests observe it).
    #[cfg(test)]
    pub(crate) fn ticking(&self) -> bool {
        self.ticker.is_some()
    }

    pub(crate) fn set_template_name(&mut self, name: &str) {
        if let GenerationTarget::Template { name: current, .. } = &mut self.target
            && current != name
        {
            *current = name.to_owned();
        }
    }

    /// A template's options belong to its row and tab.
    pub(crate) fn dismiss_template_options(
        &mut self,
        window: Option<gpui::AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) {
        if self.options.take().is_none() {
            return;
        }
        self.options_opener = None;
        self.options_focus_out = None;
        self.focus_provider = false;
        if std::mem::take(&mut self.options_dialog)
            && let Some(window) = window
        {
            cx.defer(move |cx| {
                window
                    .update(cx, |_, window, cx| window.close_dialog(cx))
                    .ok();
            });
        }
        cx.notify();
    }

    /// The template's definition was deleted: Run is disabled.
    pub fn set_definition_missing(&mut self, missing: bool, cx: &mut Context<Self>) {
        if self.definition_missing != missing {
            self.definition_missing = missing;
            cx.notify();
        }
    }

    /// Whether the panel is on screen (its tab shown); the elapsed-time
    /// timer runs only then. Shown again while
    /// a run is followed, the run is read again at once (events may have
    /// been missed meanwhile).
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible != visible {
            self.visible = visible;
            if !visible {
                self.options = None;
                self.options_opener = None;
                self.options_focus_out = None;
            }
            self.update_ticker(cx);
            if visible {
                self.check_job(cx);
            }
        }
    }

    /// Moves to `phase`.
    fn set_phase(&mut self, phase: Phase, cx: &mut Context<Self>) {
        self.phase = phase;
        if !matches!(self.phase, Phase::Queued(_) | Phase::Running(_)) {
            self.pending = None;
            self.retrying = None;
        }
        self.update_ticker(cx);
        cx.notify();
    }

    /// Follows a new run of the target's job from `phase` (queued or
    /// running) against `request`, under a fresh follow token.
    fn follow(&mut self, phase: Phase, request: Request, cx: &mut Context<Self>) {
        self.run += 1;
        self.request = Some(request);
        self.seen_jobs = None;
        self.set_phase(phase, cx);
        self.check_job(cx);
    }

    /// Runs the 1 Hz repaint only while the panel is visible and a job is
    /// queued or running; stops it otherwise. Every [`RECONCILE_TICKS`]
    /// repaints the run is read again, so a lost event cannot leave it
    /// "Generating" for ever.
    fn update_ticker(&mut self, cx: &mut Context<Self>) {
        let wanted = self.visible && matches!(self.phase, Phase::Queued(_) | Phase::Running(_));
        if !wanted {
            self.ticker = None;
        } else if self.ticker.is_none() {
            self.ticker = Some(cx.spawn(async move |this, cx| {
                let mut ticks = 0u32;
                loop {
                    cx.background_executor().timer(GENERATION_TICK).await;
                    ticks = (ticks + 1) % RECONCILE_TICKS;
                    let alive = this
                        .update(cx, |this, cx| {
                            let shown = this.visible
                                && matches!(this.phase, Phase::Queued(_) | Phase::Running(_));
                            if shown {
                                cx.notify();
                                if ticks == 0 {
                                    this.check_job(cx);
                                }
                            }
                            shown
                        })
                        .unwrap_or(false);
                    if !alive {
                        break;
                    }
                }
            }));
        }
    }

    pub(crate) fn phase(&self) -> &Phase {
        &self.phase
    }

    pub fn providers(&self) -> Option<&std::result::Result<Vec<ProviderRow>, String>> {
        self.providers.as_ref()
    }

    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn choice(&self) -> Option<&ModelChoice> {
        self.choice.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn other_error(&self) -> Option<&str> {
        self.other_error.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn model_error(&self) -> Option<&str> {
        self.models.error.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn models_loading(&self) -> bool {
        self.models.loading
    }

    /// The chosen provider's list as shown: the cached one under its current
    /// config, else the previous one while it is fetched again after a
    /// generation.
    pub fn model_list(&self, cx: &gpui::App) -> Option<ModelList> {
        let provider = self.provider.as_deref()?;
        let state = self.state.read(cx);
        state
            .model_list(&model_cache_key(provider, state.config()))
            .or(self.models.previous.as_ref())
            .cloned()
    }

    /// Whether Generate can run now (ignores the model validation).
    pub fn can_generate(&self) -> bool {
        !self.phase.in_flight()
            && !self.editing_generation
            && !self.meeting_busy
            && !self.definition_missing
            && self
                .provider
                .as_deref()
                .is_some_and(|id| self.row(id).is_some())
            && self.choice.is_some()
            && !self.refreshed_choice
            && self.pending_choice.is_none()
    }

    /// Whether the meeting has a result for the target (Regenerate).
    pub fn has_enrichment(&self) -> bool {
        self.result_created.is_some()
    }

    /// What the meeting view knows: when the target's result was created
    /// (if any), whether it is stale, and the state of the target's job. A
    /// job queued or running while this panel follows none (started before
    /// the view was opened, or elsewhere) is followed from now on. A result
    /// that moved while a run is followed makes the run be read again.
    pub fn set_meeting(
        &mut self,
        result_created: Option<DateTime<FixedOffset>>,
        stale: bool,
        job: Option<JobState>,
        cx: &mut Context<Self>,
    ) {
        let busy = matches!(
            job,
            Some(JobState::Queued | JobState::Running | JobState::Analyzed)
        );
        let moved = result_created != self.result_created;
        if (result_created, stale, busy) != (self.result_created, self.stale, self.meeting_busy) {
            self.result_created = result_created;
            self.stale = stale;
            self.meeting_busy = busy;
            cx.notify();
        }
        if busy && !self.phase.in_flight() {
            // Followed from now on: what the meeting held when following
            // started (read with the job's state) is the baseline, kept to
            // the end, so a reload that delivers the new result before the
            // job's end is not mistaken for "no new result".
            let id = self.target.job_id(&self.meeting_id);
            let request = Request {
                generation_run: None,
                provider: None,
                model: None,
                baseline: result_created,
                failures_before: self.failures_of(&id, cx).0,
                lost_answer: false,
            };
            let phase = if job == Some(JobState::Queued) {
                Phase::Queued(id)
            } else {
                Phase::Running(id)
            };
            self.follow(phase, request, cx);
        } else if moved && self.phase.job().is_some() {
            self.check_job(cx);
        }
    }

    /// Lists the providers once (and again after their config changed).
    pub fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        if self.providers.is_none() && !self.providers_loading {
            self.load_providers(cx);
        }
    }

    fn load_providers(&mut self, cx: &mut Context<Self>) {
        self.providers_generation += 1;
        let generation = self.providers_generation;
        self.providers_loading = true;
        self.providers = None;
        cx.notify();
        let task = self
            .state
            .update(cx, |state, cx| state.generation_providers(cx));
        cx.spawn(async move |this, cx| {
            let result = task.await.map(usable_providers);
            this.update(cx, |this, cx| {
                if this.providers_generation != generation {
                    return;
                }
                this.providers_loading = false;
                this.providers = Some(result);
                if this
                    .provider
                    .as_ref()
                    .is_some_and(|id| this.row(id).is_none())
                {
                    this.provider = None;
                    this.choice = None;
                    this.pending_choice = None;
                    this.models.invalidate();
                }
                this.preselect(cx);
                if let Some(row) = this.selected_row() {
                    if this.choice == Some(ModelChoice::Default)
                        && default_option(this.state.read(cx).config(), row).is_none()
                    {
                        this.choice = None;
                    }
                    this.fetch_models(Fetch::IfMissing, cx);
                }
                cx.emit(GenerateEvent::CatalogChanged);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn retry_providers(&mut self, cx: &mut Context<Self>) {
        self.state
            .update(cx, |state, _| state.retry_generation_providers());
        self.load_providers(cx);
    }

    fn preselect(&mut self, cx: &mut Context<Self>) {
        if self.provider.is_some() {
            return;
        }
        let Some(Ok(rows)) = &self.providers else {
            return;
        };
        let current = self
            .result_selection
            .as_ref()
            .filter(|(id, _)| self.row(id).is_some());
        let last = self
            .state
            .read(cx)
            .last_generation
            .as_ref()
            .filter(|selection| self.row(&selection.provider).is_some())
            .cloned();
        let (id, choice, custom) = if let Some((id, model)) = current {
            (id.clone(), model.clone().map(ModelChoice::Listed), None)
        } else if let Some(last) = last {
            (last.provider, Some(last.choice), last.model)
        } else if let Some(first) = rows.first() {
            (first.id.clone(), None, None)
        } else {
            return;
        };
        self.select_provider(id, choice, cx);
        if self.choice == Some(ModelChoice::Other)
            && let Some(model) = custom
        {
            self.preselected_other = Some(model);
        }
    }

    fn name(&self, id: &str, cx: &App) -> String {
        self.state
            .read(cx)
            .provider_name(id)
            .or_else(|| {
                self.row(id)
                    .filter(|row| row.name != row.id)
                    .map(|row| row.name.as_str())
            })
            .map(str::to_owned)
            .unwrap_or_else(|| match &self.providers {
                Some(Ok(_)) => format!("unknown provider ({id})"),
                _ => "an AI provider".to_owned(),
            })
    }

    fn row(&self, id: &str) -> Option<&ProviderRow> {
        self.providers
            .as_ref()
            .and_then(|r| r.as_ref().ok())
            .and_then(|rows| rows.iter().find(|r| r.id == id))
    }

    /// Chooses the provider and fetches its models unless cached. Another
    /// provider's list still being fetched is invalidated: its answer can
    /// no longer change the choice, the error or the loading state.
    pub fn choose_provider(&mut self, id: String, cx: &mut Context<Self>) {
        self.selection_edited = true;
        self.select_provider(id, None, cx);
    }

    fn select_provider(
        &mut self,
        id: String,
        preferred: Option<ModelChoice>,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.row(&id).cloned() else {
            return;
        };
        if self.provider.as_deref() != Some(id.as_str()) {
            let config = self.state.read(cx).config();
            self.choice = default_option(config, &row).map(|_| ModelChoice::Default);
            self.pending_choice = preferred;
            self.other_error = None;
            self.preselected_other = None;
            self.refreshed_choice = false;
            self.provider = Some(id);
            self.models.invalidate();
        } else if preferred.is_some() {
            self.pending_choice = preferred;
        }
        self.apply_preferred_model(cx);
        self.fetch_models(Fetch::IfMissing, cx);
        cx.notify();
    }

    fn apply_preferred_model(&mut self, cx: &App) {
        match self.pending_choice.as_ref() {
            Some(ModelChoice::Listed(id)) => {
                let Some(list) = self.model_list(cx) else {
                    return;
                };
                if list.models.iter().any(|model| &model.id == id) {
                    self.choice = self.pending_choice.take();
                }
            }
            Some(ModelChoice::Other) => self.choice = self.pending_choice.take(),
            _ => {}
        }
        self.pending_choice = None;
    }

    pub fn choose_model(&mut self, choice: ModelChoice, cx: &mut Context<Self>) {
        self.selection_edited = true;
        self.pending_choice = None;
        self.refreshed_choice = false;
        if self.choice.as_ref() != Some(&choice) {
            self.choice = Some(choice);
            self.other_error = None;
        }
        cx.notify();
    }

    /// Opens Settings, where AI providers are enabled with consent.
    pub fn open_provider_settings(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |_, cx| {
            cx.emit(crate::state::AppEvent::OpenSettings(Some(
                crate::state::SettingsPage::AiProviders,
            )));
        });
    }

    /// Fetches the model list again, dropping the cached one.
    pub fn refresh_models(&mut self, cx: &mut Context<Self>) {
        self.fetch_models(Fetch::Refresh, cx);
    }

    /// A generation with `provider` finished: its cached list carries old
    /// last-run hints, so it is dropped, and the chosen provider's list is
    /// fetched again once (the panel renders inline, so nothing else would
    /// ask), keeping the old options shown until the new list arrives.
    fn models_after_generation(&mut self, provider: &str, cx: &mut Context<Self>) {
        if self.provider.as_deref() != Some(provider) {
            let key = model_cache_key(provider, self.state.read(cx).config());
            self.state.update(cx, |s, _| s.forget_model_list(&key));
        }
        self.fetch_models(Fetch::Revalidate, cx);
    }

    fn fetch_models(&mut self, mode: Fetch, cx: &mut Context<Self>) {
        let Some(provider) = self.provider.clone() else {
            return;
        };
        let key = model_cache_key(&provider, self.state.read(cx).config());
        match mode {
            Fetch::IfMissing => {
                if self.state.read(cx).model_list(&key).is_some()
                    || (self.models.loading && self.models.key.as_ref() == Some(&key))
                {
                    return;
                }
                self.models.previous = None;
            }
            Fetch::Refresh => {
                self.state.update(cx, |s, _| s.forget_model_list(&key));
                self.models.previous = None;
            }
            Fetch::Revalidate => {
                if let Some(list) = self.state.read(cx).model_list(&key).cloned() {
                    self.models.previous = Some(list);
                }
                self.state.update(cx, |s, _| s.forget_model_list(&key));
            }
        }
        self.models.generation += 1;
        let generation = self.models.generation;
        self.models.key = Some(key.clone());
        self.models.loading = true;
        self.models.error = None;
        cx.notify();
        let task = self.state.read(cx).run_cli(
            args(["providers", "models", provider.as_str()]),
            ChildKind::Command,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.models.generation != generation {
                    return;
                }
                this.models.loading = false;
                match result
                    .map_err(|e| e.user_text())
                    .and_then(|v| parse_model_list(&v))
                {
                    Ok(list) => {
                        this.models.previous = None;
                        // A model the provider no longer lists is not kept.
                        if let Some(ModelChoice::Listed(id)) = &this.choice
                            && !list.models.iter().any(|m| &m.id == id)
                        {
                            this.choice = None;
                            this.refreshed_choice = true;
                        }
                        this.state
                            .update(cx, |s, _| s.cache_model_list(key.clone(), list));
                        this.apply_preferred_model(cx);
                    }
                    Err(cause) => {
                        this.pending_choice = None;
                        if this.models.previous.is_none()
                            && matches!(this.choice, Some(ModelChoice::Listed(_)))
                        {
                            this.choice = None;
                            this.refreshed_choice = true;
                        }
                        this.models.error = Some(failure_text(
                            "Couldn't get the model list.",
                            &cause,
                            "The configured model and Other… still work.",
                        ));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The `--model` value for the current choice; `Err` explains an invalid
    /// "Other…" entry.
    fn model_arg(&self, cx: &gpui::App) -> std::result::Result<Option<String>, String> {
        match &self.choice {
            None | Some(ModelChoice::Default) => Ok(None),
            Some(ModelChoice::Listed(id)) => Ok(Some(id.clone())),
            Some(ModelChoice::Other) => {
                let value = self.other.read(cx).value();
                let text = self
                    .preselected_other
                    .as_deref()
                    .unwrap_or(&value)
                    .trim()
                    .to_owned();
                validate_model_name(&text)
                    .map(|()| Some(text))
                    .map_err(|e| match e {
                        Error::InvalidInput(m) => format!("Model name {m}."),
                        e => core_user_text(&e),
                    })
            }
        }
    }

    /// Queues the generation (`enrich --queue`, `templates run --queue`)
    /// through the app, which reserves the run, reads its baseline from the
    /// meeting file and watches (and announces) exactly that run; the panel
    /// follows the receipt's job against the receipt's baseline.
    pub fn generate(&mut self, window: &Window, cx: &mut Context<Self>) {
        if !self.can_generate() {
            return;
        }
        let (Some(provider), Some(choice)) = (self.provider.clone(), self.choice.clone()) else {
            return;
        };
        let model = match self.model_arg(cx) {
            Ok(model) => {
                self.other_error = None;
                model
            }
            Err(message) => {
                self.other_error = Some(message);
                cx.notify();
                return;
            }
        };
        let instructions = self.instructions_override(cx);
        if instructions
            .as_deref()
            .is_some_and(|text| validate_instructions(text).is_err())
        {
            return;
        }
        let submission = Submission {
            selection: GenerationSelection {
                provider,
                choice,
                model,
            },
            instructions: instructions.clone().or_else(|| {
                (self.target == GenerationTarget::Summary)
                    .then(|| saved_instructions(self.state.read(cx).config()).to_owned())
            }),
        };
        self.submit(submission, instructions, window, cx);
    }

    fn retry_generation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(submission) = self.last_submission.clone() else {
            self.open_options(OptionsAnchor::Failure, window, cx);
            return;
        };
        if self.phase.in_flight()
            || self.meeting_busy
            || self.definition_missing
            || self.row(&submission.selection.provider).is_none()
        {
            return;
        }
        self.submit(submission.clone(), submission.instructions, window, cx);
    }

    fn submit(
        &mut self,
        submission: Submission,
        instructions: Option<String>,
        _window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.last_submission = Some(submission.clone());
        self.consent_refusal = false;
        self.copied = false;
        let GenerationSelection {
            provider, model, ..
        } = submission.selection.clone();
        let job_id = self.target.job_id(&self.meeting_id);
        let failures_before = self.failures_of(&job_id, cx).0;
        self.run += 1;
        let run = self.run;
        self.request = None;
        self.set_phase(Phase::Submitting, cx);
        let command = generate_args(
            &self.meeting_id,
            &self.target,
            &provider,
            model.as_deref(),
            instructions.is_some(),
        );
        // The app owns the submission and watches its run after navigation.
        // The CLI reads the durable result baseline atomically with enqueue.
        let watched = WatchedJob {
            generation_run: None,
            meeting_id: self.meeting_id.clone(),
            target: self.target.clone(),
            baseline: None,
            failures_before,
        };
        let answer = self.state.update(cx, |state, cx| {
            state.last_generation = Some(submission.selection);
            state.submit_generation(command, instructions.map(String::into_bytes), watched, cx)
        });
        // Only for a lost answer: the result the view showed when pressed.
        let shown_baseline = self.result_created;
        cx.spawn(async move |this, cx| {
            let answer = answer.await;
            this.update(cx, |this, cx| {
                if this.run != run {
                    return;
                }
                let request = |baseline, failures_before, lost_answer, generation_run| Request {
                    generation_run,
                    provider: Some(provider.clone()),
                    model,
                    baseline,
                    failures_before,
                    lost_answer,
                };
                match answer {
                    // The receipt carries the baseline locked with enqueue.
                    Ok(Ok(receipt)) => this.follow(
                        Phase::Queued(receipt.job_id),
                        request(
                            receipt.baseline,
                            receipt.failures_before,
                            false,
                            receipt.generation_run,
                        ),
                        cx,
                    ),
                    Ok(Err(e)) => {
                        this.consent_refusal = e.code == "consent_required";
                        let text = if this.consent_refusal {
                            generation_failure(
                                &this.target,
                                &format!(
                                    "{} is turned off or its terms aren’t accepted.",
                                    this.name(&provider, cx)
                                ),
                            )
                        } else {
                            refusal_text(&e, &this.target)
                        };
                        this.set_phase(Phase::Failed(text), cx);
                    }
                    // The answer never came: the queue and the meeting say
                    // whether the run was queued and how it ended.
                    Err(_) => this.follow(
                        Phase::Queued(job_id),
                        request(shown_baseline, failures_before, true, None),
                        cx,
                    ),
                }
            })
            .ok();
        })
        .detach();
    }

    fn state_changed(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let fingerprint = providers_fingerprint(self.state.read(cx).config());
        if fingerprint != self.seen_providers {
            self.seen_providers = fingerprint;
            self.models.invalidate();
            if self.providers.is_some() || self.providers_loading {
                self.load_providers(cx);
            }
            // A refreshed catalog and model list will reconcile the selection.
        }
        let Some(job) = self.phase.job().cloned() else {
            // A CLI or automatic template run may start while its row is already shown.
            let active = &self.state.read(cx).jobs().active;
            if self.visible
                && matches!(self.target, GenerationTarget::Template { .. })
                && !active.is_empty()
            {
                let job = self.target.job_id(&self.meeting_id);
                if active.iter().any(|active| active.job_id == job.as_str()) {
                    self.set_meeting(self.result_created, self.stale, Some(JobState::Running), cx);
                }
            }
            return;
        };
        let signature = {
            let jobs = self.state.read(cx).jobs();
            (
                jobs.completed,
                jobs.recent_failures.len(),
                jobs.recent_failures.back().map(|f| f.job_id.clone()),
                jobs.active.iter().any(|a| a.job_id == job.as_str()),
            )
        };
        if self.seen_jobs.as_ref() == Some(&signature) {
            return;
        }
        self.seen_jobs = Some(signature.clone());
        if signature.3 && matches!(self.phase, Phase::Queued(_)) {
            self.set_phase(Phase::Running(job), cx);
        }
        self.check_job(cx);
    }

    /// How many failures the app's job events reported for `job`, and the
    /// newest one's message.
    fn failures_of(&self, job: &JobId, cx: &gpui::App) -> (usize, Option<String>) {
        let jobs = self.state.read(cx).jobs();
        let mut failures = jobs
            .recent_failures
            .iter()
            .filter(|f| f.job_id == job.as_str());
        let count = failures.clone().count();
        // An unknown cause is left out of the failure line, never "unknown".
        let last = failures
            .next_back()
            .map(|f| f.message.clone().unwrap_or_default());
        (count, last)
    }

    /// Reads the followed run from the queue and the meeting file. The
    /// worker removes a job once it is done (before reporting it), so a done
    /// or vanished job is judged by the meeting: a result newer than the
    /// run's baseline is done (also when the job was queued again since, by
    /// another run), anything else failed (with the failure reported since,
    /// if any). A read started for another run is dropped.
    fn check_job(&mut self, cx: &mut Context<Self>) {
        let Some(job) = self.phase.job().cloned() else {
            return;
        };
        if self.checking {
            self.recheck = true;
            return;
        }
        self.checking = true;
        let run = self.run;
        let lib = self.state.read(cx).library().clone();
        let meeting = self.meeting_id.clone();
        let target = self.target.clone();
        let read = job.clone();
        let expected = self.request.as_ref().and_then(|r| r.generation_run.clone());
        let task = cx.background_executor().spawn(async move {
            let found = read_job(&lib, &read, &meeting, &target, expected.as_deref())?;
            let current = match &found {
                JobRead::Finished(created) => *created,
                _ => target.result_created(&lib.load_meeting(&meeting)?),
            };
            Ok::<_, Error>((found, current))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.checking = false;
                if this.run == run && this.phase.job() == Some(&job) {
                    this.apply_job_read(&job, result, cx);
                }
                // Events (or a new run) that arrived while reading are
                // checked again.
                if std::mem::take(&mut this.recheck) {
                    this.check_job(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Moves the followed run on from one read of `job`: the job as found
    /// and the target's result `created_at` in the meeting now.
    fn apply_job_read(
        &mut self,
        job: &JobId,
        result: Result<(JobRead, Option<DateTime<FixedOffset>>)>,
        cx: &mut Context<Self>,
    ) {
        let (count, last) = self.failures_of(job, cx);
        let (baseline, failures_before, lost_answer) = self
            .request
            .as_ref()
            .map_or((self.result_created, count, false), |r| {
                (r.baseline, r.failures_before, r.lost_answer)
            });
        let superseded = matches!(result, Ok((JobRead::Superseded, _)));
        let next = match result {
            Ok((JobRead::Cancelled, _)) => Phase::Cancelled,
            Ok((JobRead::Superseded, _)) => Phase::Idle,
            // A result newer than the baseline: this run (or, the job being
            // one at a time, a later one after it) published.
            Ok((_, Some(current))) if Some(current) != baseline => Phase::Done,
            Ok((JobRead::Pending(pending), _)) => {
                if let Some(request) = &mut self.request
                    && request.generation_run.is_none()
                {
                    request.generation_run.clone_from(&pending.generation_run);
                }
                let next = if pending.running {
                    self.retrying = None;
                    Phase::Running(job.clone())
                } else {
                    self.phase.clone()
                };
                if self.pending.as_ref() != Some(&pending) {
                    self.pending = Some(pending);
                    cx.notify();
                }
                next
            }
            // The scheduler runs it again: still followed.
            Ok((JobRead::Retrying(error), _)) => {
                let error = error.unwrap_or_default();
                if self.retrying.as_ref() != Some(&error) {
                    self.retrying = Some(error);
                    cx.notify();
                }
                Phase::Queued(job.clone())
            }
            // The stored error is the job's record: said in words here,
            // never shown as stored (WRT-04).
            Ok((JobRead::Failed(error), _)) => Phase::Failed(generation_failure(
                &self.target,
                &generation_cause(error.as_deref().unwrap_or_default()),
            )),
            Ok((JobRead::Finished(created), _)) => {
                let failure = last
                    .filter(|_| count > failures_before)
                    .map(|stored| generation_cause(&stored));
                match finished_outcome(&self.target, baseline, created, failure) {
                    Outcome::Failed(_) if lost_answer && count <= failures_before => Phase::Failed(
                        "The request got no answer, and nothing new was generated.".to_owned(),
                    ),
                    outcome => finished_phase(&self.target, outcome),
                }
            }
            Err(e) => {
                tracing::warn!(code = e.code(), "reading the generation job");
                self.phase.clone()
            }
        };
        if next == self.phase {
            return;
        }
        let finished = !next.in_flight();
        let done = next == Phase::Done;
        let pending = self.pending.take();
        self.set_phase(next, cx);
        if !finished {
            self.pending = pending;
        }
        let used = if finished {
            self.request.take().and_then(|r| r.provider)
        } else {
            None
        };
        if self.phase == Phase::Cancelled || superseded {
            cx.emit(GenerateEvent::Finished);
        }
        if done {
            if let Some(provider) = used.or_else(|| self.provider.clone()) {
                self.models_after_generation(&provider, cx);
            }
            cx.emit(GenerateEvent::Finished);
        }
    }

    // ---- instructions ----

    /// Whether the instructions disclosure is open.
    #[cfg(test)]
    pub(crate) fn instructions_open(&self) -> bool {
        self.instructions.open
    }

    #[cfg(test)]
    pub(crate) fn instructions_input(&self) -> &Entity<TextareaState> {
        &self.instructions.input
    }

    /// "Save as default" result: text and whether it is an error.
    #[cfg(test)]
    pub(crate) fn instructions_notice(&self) -> Option<&(String, bool)> {
        self.instructions.notice.as_ref()
    }

    /// Opens or closes the disclosure. The summary's is filled with the
    /// saved default the first time it opens, and again later only while
    /// the user has not changed that text; collapsing keeps the draft.
    pub fn toggle_instructions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.instructions.open = !self.instructions.open;
        self.instructions.notice = None;
        if self.instructions.open {
            let saved = match self.target {
                GenerationTarget::Summary => {
                    saved_instructions(self.state.read(cx).config()).to_owned()
                }
                GenerationTarget::Template { .. } => String::new(),
            };
            let untouched = match &self.instructions.seeded {
                None => true,
                Some(seeded) => self.instructions.input.read(cx).value().trim() == seeded.trim(),
            };
            if untouched && self.instructions.seeded.as_deref() != Some(saved.as_str()) {
                self.instructions
                    .input
                    .update(cx, |input, cx| input.set_value(saved.clone(), window, cx));
                self.instructions.seeded = Some(saved);
            }
        }
        cx.notify();
    }

    /// The per-run instructions to send on stdin, if any, whether the
    /// disclosure is open or collapsed: for the summary, the draft when it
    /// differs from the saved default (an emptied field means none for this
    /// run); for a template, non-blank extra instructions. Nothing before
    /// the disclosure was first opened.
    pub fn instructions_override(&self, cx: &gpui::App) -> Option<String> {
        self.instructions.seeded.as_ref()?;
        let text = self.instructions.input.read(cx).value().to_string();
        match self.target {
            GenerationTarget::Summary => {
                (text.trim() != saved_instructions(self.state.read(cx).config())).then_some(text)
            }
            GenerationTarget::Template { .. } => (!text.trim().is_empty()).then_some(text),
        }
    }

    /// Saves the disclosure's text as the default summary instructions
    /// (`enrich instructions set --stdin`, or `clear` when blank).
    pub fn save_instructions_default(&mut self, cx: &mut Context<Self>) {
        if self.target != GenerationTarget::Summary || self.instructions.saving {
            return;
        }
        let text = self.instructions.input.read(cx).value().to_string();
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
        self.instructions.saving = true;
        self.instructions.notice = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let reload = this
                .update(cx, |this, cx| {
                    this.instructions.saving = false;
                    this.instructions.notice = Some(match &result {
                        Ok(_) => {
                            // The saved text is now what the field was
                            // filled from.
                            this.instructions.seeded = Some(text.trim().to_owned());
                            ("Saved as your default instructions.".to_owned(), false)
                        }
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
                    result.is_ok().then(|| reload_config(&this.state, cx))
                })
                .ok()
                .flatten();
            if let Some(reload) = reload {
                reload.await;
            }
        })
        .detach();
    }

    // ---- rendering ----

    /// A named radio option for the provider or model.
    #[allow(clippy::too_many_arguments)]
    /// The status line: queue position or provider, model and elapsed time
    /// while the job is followed; the outcome after.
    pub fn status(&self, cx: &gpui::App) -> Option<(String, bool)> {
        match &self.phase {
            Phase::Idle => None,
            Phase::Submitting => Some(("Queuing the generation…".into(), false)),
            Phase::Queued(_) => {
                let position = self
                    .pending
                    .as_ref()
                    .map_or_else(|| "Queued".to_owned(), |p| queued_text(p.ahead));
                Some((
                    match &self.retrying {
                        // The attempt's stored error, in words.
                        Some(stored) => [
                            failure_text(
                                "An attempt failed.",
                                &generation_cause(stored),
                                "It will be tried again.",
                            ),
                            position,
                        ]
                        .join(" "),
                        None => position,
                    },
                    false,
                ))
            }
            Phase::Running(_) => {
                let Some(pending) = &self.pending else {
                    return Some(("Generating…".into(), false));
                };
                let provider = pending
                    .provider
                    .clone()
                    .or_else(|| self.request.as_ref().and_then(|r| r.provider.clone()));
                let Some(provider) = provider else {
                    return Some(("Generating…".into(), false));
                };
                let model = pending
                    .model
                    .clone()
                    .or_else(|| self.request.as_ref().and_then(|r| r.model.clone()));
                let label =
                    run_model_label(self.state.read(cx).config(), &provider, model.as_deref());
                Some((
                    running_status_text(&self.name(&provider, cx), &label),
                    false,
                ))
            }
            Phase::Done => Some((
                match &self.target {
                    GenerationTarget::Summary => "Summary updated.".into(),
                    GenerationTarget::Template { name, .. } => format!("{name} updated."),
                },
                false,
            )),
            Phase::Cancelled => Some((
                match self.target {
                    GenerationTarget::Summary => "Cancelled. The summary was not changed.",
                    GenerationTarget::Template { .. } => "Cancelled. The result was not changed.",
                }
                .into(),
                false,
            )),
            Phase::Restored => Some(("Previous result restored.".into(), false)),
            Phase::Failed(message) => Some((message.clone(), true)),
        }
    }

    /// Elapsed seconds of a running generation job, shown in a non-live
    /// `Role::Timer` beside the live status line (ACC-18).
    pub fn running_elapsed(&self) -> Option<i64> {
        if !matches!(self.phase, Phase::Running(_)) {
            return None;
        }
        let pending = self.pending.as_ref()?;
        Some(
            (chrono::Utc::now().fixed_offset() - pending.since)
                .num_seconds()
                .max(0),
        )
    }
}

impl Render for GeneratePanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.target == GenerationTarget::Summary {
            return self.render_overview(window, cx).into_any_element();
        }
        kit::stack()
            .children(self.render_status(cx))
            .into_any_element()
    }
}

gpui::actions!(generation_options, [SubmitGeneration]);

pub(crate) const OPTIONS_CONTEXT: &str = "BroScribeGenerationOptions";
pub(crate) const AI_RESULT_NOTE: &str = "Written by AI from the transcript. It may contain mistakes. Check names, dates and decisions before you share it.";

fn destination(row: &ProviderRow) -> String {
    let destination = match row.location {
        ProviderLocation::Local => format!(
            "Runs locally via {} (Local). Transcript text, notes and instructions stay on this computer.",
            row.name
        ),
        ProviderLocation::Cloud => format!(
            "Sends the transcript text, your notes and instructions to {} (Cloud).",
            row.name
        ),
        ProviderLocation::Cli => format!(
            "Sends the transcript text, your notes and instructions to {}. That tool may use its online service.",
            row.name
        ),
        ProviderLocation::Unknown => format!(
            "Sends the transcript text, your notes and instructions to {}. Its location is unknown.",
            row.name
        ),
    };
    format!(
        "{destination} Speakers are sent as numbers. Speaker names, audio and voice data stay on this computer."
    )
}

fn provider_option(row: &ProviderRow) -> OptionItem {
    let location = match row.location {
        ProviderLocation::Local => "Local",
        ProviderLocation::Cloud => "Cloud",
        ProviderLocation::Cli => "Command-line tool",
        ProviderLocation::Unknown => "Location unknown",
    };
    OptionItem::new(&row.id, format!("{} · {location}", row.name))
}

type ProviderFocus = std::rc::Rc<std::cell::RefCell<Option<gpui::FocusHandle>>>;

impl GeneratePanel {
    fn selected_row(&self) -> Option<&ProviderRow> {
        self.provider.as_deref().and_then(|id| self.row(id))
    }

    fn disabled_reason(&self) -> Option<String> {
        if self.can_generate() {
            return None;
        }
        Some(if self.phase.in_flight() {
            "A generation is queued or running.".into()
        } else if self.meeting_busy {
            "A generation for this meeting is already queued or running.".into()
        } else if self.definition_missing {
            "This template was deleted. Its result is kept.".into()
        } else if self.providers.is_none() {
            "Listing AI providers…".into()
        } else if let Some(row) = self.selected_row() {
            if self.pending_choice.is_some() {
                "Listing models…".into()
            } else if self.refreshed_choice {
                "Choose a model again: the model list was refreshed.".into()
            } else {
                format!("Choose a model: {} has no configured model.", row.name)
            }
        } else {
            "Choose a provider to generate.".into()
        })
    }

    fn seed_other(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(model) = self.preselected_other.take() {
            self.other
                .update(cx, |input, cx| input.set_value(model, window, cx));
        }
    }

    fn generation_fields(&self, provider_focus: Option<ProviderFocus>, cx: &Context<Self>) -> Div {
        let idle = !self.phase.in_flight();
        let rows = self.providers.as_ref().and_then(|v| v.as_ref().ok());
        let provider = Picker::new(
            "generate-provider",
            "Provider",
            rows.into_iter().flatten().map(provider_option).collect(),
            self.provider.as_deref().unwrap_or_default(),
            cx,
            |this, id, _, cx| this.choose_provider(id.to_owned(), cx),
        )
        .disabled(!idle)
        .when_some(provider_focus, Picker::track_control_focus);
        let field = |id, label, control: gpui::AnyElement| {
            div()
                .flex()
                .gap_2()
                .items_center()
                .child(text_line(id, Role::Label, label).w(rems(6.)).flex_none())
                .child(div().flex_1().min_w_0().child(control))
        };
        let mut form = kit::stack().child(field(
            "generate-provider-label",
            "Provider",
            provider.into_any_element(),
        ));
        if let Some(row) = self.selected_row() {
            form = form.child(self.destination_line(row, cx));
            let list = self.model_list(cx);
            let config = self.state.read(cx).config();
            let mut options = Vec::new();
            if let Some(label) = default_option(config, row) {
                options.push(
                    OptionItem::new("default", label).detail(
                        list.as_ref()
                            .and_then(|list| default_resolved(config, row, list))
                            .map(|model| format!("last run: {model}")),
                    ),
                );
            }
            if let Some(list) = &list {
                options.extend(list.models.iter().map(|model| {
                    OptionItem::new(format!("model:{}", model.id), model.label()).detail(
                        model
                            .resolved
                            .as_ref()
                            .filter(|value| *value != &model.id)
                            .map(|value| format!("last run: {value}")),
                    )
                }));
            }
            options.push(OptionItem::new("other", "Other…"));
            let selected = match &self.choice {
                Some(ModelChoice::Default) => "default".into(),
                Some(ModelChoice::Other) => "other".into(),
                Some(ModelChoice::Listed(id)) => format!("model:{id}"),
                None => String::new(),
            };
            let listing = self.models.loading && self.models.previous.is_none();
            let model = Picker::new(
                "generate-model",
                "Model",
                options,
                if listing { String::new() } else { selected },
                cx,
                |this, id, _, cx| {
                    let choice = match id {
                        "default" => ModelChoice::Default,
                        "other" => ModelChoice::Other,
                        id => ModelChoice::Listed(id.trim_start_matches("model:").to_owned()),
                    };
                    this.choose_model(choice, cx);
                },
            )
            .placeholder(if listing {
                "Listing models…"
            } else {
                "Choose model"
            })
            .disabled(!idle || listing);
            form = form.child(field(
                "generate-model-label",
                "Model",
                div()
                    .flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().min_w_0().child(model))
                    .child(
                        button(
                            "models-refresh",
                            "Refresh",
                            idle && !self.models.loading,
                            cx,
                            |this, _, cx| this.refresh_models(cx),
                        )
                        .ghost()
                        .small()
                        .icon(gpui::component::IconName::RefreshCw)
                        .tooltip("Get the provider’s current model list"),
                    )
                    .into_any_element(),
            ));
            if self.models.loading {
                form = form.child(status_line(
                    self.live_id("models-loading"),
                    StatusKind::Pending,
                    "Listing models…",
                    cx,
                ));
            } else if list.as_ref().is_some_and(|list| list.models.is_empty()) {
                form = form.child(
                    text_line(
                        "models-unlisted",
                        Role::Label,
                        format!(
                            "{} doesn't list its models. Choose Other… to type one.",
                            row.name
                        ),
                    )
                    .text_role(TextRole::Caption)
                    .text_color(cx.theme().muted_foreground),
                );
            }
            if let Some(error) = &self.models.error {
                form = form.child(kit::retry_line_with_id(
                    self.live_id("models-error"),
                    self.live_id("models-retry"),
                    error.clone(),
                    cx,
                    |this, _, cx| this.refresh_models(cx),
                ));
            }
            if self.choice == Some(ModelChoice::Other) {
                form = form.child(kit::field(
                    "model-other-label",
                    "Model name",
                    None,
                    |label| Input::new(&self.other).aria_label(label).disabled(!idle),
                    cx,
                ));
                if let Some(error) = &self.other_error {
                    form = form.child(status_line(
                        self.live_id("model-other-error"),
                        StatusKind::Failed,
                        error.clone(),
                        cx,
                    ));
                }
            }
        }
        if let Some(reason) = self
            .disabled_reason()
            .filter(|_| !self.models.loading || self.pending_choice.is_none())
        {
            form = form.child(status_line(
                self.live_id("disabled"),
                StatusKind::Info,
                reason,
                cx,
            ));
        }
        form.children(self.render_instructions(idle, cx))
    }

    fn destination_line(&self, row: &ProviderRow, cx: &App) -> gpui::Stateful<Div> {
        text_line("generate-destination", Role::Label, destination(row))
            .text_role(TextRole::Caption)
            .text_color(cx.theme().muted_foreground)
    }

    fn narrow_options(&self, window: &Window) -> bool {
        let width = if self.options_width > gpui::px(0.) {
            self.options_width
        } else {
            window.viewport_size().width
        };
        width < window.rem_size() * 30.
    }

    pub(crate) fn show_options(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ensure_loaded(cx);
        self.open_options(OptionsAnchor::Header, window, cx);
    }

    fn open_options(&mut self, anchor: OptionsAnchor, window: &mut Window, cx: &mut Context<Self>) {
        if self.options.is_some() || self.phase.in_flight() {
            return;
        }
        self.copied = false;
        self.options = Some(anchor);
        if anchor == OptionsAnchor::Header {
            self.header_focus.focus(window, cx);
        }
        self.options_opener = window.focused(cx);
        self.focus_provider = true;
        self.options_dialog = self.narrow_options(window)
            && window.root::<gpui::component::Root>().flatten().is_some();
        if self.options_dialog {
            let panel = cx.weak_entity();
            let close = panel.clone();
            let width = (window.rem_size() * 25.).min(window.viewport_size().width - gpui::px(24.));
            window.open_dialog(cx, move |dialog, window, cx| {
                // BroScribe's dialogs stand on the popover ground, not the desk.
                let dialog = dialog.bg(cx.theme().popover);
                let content = panel
                    .update(cx, |panel, cx| panel.options_content(window, cx))
                    .ok();
                let close = close.clone();
                dialog
                    .width(width)
                    .close_button(false)
                    .children(content)
                    .on_close(move |_, _, cx| {
                        close
                            .update(cx, |panel, cx| {
                                panel.options = None;
                                panel.options_dialog = false;
                                panel.options_opener = None;
                                cx.notify();
                            })
                            .ok();
                    })
            });
        }
        cx.notify();
    }

    fn close_options(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.options = None;
        self.options_focus_out = None;
        if std::mem::take(&mut self.options_dialog) {
            window.close_dialog(cx);
        }
        if let Some(opener) = self.options_opener.take() {
            opener.focus(window, cx);
        }
        cx.notify();
    }

    fn submit_options(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.generate(window, cx);
        if self.phase.in_flight() {
            self.close_options(window, cx);
        }
    }

    fn settings_button(&self, cx: &Context<Self>) -> Button {
        button(
            "open-ai-providers",
            "Set up AI providers",
            true,
            cx,
            |this, _, cx| {
                this.open_provider_settings(cx);
            },
        )
        .outline()
        .small()
    }

    fn options_content(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        self.seed_other(window, cx);
        let (title, label, replace_note, no_provider) = match (&self.target, self.has_enrichment())
        {
            (GenerationTarget::Summary, false) => (
                "Generate summary".to_owned(),
                "Generate",
                None,
                "Turn on an AI provider to generate a summary.",
            ),
            (GenerationTarget::Summary, true) => (
                "Regenerate summary, decisions and action items".to_owned(),
                "Regenerate",
                Some("Replaces the summary. Action item statuses you set are kept."),
                "Turn on an AI provider to generate a summary.",
            ),
            (GenerationTarget::Template { name, .. }, has_result) => {
                let label = if has_result { "Run again" } else { "Run" };
                (
                    format!("{label} {name}"),
                    label,
                    has_result.then_some("Running again replaces this result."),
                    "Turn on an AI provider to run this template.",
                )
            }
        };
        let mut body = kit::stack()
            .key_context(OPTIONS_CONTEXT)
            .on_action(cx.listener(|this, _: &SubmitGeneration, window, cx| {
                this.submit_options(window, cx);
            }))
            .child(heading("generate-title", title));
        if matches!(&self.providers, Some(Ok(rows)) if !rows.is_empty()) {
            let focus = std::mem::take(&mut self.focus_provider).then(|| {
                let focus = std::rc::Rc::new(std::cell::RefCell::new(None::<gpui::FocusHandle>));
                let target = focus.clone();
                window.defer(cx, move |window, cx| {
                    if let Some(handle) = target.borrow().as_ref() {
                        handle.focus(window, cx);
                    }
                });
                focus
            });
            body = body.child(self.generation_fields(focus, cx));
        } else if let Some(status) = self.provider_status(cx) {
            body = body.child(status);
        } else {
            body = body.child(text_line("options-no-provider", Role::Status, no_provider));
            body = body.child(self.settings_button(cx));
        }
        if let Some(note) = replace_note {
            body = body.child(
                text_line("generate-replace-note", Role::Label, note)
                    .text_role(TextRole::Caption)
                    .text_color(cx.theme().muted_foreground),
            );
        }
        let instructions_valid = self
            .instructions_override(cx)
            .as_deref()
            .is_none_or(|text| validate_instructions(text).is_ok());
        body.child(
            div()
                .flex()
                .justify_end()
                .gap_2()
                .child(
                    button(
                        "generation-cancel",
                        "Cancel",
                        true,
                        cx,
                        |this, window, cx| this.close_options(window, cx),
                    )
                    .ghost(),
                )
                .child(
                    button(
                        "generate",
                        label,
                        self.can_generate() && instructions_valid,
                        cx,
                        |this, window, cx| this.submit_options(window, cx),
                    )
                    .when_some(self.disabled_reason(), |button, reason| {
                        kit::disable_with_reason(button, true, reason, cx)
                    }),
                ),
        )
    }

    fn provider_status(&self, cx: &Context<Self>) -> Option<gpui::AnyElement> {
        match &self.providers {
            None => Some(
                status_line(
                    self.live_id("providers-loading"),
                    StatusKind::Pending,
                    "Listing AI providers…",
                    cx,
                )
                .into_any_element(),
            ),
            Some(Err(error)) => Some(
                kit::retry_line_with_id(
                    self.live_id("providers-error"),
                    "providers-retry",
                    error.clone(),
                    cx,
                    |this, _, cx| this.retry_providers(cx),
                )
                .into_any_element(),
            ),
            Some(Ok(_)) => None,
        }
    }

    /// The tab owns catalog availability. The row loads models when Run opens.
    pub(crate) fn template_run_trigger(
        &self,
        window: &Window,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let GenerationTarget::Template { id, name } = &self.target else {
            unreachable!("only templates have Run controls");
        };
        let (verb, label) = if self.has_enrichment() {
            ("Run again", format!("Run {name} again…"))
        } else {
            ("Run", format!("Run {name}…"))
        };
        let enabled = !self.phase.in_flight();
        let trigger = kit::disable_action(
            Button::new(SharedString::from(format!("template-run-{id}")))
                .label(format!("{verb}…"))
                .accessibility_label(label)
                .outline()
                .small()
                .track_focus(&self.header_focus),
            !enabled,
            cx,
        );
        self.options_trigger(trigger, enabled, window, cx)
    }

    fn options_trigger(
        &self,
        trigger: Button,
        enabled: bool,
        window: &Window,
        cx: &Context<Self>,
    ) -> gpui::AnyElement {
        let open = self.options.is_some() && !self.options_dialog;
        if !open {
            if !enabled {
                return trigger.into_any_element();
            }
            if self.narrow_options(window)
                && window.root::<gpui::component::Root>().flatten().is_some()
            {
                return trigger
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.show_options(window, cx);
                    }))
                    .into_any_element();
            }
        }
        let entity = cx.weak_entity();
        Popover::new("generate-options")
            .anchor(gpui::Anchor::TopRight)
            .w(rems(25.))
            .p_4()
            .trigger(trigger)
            .open(open)
            .overlay_closable(true)
            .on_open_change(cx.listener(|this, open: &bool, window, cx| {
                if *open {
                    this.ensure_loaded(cx);
                    this.options = Some(OptionsAnchor::Header);
                    this.focus_provider = true;
                    this.options_opener = Some(this.header_focus.clone());
                    this.copied = false;
                } else {
                    this.close_options(window, cx);
                }
                cx.notify();
            }))
            .content(move |popover, window, cx| {
                entity
                    .update(cx, |this, cx| {
                        if this.options_focus_out.is_none() {
                            this.options_focus_out = Some(cx.on_focus_out(
                                &popover.focus_handle(cx),
                                window,
                                |this, _, _, cx| {
                                    this.options = None;
                                    this.options_opener = None;
                                    this.options_focus_out = None;
                                    cx.notify();
                                },
                            ));
                        }
                        this.options_content(window, cx)
                    })
                    .unwrap_or_else(|_| div())
            })
            .into_any_element()
    }

    fn render_overview(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        self.seed_other(window, cx);
        let has_result = self.summary_credit.is_some();
        let no_providers = matches!(&self.providers, Some(Ok(rows)) if rows.is_empty());
        let enabled = !self.phase.in_flight()
            && matches!(&self.providers, Some(Ok(rows)) if !rows.is_empty());
        let label = if self.phase.in_flight() {
            "Generating…"
        } else if has_result {
            "Regenerate…"
        } else {
            "Options…"
        };
        let trigger = kit::disable_action(
            Button::new("regenerate")
                .label(label)
                .outline()
                .small()
                .track_focus(&self.header_focus),
            !enabled,
            cx,
        );
        let mut header = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(heading("overview-summary", "Summary"))
            .child(div().flex_1())
            .children(self.restore_control(cx))
            .when(has_result, |row| {
                row.child(
                    button("overview-copy", "Copy", true, cx, |_, _, cx| {
                        cx.emit(GenerateEvent::CopySummary)
                    })
                    .ghost()
                    .small()
                    .icon(gpui::component::IconName::Copy)
                    .accessibility_label("Copy the summary, decisions and action items"),
                )
            })
            .when(self.copied, |row| {
                row.child(status_line(
                    "overview-copied",
                    StatusKind::Done,
                    "Copied.",
                    cx,
                ))
            });
        let has_open_popover = self.options.is_some() && !self.options_dialog;
        if has_result && no_providers && !has_open_popover && !self.phase.in_flight() {
            header = header
                .child(
                    text_line(
                        "summary-provider-needed",
                        Role::Status,
                        "To regenerate, turn on an AI provider.",
                    )
                    .text_role(TextRole::Caption),
                )
                .child(self.settings_button(cx).link());
        } else {
            header = header.child(self.options_trigger(trigger, enabled, window, cx));
        }
        let mut panel = kit::stack().child(header);
        if let Some(credit) = &self.summary_credit {
            let (words, spoken) = crate::views::meeting::credit_text(
                "AI summary",
                &self.name(&credit.provider, cx),
                credit.model.as_deref(),
                credit.requested.as_deref(),
                credit.reported,
                (
                    &crate::ui::format::local_date_time(&credit.created),
                    &crate::ui::format::spoken_date_time(&credit.created),
                ),
            );
            panel = panel.child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .id("enrichment-credit")
                            .aria_label(spoken)
                            .role(Role::Label)
                            .text_role(TextRole::Caption)
                            .text_color(cx.theme().muted_foreground)
                            .child(words),
                    )
                    .children(
                        credit
                            .instructions
                            .as_deref()
                            .map(str::trim)
                            .filter(|text| !text.is_empty())
                            .map(credit_instructions),
                    ),
            );
            if self.stale {
                panel = panel.child(div().flex().items_center().flex_wrap().gap_2()
                    .child(status_line("summary-stale", StatusKind::Attention,
                        "Out of date: the transcript or its names changed after this summary was written.", cx))
                    .when(enabled, |row| row.child(
                        button("stale-regenerate", "Regenerate…", true, cx, |this, window, cx| {
                            this.open_options(OptionsAnchor::Stale, window, cx);
                        }).link().small())));
            }
            panel = panel.child(
                text_line("summary-ai-note", Role::Label, AI_RESULT_NOTE)
                    .text_role(TextRole::Caption)
                    .text_color(crate::ui::brand::brand(cx).ai_foreground),
            );
        }
        if let Some(line) = self.render_status(cx) {
            panel = panel.child(line);
        }
        if self.options.is_none() {
            panel = panel.children(self.provider_status(cx));
        }
        if has_result || self.phase.in_flight() || matches!(self.phase, Phase::Failed(_)) {
            return panel;
        }
        if no_providers {
            panel = panel.child(kit::empty_state("summary-no-provider-title", "Summaries need an AI provider",
                text_line("providers-none", Role::Status, "Turn on an AI provider to write a summary, the decisions and the action items from your transcripts. Providers are off until you turn one on, and nothing is sent until you choose Generate summary."),
                false, None, Some(self.settings_button(cx)), cx));
        } else if let Some(row) = self.selected_row() {
            let mut primary = button(
                "generate-summary",
                "Generate summary",
                self.can_generate(),
                cx,
                |this, window, cx| this.generate(window, cx),
            );
            if let Some(reason) = self.disabled_reason() {
                primary = primary.tooltip(reason.clone());
                panel = panel.child(status_line(
                    self.live_id("disabled"),
                    StatusKind::Pending,
                    reason,
                    cx,
                ));
            }
            panel = panel.child(kit::empty_state("summary-empty", "No summary yet",
                text_line("summary-empty-description", Role::Label,
                    format!("{} can write a summary, the decisions and the action items from this transcript.", row.name)),
                false, Some(primary), None, cx)).child(self.destination_line(row, cx));
        }
        panel
    }
}

fn credit_instructions(instructions: &str) -> Popover {
    let text: SharedString = instructions.to_owned().into();
    Popover::new("credit-instructions-popover")
        .anchor(gpui::Anchor::TopRight)
        .w(rems(25.))
        .overlay_closable(true)
        .trigger(
            Button::new("credit-instructions")
                .link()
                .xsmall()
                .label("custom instructions")
                .tooltip(text.clone()),
        )
        .child(
            div()
                .id("credit-instructions-text")
                .role(Role::Label)
                .aria_label(text.clone())
                .max_h(rems(15.))
                .overflow_y_scroll()
                .child(SelectableText::new("credit-instructions-run", text)),
        )
}

impl GeneratePanel {
    /// The ID of one of the panel's live (Status or Alert) lines, with the
    /// target in it: the live announcer keys a window's lines by ID, so two
    /// panels open in one window (two templates) never share one, and one
    /// panel's text is never taken for a change of the other's (ACC-01).
    fn live_id(&self, part: &str) -> SharedString {
        match &self.target {
            GenerationTarget::Summary => format!("generate-summary-{part}"),
            GenerationTarget::Template { id, .. } => format!("generate-template-{id}-{part}"),
        }
        .into()
    }

    pub(crate) fn set_can_restore(&mut self, can_restore: bool, cx: &mut Context<Self>) {
        if self.can_restore != can_restore {
            self.can_restore = can_restore;
            cx.notify();
        }
    }

    pub(crate) fn restore_control(&self, cx: &Context<Self>) -> Option<Button> {
        let expected = self
            .result_created
            .filter(|_| self.can_restore && !self.phase.in_flight() && !self.meeting_busy)?;
        let label = match self.target {
            GenerationTarget::Summary => "Restore previous summary",
            GenerationTarget::Template { .. } => "Restore previous result",
        };
        Some(
            button(
                self.live_id("restore"),
                label,
                !self.editing_generation,
                cx,
                move |this, _, cx| {
                    if this.result_created == Some(expected)
                        && this.can_restore
                        && !this.phase.in_flight()
                        && !this.meeting_busy
                    {
                        let meeting = this.meeting_id.as_str();
                        let date = expected.to_rfc3339();
                        let command = match &this.target {
                            GenerationTarget::Summary => {
                                args(["enrich", "restore", meeting, "--expect", &date])
                            }
                            GenerationTarget::Template { id, .. } => {
                                args(["templates", "restore", meeting, id, "--expect", &date])
                            }
                        };
                        this.edit_generation(command, Phase::Restored, cx);
                    }
                },
            )
            .link()
            .small(),
        )
    }

    fn cancel_control(&self, cx: &Context<Self>) -> Option<Button> {
        let job = self.phase.job()?.clone();
        let token = self.request.as_ref()?.generation_run.clone()?;
        let follow = self.run;
        Some(
            button(
                self.live_id("cancel"),
                "Cancel",
                !self.editing_generation,
                cx,
                move |this, _, cx| {
                    if this.run == follow && this.phase.job() == Some(&job) {
                        this.edit_generation(
                            args(["queue", "cancel", job.as_str(), "--run", &token]),
                            Phase::Cancelled,
                            cx,
                        );
                    }
                },
            )
            .ghost()
            .small(),
        )
    }

    fn edit_generation(
        &mut self,
        command: Vec<std::ffi::OsString>,
        outcome: Phase,
        cx: &mut Context<Self>,
    ) {
        if self.editing_generation {
            return;
        }
        self.edit_error = None;
        self.editing_generation = true;
        let follow = self.run;
        let task = self.state.read(cx).run_cli(command, ChildKind::Command, cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.editing_generation = false;
                if this.run == follow {
                    match result {
                        Ok(_) => {
                            if outcome == Phase::Restored {
                                this.can_restore = false;
                            }
                            this.meeting_busy = false;
                            this.request = None;
                            this.set_phase(outcome, cx);
                            cx.emit(GenerateEvent::Finished);
                        }
                        Err(error) => {
                            this.edit_error = Some(error.user_text());
                            this.check_job(cx);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The status line and, while a job is followed, the elapsed timer and hint.
    pub(crate) fn render_status(&self, cx: &Context<Self>) -> Option<Div> {
        let Some((mut text, error)) = self.status(cx) else {
            return self.edit_error.as_ref().map(|error| {
                div().child(status_line(
                    self.live_id("edit-error"),
                    StatusKind::Failed,
                    error.clone(),
                    cx,
                ))
            });
        };
        let failed_attempt = self
            .retrying
            .as_ref()
            .filter(|_| matches!(self.phase, Phase::Queued(_)));
        if failed_attempt.is_some() {
            text = self
                .pending
                .as_ref()
                .map_or_else(|| "Queued".to_owned(), |p| queued_text(p.ahead));
        }
        let elapsed = self.running_elapsed();
        let theme = cx.theme();
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .children(self.edit_error.as_ref().map(|error| {
                    status_line(
                        self.live_id("edit-error"),
                        StatusKind::Failed,
                        error.clone(),
                        cx,
                    )
                }))
                .children(failed_attempt.map(|error| {
                    status_line(
                        self.live_id("retrying"),
                        StatusKind::Attention,
                        generation_failure(&self.target, &generation_cause(error)),
                        cx,
                    )
                }))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child({
                            let kind = if error {
                                StatusKind::Failed
                            } else if self.phase.in_flight() {
                                StatusKind::Pending
                            } else {
                                StatusKind::Done
                            };
                            if elapsed.is_some() {
                                crate::ui::kit::status_without_wait_clock(
                                    self.live_id("status"),
                                    kind,
                                    text,
                                    cx,
                                )
                            } else {
                                status_line(self.live_id("status"), kind, text, cx)
                            }
                        })
                        .children(self.cancel_control(cx))
                        .when_some(elapsed, |el, seconds| {
                            el.child(
                                timer_line(
                                    "generate-elapsed",
                                    "Elapsed time",
                                    spoken_duration(seconds.max(0) as u64),
                                    elapsed_text(seconds),
                                )
                                .text_role(TextRole::Caption)
                                .text_color(theme.muted_foreground),
                            )
                        })
                        .when(error && self.target == GenerationTarget::Summary, |row| {
                            let retryable = !self.meeting_busy
                                && self.last_submission.as_ref().is_some_and(|submission| {
                                    self.row(&submission.selection.provider).is_some()
                                });
                            row.when(retryable && !self.consent_refusal, |row| {
                                row.child(
                                    button(
                                        "generation-retry",
                                        "Try again",
                                        true,
                                        cx,
                                        |this, window, cx| this.retry_generation(window, cx),
                                    )
                                    .outline()
                                    .small(),
                                )
                            })
                            .child(
                                button(
                                    "generation-failure-options",
                                    "Options…",
                                    true,
                                    cx,
                                    |this, window, cx| {
                                        this.open_options(OptionsAnchor::Failure, window, cx)
                                    },
                                )
                                .ghost()
                                .small(),
                            )
                            .when(self.consent_refusal, |row| {
                                row.child(self.settings_button(cx).link())
                            })
                        }),
                )
                .children(self.phase.in_flight().then(|| {
                    text_line("generate-hint", Role::Label, GENERATION_HINT)
                        .text_role(TextRole::Caption)
                        .text_color(theme.muted_foreground)
                })),
        )
    }

    fn add_instruction_example(
        &mut self,
        example: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let input = &self.instructions.input;
        let mut draft = input.read(cx).value().to_string();
        if draft.lines().any(|line| line == example) {
            return;
        }
        if !draft.is_empty() && !draft.ends_with('\n') {
            draft.push('\n');
        }
        draft.push_str(example);
        input.update(cx, |input, cx| input.set_value(draft, window, cx));
        cx.notify();
    }

    /// The "Instructions" disclosure (summary) or "Extra instructions"
    /// (template).
    fn render_instructions(&self, idle: bool, cx: &Context<Self>) -> Option<Div> {
        let summary = self.target == GenerationTarget::Summary;
        let title = if summary {
            "Instructions"
        } else {
            "Extra instructions"
        };
        // A collapsed draft still applies. Keep that fact in the disclosure's title.
        let pending = !self.instructions.open && self.instructions_override(cx).is_some();
        let label = if pending {
            format!("{title} · changed for this run")
        } else {
            title.to_owned()
        };
        let mut el = crate::ui::kit::stack()
            .w_full()
            .child(crate::ui::kit::disclosure(
                "instructions-toggle",
                label,
                self.instructions.open,
                cx,
                |this, window, cx| this.toggle_instructions(window, cx),
            ));
        if !self.instructions.open {
            return Some(el);
        }
        let help = if summary {
            "What the summary should focus on. Changes apply to this run. Save as default keeps them for every summary. The summary keeps its structure."
        } else {
            "Added to the template's instructions for this run only."
        };
        el = el.child(crate::ui::kit::field(
            "instructions-label",
            title,
            Some(("instructions-help".into(), help.into())),
            |label| {
                div().key_context("GenerationInstructions").child(
                    Textarea::new(&self.instructions.input)
                        .w_full()
                        .h(gpui::rems(6.))
                        .aria_label(label)
                        .disabled(!idle),
                )
            },
            cx,
        ));
        let length = self.instructions.input.read(cx).value().chars().count();
        let role = if length >= INSTRUCTIONS_MAX_CHARS * 9 / 10 {
            Role::Status
        } else {
            Role::Label
        };
        el = el
            .child(
                text_line(
                    "instructions-count",
                    role,
                    format!("{length} of 4,000 characters"),
                )
                .text_role(TextRole::Caption)
                .text_color(cx.theme().muted_foreground),
            )
            .child(
                div().flex().flex_wrap().gap_2().children(
                    ["Keep it short", "Focus on decisions", "List open questions"]
                        .into_iter()
                        .enumerate()
                        .map(|(ix, example)| {
                            button(
                                ("instructions-example", ix),
                                example,
                                idle,
                                cx,
                                move |this, window, cx| {
                                    this.add_instruction_example(example, window, cx)
                                },
                            )
                            .ghost()
                            .small()
                        }),
                ),
            );
        if summary {
            el = el.child(button(
                "instructions-save",
                "Save as default",
                idle && !self.instructions.saving && length <= INSTRUCTIONS_MAX_CHARS,
                cx,
                |this, _, cx| this.save_instructions_default(cx),
            ));
        }
        if let Some((text, error)) = &self.instructions.notice {
            el = el.child(status_line(
                self.live_id("instructions-notice"),
                if *error {
                    StatusKind::Failed
                } else {
                    StatusKind::Done
                },
                text.clone(),
                cx,
            ));
        }
        Some(el)
    }
}

/// Session cache of model lists, held by [`AppState`].
#[derive(Default)]
pub struct ModelLists(HashMap<ModelCacheKey, ModelList>);

impl ModelLists {
    pub fn get(&self, key: &ModelCacheKey) -> Option<&ModelList> {
        self.0.get(key)
    }

    /// Stores `list`, dropping the provider's lists under older configs.
    pub fn insert(&mut self, key: ModelCacheKey, list: ModelList) {
        self.0.retain(|k, _| k.0 != key.0);
        self.0.insert(key, list);
    }

    pub fn remove(&mut self, key: &ModelCacheKey) {
        self.0.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use broscribe_core::config::ProviderConfig;
    use broscribe_core::queue::ENRICH_KIND;
    use broscribe_core::schema::Meeting;
    use broscribe_core::{Library, MeetingId};
    use broscribe_platform::fakes::FakeProcessProbe;
    use gpui::{TestAppContext, WindowHandle};
    use serde_json::json;

    use super::*;
    use crate::jobs::{ProgressEvent, ProgressEventKind, WorkerBatch, WorkerMsg};
    use crate::views::meeting::{MeetingTab, MeetingView};
    use crate::views::person_picker::fixtures::{self, Synthetic};

    fn row(id: &str, purpose: &str, enabled: bool, consented: bool, http: bool) -> ProviderRow {
        ProviderRow {
            id: id.into(),
            name: id.into(),
            purpose: Some(purpose.into()),
            enabled,
            consented,
            status: None,
            consent_text: None,
            http,
            location: ProviderLocation::Unknown,
            has_key: None,
        }
    }

    #[test]
    fn args_filtering_defaults_and_parsing() {
        let m = MeetingId::parse("m-0000a1").unwrap();
        let summary = GenerationTarget::Summary;
        assert_eq!(
            generate_args(&m, &summary, "synthetic-agent", Some("alpha-1"), false),
            args([
                "enrich",
                "m-0000a1",
                "--queue",
                "--provider",
                "synthetic-agent",
                "--model",
                "alpha-1"
            ])
        );
        assert_eq!(
            generate_args(&m, &summary, "synthetic-agent", None, true),
            args([
                "enrich",
                "m-0000a1",
                "--queue",
                "--provider",
                "synthetic-agent",
                "--instructions-stdin"
            ])
        );
        let template = GenerationTarget::Template {
            id: "synthetic-notes".into(),
            name: "Synthetic notes".into(),
        };
        assert_eq!(
            generate_args(&m, &template, "synthetic-agent", None, false),
            args([
                "templates",
                "run",
                "m-0000a1",
                "synthetic-notes",
                "--queue",
                "--provider",
                "synthetic-agent"
            ])
        );

        let usable = usable_providers(vec![
            row("ready", "llm", true, true, false),
            row("off", "llm", false, true, false),
            row("no-consent", "llm", true, false, false),
            row("embed", "text_embedding", true, true, true),
            row("remote", "llm", true, true, true),
        ]);
        let ids: Vec<&str> = usable.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["ready", "remote"]);

        let mut config = Config::default();
        assert_eq!(
            default_option(&config, &usable[0]).as_deref(),
            Some("Provider default")
        );
        // HTTP providers need a model: no default without a configured one.
        assert_eq!(default_option(&config, &usable[1]), None);
        let before = model_cache_key("ready", &config);
        config.providers.insert(
            "ready".into(),
            ProviderConfig {
                model: Some("configured-x".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            default_option(&config, &usable[0]).as_deref(),
            Some("Configured model (configured-x)")
        );
        assert_ne!(model_cache_key("ready", &config), before);

        let list = parse_model_list(&json!({
            "provider": "ready", "source": "cli", "configured_model": null,
            "models": [
                {"id": "alpha-1", "name": "Alpha One", "default": true},
                {"id": "beta-2", "name": null, "default": false},
                {"name": "no id"}
            ]
        }))
        .unwrap();
        assert_eq!(list.source, "cli");
        let labels: Vec<String> = list.models.iter().map(ListedModel::label).collect();
        assert_eq!(labels, ["Alpha One (alpha-1) — provider default", "beta-2"]);
        assert!(parse_model_list(&json!({"provider": "ready"})).is_err());

        let busy = CliError::answered("library_busy", "x");
        assert!(
            refusal_text(&busy, &GenerationTarget::Summary).contains("already being generated")
        );
    }

    #[test]
    fn feedback_texts_and_instruction_commands() {
        assert_eq!(elapsed_text(0), "0:00");
        assert_eq!(elapsed_text(83), "1:23");
        assert_eq!(elapsed_text(-5), "0:00");
        assert_eq!(elapsed_text(3_725), "1:02:05");
        assert_eq!(queued_text(0), "Queued · next to run");
        assert_eq!(queued_text(1), "Queued · 1 job ahead");
        assert_eq!(queued_text(3), "Queued · 3 jobs ahead");
        assert_eq!(
            running_text("Synthetic Agent", "alpha-1", 83),
            "Generating with Synthetic Agent · alpha-1 · 1:23"
        );
        let mut config = Config::default();
        assert_eq!(run_model_label(&config, "ready", None), "provider default");
        assert_eq!(run_model_label(&config, "ready", Some("beta-2")), "beta-2");
        config.providers.insert(
            "ready".into(),
            ProviderConfig {
                model: Some("configured-x".into()),
                ..Default::default()
            },
        );
        assert_eq!(run_model_label(&config, "ready", None), "configured-x");

        // Instructions never reach argv.
        let (set, stdin) = save_instructions_command("Focus on the synthetic budget.");
        assert_eq!(set, args(["enrich", "instructions", "set", "--stdin"]));
        assert_eq!(
            stdin.as_deref(),
            Some(&b"Focus on the synthetic budget."[..])
        );
        let (clear, stdin) = save_instructions_command("  \n");
        assert_eq!(clear, args(["enrich", "instructions", "clear"]));
        assert!(stdin.is_none());
    }

    #[test]
    fn picker_shows_what_aliases_and_the_default_resolved_to() {
        let list = parse_model_list(&json!({
            "provider": "ready", "source": "help",
            "models": [
                {"id": "large", "resolved": "synthetic-large-2"},
                {"id": "small"},
                {"id": "exact-1", "resolved": "exact-1"},
                {"id": "blank", "resolved": "  "}
            ],
            "default_resolved": "synthetic-medium-3"
        }))
        .unwrap();
        assert_eq!(list.default_resolved.as_deref(), Some("synthetic-medium-3"));
        assert_eq!(
            list.models[0].resolved.as_deref(),
            Some("synthetic-large-2")
        );
        assert_eq!(list.models[3].resolved, None);

        let ready = row("ready", "llm", true, true, false);
        let mut config = Config::default();
        let (first, listed) = model_option_texts(&config, &ready, Some(&list));
        let (text, aria) = first.unwrap();
        assert_eq!(text, "Provider default — synthetic-medium-3 (last run)");
        assert!(aria.contains("used model synthetic-medium-3"), "{aria}");
        let texts: Vec<&str> = listed.iter().map(|(t, _)| t.as_str()).collect();
        // A hint that only repeats the ID is not shown.
        assert_eq!(
            texts,
            [
                "large — synthetic-large-2 (last run)",
                "small",
                "exact-1",
                "blank"
            ]
        );
        assert_eq!(
            listed[0].1,
            "large; the last summary generated with it used model synthetic-large-2"
        );
        assert_eq!(listed[1].1, "small");

        // A configured model takes its listed entry's hint, not the default's.
        config.providers.insert(
            "ready".into(),
            ProviderConfig {
                model: Some("large".into()),
                ..Default::default()
            },
        );
        let (first, _) = model_option_texts(&config, &ready, Some(&list));
        assert_eq!(
            first.unwrap().0,
            "Configured model (large) — synthetic-large-2 (last run)"
        );
        config.providers.get_mut("ready").unwrap().model = Some("unlisted".into());
        let (first, _) = model_option_texts(&config, &ready, Some(&list));
        assert_eq!(first.unwrap().0, "Configured model (unlisted)");
        // Nothing listed yet: no hint.
        let (first, listed) = model_option_texts(&Config::default(), &ready, None);
        assert_eq!(first.unwrap().0, "Provider default");
        assert!(listed.is_empty());
    }

    /// A synthetic `broscribe`: argv to `args.log`; `providers list` prints
    /// `providers.json`; `providers models <id>` logs the ID to `models.log`
    /// and prints `models.json` (or fails while `models-fail` exists);
    /// `enrich` answers queued with `job` (or `library_busy` while `busy`
    /// exists). Nothing reaches a vendor.
    fn synthetic_cli(dir: &Path, job: &JobId, meeting_file: &Path) -> PathBuf {
        use crate::test_support::{ps_path, ps_str, write_cli};
        let d = dir.display();
        let meeting = meeting_file.display();
        std::fs::write(
            dir.join("providers.json"),
            json!({"providers": [
                {"id": "agent-a", "name": "Synthetic Agent", "purpose": "llm", "location":"cli",
                 "runtime": "agent-cli", "enabled": true, "consented": true},
                {"id": "agent-off", "name":"Disabled Synthetic Agent", "purpose": "llm", "runtime": "agent-cli",
                 "enabled": false, "consented": false},
                {"id": "remote", "name": "Synthetic Remote", "purpose": "llm",
                 "runtime": "openai-compatible-http", "enabled": true,
                 "consented_at": "2026-11-15T00:00:00+00:00"},
                {"id": "embed", "purpose": "text_embedding",
                 "runtime": "openai-compatible-http", "enabled": true, "consented": true}
            ]})
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join("models.json"),
            json!({"provider": "agent-a", "source": "cli", "configured_model": null,
                   "models": [{"id": "alpha-1", "name": "Alpha One", "default": true},
                              {"id": "beta-2", "name": null, "default": false}]})
            .to_string(),
        )
        .unwrap();
        let file = |name: &str| ps_path(&dir.join(name));
        let ps = format!(
            "fx_log_args {args_log};$c=fx_arg 4;$s=fx_arg 5;\
             if($c -ceq 'providers'){{\
               if($s -ceq 'list'){{fx_cat {providers}}}\
               elseif($s -ceq 'models'){{fx_append {models_log} ((fx_arg 6)+[char]10);\
                 if(fx_exists {models_fail}){{fx_print {listing_failed};exit 1}};fx_cat {models}}}}}\
             elseif($c -ceq 'enrich'){{\
               if($s -ceq 'instructions'){{fx_save_stdin {saved_stdin};fx_print {no_instructions};exit 0}};\
               if(fx_exists {busy}){{fx_print {busy_error};exit 1}};\
               if($all -clike '*--instructions-stdin*'){{fx_save_stdin {enrich_stdin}}};\
               $e=([IO.File]::ReadAllText({meeting}) | ConvertFrom-Json).enrichment;\
               $b=$null;if($e){{$b=$e.created_at}};\
               fx_echo ([ordered]@{{queued=$true;job_id={job};baseline=$b}} | ConvertTo-Json -Compress)}}",
            args_log = file("args.log"),
            providers = file("providers.json"),
            models_log = file("models.log"),
            models_fail = file("models-fail"),
            listing_failed = ps_str(
                r#"{"error":{"code":"provider_failed","message":"synthetic listing failed"}}"#
            ),
            models = file("models.json"),
            saved_stdin = file("saved-stdin"),
            no_instructions = ps_str(r#"{"instructions":null}"#),
            busy = file("busy"),
            busy_error = ps_str(r#"{"error":{"code":"library_busy","message":"busy"}}"#),
            enrich_stdin = file("enrich-stdin"),
            meeting = ps_path(meeting_file),
            job = ps_str(&job.to_string()),
        );
        write_cli(
            &dir.join("synthetic-cli"),
            &format!(
                "printf '%s\\n' \"$*\" >> '{d}/args.log'\n\
                 case \"$4\" in\n\
                 providers)\n\
                   case \"$5\" in\n\
                   list) cat '{d}/providers.json' ;;\n\
                   models)\n\
                     printf '%s\\n' \"$6\" >> '{d}/models.log'\n\
                     if [ -f '{d}/models-fail' ]; then printf '{{\"error\":{{\"code\":\"provider_failed\",\"message\":\"synthetic listing failed\"}}}}'; exit 1; fi\n\
                     cat '{d}/models.json' ;;\n\
                   esac ;;\n\
                 enrich)\n\
                   if [ \"$5\" = instructions ]; then cat > '{d}/saved-stdin'; printf '{{\"instructions\":null}}'; exit 0; fi\n\
                   if [ -f '{d}/busy' ]; then printf '{{\"error\":{{\"code\":\"library_busy\",\"message\":\"busy\"}}}}'; exit 1; fi\n\
                   case \"$*\" in *--instructions-stdin*) cat > '{d}/enrich-stdin' ;; esac\n\
                   python3 -c 'import json,sys; result=json.load(open(sys.argv[1])).get(\"enrichment\") or {{}}; print(json.dumps({{\"queued\":True,\"job_id\":sys.argv[2],\"baseline\":result.get(\"created_at\")}}))' '{meeting}' '{job}' ;;\n\
                 esac\n"
            ),
            &ps,
        )
    }

    fn log(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(name)).unwrap_or_default()
    }

    fn enrich_calls(dir: &Path) -> Vec<String> {
        log(dir, "args.log")
            .lines()
            .filter(|l| l.contains(" enrich "))
            .map(str::to_owned)
            .collect()
    }

    struct Fixture {
        window: WindowHandle<MeetingView>,
        panel: Entity<GeneratePanel>,
        app: Entity<AppState>,
        lib: Library,
        s: Synthetic,
        job: JobId,
        dir: tempfile::TempDir,
    }

    /// The seeded meeting open on its Summary tab with [`synthetic_cli`].
    fn open(cx: &mut TestAppContext) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        // The job ID follows the seeded meeting's random ID: the script is
        // written once the library is seeded, before anything runs it.
        let cli = crate::test_support::native_cli_path(&dir.path().join("synthetic-cli"));
        let (app, lib, s) = fixtures::state_with_cli(cx, dir.path(), cli);
        let job = enrich_job_id(&s.meeting);
        synthetic_cli(
            dir.path(),
            &job,
            &lib.meeting_dir(&s.meeting)
                .unwrap()
                .join(broscribe_core::paths::MEETING_JSON),
        );
        let id = s.meeting.clone();
        app.update(cx, |st, cx| st.navigate(Route::meeting(id.clone()), cx));
        let window = cx.add_window({
            let app = app.clone();
            move |window, cx| MeetingView::new(app, id, window, cx)
        });
        cx.run_until_parked();
        let panel = window
            .update(cx, |view, _, cx| {
                view.set_tab(MeetingTab::Overview, cx);
                view.generate_panel().clone()
            })
            .unwrap();
        cx.run_until_parked();
        Fixture {
            window,
            panel,
            app,
            lib,
            s,
            job,
            dir,
        }
    }

    fn draw(f: &Fixture, cx: &mut TestAppContext) {
        cx.update_window(f.window.into(), |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
    }

    fn event(kind: ProgressEventKind, job: &JobId) -> WorkerBatch {
        let mut batch = WorkerBatch::default();
        batch.push(WorkerMsg::Progress(ProgressEvent {
            event: kind,
            job_id: Some(job.to_string()),
            kind: Some(ENRICH_KIND.into()),
            meeting_id: None,
            fraction: None,
            message: None,
        }));
        batch
    }

    fn enqueue_job(lib: &Library, meeting: &MeetingId) -> JobId {
        lib.write(|tx| {
            Queue::open(tx.root())?.enqueue(
                tx,
                ENRICH_KIND,
                meeting.as_str(),
                json!({ "meeting_id": meeting }),
            )
        })
        .unwrap()
    }

    fn set_other(f: &Fixture, cx: &mut TestAppContext, text: &str) {
        let input = f.panel.read_with(cx, |p, _| p.other.clone());
        f.window
            .update(cx, |_, window, cx| {
                input.update(cx, |i, cx| i.set_value(text.to_owned(), window, cx))
            })
            .unwrap();
    }

    fn generate(f: &Fixture, cx: &mut TestAppContext) {
        let panel = f.panel.clone();
        f.window
            .update(cx, |_, window, cx| {
                panel.update(cx, |p, cx| p.generate(window, cx))
            })
            .unwrap();
    }

    #[gpui::test]
    fn generate_queues_the_chosen_model_follows_the_job_and_reloads(cx: &mut TestAppContext) {
        let f = open(cx);
        draw(&f, cx);
        f.panel.read_with(cx, |p, _| {
            let ids: Vec<&str> = p
                .providers()
                .unwrap()
                .as_ref()
                .unwrap()
                .iter()
                .map(|r| r.id.as_str())
                .collect();
            // Enabled and consented LLM providers only.
            assert_eq!(ids, ["agent-a", "remote"]);
            assert_eq!(p.provider(), Some("agent-a"));
            assert_eq!(p.choice(), Some(&ModelChoice::Default));
            assert!(p.can_generate());
        });
        assert_eq!(log(f.dir.path(), "models.log"), "agent-a\n");
        f.panel.update(cx, |p, cx| {
            let ids: Vec<String> = p
                .model_list(cx)
                .unwrap()
                .models
                .iter()
                .map(|m| m.id.clone())
                .collect();
            assert_eq!(ids, ["alpha-1", "beta-2"]);
            p.choose_model(ModelChoice::Listed("alpha-1".into()), cx);
        });
        draw(&f, cx);

        // The CLI enqueues the job; the script only answers.
        assert_eq!(enqueue_job(&f.lib, &f.s.meeting), f.job);
        generate(&f, cx);
        f.panel.read_with(cx, |p, _| {
            assert_eq!(p.phase(), &Phase::Submitting);
            assert!(!p.can_generate());
        });
        // One at a time: a second press while in flight does nothing.
        generate(&f, cx);
        cx.run_until_parked();
        assert_eq!(
            enrich_calls(f.dir.path()),
            [format!(
                "--library {} --json enrich {} --queue --provider agent-a --model alpha-1",
                f.lib.root().display(),
                f.s.meeting
            )]
        );
        f.panel.read_with(cx, |p, _| {
            assert_eq!(p.phase(), &Phase::Queued(f.job.clone()))
        });
        draw(&f, cx);

        // The worker starts it…
        let queue = Queue::open(f.lib.root()).unwrap();
        let lease = queue
            .claim(&f.lib, &f.job, &FakeProcessProbe::new(4242))
            .unwrap()
            .unwrap();
        f.app.update(cx, |s, cx| {
            s.apply_worker_batch(event(ProgressEventKind::JobStarted, &f.job), cx)
        });
        cx.run_until_parked();
        f.panel.read_with(cx, |p, _| {
            assert_eq!(p.phase(), &Phase::Running(f.job.clone()))
        });

        // …publishes, releases and removes the finished job, then reports
        // it (the real worker's order): the meeting's new enrichment says
        // done, and the meeting reloads.
        let meeting = f.s.meeting.clone();
        lease
            .commit(&f.lib, JobState::Done, |tx, _| {
                let mut m: Meeting = tx.library().load_meeting(&meeting)?;
                m.enrichment = Some(
                    serde_json::from_value(json!({
                        "provider": "agent-a", "created_at": "2026-11-15T00:00:00Z",
                        "summary": "Synthetic summary."
                    }))
                    .unwrap(),
                );
                tx.save_meeting(&m)
            })
            .unwrap();
        finish_like_the_worker(&f, lease);
        f.app.update(cx, |s, cx| {
            s.apply_worker_batch(event(ProgressEventKind::JobDone, &f.job), cx)
        });
        cx.run_until_parked();
        f.panel.read_with(cx, |p, _| {
            assert_eq!(p.phase(), &Phase::Done);
            assert!(p.has_enrichment() && p.can_generate());
        });
        f.window
            .update(cx, |view, _, _| {
                let enrichment = view.data().unwrap().meeting.enrichment.as_ref().unwrap();
                assert_eq!(enrichment.summary, "Synthetic summary.");
            })
            .unwrap();
        draw(&f, cx);

        // Regenerate, but the job finishes without publishing (skipped):
        // the same removal, no new enrichment, so it is not "done".
        enqueue_job(&f.lib, &f.s.meeting);
        generate(&f, cx);
        cx.run_until_parked();
        let lease = queue
            .claim(&f.lib, &f.job, &FakeProcessProbe::new(4242))
            .unwrap()
            .unwrap();
        lease.commit(&f.lib, JobState::Done, |_, _| Ok(())).unwrap();
        finish_like_the_worker(&f, lease);
        f.app.update(cx, |s, cx| {
            s.apply_worker_batch(event(ProgressEventKind::JobDone, &f.job), cx)
        });
        cx.run_until_parked();
        f.panel.read_with(cx, |p, _| match p.phase() {
            Phase::Failed(text) => assert!(text.contains("without a new summary"), "{text}"),
            other => panic!("expected a failure, got {other:?}"),
        });

        // A reported failure whose job is gone shows the event's message.
        enqueue_job(&f.lib, &f.s.meeting);
        generate(&f, cx);
        cx.run_until_parked();
        let job_path = queue.dir().join(f.job.as_str());
        if job_path.is_dir() {
            std::fs::remove_dir_all(&job_path).unwrap();
        } else {
            std::fs::remove_file(&job_path).unwrap();
        }
        let mut failed = WorkerBatch::default();
        failed.push(WorkerMsg::Progress(ProgressEvent {
            event: ProgressEventKind::JobFailed,
            job_id: Some(f.job.to_string()),
            kind: Some(ENRICH_KIND.into()),
            meeting_id: None,
            fraction: None,
            message: Some("synthetic provider error".into()),
        }));
        f.app.update(cx, |s, cx| s.apply_worker_batch(failed, cx));
        cx.run_until_parked();
        // Words BroScribe can't classify are never shown as they are.
        f.panel.read_with(cx, |p, _| {
            assert_eq!(
                p.phase(),
                &Phase::Failed("Couldn't generate the summary. Something went wrong.".into())
            )
        });
        draw(&f, cx);
    }

    /// WRT-04: a summary run's stored error (the queue's record and the
    /// job event's message, `e.to_string()`) is said in words where it is
    /// shown, the record left as it is: neither its path, its provider's id
    /// nor the system's words reach the panel or the meeting's nodes.
    #[gpui::test]
    fn a_stored_job_error_is_said_in_words(cx: &mut TestAppContext) {
        const PRIVATE: &str = "/Users/synthetic-private/Library/BroScribe";
        let f = open(cx);
        f.panel
            .update(cx, |p, cx| p.choose_provider("agent-a".into(), cx));
        cx.run_until_parked();
        enqueue_job(&f.lib, &f.s.meeting);
        generate(&f, cx);
        cx.run_until_parked();
        let rendered = |cx: &mut TestAppContext| {
            cx.update_window(f.window.into(), |_, window, cx| {
                crate::ui::spoken_log::while_building(|| window.draw(cx).clear(cx)).1
            })
            .unwrap()
        };
        let assert_clean = |texts: &[String]| {
            for text in texts {
                for raw in ["synthetic-private", "zq8", "os error", "No space", "error:"] {
                    assert!(!text.contains(raw), "{raw} shown: {text}");
                }
            }
        };

        // An attempt fails with the system's words and a path; attempts
        // are left, so it is tried again.
        let queue = Queue::open(f.lib.root()).unwrap();
        let lease = queue
            .claim(&f.lib, &f.job, &FakeProcessProbe::new(4242))
            .unwrap()
            .unwrap();
        let stored =
            format!("{PRIVATE}/meetings/zq8/meeting.json: No space left on device (os error 28)");
        lease.fail(&f.lib, stored.clone()).unwrap();
        lease.release(&f.lib).unwrap();
        assert_eq!(
            queue.get(&f.job).unwrap().error.as_deref(),
            Some(stored.as_str())
        );
        for _ in 0..RECONCILE_TICKS {
            cx.executor().advance_clock(GENERATION_TICK);
            cx.run_until_parked();
        }
        let (status, _) = f.panel.read_with(cx, |p, cx| p.status(cx)).unwrap();
        assert!(
            status.starts_with(
                "An attempt failed. BroScribe couldn't read or write a file. It will be tried \
                 again."
            ),
            "{status}"
        );
        let nodes = rendered(cx);
        assert_clean(&[status]);
        assert_clean(&nodes.into_iter().map(|(_, _, t)| t).collect::<Vec<_>>());
        // The record is the job's, kept as it was.
        assert_eq!(
            queue.get(&f.job).unwrap().error.as_deref(),
            Some(stored.as_str())
        );

        // The run then ends on a provider's bad answer, reported by its event.
        let job_path = queue.dir().join(f.job.as_str());
        if job_path.is_dir() {
            std::fs::remove_dir_all(&job_path).unwrap();
        } else {
            std::fs::remove_file(&job_path).unwrap();
        }
        let mut failed = WorkerBatch::default();
        failed.push(WorkerMsg::Progress(ProgressEvent {
            event: ProgressEventKind::JobFailed,
            job_id: Some(f.job.to_string()),
            kind: Some(ENRICH_KIND.into()),
            meeting_id: None,
            fraction: None,
            message: Some(format!(
                "inference error: synthetic-zq8 returned {PRIVATE}/reply.json (exit status: 9)"
            )),
        }));
        f.app.update(cx, |s, cx| s.apply_worker_batch(failed, cx));
        cx.run_until_parked();
        let line = "Couldn't generate the summary. The AI provider didn't give a usable answer.";
        f.panel.read_with(cx, |p, _| {
            assert_eq!(p.phase(), &Phase::Failed(line.into()))
        });
        let nodes = rendered(cx);
        assert!(nodes.iter().any(|(_, _, t)| t == line), "{nodes:?}");
        assert_clean(&nodes.into_iter().map(|(_, _, t)| t).collect::<Vec<_>>());
    }

    /// What the worker does after a job's commit: release the lease and
    /// remove the finished job, before any event is sent.
    fn finish_like_the_worker(f: &Fixture, lease: broscribe_core::queue::LeaseHandle) {
        lease.release(&f.lib).unwrap();
        f.lib
            .write(|tx| Queue::open(tx.root())?.remove_finished(tx, ENRICH_KIND))
            .unwrap();
        assert_eq!(job_state(f.lib.root(), &f.job).unwrap(), None);
    }

    #[gpui::test]
    fn a_finished_generation_refetches_the_models_keeping_them_shown(cx: &mut TestAppContext) {
        use std::cell::RefCell;
        let f = open(cx);
        let models_log = || log(f.dir.path(), "models.log");
        f.panel
            .update(cx, |p, cx| p.choose_provider("agent-a".into(), cx));
        cx.run_until_parked();
        f.panel.update(cx, |p, cx| {
            p.choose_model(ModelChoice::Listed("alpha-1".into()), cx)
        });
        draw(&f, cx);
        assert_eq!(models_log(), "agent-a\n");

        // What the picker showed when the generation finished: whether a
        // fetch was running, and the listed model ids.
        type Seen = Vec<(bool, Option<Vec<String>>)>;
        let seen: Rc<RefCell<Seen>> = Rc::default();
        let _watch = cx.update({
            let seen = seen.clone();
            |cx| {
                cx.subscribe(&f.panel, move |panel, event: &GenerateEvent, cx| {
                    if *event != GenerateEvent::Finished {
                        return;
                    }
                    let p = panel.read(cx);
                    let ids = p
                        .model_list(cx)
                        .map(|l| l.models.into_iter().map(|m| m.id).collect());
                    seen.borrow_mut().push((p.models_loading(), ids));
                })
            }
        });

        enqueue_job(&f.lib, &f.s.meeting);
        generate(&f, cx);
        cx.run_until_parked();
        let queue = Queue::open(f.lib.root()).unwrap();
        let lease = queue
            .claim(&f.lib, &f.job, &FakeProcessProbe::new(4242))
            .unwrap()
            .unwrap();
        let meeting = f.s.meeting.clone();
        lease
            .commit(&f.lib, JobState::Done, |tx, _| {
                let mut m: Meeting = tx.library().load_meeting(&meeting)?;
                m.enrichment = Some(
                    serde_json::from_value(json!({
                        "provider": "agent-a", "model": "alpha-1-2026",
                        "requested_model": "alpha-1", "model_reported": true,
                        "created_at": "2026-11-15T00:00:00Z", "summary": "Synthetic summary."
                    }))
                    .unwrap(),
                );
                tx.save_meeting(&m)
            })
            .unwrap();
        finish_like_the_worker(&f, lease);
        // The CLI now knows what the alias resolved to (from the index).
        std::fs::write(
            f.dir.path().join("models.json"),
            json!({"provider": "agent-a", "source": "cli", "configured_model": null,
                   "models": [{"id": "alpha-1", "name": null, "resolved": "alpha-1-2026"},
                              {"id": "beta-2", "name": null}],
                   "default_resolved": null})
            .to_string(),
        )
        .unwrap();
        f.app.update(cx, |s, cx| {
            s.apply_worker_batch(event(ProgressEventKind::JobDone, &f.job), cx)
        });
        cx.run_until_parked();

        // While the list was fetched again the old options stayed shown.
        assert_eq!(
            *seen.borrow(),
            [(true, Some(vec!["alpha-1".to_owned(), "beta-2".to_owned()]))]
        );
        // One listing, without Refresh or choosing the provider again; the
        // chosen model is kept and shows its new hint.
        assert_eq!(models_log(), "agent-a\nagent-a\n");
        f.panel.update(cx, |p, cx| {
            assert_eq!(p.phase(), &Phase::Done);
            assert!(!p.models_loading());
            assert_eq!(p.choice(), Some(&ModelChoice::Listed("alpha-1".into())));
            let list = p.model_list(cx).unwrap();
            let row = p.row("agent-a").unwrap().clone();
            let config = p.state.read(cx).config().clone();
            let (_, listed) = model_option_texts(&config, &row, Some(&list));
            assert_eq!(listed[0].0, "alpha-1 — alpha-1-2026 (last run)");
        });
        draw(&f, cx);
        // The summary names the provider and the reported model.
        f.window
            .update(cx, |view, _, cx| {
                let (text, _) = view.credit_line(cx).unwrap();
                assert!(
                    text.starts_with("AI summary · Synthetic Agent · alpha-1-2026 · "),
                    "{text}"
                );
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(models_log(), "agent-a\nagent-a\n");
    }

    #[gpui::test]
    fn refusals_validation_and_busy_meetings_disable_generate(cx: &mut TestAppContext) {
        let f = open(cx);
        f.panel
            .update(cx, |p, cx| p.choose_provider("agent-a".into(), cx));
        cx.run_until_parked();

        // A busy library is reported, not retried.
        std::fs::write(f.dir.path().join("busy"), b"").unwrap();
        generate(&f, cx);
        cx.run_until_parked();
        f.panel.read_with(cx, |p, _| match p.phase() {
            Phase::Failed(text) => assert!(text.contains("already being generated"), "{text}"),
            other => panic!("expected a refusal, got {other:?}"),
        });
        assert_eq!(
            enrich_calls(f.dir.path()),
            [format!(
                "--library {} --json enrich {} --queue --provider agent-a",
                f.lib.root().display(),
                f.s.meeting
            )]
        );
        std::fs::remove_file(f.dir.path().join("busy")).unwrap();

        // "Other…" is validated before anything runs.
        f.panel
            .update(cx, |p, cx| p.choose_model(ModelChoice::Other, cx));
        draw(&f, cx);
        for bad in ["", "-flag", "two words", &"x".repeat(201)] {
            set_other(&f, cx, bad);
            generate(&f, cx);
            cx.run_until_parked();
            f.panel
                .read_with(cx, |p, _| assert!(p.other_error().is_some(), "{bad:?}"));
        }
        draw(&f, cx);
        assert_eq!(enrich_calls(f.dir.path()).len(), 1);
        set_other(&f, cx, "synthetic-model");
        enqueue_job(&f.lib, &f.s.meeting);
        generate(&f, cx);
        cx.run_until_parked();
        f.panel
            .read_with(cx, |p, _| assert!(p.other_error().is_none()));
        assert!(
            enrich_calls(f.dir.path())[1]
                .ends_with("--queue --provider agent-a --model synthetic-model")
        );

        // A meeting whose generation is already queued cannot start another.
        f.window
            .update(cx, |view, window, cx| view.reload(window, cx))
            .unwrap();
        cx.run_until_parked();
        f.panel.update(cx, |p, cx| {
            p.phase = Phase::Idle;
            assert!(p.meeting_busy && !p.can_generate());
            cx.notify();
        });
        draw(&f, cx);
    }

    #[gpui::test]
    fn model_lists_are_cached_refreshed_invalidated_and_errors_shown(cx: &mut TestAppContext) {
        let f = open(cx);
        let models_log = || log(f.dir.path(), "models.log");
        f.panel
            .update(cx, |p, cx| p.choose_provider("agent-a".into(), cx));
        cx.run_until_parked();
        // An HTTP provider without a configured model has no default.
        f.panel
            .update(cx, |p, cx| p.choose_provider("remote".into(), cx));
        cx.run_until_parked();
        f.panel.read_with(cx, |p, _| {
            assert_eq!(p.choice(), None);
            assert!(!p.can_generate());
        });
        // Back to a cached provider: no new listing.
        f.panel
            .update(cx, |p, cx| p.choose_provider("agent-a".into(), cx));
        cx.run_until_parked();
        assert_eq!(models_log(), "agent-a\nremote\n");
        // Refresh lists again.
        f.panel.update(cx, |p, cx| p.refresh_models(cx));
        cx.run_until_parked();
        assert_eq!(models_log(), "agent-a\nremote\nagent-a\n");
        // A config change (here: the configured model) is a new cache key.
        let mut config = f.app.read_with(cx, |s, _| s.config().clone());
        config.providers.insert(
            "agent-a".into(),
            ProviderConfig {
                enabled: true,
                consented_at: Some("2026-11-15T00:00:00+00:00".parse().unwrap()),
                model: Some("configured-x".into()),
                base_url: None,
            },
        );
        f.app.update(cx, |s, cx| s.set_config(config, cx));
        cx.run_until_parked();
        assert_eq!(models_log(), "agent-a\nremote\nagent-a\nagent-a\n");
        f.panel
            .update(cx, |p, cx| assert!(p.model_list(cx).is_some()));
        draw(&f, cx);

        // A failed listing is shown; free text still works.
        std::fs::write(f.dir.path().join("models-fail"), b"").unwrap();
        f.panel.update(cx, |p, cx| p.refresh_models(cx));
        cx.run_until_parked();
        f.panel.update(cx, |p, cx| {
            assert!(
                p.model_error()
                    .unwrap()
                    .contains("Synthetic listing failed."),
                "{:?}",
                p.model_error()
            );
            assert!(p.model_list(cx).is_none());
            p.choose_model(ModelChoice::Other, cx);
            assert_eq!(p.choice(), Some(&ModelChoice::Other));
        });
        draw(&f, cx);
    }

    /// INS-01: every text of the generation controls is a named node, in
    /// each state: waits are Status, the listing failure an Alert, notes
    /// and help Labels, the title a heading.
    #[gpui::test]
    fn overview_provider_states_and_disabled_reasons_are_named(cx: &mut TestAppContext) {
        use crate::ui::spoken_log::{node, while_building};
        let f = open(cx);
        let render = |cx: &mut TestAppContext| {
            f.window
                .update(cx, |_, window, cx| {
                    f.panel.update(cx, |panel, cx| {
                        while_building(|| panel.render_overview(window, cx)).1
                    })
                })
                .unwrap()
        };
        let fields = |cx: &mut TestAppContext| {
            f.window
                .update(cx, |_, window, cx| {
                    f.panel.update(cx, |panel, cx| {
                        while_building(|| panel.options_content(window, cx)).1
                    })
                })
                .unwrap()
        };
        assert_eq!(
            node(&render(cx), "summary-empty"),
            (Role::Heading, "No summary yet")
        );
        f.panel.update(cx, |p, _| p.providers = None);
        assert_eq!(
            node(&render(cx), "generate-summary-providers-loading"),
            (Role::Status, "Listing AI providers…")
        );
        f.panel.update(cx, |p, _| {
            p.providers = Some(Err(
                "Couldn't list the AI providers. Synthetic refusal.".into()
            ))
        });
        assert_eq!(
            node(&render(cx), "generate-summary-providers-error"),
            (
                Role::Alert,
                "Couldn't list the AI providers. Synthetic refusal."
            )
        );
        f.panel
            .update(cx, |p, _| p.providers = Some(Ok(Vec::new())));
        assert_eq!(
            node(&render(cx), "summary-no-provider-title"),
            (Role::Heading, "Summaries need an AI provider")
        );
        f.panel.update(cx, |p, cx| {
            p.set_summary(Some(&serde_json::from_value(json!({
                "provider":"agent-off","summary":"Synthetic copy.", "created_at":"2026-11-15T00:00:00Z"
            })).unwrap()), cx);
        });
        assert_eq!(
            node(&render(cx), "summary-provider-needed"),
            (Role::Status, "To regenerate, turn on an AI provider.")
        );

        f.panel.update(cx, |p, cx| p.load_providers(cx));
        cx.run_until_parked();
        f.panel.update(cx, |p, _| p.provider = None);
        assert_eq!(
            node(&fields(cx), "generate-summary-disabled"),
            (Role::Status, "Choose a provider to generate.")
        );
        f.panel
            .update(cx, |p, cx| p.choose_provider("remote".into(), cx));
        assert_eq!(
            node(&fields(cx), "generate-summary-disabled"),
            (
                Role::Status,
                "Choose a model: Synthetic Remote has no configured model."
            )
        );
        f.panel.update(cx, |p, _| p.refreshed_choice = true);
        assert_eq!(
            node(&fields(cx), "generate-summary-disabled"),
            (
                Role::Status,
                "Choose a model again: the model list was refreshed."
            )
        );
        let providers = f.dir.path().join("providers.json");
        let good = std::fs::read(&providers).unwrap();
        std::fs::write(&providers, "invalid synthetic response").unwrap();
        f.app.update(cx, |app, cx| {
            let mut config = app.config().clone();
            config.providers.entry("agent-a".into()).or_default().model = Some("changed".into());
            app.set_config(config, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            node(&render(cx), "generate-summary-providers-error").0,
            Role::Alert
        );
        std::fs::write(providers, good).unwrap();
        let mut visual = gpui::VisualTestContext::from_window(f.window.into(), cx);
        kit::dialog_gate::click(&mut visual, "providers-retry", Role::Button);
        assert!(f.panel.read_with(
            &visual,
            |panel, _| matches!(panel.providers(), Some(Ok(rows)) if rows.len() == 2)
        ));
    }

    #[gpui::test]
    fn overview_popover_uses_selects_and_returns_focus(cx: &mut TestAppContext) {
        use crate::ui::kit::dialog_gate::{click, draw};
        let f = open(cx);
        let mut visual = gpui::VisualTestContext::from_window(f.window.into(), cx);
        visual.update(|window, cx| {
            window.activate_window();
            gpui::init(cx);
            let keys = crate::ui::keymap::key_bindings(cx);
            cx.bind_keys(keys);
        });
        let snapshots = |visual: &mut gpui::VisualTestContext| {
            visual.update(|window, _| gpui::base::test_support::snapshots(window))
        };
        draw(&mut visual);
        assert!(
            !snapshots(&mut visual)
                .iter()
                .any(|node| node.label() == Some("Provider"))
        );
        click(&mut visual, "regenerate", Role::Button);
        let nodes = snapshots(&mut visual);
        let provider = nodes
            .iter()
            .find(|node| node.label() == Some("Provider"))
            .unwrap();
        let model = nodes
            .iter()
            .find(|node| node.label() == Some("Model"))
            .unwrap();
        assert_eq!(
            provider.value(),
            Some("Synthetic Agent · Command-line tool")
        );
        assert_eq!(provider.focused(), Some(true));
        assert_eq!(model.value(), Some("Provider default"));
        assert!(
            !nodes
                .iter()
                .any(|node| node.role() == Some(Role::RadioButton))
        );
        visual.simulate_keystrokes("escape");
        draw(&mut visual);
        assert!(f.panel.read_with(&visual, |p, _| p.options.is_none()));
        visual.update(|window, cx| assert!(f.panel.read(cx).header_focus.is_focused(window)));
        click(&mut visual, "regenerate", Role::Button);
        click(&mut visual, "generation-cancel", Role::Button);
        visual.update(|window, cx| assert!(f.panel.read(cx).header_focus.is_focused(window)));
        click(&mut visual, "regenerate", Role::Button);
        visual.update(|window, cx| window.focus_prev(cx));
        draw(&mut visual);
        assert!(f.panel.read_with(&visual, |p, _| p.options.is_none()));
        assert!(enrich_calls(f.dir.path()).is_empty());
    }

    #[gpui::test]
    fn overview_narrow_dialog_focuses_provider_and_preserves_instruction_enter(
        cx: &mut TestAppContext,
    ) {
        use crate::ui::kit::dialog_gate::{click, draw};
        use gpui::{VisualTestContext, component::Root};
        let f = open(cx);
        let created = Rc::new(std::cell::RefCell::new(None));
        let panel_ref = created.clone();
        let root = cx.add_window(|window, cx| {
            gpui::init(cx);
            let keys = crate::ui::keymap::key_bindings(cx);
            cx.bind_keys(keys);
            let panel = cx.new(|cx| {
                let mut panel = GeneratePanel::new(
                    f.app.clone(),
                    f.s.meeting.clone(),
                    GenerationTarget::Summary,
                    window,
                    cx,
                );
                panel.set_options_width(gpui::px(420.), cx);
                panel.ensure_loaded(cx);
                panel
            });
            *panel_ref.borrow_mut() = Some(panel.clone());
            Root::new(panel, window, cx)
        });
        let panel = created.borrow_mut().take().unwrap();
        cx.run_until_parked();
        let mut visual = VisualTestContext::from_window(root.into(), cx);
        draw(&mut visual);
        click(&mut visual, "regenerate", Role::Button);
        let snapshots = |visual: &mut VisualTestContext| {
            visual.update(|window, _| gpui::base::test_support::snapshots(window))
        };
        assert!(panel.read_with(&visual, |panel, _| panel.options_dialog));
        assert!(
            snapshots(&mut visual)
                .iter()
                .any(|node| node.label() == Some("Provider") && node.focused() == Some(true))
        );
        visual.simulate_keystrokes("escape");
        draw(&mut visual);
        assert!(!panel.read_with(&visual, |panel, _| panel.options_dialog));
        visual.update(|window, cx| assert!(panel.read(cx).header_focus.is_focused(window)));
        click(&mut visual, "regenerate", Role::Button);
        click(&mut visual, "generate-model", Role::ComboBox);
        visual.simulate_input("beta-2");
        visual.simulate_keystrokes("enter");
        draw(&mut visual);
        assert_eq!(
            panel.read_with(&visual, |panel, _| panel.choice().cloned()),
            Some(ModelChoice::Listed("beta-2".into()))
        );
        click(&mut visual, "instructions-toggle", Role::Button);
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.instructions.input.update(cx, |input, cx| {
                    input.focus_handle(cx).focus(window, cx);
                    input.set_value("Synthetic one", window, cx);
                });
            });
        });
        visual.simulate_keystrokes("enter");
        assert!(enrich_calls(f.dir.path()).is_empty());
        let value = panel.read_with(&visual, |panel, cx| {
            panel.instructions.input.read(cx).value().to_string()
        });
        assert!(value.contains('\n'), "{value:?}");
        let shortcut = visual.update(|_, cx| {
            if gpui::Modifiers::secondary_key(cx).platform {
                "cmd-enter"
            } else {
                "ctrl-enter"
            }
        });
        visual.simulate_keystrokes(shortcut);
        visual.run_until_parked();
        draw(&mut visual);
        assert!(!panel.read_with(&visual, |panel, _| panel.options_dialog));
        assert!(enrich_calls(f.dir.path())[0].contains("--model beta-2 --instructions-stdin"));
        assert_eq!(log(f.dir.path(), "enrich-stdin").trim(), "Synthetic one");
        visual.update(|window, cx| assert!(panel.read(cx).header_focus.is_focused(window)));
    }

    #[gpui::test]
    fn overview_provider_preselection_shares_catalog_and_uses_retry_inputs(
        cx: &mut TestAppContext,
    ) {
        let f = open(cx);
        f.panel.update(cx, |p, cx| {
            p.provider = None;
            p.set_result_selection(Some(("agent-a", Some("beta-2"))), cx);
            assert_eq!(p.provider(), Some("agent-a"));
            assert_eq!(p.choice(), Some(&ModelChoice::Listed("beta-2".into())));
            assert_eq!(p.name("agent-off", cx), "Disabled Synthetic Agent");
            assert_eq!(p.name("missing", cx), "unknown provider (missing)");
        });
        f.window
            .update(cx, |_, window, cx| {
                f.panel
                    .update(cx, |p, cx| p.toggle_instructions(window, cx));
            })
            .unwrap();
        let set = |value: &str, cx: &mut TestAppContext| {
            f.window
                .update(cx, |_, window, cx| {
                    f.panel.update(cx, |p, cx| {
                        p.instructions.input.update(cx, |input, cx| {
                            input.set_value(value.to_owned(), window, cx)
                        });
                    })
                })
                .unwrap();
        };
        set("Use synthetic notes.", cx);
        std::fs::write(f.dir.path().join("busy"), "").unwrap();
        generate(&f, cx);
        cx.run_until_parked();
        f.panel
            .read_with(cx, |p, _| assert!(matches!(p.phase(), Phase::Failed(_))));
        set("A different draft.", cx);
        f.panel
            .update(cx, |p, cx| p.choose_model(ModelChoice::Default, cx));
        std::fs::remove_file(f.dir.path().join("busy")).unwrap();
        let mut visual = gpui::VisualTestContext::from_window(f.window.into(), cx);
        kit::dialog_gate::click(&mut visual, "generation-retry", Role::Button);
        assert_eq!(log(f.dir.path(), "enrich-stdin"), "Use synthetic notes.");
        let calls = enrich_calls(f.dir.path());
        assert_eq!(calls.len(), 2);
        assert!(
            calls
                .iter()
                .all(|call| call.contains("--model beta-2") && !call.contains("synthetic notes"))
        );
        let another = f
            .window
            .update(&mut visual, |_, window, cx| {
                let another = cx.new(|cx| {
                    GeneratePanel::new(
                        f.app.clone(),
                        f.s.meeting.clone(),
                        GenerationTarget::Summary,
                        window,
                        cx,
                    )
                });
                another.update(cx, |p, cx| p.ensure_loaded(cx));
                another
            })
            .unwrap();
        visual.run_until_parked();
        assert_eq!(
            another.read_with(&visual, |p, _| p.choice().cloned()),
            Some(ModelChoice::Listed("beta-2".into()))
        );
        assert_eq!(
            log(f.dir.path(), "args.log")
                .lines()
                .filter(|line| line.contains(" providers list"))
                .count(),
            1
        );
    }

    #[gpui::test]
    fn provider_location_labels_and_captions_distinguish_every_destination(
        cx: &mut TestAppContext,
    ) {
        use crate::ui::spoken_log::{node, while_building};
        let fixture = open(cx);
        let rows = parse_providers(&json!([
            {"id":"here","name":"Synthetic Local","location":"local"},
            {"id":"away","name":"Synthetic Cloud","location":"cloud"},
            {"id":"tool","name":"Synthetic CLI","location":"cli"},
            {"id":"unknown","name":"Synthetic Unknown"}
        ]));
        for (row, (label, caption)) in rows.iter().zip([
            ("Synthetic Local · Local", "Transcript text, notes and instructions stay on this computer."),
            ("Synthetic Cloud · Cloud", "Sends the transcript text, your notes and instructions to Synthetic Cloud (Cloud)."),
            ("Synthetic CLI · Command-line tool", "That tool may use its online service."),
            ("Synthetic Unknown · Location unknown", "Its location is unknown."),
        ]) {
            use gpui::component::searchable_list::SearchableListItem as _;
            assert_eq!(provider_option(row).title(), label);
            let text = fixture.window.update(cx, |_, window, cx| {
                fixture.panel.update(cx, |panel, cx| {
                    panel.provider = Some(row.id.clone());
                    panel.providers = Some(Ok(vec![row.clone()]));
                    let (_, shown) = while_building(|| panel.options_content(window, cx));
                    let (role, caption) = node(&shown, "generate-destination");
                    assert_eq!(role, Role::Label);
                    caption.to_owned()
                })
            }).unwrap();
            assert!(text.contains(caption), "{text}");
            assert!(text.ends_with("Speakers are sent as numbers. Speaker names, audio and voice data stay on this computer."));
        }
    }

    /// ACC-01 with INS-01: two template panels open in one window keep
    /// their live lines apart (the target is in each ID), so frames that
    /// change nothing announce nothing, and a change to one panel's line is
    /// announced once.
    #[gpui::test]
    fn two_open_panels_never_announce_each_others_lines(cx: &mut TestAppContext) {
        use std::sync::Arc;

        use broscribe_platform::fakes::FakeAnnouncer;

        struct TwoPanels(Entity<GeneratePanel>, Entity<GeneratePanel>);
        impl Render for TwoPanels {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().child(self.0.clone()).child(self.1.clone())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let cli = crate::test_support::native_cli_path(&dir.path().join("synthetic-cli"));
        let (app, _lib, s) = fixtures::state_with_cli(cx, dir.path(), cli);
        let announcer = Arc::new(FakeAnnouncer::new());
        cx.update(|cx| crate::ui::install_announcer(announcer.clone(), cx));
        let template = |id: &str, name: &str| GenerationTarget::Template {
            id: id.into(),
            name: name.into(),
        };
        let window = cx.add_window({
            let (app, meeting) = (app.clone(), s.meeting.clone());
            move |window, cx| {
                let mut panel = |target: GenerationTarget| {
                    cx.new(|cx| {
                        GeneratePanel::new(app.clone(), meeting.clone(), target, window, cx)
                    })
                };
                let notes = panel(template("meeting-notes", "Meeting notes"));
                let outline = panel(template("meeting-outline", "Meeting outline"));
                TwoPanels(notes, outline)
            }
        });
        cx.run_until_parked();
        let (notes, outline) = window
            .read_with(cx, |two, _| (two.0.clone(), two.1.clone()))
            .unwrap();
        let fail = |panel: &Entity<GeneratePanel>, text: &str, cx: &mut TestAppContext| {
            let text = text.to_owned();
            panel.update(cx, |p, cx| {
                p.set_phase(Phase::Failed(text), cx);
            });
        };
        fail(&notes, "Couldn't run the template. first", cx);
        fail(&outline, "Couldn't run the template. second", cx);
        let draw = |cx: &mut TestAppContext| {
            cx.update_window(window.into(), |_, window, cx| {
                window.refresh();
                window.draw(cx).clear(cx)
            })
            .unwrap();
            cx.run_until_parked();
        };
        draw(cx);
        announcer.take();
        draw(cx);
        draw(cx);
        assert!(
            announcer.take().is_empty(),
            "unchanged frames announce nothing"
        );

        fail(&notes, "Couldn't run the template. again", cx);
        draw(cx);
        draw(cx);
        let said: Vec<String> = announcer.take().into_iter().map(|(text, _)| text).collect();
        assert_eq!(said, ["Couldn't run the template. again"]);

        let ids = window
            .update(cx, |two, window, cx| {
                [&two.0, &two.1].map(|panel| {
                    panel.update(cx, |p, cx| {
                        crate::ui::spoken_log::while_building(|| Render::render(p, window, cx)).1
                    })
                })
            })
            .unwrap()
            .map(|named| {
                named
                    .into_iter()
                    .filter(|(_, role, _)| matches!(role, Role::Status | Role::Alert))
                    .map(|(id, ..)| id)
                    .collect::<Vec<_>>()
            });
        assert_eq!(
            ids,
            [
                ["generate-template-meeting-notes-status"],
                ["generate-template-meeting-outline-status"],
            ]
        );
    }

    #[gpui::test]
    fn without_a_usable_provider_settings_is_offered(cx: &mut TestAppContext) {
        let f = open(cx);
        std::fs::write(
            f.dir.path().join("providers.json"),
            json!({"providers": [
                {"id": "agent-off", "name":"Disabled Synthetic Agent", "purpose": "llm", "enabled": false, "consented": false}
            ]})
            .to_string(),
        )
        .unwrap();
        f.app.update(cx, |state, cx| {
            let mut config = state.config().clone();
            config
                .providers
                .entry("agent-a".into())
                .or_default()
                .enabled = false;
            state.set_config(config, cx);
        });
        cx.run_until_parked();
        f.panel.read_with(cx, |p, _| {
            assert!(p.providers().unwrap().as_ref().unwrap().is_empty())
        });
        draw(&f, cx);
        let requested = Rc::new(std::cell::Cell::new(None));
        let _subscription = cx.update(|cx| {
            cx.subscribe(&f.app, {
                let requested = requested.clone();
                move |_, event: &crate::state::AppEvent, _| {
                    if let crate::state::AppEvent::OpenSettings(page) = event {
                        requested.set(*page);
                    }
                }
            })
        });
        f.panel.update(cx, |p, cx| p.open_provider_settings(cx));
        assert_eq!(
            requested.get(),
            Some(crate::state::SettingsPage::AiProviders)
        );
    }

    #[gpui::test]
    fn panel_shows_queue_position_then_provider_model_and_elapsed_and_stops_ticking(
        cx: &mut TestAppContext,
    ) {
        let f = open(cx);
        f.panel
            .update(cx, |p, cx| p.choose_provider("agent-a".into(), cx));
        cx.run_until_parked();
        f.panel.update(cx, |p, cx| {
            p.choose_model(ModelChoice::Listed("alpha-1".into()), cx)
        });
        // Another meeting's summary was queued first: one job ahead.
        f.lib
            .write(|tx| {
                Queue::open(tx.root())?.enqueue(
                    tx,
                    ENRICH_KIND,
                    "m-0000ff",
                    json!({ "meeting_id": "m-0000ff" }),
                )
            })
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        enqueue_job(&f.lib, &f.s.meeting);
        f.panel.read_with(cx, |p, _| assert!(!p.ticking()));
        generate(&f, cx);
        cx.run_until_parked();
        f.panel.read_with(cx, |p, cx| {
            assert_eq!(p.phase(), &Phase::Queued(f.job.clone()));
            assert!(p.ticking());
            assert_eq!(p.status(cx).unwrap().0, "Queued · 1 job ahead");
        });
        // The user-started job is watched for its notification.
        f.app.read_with(cx, |s, _| {
            assert!(s.generations().is_watching(&f.job));
        });
        draw(&f, cx);

        let queue = Queue::open(f.lib.root()).unwrap();
        let lease = queue
            .claim(&f.lib, &f.job, &FakeProcessProbe::new(4242))
            .unwrap()
            .unwrap();
        f.app.update(cx, |s, cx| {
            s.apply_worker_batch(event(ProgressEventKind::JobStarted, &f.job), cx)
        });
        cx.run_until_parked();
        f.panel.read_with(cx, |p, cx| {
            assert_eq!(p.phase(), &Phase::Running(f.job.clone()));
            assert!(p.ticking());
            let (text, error) = p.status(cx).unwrap();
            assert!(!error);
            assert_eq!(text, "Generating with Synthetic Agent · alpha-1");
            let elapsed = p
                .running_elapsed()
                .expect("running job reports elapsed time");
            assert!(
                elapsed_text(elapsed).starts_with("0:0"),
                "{}",
                elapsed_text(elapsed)
            );
        });
        draw(&f, cx);

        let meeting = f.s.meeting.clone();
        lease
            .commit(&f.lib, JobState::Done, |tx, _| {
                let mut m: Meeting = tx.library().load_meeting(&meeting)?;
                m.enrichment = Some(
                    serde_json::from_value(json!({
                        "provider": "agent-a", "created_at": "2026-11-15T00:00:00Z",
                        "summary": "Synthetic summary."
                    }))
                    .unwrap(),
                );
                tx.save_meeting(&m)
            })
            .unwrap();
        finish_like_the_worker(&f, lease);
        f.app.update(cx, |s, cx| {
            s.apply_worker_batch(event(ProgressEventKind::JobDone, &f.job), cx)
        });
        cx.run_until_parked();
        f.panel.read_with(cx, |p, cx| {
            assert_eq!(p.phase(), &Phase::Done);
            // The timer stops once nothing is queued or running.
            assert!(!p.ticking());
            assert_eq!(p.status(cx).unwrap().0, "Summary updated.");
        });
        f.app.read_with(cx, |s, _| {
            assert!(!s.generations().is_watching(&f.job));
        });
    }

    #[gpui::test]
    fn a_job_started_elsewhere_is_followed_when_the_view_opens(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = crate::test_support::native_cli_path(&dir.path().join("synthetic-cli"));
        let (app, lib, s) = fixtures::state_with_cli(cx, dir.path(), cli);
        let job = enrich_job_id(&s.meeting);
        synthetic_cli(
            dir.path(),
            &job,
            &lib.meeting_dir(&s.meeting)
                .unwrap()
                .join(broscribe_core::paths::MEETING_JSON),
        );
        enqueue_job(&lib, &s.meeting);
        let id = s.meeting.clone();
        let window = cx.add_window({
            let app = app.clone();
            move |window, cx| MeetingView::new(app, id, window, cx)
        });
        cx.run_until_parked();
        let panel = window
            .update(cx, |view, _, _| view.generate_panel().clone())
            .unwrap();
        panel.read_with(cx, |p, cx| {
            assert_eq!(p.phase(), &Phase::Queued(job.clone()));
            assert!(!p.can_generate());
            assert_eq!(p.status(cx).unwrap().0, "Queued · next to run");
            // The meeting needs speaker review, so Overview shows the panel.
            assert!(p.ticking());
        });
        // Showing its tab starts the timer; another tab stops it.
        let set_tab = |tab: MeetingTab, cx: &mut TestAppContext| {
            window
                .update(cx, |view, _, cx| view.set_tab(tab, cx))
                .unwrap();
        };
        set_tab(MeetingTab::Transcript, cx);
        panel.read_with(cx, |p, _| assert!(!p.ticking()));
        set_tab(MeetingTab::Overview, cx);
        panel.read_with(cx, |p, _| assert!(p.ticking()));
        set_tab(MeetingTab::Templates, cx);
        panel.read_with(cx, |p, _| assert!(!p.ticking()));
        set_tab(MeetingTab::Overview, cx);
        panel.read_with(cx, |p, _| assert!(p.ticking()));
        // Not started here: nothing to announce.
        app.read_with(cx, |s, _| assert!(!s.generations().is_watching(&job)));

        // The worker runs it and publishes; a reload of the meeting brings
        // the new summary (and no job) before the job's end is reported.
        // The moved result makes the run be read again: against the
        // baseline taken when following began it is new, so the durable
        // outcome ends the run without waiting for the event.
        let queue = Queue::open(lib.root()).unwrap();
        let lease = queue
            .claim(&lib, &job, &FakeProcessProbe::new(4242))
            .unwrap()
            .unwrap();
        let meeting = s.meeting.clone();
        lease
            .commit(&lib, JobState::Done, |tx, _| {
                let mut m: Meeting = tx.library().load_meeting(&meeting)?;
                m.enrichment = Some(
                    serde_json::from_value(json!({
                        "provider": "agent-a", "created_at": "2026-11-15T00:00:00Z",
                        "summary": "Synthetic summary."
                    }))
                    .unwrap(),
                );
                tx.save_meeting(&m)
            })
            .unwrap();
        lease.release(&lib).unwrap();
        lib.write(|tx| Queue::open(tx.root())?.remove_finished(tx, ENRICH_KIND))
            .unwrap();
        window
            .update(cx, |view, window, cx| view.reload(window, cx))
            .unwrap();
        cx.run_until_parked();
        panel.read_with(cx, |p, _| {
            assert!(p.has_enrichment());
            assert_eq!(p.phase(), &Phase::Done);
            assert!(!p.ticking());
        });
        // The end reported late changes nothing.
        app.update(cx, |s, cx| {
            s.apply_worker_batch(event(ProgressEventKind::JobDone, &job), cx)
        });
        cx.run_until_parked();
        panel.read_with(cx, |p, _| {
            assert_eq!(p.phase(), &Phase::Done);
            assert!(!p.ticking());
        });
    }

    #[gpui::test]
    fn a_generation_is_watched_and_announced_when_its_view_closes_before_queueing(
        cx: &mut TestAppContext,
    ) {
        use std::cell::RefCell;
        let Fixture {
            window,
            panel,
            app,
            lib,
            s,
            job,
            dir: _dir,
        } = open(cx);
        let posted: Rc<RefCell<Vec<broscribe_ipc::NotificationRequest>>> = Rc::default();
        app.update(cx, |st, _| {
            let posted = posted.clone();
            st.set_generation_sink(Rc::new(move |request, _| posted.borrow_mut().push(request)))
        });
        panel.update(cx, |p, cx| p.choose_provider("agent-a".into(), cx));
        cx.run_until_parked();
        enqueue_job(&lib, &s.meeting);
        window
            .update(cx, |_, window, cx| {
                panel.update(cx, |p, cx| p.generate(window, cx))
            })
            .unwrap();
        // The meeting closes before the CLI answers: view and panel go.
        let weak = panel.downgrade();
        drop(panel);
        window
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        assert!(weak.upgrade().is_none(), "the panel outlived its view");
        app.read_with(cx, |st, _| assert!(st.generations().is_watching(&job)));

        let queue = Queue::open(lib.root()).unwrap();
        let lease = queue
            .claim(&lib, &job, &FakeProcessProbe::new(4242))
            .unwrap()
            .unwrap();
        let meeting = s.meeting.clone();
        lease
            .commit(&lib, JobState::Done, |tx, _| {
                let mut m: Meeting = tx.library().load_meeting(&meeting)?;
                m.enrichment = Some(
                    serde_json::from_value(json!({
                        "provider": "agent-a", "created_at": "2026-11-15T00:00:00Z",
                        "summary": "Synthetic summary."
                    }))
                    .unwrap(),
                );
                tx.save_meeting(&m)
            })
            .unwrap();
        lease.release(&lib).unwrap();
        lib.write(|tx| Queue::open(tx.root())?.remove_finished(tx, ENRICH_KIND))
            .unwrap();
        app.update(cx, |s, cx| {
            s.apply_worker_batch(event(ProgressEventKind::JobDone, &job), cx)
        });
        cx.run_until_parked();
        let posted = posted.borrow();
        assert_eq!(posted.len(), 1, "{posted:?}");
        assert_eq!(
            posted[0].title,
            broscribe_platform_native::NativePlatform::notification_title(
                "Summary Ready",
                "Summary ready"
            )
        );
    }

    #[gpui::test]
    fn instructions_prefill_override_on_stdin_and_save_as_default(cx: &mut TestAppContext) {
        let f = open(cx);
        let mut config = f.app.read_with(cx, |s, _| s.config().clone());
        config.enrichment.instructions = Some("Synthetic default focus.".into());
        f.app.update(cx, |s, cx| s.set_config(config, cx));
        f.panel
            .update(cx, |p, cx| p.choose_provider("agent-a".into(), cx));
        cx.run_until_parked();
        let panel = f.panel.clone();
        f.window
            .update(cx, |_, window, cx| {
                panel.update(cx, |p, cx| p.toggle_instructions(window, cx))
            })
            .unwrap();
        f.panel.read_with(cx, |p, cx| {
            assert!(p.instructions_open());
            assert_eq!(
                p.instructions_input().read(cx).value().as_ref(),
                "Synthetic default focus."
            );
            // Unchanged: the worker reads the saved default itself.
            assert_eq!(p.instructions_override(cx), None);
        });
        draw(&f, cx);
        let input = f.panel.read_with(cx, |p, _| p.instructions_input().clone());
        let custom = "Only the synthetic decisions, in two lines.";
        f.window
            .update(cx, |_, window, cx| {
                input.update(cx, |i, cx| i.set_value(custom.to_owned(), window, cx))
            })
            .unwrap();
        enqueue_job(&f.lib, &f.s.meeting);
        generate(&f, cx);
        cx.run_until_parked();
        let call = enrich_calls(f.dir.path()).pop().unwrap();
        assert!(
            call.ends_with("--queue --provider agent-a --instructions-stdin"),
            "{call}"
        );
        assert!(
            !call.contains("synthetic decisions"),
            "instructions leaked into argv"
        );
        assert_eq!(log(f.dir.path(), "enrich-stdin"), custom);

        f.panel.update(cx, |p, cx| p.save_instructions_default(cx));
        cx.run_until_parked();
        assert!(log(f.dir.path(), "args.log").contains(" enrich instructions set --stdin\n"));
        assert_eq!(log(f.dir.path(), "saved-stdin"), custom);
        f.panel.read_with(cx, |p, _| {
            assert!(!p.instructions_notice().unwrap().1);
        });
    }

    /// Publishes a new summary for the claimed job and finishes it the way
    /// the worker does, without reporting anything.
    fn publish_silently(f: &Fixture, created_at: &str) {
        let queue = Queue::open(f.lib.root()).unwrap();
        let lease = queue
            .claim(&f.lib, &f.job, &FakeProcessProbe::new(4242))
            .unwrap()
            .unwrap();
        let meeting = f.s.meeting.clone();
        let created_at = created_at.to_owned();
        lease
            .commit(&f.lib, JobState::Done, |tx, _| {
                let mut m: Meeting = tx.library().load_meeting(&meeting)?;
                m.enrichment = Some(
                    serde_json::from_value(json!({
                        "provider": "agent-a", "created_at": created_at,
                        "summary": "Synthetic summary."
                    }))
                    .unwrap(),
                );
                tx.save_meeting(&m)
            })
            .unwrap();
        finish_like_the_worker(f, lease);
    }

    /// Counts the panel's `Finished` events.
    fn count_finished(
        f: &Fixture,
        cx: &mut TestAppContext,
    ) -> (Rc<std::cell::Cell<u32>>, Subscription) {
        let finished = Rc::new(std::cell::Cell::new(0));
        let seen = finished.clone();
        let sub = cx.update(|cx| {
            cx.subscribe(&f.panel, move |_, ev: &GenerateEvent, _| {
                if *ev == GenerateEvent::Finished {
                    seen.set(seen.get() + 1);
                }
            })
        });
        (finished, sub)
    }

    #[gpui::test]
    fn a_lost_job_event_still_ends_the_run_from_the_files(cx: &mut TestAppContext) {
        let f = open(cx);
        f.panel
            .update(cx, |p, cx| p.choose_provider("agent-a".into(), cx));
        cx.run_until_parked();
        enqueue_job(&f.lib, &f.s.meeting);
        generate(&f, cx);
        cx.run_until_parked();
        f.panel.read_with(cx, |p, _| {
            assert_eq!(p.phase(), &Phase::Queued(f.job.clone()));
            assert!(p.ticking());
        });
        // The worker finishes, but its events never reach the app.
        publish_silently(&f, "2026-11-15T00:00:00Z");
        for _ in 0..RECONCILE_TICKS {
            cx.executor().advance_clock(GENERATION_TICK);
            cx.run_until_parked();
        }
        f.panel.read_with(cx, |p, _| {
            assert_eq!(p.phase(), &Phase::Done, "not Generating for ever");
            assert!(!p.ticking());
        });
    }

    #[gpui::test]
    fn a_run_ends_on_its_own_result_even_when_the_job_is_queued_again(cx: &mut TestAppContext) {
        let f = open(cx);
        let (finished, _sub) = count_finished(&f, cx);
        f.panel
            .update(cx, |p, cx| p.choose_provider("agent-a".into(), cx));
        cx.run_until_parked();
        enqueue_job(&f.lib, &f.s.meeting);
        generate(&f, cx);
        cx.run_until_parked();
        // Our run publishes, and before the panel reads again another
        // surface queues the same job (same ID) for a new run.
        publish_silently(&f, "2026-11-15T00:00:00Z");
        assert_eq!(enqueue_job(&f.lib, &f.s.meeting), f.job);
        f.app.update(cx, |s, cx| {
            s.apply_worker_batch(event(ProgressEventKind::JobDone, &f.job), cx)
        });
        cx.run_until_parked();
        assert_eq!(finished.get(), 1, "our run is reported generated once");
        // The other run is then followed as one this panel did not start.
        f.panel.read_with(cx, |p, _| {
            assert!(p.phase().in_flight(), "{:?}", p.phase());
            assert!(p.has_enrichment());
        });
    }

    #[gpui::test]
    fn an_instructions_draft_survives_collapsing(cx: &mut TestAppContext) {
        let f = open(cx);
        let mut config = f.app.read_with(cx, |s, _| s.config().clone());
        config.enrichment.instructions = Some("Synthetic default focus.".into());
        f.app.update(cx, |s, cx| s.set_config(config, cx));
        let panel = f.panel.clone();
        let toggle = |cx: &mut TestAppContext| {
            f.window
                .update(cx, |_, window, cx| {
                    panel.update(cx, |p, cx| p.toggle_instructions(window, cx))
                })
                .unwrap();
        };
        toggle(cx);
        let input = f.panel.read_with(cx, |p, _| p.instructions_input().clone());
        let draft = "Only the synthetic risks.";
        f.window
            .update(cx, |_, window, cx| {
                input.update(cx, |i, cx| i.set_value(draft.to_owned(), window, cx))
            })
            .unwrap();
        // Collapsed: the draft is kept and still applies to the next run.
        toggle(cx);
        f.panel.read_with(cx, |p, cx| {
            assert!(!p.instructions_open());
            assert_eq!(p.instructions_override(cx).as_deref(), Some(draft));
        });
        draw(&f, cx);
        // Reopened: the draft, not the saved default.
        toggle(cx);
        f.panel.read_with(cx, |p, cx| {
            assert_eq!(p.instructions_input().read(cx).value().as_ref(), draft);
        });
    }

    #[gpui::test]
    fn switching_to_a_cached_provider_drops_the_old_lists_late_answer(cx: &mut TestAppContext) {
        let f = open(cx);
        // `remote`'s list is cached.
        f.panel
            .update(cx, |p, cx| p.choose_provider("remote".into(), cx));
        cx.run_until_parked();
        // Refresh the automatically preselected provider's cached list.
        // The request fails, but the user returns to `remote` before it answers.
        std::fs::write(f.dir.path().join("models-fail"), "").unwrap();
        f.panel.update(cx, |p, cx| {
            p.choose_provider("agent-a".into(), cx);
            assert!(!p.models_loading());
            p.refresh_models(cx);
            assert!(p.models_loading());
            p.choose_provider("remote".into(), cx);
            assert!(!p.models_loading());
            p.choose_model(ModelChoice::Listed("beta-2".into()), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            log(f.dir.path(), "models.log"),
            "agent-a\nremote\nagent-a\n"
        );
        f.panel.read_with(cx, |p, _| {
            assert_eq!(p.provider(), Some("remote"));
            assert_eq!(p.model_error(), None, "agent-a's failure is not remote's");
            assert!(!p.models_loading());
            assert_eq!(p.choice(), Some(&ModelChoice::Listed("beta-2".into())));
        });
    }
}
