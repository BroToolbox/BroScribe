//! The setup assistant (design §4.1; plan §4.6, §8 items 1, 11, 16, 18, 19,
//! 22).
//!
//! Four essentials and three recommendations use the existing saved step
//! IDs. Language and speech model covers both `languages` and `models`.
//!
//! Microphone capture runs in the installed helper, or in the app only when
//! no helper is installed; model work runs in a CLI child (`setup voice
//! --check-recording`, `setup voice --from-file`, `setup try --from-file`).
//! Captures are bounded (45 s passage, 25 s confirmation reading, 25 s Try
//! it, 2 s microphone check), carry a generation id so the result of a
//! cancelled or superseded capture is dropped, live in a 0700 temp dir under
//! the cache folder named for this app process (removed on completion,
//! cancel and close; folders of earlier processes removed by
//! [`remove_orphan_capture_dirs`]), and pause app playback first. A capture
//! keeps the device it was recorded on (the helper names the one it opened),
//! which is what the enrollment records. Device opens and stops run off the
//! UI thread, and drawing the meter never locks the audio buffer. The meter
//! redraws at most 10 times a second and only when what it shows changed
//! (level, clipping or elapsed second), so elapsed time also advances in
//! silence.
//! Hiding the assistant ([`SetupAssistant::set_visible`]) stops its capture
//! and its Accessibility polling. Cancellations of helper captures are
//! tracked app-wide in [`SetupRuns`], so quitting waits (bounded) for them
//! to end even after their assistant closed, and never touches a recording.
//!
//! First-recording check (design §4.1): recording 1 (the reading) goes to
//! `setup voice --check-recording --from-file <copy> --session <cache-id>`
//! off the UI thread before recording 2 (the optional phrase) or its skip
//! is offered. The child receives a 0600 copy under
//! `setup-capture/<cache-id>`; the original clip stays with the check task
//! and becomes the held passage clip only when the reply says
//! `ready` (enough speech for the enrollment's parts, not an identity
//! guarantee). The check's `--session` places the copy only; it is never an
//! enrollment token. Cancel, Read again, navigation, close and quit drop the
//! check: its late reply is ignored, its child is killed and reaped, then
//! its session folder is removed, chained with the pending cancellation so
//! the kill never reaches a later child.
//!
//! Enrollment tokens (plan items 19, 22): the app writes a fresh session id
//! to `config.setup.voice_session` inside `Library::write` before spawning
//! `broscribe setup voice --from-file … --session <id> --keep-as-enrollment`;
//! the child commits only if the id still matches under the same lock.
//! Cancel / supersede / close clear the id under the lock first, then kill
//! and reap the setup child; if the child had already committed (new
//! enrollment voiceprint IDs appeared), enrollment is reported as completed.
//!
//! Inconclusive confirmation check: a submission with the confirmation
//! reading passes `--report-unconfirmed`, and recording 1 itself moves
//! through preparation into the job (RAM only; the child receives its own
//! private copy). Only a typed `confirmation_unconfirmed` reply of the
//! current job gives it back: a comparison below its threshold, less
//! detected speech than `CONFIRM_MIN_SPEECH_S` (`too_short`), or enough
//! speech the model could not use (`unusable`); the last two carry the
//! measured seconds and no score. It comes back as ready with the reply's
//! own reading warnings, and offers three explicit
//! choices: record the phrase again (recording 1 is not recorded again),
//! save recording 1 without the phrase through the normal skip submission,
//! or discard it and read again. Nothing is saved before a choice; cancel,
//! supersede, a late reply, navigation, close and quit drop the held clip.
//! The finished job's token clear and folder removal join the pending
//! cancellation chain, so the next job prepares after them.
//!
//! First run and Settings share one standalone setup window. The window
//! removes itself after [`SetupEvent::Closed`].

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use broscribe_core::config::{
    CONFIRM_MAX_CAPTURE_S, CONFIRM_MIN_SPEECH_S, CONFIRM_TARGET_CAPTURE_S, Config, Retention,
};
use broscribe_core::names::validate_name;
use broscribe_core::schema::{Handling, VoiceprintKind};
use broscribe_core::{Library, MeetingId};
use broscribe_ipc::Call;
use broscribe_platform::system::NotificationAuthorization;
use broscribe_platform::{
    AudioDevice, CaptureSession, HostPlatform, MicCapture, MicChunk, PcmSink, SettingsPane,
    SystemInfo, SystemSettingsOpener,
};
use broscribe_platform_native::NativePlatform;
use broscribe_platform_native::ipc::{ANSWER_TIMEOUT, Client, Endpoint};
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::Shared;
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui::component::{ActiveTheme as _, Disableable as _};
use gpui::prelude::FluentBuilder as _;
#[cfg(test)]
use gpui::px;
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Div, ElementId, Entity, EventEmitter,
    FocusHandle, Focusable as _, FontWeight, Global, InteractiveElement as _, IntoElement,
    ParentElement as _, PathPromptOptions, Render, Role, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div,
};
use serde_json::Value;

use super::settings::{
    ChoiceGroup, ModelRow, ModelsList, PullCancel, PullState, RetentionGuard, RetentionTrigger,
    args, choice, error_line, hotword_note, mark_setup_completed, mark_step, muted,
    parse_models_list, reload_config, request_retention_change, row, spawn_pull, storage_section,
    update_config, valid_language_tag,
};
use crate::daemon::DaemonEvent;
use crate::jobs::{
    ChildKind, ChildrenStopping, CliError, CliRunner, core_user_text, failure_text, io_user_text,
    sentence, unsendable_path,
};
use crate::launch::{AgentRecovery, AgentStartStatus};
use crate::state::{AppEvent, AppState, Route, SettingsPage};
use crate::ui::format::plural;
use crate::ui::type_scale::{TextRole, TextRoleExt as _};
use crate::ui::{Spoken as _, text_line};
use crate::ui::{assets, kit};
use crate::views::agents::AgentsView;
use crate::views::providers::{ProviderScope, ProvidersView};
use crate::views::settings::role_label;

/// Platform services the assistant uses besides [`AppState`].
#[derive(Clone)]
pub struct SetupDeps {
    pub mic: Arc<dyn MicCapture>,
    pub system: Arc<dyn SystemInfo>,
}

/// Emitted when the user closes or finishes the assistant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetupEvent {
    Closed,
}

/// Recorded in `completed_steps` when the user finished the assistant.
pub const ASSISTANT_DONE: &str = "assistant";

/// Folder (under the cache dir) holding capture temp dirs.
pub const CAPTURE_DIR: &str = "setup-capture";

/// Whether to open the assistant at launch: until the user finished it once.
/// Steps done with the CLI count, so the assistant resumes after them.
pub fn should_show_first_run(config: &Config) -> bool {
    !config.setup.defer_assistant
        && !config
            .setup
            .completed_steps
            .iter()
            .any(|s| s == ASSISTANT_DONE)
}

pub(crate) const ESSENTIAL_SETUP_COUNT: usize = 4;

/// The essentials use the existing saved step IDs, including both parts
/// of Language and speech model.
pub(crate) fn unfinished_essentials(config: &Config) -> Option<usize> {
    let done = &config.setup.completed_steps;
    if done.iter().any(|step| step == ASSISTANT_DONE) {
        return None;
    }
    let complete = |step: Step| done.iter().any(|id| id == step.id());
    let completed = [
        complete(Step::Languages) && complete(Step::Models),
        complete(Step::Permissions),
        complete(Step::Voice),
        complete(Step::TryIt),
    ]
    .into_iter()
    .filter(|done| *done)
    .count();
    (completed < ESSENTIAL_SETUP_COUNT).then_some(completed)
}

/// Removes the capture temp dirs of earlier app processes (a crash or a
/// forced quit), never this process's: an assistant of this process may
/// still be writing, checking or cancelling in its own. Safe to call at any
/// time, from any thread (launch calls it; each new assistant too).
pub fn remove_orphan_capture_dirs(cache_dir: &Path) {
    let dir = cache_dir.join(CAPTURE_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            tracing::warn!(kind = ?e.kind(), "listing setup capture folders");
            return;
        }
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_str().is_some_and(owned_by_this_process) {
            continue;
        }
        let path = entry.path();
        let removed = if entry.file_type().is_ok_and(|t| t.is_dir()) {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(e) = removed
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(kind = ?e.kind(), "removing an orphaned setup capture");
        }
    }
}

/// This app process's tag in capture folder names: its PID and start time,
/// so a folder left by an earlier process that had the same PID is still
/// an orphan.
static CAPTURE_OWNER: LazyLock<String> = LazyLock::new(|| {
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u32)
        .unwrap_or(0);
    format!("{}_{started:08x}", std::process::id())
});

/// Whether a capture folder name (`<prefix>-<owner>-<generation>-<nanos>`)
/// belongs to this process.
fn owned_by_this_process(name: &str) -> bool {
    name.split('-').nth(1) == Some(CAPTURE_OWNER.as_str())
}

/// Setup work that must end before the app exits even when the assistant
/// that started it is gone: helper-capture cancellations (stop with discard,
/// then the capture request's own end) and stops of in-app capture devices.
/// Held as a GPUI global; never contains anything that stops or discards a
/// meeting recording.
#[derive(Clone, Default)]
pub struct SetupRuns(Rc<RefCell<Vec<Shared<Task<()>>>>>);

impl Global for SetupRuns {}

impl SetupRuns {
    fn global(cx: &mut App) -> Self {
        cx.default_global::<Self>().clone()
    }

    /// Keeps `task` until it ends; finished ones are dropped.
    fn track(cx: &mut App, task: Shared<Task<()>>) {
        let runs = Self::global(cx);
        let mut tasks = runs.0.borrow_mut();
        tasks.retain(|t| t.peek().is_none());
        tasks.push(task);
    }

    /// For `cx.on_app_quit`: resolves once every setup cancellation tracked
    /// by the time it is first polled has ended (so cancellations other quit
    /// hooks start count too), or after [`QUIT_CANCEL_TIMEOUT`], within
    /// GPUI's shutdown limit. Sends nothing itself and never touches a
    /// recording session.
    pub fn quit(cx: &mut App) -> impl Future<Output = ()> + 'static + use<> {
        let runs = Self::global(cx);
        let deadline = cx.background_executor().timer(QUIT_CANCEL_TIMEOUT);
        async move {
            let pending: Vec<_> = runs
                .0
                .borrow()
                .iter()
                .filter(|t| t.peek().is_none())
                .cloned()
                .collect();
            let all = futures::future::join_all(pending);
            futures::pin_mut!(all, deadline);
            let _ = futures::future::select(all, deadline).await;
        }
    }
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Welcome,
    Ready,
    Overview,
    Languages,
    Permissions,
    Models,
    Voice,
    AudioCheck,
    Summaries,
    Words,
    MeetingApps,
    Storage,
    Calendars,
    Agents,
    Existing,
    TryIt,
}

impl Step {
    pub const ALL: [Step; 13] = [
        Step::Languages,
        Step::Models,
        Step::Permissions,
        Step::Voice,
        Step::TryIt,
        Step::AudioCheck,
        Step::Summaries,
        Step::MeetingApps,
        Step::Words,
        Step::Storage,
        Step::Calendars,
        Step::Agents,
        Step::Existing,
    ];
    const DISPLAYED: [Step; 7] = [
        Step::Models,
        Step::Permissions,
        Step::Voice,
        Step::TryIt,
        Step::AudioCheck,
        Step::Summaries,
        Step::MeetingApps,
    ];

    fn completed(self, config: &Config) -> bool {
        let done = &config.setup.completed_steps;
        done.iter().any(|id| id == self.id())
            && (self != Self::Models || done.iter().any(|id| id == Self::Languages.id()))
    }

    /// The name stored in `completed_steps` (shared with the CLI).
    pub fn id(self) -> &'static str {
        match self {
            Step::Welcome | Step::Ready | Step::Overview => ASSISTANT_DONE,
            Step::Languages => "languages",
            Step::Permissions => "permissions",
            Step::Models => "models",
            Step::Voice => "voice",
            Step::AudioCheck => "audio_check",
            Step::Summaries => "summaries",
            Step::Words => "words",
            Step::MeetingApps => "meeting_apps",
            Step::Storage => "storage",
            Step::Calendars => "calendars",
            Step::Agents => "agents",
            Step::Existing => "existing",
            Step::TryIt => "try",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Step::Welcome => "Welcome to BroScribe",
            Step::Ready => "BroScribe is ready",
            Step::Overview => "Setup",
            Step::Languages | Step::Models => "Language and speech model",
            Step::Permissions => "Recording access",
            Step::Voice => "Your voice",
            Step::AudioCheck => "Audio check",
            Step::Summaries => "Summaries",
            Step::Words => "Words and people",
            Step::MeetingApps => "Meeting apps",
            Step::Storage => "Storage",
            Step::Calendars => "Calendars",
            Step::Agents => "AI tools",
            Step::Existing => "Existing recordings",
            Step::TryIt => "Try it",
        }
    }

    fn position(self) -> Option<usize> {
        Self::DISPLAYED.iter().position(|step| *step == self)
    }

    fn next(self) -> Step {
        match self {
            Self::Welcome => Self::Models,
            Self::Languages | Self::Models => Self::Permissions,
            Self::TryIt | Self::MeetingApps => Self::Ready,
            _ => self
                .position()
                .and_then(|i| Self::DISPLAYED.get(i + 1).copied())
                .unwrap_or(Self::Ready),
        }
    }

    fn prev(self) -> Option<Step> {
        match self {
            Self::Welcome | Self::Overview => None,
            Self::Ready => Some(Self::TryIt),
            Self::Models | Self::Languages => Some(Self::Welcome),
            Self::AudioCheck => Some(Self::Ready),
            _ => self
                .position()
                .and_then(|i| i.checked_sub(1))
                .map(|i| Self::DISPLAYED[i]),
        }
    }
}

pub fn resume_step(config: &Config) -> Step {
    let done = &config.setup.completed_steps;
    if done.iter().any(|id| id == ASSISTANT_DONE) {
        return Step::Overview;
    }
    if done.is_empty() {
        return Step::Welcome;
    }
    Step::DISPLAYED
        .into_iter()
        .find(|step| !step.completed(config))
        .unwrap_or(Step::Overview)
}

#[derive(Clone, Copy)]
enum Departure {
    Step(Step),
    Later,
}

struct DiscardConfirmation {
    destination: Departure,
    opener: Option<FocusHandle>,
    try_it: bool,
    seconds: Option<f32>,
}

struct DownloadModel {
    id: String,
    name: String,
    bytes: u64,
    installed: bool,
}

fn required_models(list: &ModelsList) -> Result<Vec<&ModelRow>, String> {
    let ids = list.profile_models.as_ref().map_err(|e| {
        failure_text(
            "Couldn't find the models needed for transcription.",
            &sentence(e),
            "",
        )
    })?;
    ids.iter()
        .map(|id| {
            list.rows
                .iter()
                .find(|row| &row.id == id)
                .ok_or_else(|| "A required model is missing from the registry.".into())
        })
        .collect()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum DownloadStatus {
    #[default]
    Idle,
    Running {
        percent: u8,
        total: u64,
    },
    Done(u64),
    Cancelled,
    Failed(String),
}

/// Lives in AppState so setup, the main window and Settings can outlive each other.
#[derive(Default)]
pub(crate) struct ModelDownloads {
    status: DownloadStatus,
    stop: Option<Arc<AtomicBool>>,
    pull: Option<PullCancel>,
}

impl ModelDownloads {
    pub(crate) fn running(&self) -> bool {
        matches!(self.status, DownloadStatus::Running { .. })
    }

    pub(crate) fn percent(&self) -> Option<u8> {
        match self.status {
            DownloadStatus::Running { percent, .. } => Some(percent),
            _ => None,
        }
    }

    pub(crate) fn stopping(&self) -> bool {
        self.stop
            .as_ref()
            .is_some_and(|stop| stop.load(Ordering::SeqCst))
    }

    pub(crate) fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::SeqCst);
            if let Some(pull) = &self.pull {
                pull.cancel();
            }
            cx.notify();
        }
    }

    fn set_status(&mut self, status: DownloadStatus, cx: &mut Context<Self>) {
        if self.status != status {
            self.status = status;
            cx.notify();
        }
    }

    fn start(
        &mut self,
        state: Entity<AppState>,
        models: Vec<DownloadModel>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.running() || models.iter().all(|row| row.installed) {
            return false;
        }
        let total = models.iter().map(|model| model.bytes).sum();
        let mut installed = models
            .iter()
            .filter(|model| model.installed)
            .map(|model| model.bytes)
            .sum();
        self.set_status(
            DownloadStatus::Running {
                percent: download_percent(installed, 0, 0, total),
                total,
            },
            cx,
        );
        let stop = Arc::new(AtomicBool::new(false));
        self.stop = Some(stop.clone());
        let cli = state.read(cx).cli().clone();
        let profile = state.read(cx).config().profile.clone();
        cx.spawn(async move |this, cx| {
            let mut end = DownloadStatus::Done(total);
            for model in models.into_iter().filter(|row| !row.installed) {
                if stop.load(Ordering::SeqCst) {
                    end = DownloadStatus::Cancelled;
                    break;
                }
                let (task, mut progress, pull) =
                    spawn_pull(cli.clone(), model.id, false, cx.background_executor());
                if this
                    .update(cx, |this, _| this.pull = Some(pull.clone()))
                    .is_err()
                {
                    pull.cancel();
                    return;
                }
                while let Some(pct) = progress.next().await {
                    let percent = download_percent(installed, model.bytes, pct, total);
                    let _ = this.update(cx, |this, cx| {
                        this.set_status(DownloadStatus::Running { percent, total }, cx)
                    });
                }
                match PullState::finished(&model.name, &task.await) {
                    PullState::Done => {
                        installed += model.bytes;
                        let percent = download_percent(installed, 0, 0, total);
                        let _ = this.update(cx, |this, cx| {
                            this.set_status(DownloadStatus::Running { percent, total }, cx)
                        });
                    }
                    PullState::Cancelled => {
                        end = DownloadStatus::Cancelled;
                        break;
                    }
                    PullState::Failed(error) => {
                        end = DownloadStatus::Failed(error);
                        break;
                    }
                    _ => unreachable!("a finished pull returns a terminal state"),
                }
            }
            if matches!(end, DownloadStatus::Done(_)) {
                let saved = this.update(cx, |_, cx| {
                    update_config(
                        &state,
                        move |config| {
                            if config.profile == profile {
                                mark_setup_completed(config, Step::Models.id());
                            }
                        },
                        cx,
                    )
                });
                if let Ok(saved) = saved
                    && let Err(error) = saved.await
                {
                    end = DownloadStatus::Failed(error);
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.pull = None;
                this.stop = None;
                this.set_status(end, cx);
            });
        })
        .detach();
        true
    }
}

fn download_percent(installed: u64, bytes: u64, model_percent: u8, total: u64) -> u8 {
    if total == 0 {
        return 0;
    }
    ((u128::from(installed) * 100 + u128::from(bytes) * u128::from(model_percent))
        / u128::from(total))
    .min(100) as u8
}

// ---------------------------------------------------------------------------
// Capture
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapturePurpose {
    MicCheck,
    Passage,
    Confirmation,
    TryIt,
}

impl CapturePurpose {
    pub fn limit(self) -> Duration {
        Duration::from_secs(match self {
            CapturePurpose::MicCheck => 2,
            CapturePurpose::Passage => 45,
            CapturePurpose::Confirmation => CONFIRM_MAX_CAPTURE_S,
            CapturePurpose::TryIt => 25,
        })
    }
}

/// Fastest meter redraw while capturing: the product's 10 Hz meter limit.
pub const METER_INTERVAL: Duration = Duration::from_millis(100);
/// How long quitting waits for setup cancellations (under GPUI's 200 ms
/// shutdown limit).
pub const QUIT_CANCEL_TIMEOUT: Duration = Duration::from_millis(150);

/// Quitting with an assistant open: `cancellation` (token clears under the
/// library lock, then each run's own stop) runs first; whatever setup run is
/// still going is then cancelled with `stop` and its reaping awaited, all
/// within the one `deadline`.
async fn finish_setup_quit(
    cancellation: impl Future<Output = ()>,
    deadline: impl Future<Output = ()>,
    stop: impl FnOnce() -> ChildrenStopping,
) {
    futures::pin_mut!(cancellation, deadline);
    let in_time = matches!(
        futures::future::select(cancellation, deadline.as_mut()).await,
        futures::future::Either::Left(_)
    );
    let stopping = stop();
    if in_time {
        let finished = stopping.finished();
        futures::pin_mut!(finished);
        let _ = futures::future::select(finished, deadline).await;
    }
}

/// Audio captured by one session (mono, device rate) and its live levels.
#[derive(Debug, Default)]
pub struct Recorder {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub max_samples: Option<usize>,
    pub peak: f32,
    pub rms: f32,
    pub clipped: usize,
}

impl Recorder {
    pub fn push(&mut self, chunk: &MicChunk, limit: Duration) {
        if self.sample_rate == 0 {
            self.sample_rate = chunk.sample_rate;
            self.max_samples = Some((limit.as_secs_f64() * f64::from(chunk.sample_rate)) as usize);
        }
        let room = self
            .max_samples
            .map_or(usize::MAX, |m| m.saturating_sub(self.samples.len()));
        let take = chunk.samples_mono_f32.len().min(room);
        let samples = &chunk.samples_mono_f32[..take];
        self.clipped += samples.iter().filter(|s| s.abs() >= 0.99).count();
        self.samples.extend_from_slice(samples);
        self.peak = chunk.level_peak;
        self.rms = chunk.level_rms;
    }

    pub fn full(&self) -> bool {
        self.max_samples.is_some_and(|m| self.samples.len() >= m)
    }
}

/// One in-app capture's audio, filled by the device callback. The callback
/// also publishes the meter's values in atomics, so drawing the meter never
/// locks the recorder (and never makes the callback wait for the UI).
struct CaptureShared {
    rec: Mutex<Recorder>,
    /// The sink was dropped: capture ended (stopped, or the device failed).
    ended: AtomicBool,
    /// The audio was taken or discarded: later chunks are dropped.
    closed: AtomicBool,
    limit: Duration,
    peak: AtomicU32,
    rms: AtomicU32,
    recorded: AtomicUsize,
    sample_rate: AtomicU32,
    full: AtomicBool,
}

impl CaptureShared {
    fn new(limit: Duration) -> Self {
        Self {
            rec: Mutex::new(Recorder::default()),
            ended: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            limit,
            peak: AtomicU32::new(0),
            rms: AtomicU32::new(0),
            recorded: AtomicUsize::new(0),
            sample_rate: AtomicU32::new(0),
            full: AtomicBool::new(false),
        }
    }

    fn rec(&self) -> std::sync::MutexGuard<'_, Recorder> {
        self.rec.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The device callback: records `chunk` (up to the limit) and publishes
    /// the meter's values.
    fn push(&self, chunk: &MicChunk) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let mut rec = self.rec();
        rec.push(chunk, self.limit);
        self.peak.store(rec.peak.to_bits(), Ordering::Relaxed);
        self.rms.store(rec.rms.to_bits(), Ordering::Relaxed);
        self.recorded.store(rec.samples.len(), Ordering::Relaxed);
        self.sample_rate.store(rec.sample_rate, Ordering::Relaxed);
        self.full.store(rec.full(), Ordering::Release);
    }

    /// The last published level and recorded seconds.
    fn meter(&self) -> (MeterLevel, f64) {
        let level = MeterLevel {
            peak: f32::from_bits(self.peak.load(Ordering::Relaxed)),
            rms: f32::from_bits(self.rms.load(Ordering::Relaxed)),
        };
        let rate = self.sample_rate.load(Ordering::Relaxed);
        let seconds = if rate == 0 {
            0.0
        } else {
            self.recorded.load(Ordering::Relaxed) as f64 / f64::from(rate)
        };
        (level, seconds)
    }

    /// Takes the recorded audio; chunks arriving later are dropped.
    fn take(&self) -> Clip {
        self.closed.store(true, Ordering::Release);
        let mut rec = self.rec();
        Clip {
            samples: std::mem::take(&mut rec.samples),
            sample_rate: rec.sample_rate,
            clipped: rec.clipped,
        }
    }
}

/// Lives inside the capture sink; marks the capture ended when dropped.
struct SinkGuard {
    shared: Arc<CaptureShared>,
    _admission: Arc<broscribe_core::maintenance::ModeLease>,
}

impl SinkGuard {
    fn push(&self, chunk: &MicChunk) {
        self.shared.push(chunk);
    }
}

impl Drop for SinkGuard {
    fn drop(&mut self) {
        self.shared.ended.store(true, Ordering::SeqCst);
    }
}

/// What a capture's meter shows: the level bar in whole percent (`None`
/// before the helper's first level), the clipping warning and the elapsed
/// second. The meter redraws only when this changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MeterView {
    level: Option<(usize, bool)>,
    clipping: bool,
    seconds: u64,
}

impl MeterView {
    fn new(level: Option<MeterLevel>, seconds: f64, clipped: bool) -> Self {
        Self {
            level: level.map(MeterLevel::readout),
            clipping: clipped,
            seconds: seconds.round() as u64,
        }
    }
}

struct ActiveCapture {
    generation: u64,
    purpose: CapturePurpose,
    shared: Arc<CaptureShared>,
    started: Instant,
    /// `false` until the device delivered its session.
    running: bool,
    /// The in-app device session; stopped off the UI thread.
    session: Option<Box<dyn CaptureSession>>,
    /// The in-app device recorded on (an `AudioDevice::id`; `None`: the
    /// system default). A helper capture learns its device from the reply.
    device: Option<String>,
    helper_session: Option<String>,
    /// Levels the helper reports for `helper_session`.
    helper_meters: HelperMeters,
    /// Resolves once the helper's capture request has ended.
    helper_done: Option<Shared<oneshot::Receiver<()>>>,
    /// What the meter showed at its last redraw.
    shown: Option<MeterView>,
    clipped: bool,
    _ticker: Option<Task<()>>,
}

impl ActiveCapture {
    fn new(
        generation: u64,
        purpose: CapturePurpose,
        device: Option<String>,
        helper_session: Option<String>,
    ) -> Self {
        Self {
            generation,
            purpose,
            shared: Arc::new(CaptureShared::new(purpose.limit())),
            started: Instant::now(),
            running: false,
            session: None,
            device,
            helper_session,
            helper_meters: HelperMeters::default(),
            helper_done: None,
            shown: None,
            clipped: false,
            _ticker: None,
        }
    }

    /// The meter's level (`None`: no helper level yet) and elapsed seconds;
    /// `None` while an in-app device is still opening. A helper capture's
    /// time runs on its own clock, so it advances in silence too.
    fn meter(&self) -> Option<(Option<MeterLevel>, f64)> {
        if self.helper_session.is_some() {
            let limit = self.shared.limit.as_secs_f64();
            return Some((
                self.helper_meters.me,
                self.started.elapsed().as_secs_f64().min(limit),
            ));
        }
        if !self.running {
            return None;
        }
        let (level, seconds) = self.shared.meter();
        Some((Some(level), seconds))
    }

    /// Whether the meter shows something new since its last redraw (and
    /// records it as shown).
    fn redraw_needed(&mut self) -> bool {
        let view = self.meter().map(|(level, seconds)| {
            self.clipped |= level.is_some_and(MeterLevel::clipping);
            MeterView::new(level, seconds, self.clipped)
        });
        let changed = view != self.shown;
        self.shown = view;
        changed
    }
}

/// Captured audio handed to the CLI.
#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub clipped: usize,
}

/// Encodes mono f32 samples as 16-bit PCM WAV.
pub fn wav_bytes(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + samples.len() * 2);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// A temp dir under the user's cache (0700 on Unix; inherited ACLs elsewhere).
pub fn private_dir(cache_dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    let parent = cache_dir.join(CAPTURE_DIR);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder.create(&parent)?;
    let dir = parent.join(name);
    builder.recursive(false).create(&dir)?;
    Ok(dir)
}

/// Creates a new file (0600 on Unix; inherited ACLs elsewhere).
pub fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut f = options.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

fn remove_dir(dir: &Path) {
    if let Err(e) = std::fs::remove_dir_all(dir)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("removing {}: {e}", dir.display());
    }
}

/// Stops an in-app capture device off the UI thread (a stop waits for the
/// device's last chunk), tracked in [`SetupRuns`] so quitting waits for it.
fn stop_device(
    session: Box<dyn CaptureSession>,
    admission: Arc<broscribe_core::maintenance::ModeLease>,
    cx: &mut App,
) {
    let task = cx.background_executor().spawn(async move {
        let _admission = admission;
        if let Err(e) = session.stop() {
            tracing::warn!("stopping setup capture: {e}");
        }
    });
    SetupRuns::track(cx, task.shared());
}

/// A capture folder / helper session name: `<prefix>-<owner>-<generation>-<nanos>`,
/// `owner` naming this process (see [`remove_orphan_capture_dirs`]).
fn unique_name(prefix: &str, generation: u64) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{}-{generation}-{nanos:x}", CAPTURE_OWNER.as_str())
}

/// Linear peak and RMS of one meter, as [`Recorder`] keeps them.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MeterLevel {
    pub peak: f32,
    pub rms: f32,
}

impl MeterLevel {
    fn rms_dbfs(self) -> f32 {
        20.0 * self.rms.max(1e-9).log10()
    }

    fn readout(self) -> (usize, bool) {
        kit::meter_readout(self.rms_dbfs())
    }

    fn clipping(self) -> bool {
        self.peak >= 0.99
    }

    fn from_dbfs(level: &broscribe_ipc::SetupLevel) -> Self {
        let linear = |dbfs: f32| {
            if dbfs.is_finite() {
                10f32.powf(dbfs.clamp(-120.0, 0.0) / 20.0)
            } else {
                0.0
            }
        };
        Self {
            peak: linear(level.peak_dbfs),
            rms: linear(level.rms_dbfs),
        }
    }
}

/// Live meters of a capture or audio check running in the helper, fed by its
/// `setup_levels` events (design §4.1).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HelperMeters {
    /// The microphone; `None` until the first event.
    pub me: Option<MeterLevel>,
    /// The audio check's test sound as played to the output.
    pub others: Option<MeterLevel>,
}

impl HelperMeters {
    /// Takes `event` when it carries the levels of `session` (`None`: the
    /// audio check). Returns whether the meters changed, i.e. whether to redraw.
    pub fn apply(&mut self, session: Option<&str>, event: &broscribe_ipc::Event) -> bool {
        let broscribe_ipc::Event::SetupLevels {
            session: from,
            me,
            others,
        } = event
        else {
            return false;
        };
        if from.as_deref() != session {
            return false;
        }
        let next = Self {
            me: Some(MeterLevel::from_dbfs(me)),
            others: others.as_ref().map(MeterLevel::from_dbfs),
        };
        let changed = *self != next;
        *self = next;
        changed
    }
}

/// What the reading aims for; [`CapturePurpose::limit`] is only its maximum.
const READING_TARGET_SECONDS: u64 = 30;

/// The meter's time line: the first reading names its 30 s target apart
/// from the 45 s maximum, the confirmation reading its elapsed-time target
/// (detected speech is only measured after the capture stops), other
/// captures show elapsed / limit.
fn meter_time_text(
    purpose: CapturePurpose,
    seconds: u64,
    limit: u64,
    unit: impl Fn(u64) -> String,
) -> String {
    let elapsed = unit(seconds);
    match purpose {
        CapturePurpose::Passage => format!("{elapsed} of about {}", unit(READING_TARGET_SECONDS)),
        CapturePurpose::Confirmation => {
            format!("{elapsed} of about {}", unit(CONFIRM_TARGET_CAPTURE_S))
        }
        _ => format!("{elapsed} of {}", unit(limit)),
    }
}

fn clipping_warning(cx: &App) -> Stateful<Div> {
    kit::status_line(
        "setup-too-loud",
        kit::StatusKind::Attention,
        "Too loud at times: move a little away from the microphone.",
        cx,
    )
}

// ---------------------------------------------------------------------------
// Voice enrollment state
// ---------------------------------------------------------------------------

struct VoiceJob {
    generation: u64,
    session: String,
    dir: PathBuf,
    /// Enrollment voiceprint IDs before the child started.
    before: Option<BTreeSet<String>>,
    /// Cancel was asked: preparation writes no token from then on and the
    /// reply is ignored.
    cancelled: Arc<AtomicBool>,
    /// The CLI run's own stop flag, set only after the token is cleared
    /// (setting it kills and reaps that run's process tree).
    stop: Arc<AtomicBool>,
    /// The run is over (`None` until it starts).
    ended: Option<RunEnd>,
    /// Recording 1 itself, held only in RAM while a submission with the
    /// phrase runs: given back only on a typed `confirmation_unconfirmed`
    /// report of this job, dropped on every other end.
    reading: Option<Clip>,
}

/// The private files of a submission, written before its child starts.
struct PreparedVoice {
    dir: PathBuf,
    passage: PathBuf,
    confirm: Option<PathBuf>,
    before: BTreeSet<String>,
    /// The submitted recording 1, moved through preparation (not copied).
    reading: Clip,
    /// The input device recording 1 was made on (`--device`).
    device: Option<String>,
}

/// A `PhraseCheck` as `setup voice` reports it; a field missing from the
/// reply stays missing rather than reading as zero.
#[derive(Clone, Debug, PartialEq)]
pub struct PhraseComparison {
    pub score: Option<f64>,
    pub threshold: Option<f64>,
    /// `calibrated` or `relative`.
    pub method: Option<String>,
}

impl PhraseComparison {
    fn parse(v: &Value) -> Self {
        Self {
            score: v["score"].as_f64(),
            threshold: v["threshold"].as_f64(),
            method: v["method"].as_str().map(str::to_owned),
        }
    }

    fn details(&self) -> Option<(String, String)> {
        let (voice, needed) = (self.score?, self.threshold?);
        Some((
            format!("voice {voice:.2} · needed {needed:.2}"),
            format!("voice similarity {voice:.2}, needed {needed:.2}"),
        ))
    }
}

/// Why the optional confirmation reading did not confirm recording 1.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfirmationIssue {
    /// The comparison ran and its similarity stayed below the threshold.
    BelowThreshold(PhraseComparison),
    /// Less detected speech than the minimum: nothing was compared and
    /// there is no score. Both numbers are the reply's own seconds.
    TooShort { speech_s: f64, min_speech_s: f64 },
    /// Enough detected speech, but the model gave no voiceprint for it:
    /// nothing was compared and there is no score.
    Unusable { speech_s: f64, min_speech_s: f64 },
}

/// A `confirmation_unconfirmed` report: nothing was saved, and recording 1
/// remains usable.
#[derive(Clone, Debug, PartialEq)]
pub struct Unconfirmed {
    pub issue: ConfirmationIssue,
    /// Recording 1's warnings (top-level `checks.warnings`), shown again as
    /// its ready feedback.
    pub reading_warnings: Vec<String>,
    /// The phrase's warnings, without the reading-oriented `short_speech`.
    pub phrase_warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum VoiceOutcome {
    Enrolled {
        kept: bool,
        warnings: Vec<String>,
        /// `None`: the phrase check was skipped, not passed.
        confirmation: Option<PhraseComparison>,
    },
    /// The phrase check was inconclusive; nothing was saved and recording 1
    /// is held again for an explicit choice.
    Unconfirmed(Unconfirmed),
    /// The session was superseded or cancelled before the child committed.
    Cancelled,
    /// Cancelled after the child had already committed.
    CompletedBeforeCancel,
    Failed(String),
}

fn warning_text(w: &str) -> &'static str {
    match w {
        "clipping" => "The recording clipped: move a little away from the microphone.",
        "low_level" => "The level was low: speak closer to the microphone.",
        "short_speech" => "Less than 10 s of speech was found.",
        "noisy" => "Possible background noise or missed speech (pause-based estimate).",
        _ => "The recording quality check reported a problem.",
    }
}

const MALFORMED_UNCONFIRMED: &str =
    "The confirmation reading's result was incomplete. Choose Start reading to record again.";

fn voice_outcome(v: &Value) -> VoiceOutcome {
    // The typed report comes first: it is `committed: false` too.
    if v["reason"].as_str() == Some("confirmation_unconfirmed") {
        return unconfirmed(v).map_or_else(
            || VoiceOutcome::Failed(MALFORMED_UNCONFIRMED.into()),
            VoiceOutcome::Unconfirmed,
        );
    }
    if v["committed"].as_bool() != Some(true) {
        return VoiceOutcome::Cancelled;
    }
    VoiceOutcome::Enrolled {
        kept: v["enrollment_file"].is_string(),
        warnings: check_warnings(v),
        confirmation: v
            .get("confirmation")
            .filter(|c| c.is_object())
            .map(PhraseComparison::parse),
    }
}

/// A well-formed `confirmation_unconfirmed` report, or `None`. Every report
/// carries the confirmation's detected speech and the minimum it needs; a
/// missing, invalid or contradictory value offers no recovery.
fn unconfirmed(v: &Value) -> Option<Unconfirmed> {
    if v["committed"].as_bool() != Some(false)
        || !v["checks"]["warnings"].is_array()
        || !v["confirmation_checks"]["warnings"].is_array()
    {
        return None;
    }
    let speech_s = v["confirmation_checks"]["speech_s"]
        .as_f64()
        .filter(|s| s.is_finite() && *s >= 0.0)?;
    // The minimum the instruction names; another value is a mismatched CLI.
    let min_speech_s = v["confirmation_min_speech_s"]
        .as_f64()
        .filter(|m| *m == CONFIRM_MIN_SPEECH_S)?;
    let enough = speech_s >= min_speech_s;
    let confirmation = &v["confirmation"];
    let issue = match v["confirmation_issue"].as_str()? {
        "below_threshold"
            if enough
                && confirmation.is_object()
                && confirmation["passed"].as_bool() == Some(false) =>
        {
            ConfirmationIssue::BelowThreshold(PhraseComparison::parse(confirmation))
        }
        "too_short" if !enough && confirmation.is_null() => ConfirmationIssue::TooShort {
            speech_s,
            min_speech_s,
        },
        "unusable" if enough && confirmation.is_null() => ConfirmationIssue::Unusable {
            speech_s,
            min_speech_s,
        },
        _ => return None,
    };
    let mut phrase_warnings = warnings_of(&v["confirmation_checks"]);
    phrase_warnings.retain(|w| w != "short_speech");
    Some(Unconfirmed {
        issue,
        reading_warnings: check_warnings(v),
        phrase_warnings,
    })
}

/// The quality warnings (`checks.warnings`) of a `setup voice` reply.
fn check_warnings(v: &Value) -> Vec<String> {
    warnings_of(&v["checks"])
}

/// The `warnings` of one recording's `AudioChecks`.
fn warnings_of(checks: &Value) -> Vec<String> {
    checks["warnings"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Shown with a successful enrollment that ran without the phrase.
const PHRASE_SKIPPED: &str =
    "The optional confirmation reading was skipped. Only the first reading was used.";

fn enrolled_confirmation_line(confirmation: Option<&PhraseComparison>) -> &'static str {
    if confirmation.is_some() {
        "Your confirmation reading matched your first reading."
    } else {
        PHRASE_SKIPPED
    }
}

/// Detected speech in seconds, rounded down to 0.1 s so speech just under
/// the minimum never reads as reaching it.
fn speech_seconds(s: f64) -> String {
    format!("{:.1}", (s * 10.0).floor() / 10.0)
}

/// Lines shown for an inconclusive confirmation check: `(is_error, text)`.
/// The numbers are the reply's own; without a comparison there is no score.
fn unconfirmed_lines(u: &Unconfirmed) -> Vec<(bool, String)> {
    let mut lines = vec![(
        false,
        match &u.issue {
            ConfirmationIssue::BelowThreshold(_) =>
                "The confirmation reading didn't match your first reading closely enough. Nothing was saved. Your first reading is still here.".into(),
            ConfirmationIssue::TooShort {
                speech_s,
                min_speech_s,
            } => format!(
                "The confirmation reading was inconclusive. It contained {} s of detected speech out of \
                 the {min_speech_s} s needed (silence doesn't count), so nothing was compared \
                 and there is no score. Nothing was saved. Your first reading is still here.",
                speech_seconds(*speech_s)
            ),
            ConfirmationIssue::Unusable {
                speech_s,
                min_speech_s,
            } => format!(
                "The confirmation reading was inconclusive. It contained {} s of detected speech (at \
                 least {min_speech_s} s needed), but the voice model could not use it, so \
                 nothing was compared and there is no score. Nothing was saved. Your first \
                 reading is still here.",
                speech_seconds(*speech_s)
            ),
        },
    )];
    if matches!(
        u.issue,
        ConfirmationIssue::TooShort { .. } | ConfirmationIssue::Unusable { .. }
    ) {
        lines.push((
            false,
            format!(
                "To retry, keep the same microphone and position and read the text naturally \
                 for about {CONFIRM_TARGET_CAPTURE_S} s, reading it again if you finish early."
            ),
        ));
    }
    lines.extend(
        u.phrase_warnings
            .iter()
            .map(|w| (true, format!("Confirmation reading: {}", warning_text(w)))),
    );
    lines
}

const FIRST_RECORDING_LABEL: &str = "Recording 1 of 2: first reading (about 30 s)";
const FIRST_RECORDING_INSTRUCTION: &str =
    "Read the text aloud at your normal pace. If you finish before 30 s, start again from the top.";
const PHRASE_LABEL: &str = "Recording 2 of 2 (optional): confirmation reading";

/// What the confirmation reading asks for. Elapsed time is only guidance:
/// detected speech is measured after the capture stops.
fn phrase_instruction() -> String {
    format!(
        "Keep the same microphone and position. Read this text naturally for about \
         {CONFIRM_TARGET_CAPTURE_S} s; if you finish early, read it again. The check needs at \
         least {CONFIRM_MIN_SPEECH_S} s of detected speech (silence doesn't count), measured \
         after you choose \"Done reading\"."
    )
}

/// The first recording did not have enough clear speech for the enrollment.
const READING_NOT_READY: &str = "The first recording needs more clear voice. Choose \"Start \
     reading\" and read the text again for about 30 seconds, repeating it from the start if you \
     finish early.";
/// A quiet first recording is fixed by recording it again, not by changing
/// the microphone for the confirmation reading alone.
const QUIET_HINT: &str = "If you spoke quietly, record the first reading again closer to the \
     microphone; don't change the microphone only for the confirmation reading.";

/// A running `setup voice --check-recording` of the first recording. The
/// clip itself travels with the check task, not here.
struct ReadingCheckJob {
    generation: u64,
    /// `<cache>/setup-capture/<session>`, removed after the child is gone.
    dir: PathBuf,
    /// The run's own stop flag (kills and reaps only this check's child).
    cancelled: Arc<AtomicBool>,
    /// The check's run is over.
    ended: RunEnd,
    /// The device recording 1 was made on; held with it once ready.
    device: Option<String>,
}

/// What `setup voice --check-recording` says about the first recording.
/// `ready` means enough speech for the enrollment's parts, not a confirmed
/// identity; the part counts stay internal.
#[derive(Clone, Debug, PartialEq)]
struct ReadingCheck {
    ready: bool,
    warnings: Vec<String>,
}

fn reading_check(v: &Value) -> ReadingCheck {
    ReadingCheck {
        ready: v["ready"].as_bool() == Some(true),
        warnings: check_warnings(v),
    }
}

/// The last first-recording check, shown against recording 1.
#[derive(Clone, Debug, PartialEq)]
enum ReadingFeedback {
    /// Recording 2 may follow; the warnings are not fatal.
    Ready { warnings: Vec<String> },
    /// Not enough clear speech: recording 1 must be done again.
    NotReady { warnings: Vec<String> },
    /// The check itself failed.
    Failed(String),
}

/// "Couldn't transcribe the test. {Cause}." (WRT-08): Try it's
/// failure.
pub fn try_failure(cause: &str) -> String {
    failure_text("Couldn't transcribe the test.", cause, "")
}

/// Lines shown under recording 1: `(is_error, text)`.
fn reading_feedback_lines(feedback: &ReadingFeedback) -> Vec<(bool, String)> {
    let (mut lines, warnings) = match feedback {
        ReadingFeedback::Ready { warnings } => (Vec::new(), warnings),
        ReadingFeedback::NotReady { warnings } => {
            (vec![(true, READING_NOT_READY.to_owned())], warnings)
        }
        ReadingFeedback::Failed(e) => {
            return vec![(
                true,
                failure_text(
                    "Couldn't check the first recording.",
                    &sentence(e),
                    "Choose \"Start reading\" to record it again.",
                ),
            )];
        }
    };
    lines.extend(warnings.iter().map(|w| (true, warning_text(w).to_owned())));
    if warnings.iter().any(|w| w == "low_level") {
        lines.push((false, QUIET_HINT.to_owned()));
    }
    lines
}

/// `setup voice --check-recording` for the first recording's private copy;
/// `session` only places the CLI's working folder under `setup-capture`.
fn reading_check_args(copy: &Path, session: &str) -> Vec<OsString> {
    vec![
        "setup".into(),
        "voice".into(),
        "--check-recording".into(),
        "--from-file".into(),
        copy.into(),
        "--session".into(),
        session.into(),
    ]
}

/// Checks a 0600 copy of `clip` in `<cache>/setup-capture/<session>` and
/// removes that folder once the child is gone. Returns the original clip.
fn run_reading_check(
    cli: &CliRunner,
    cache_dir: &Path,
    session: &str,
    clip: Clip,
    cancelled: Arc<AtomicBool>,
) -> (Clip, std::result::Result<Value, String>) {
    if cancelled.load(Ordering::SeqCst) {
        return (clip, Err("It was cancelled.".into()));
    }
    let result = private_dir(cache_dir, session)
        .map_err(|e| io_user_text(&e))
        .and_then(|dir| {
            let copy = dir.join("passage.wav");
            let result = write_private_file(&copy, &wav_bytes(&clip.samples, clip.sample_rate))
                .map_err(|e| io_user_text(&e))
                .and_then(|()| {
                    cli.run_blocking_cancellable(
                        &reading_check_args(&copy, session),
                        ChildKind::Setup,
                        cancelled,
                    )
                    .map_err(|e| e.user_text())
                });
            remove_dir(&dir);
            result
        });
    (clip, result)
}

/// Background part of a check cancel (its flag already set): waits until
/// this check's own child is reaped, then removes its session folder.
async fn stop_reading_check(ended: RunEnd, dir: PathBuf) {
    let _ = ended.await;
    remove_dir(&dir);
}

fn enrollment_ids(library: &Library) -> broscribe_core::Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    for person in library.list_people()?.into_iter().filter(|p| p.is_self) {
        ids.extend(
            library
                .load_voiceprints(&person.id)?
                .embeddings
                .into_iter()
                .filter(|v| v.source.kind == VoiceprintKind::Enrollment)
                .map(|v| v.id),
        );
    }
    Ok(ids)
}

/// Invalidates a child under the same lock as its commit, and detects a
/// completed enrollment independently of audio retention. The token is
/// cleared with `Config::update` inside the library lock (library, then
/// config: the order the child's commit uses).
fn invalidate_session(
    library: &Library,
    data_dir: &Path,
    session: &str,
    before: Option<&BTreeSet<String>>,
) -> broscribe_core::Result<bool> {
    library.write(|_| {
        Config::update(data_dir, |config| {
            if config.setup.voice_session.as_deref() == Some(session) {
                config.setup.voice_session = None;
            }
            Ok(())
        })?;
        match before {
            Some(before) => Ok(enrollment_ids(library)?
                .iter()
                .any(|id| !before.contains(id))),
            None => Ok(false),
        }
    })
}

/// The Voice step's Name and Email fields: the trimmed name and the emails
/// (separated by commas, semicolons or spaces). An invalid email is an
/// error, and so is a typed name the library would refuse
/// ([`validate_name`], checked here only to say so before anything runs;
/// the CLI checks it again). An empty name leaves Me's name as it is.
#[derive(Clone, Debug, PartialEq, Eq)]
enum IdentityError {
    Name(String),
    Email(String),
}

fn me_fields(name: &str, emails: &str) -> Result<(String, Vec<String>), IdentityError> {
    let emails: Vec<String> = emails
        .split([',', ';', ' ', '\n'])
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_owned)
        .collect();
    if let Some(bad) = emails.iter().find(|e| {
        !super::import_sheet::is_email(e) || e.chars().any(broscribe_core::names::is_name_control)
    }) {
        return Err(IdentityError::Email(format!(
            "Check this email address: {bad}."
        )));
    }
    let name = name.trim();
    if !name.is_empty() {
        validate_name(name).map_err(|e| {
            IdentityError::Name(failure_text(
                "This name can't be used.",
                &core_user_text(&e),
                "",
            ))
        })?;
    }
    Ok((name.to_owned(), emails))
}

/// `setup voice` arguments for prepared recordings. The typed name and
/// emails go on stdin ([`identity_spec`], `--spec-stdin`), never in argv,
/// so the child creates or updates Me under its lock. With the phrase, an
/// inconclusive comparison comes back as a typed report
/// (`--report-unconfirmed`) that saves nothing, so recording 1 can be kept.
pub fn voice_args(
    passage: &Path,
    confirm: Option<&Path>,
    session: &str,
    device: Option<String>,
) -> Vec<OsString> {
    let mut a: Vec<OsString> = args(["setup", "voice", "--from-file"]);
    a.push(passage.into());
    if let Some(c) = confirm {
        a.push("--confirm-file".into());
        a.push(c.into());
        a.push("--report-unconfirmed".into());
    }
    a.push("--session".into());
    a.push(session.into());
    a.push("--keep-as-enrollment".into());
    if let Some(device) = device {
        a.push("--device".into());
        a.push(device.into());
    }
    a.push("--spec-stdin".into());
    a
}

/// The `--spec-stdin` identity of `setup` and `setup voice`: the name
/// (omitted when empty, so a saved one stays) and the email addresses.
pub fn identity_spec(name: &str, emails: &[String]) -> Vec<u8> {
    let mut spec = serde_json::json!({ "emails": emails });
    if !name.is_empty() {
        spec["name"] = name.into();
    }
    spec.to_string().into_bytes()
}

/// `setup --non-interactive --spec-stdin`, seeding people from the contacts file
/// of [`people_file_spec`]: its path never goes in argv.
pub fn people_file_args() -> Vec<OsString> {
    args(["setup", "--non-interactive", "--spec-stdin"])
}

/// The `setup --spec-stdin` input naming only the chosen contacts file, so Me
/// stays as it is. `Err` for a path JSON cannot carry (not UTF-8).
pub fn people_file_spec(path: &Path) -> Result<Vec<u8>, String> {
    let path = path.to_str().ok_or_else(|| unsendable_path(path, "used"))?;
    Ok(serde_json::json!({ "people_file": path })
        .to_string()
        .into_bytes())
}

/// `import --spec-stdin` for the manifest of [`manifest_import_spec`].
pub fn manifest_import_args() -> Vec<OsString> {
    args(["import", "--spec-stdin"])
}

/// The `import --spec-stdin` input naming the chosen manifest where it is.
/// `Err` for a path JSON cannot carry (not UTF-8).
pub fn manifest_import_spec(path: &Path) -> Result<Vec<u8>, String> {
    let path = path.to_str().ok_or_else(|| unsendable_path(path, "used"))?;
    Ok(serde_json::json!({ "manifest": path })
        .to_string()
        .into_bytes())
}

/// Resolves once one setup run is over: its CLI child (if it started) is
/// reaped and its own files are handled. Only that run's cancellation
/// waits on it, never another run's.
type RunEnd = Shared<oneshot::Receiver<()>>;

/// A [`RunEnd`] and the sender its run drops when it is over.
fn run_end() -> (oneshot::Sender<()>, RunEnd) {
    let (over, end) = oneshot::channel();
    (over, end.shared())
}

/// Detection policies a meeting app can have (design §4.1 step 8).
pub const DETECT_POLICIES: [&str; 3] = ["auto", "ask", "off"];

/// A supported meeting app (from `detect status`) and its detection policy.
#[derive(Clone, Debug, PartialEq)]
pub struct MeetingApp {
    pub id: String,
    pub name: String,
    /// `auto`, `ask` or `off`; `None` when the answer did not say.
    pub policy: Option<String>,
    pub installed: Option<bool>,
}

pub fn parse_meeting_apps(v: &serde_json::Value) -> Vec<MeetingApp> {
    v["apps"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|r| {
                    let id = r["app_id"].as_str()?.to_owned();
                    Some(MeetingApp {
                        name: r["name"].as_str().unwrap_or(&id).to_owned(),
                        policy: r["policy"]
                            .as_str()
                            .filter(|p| DETECT_POLICIES.contains(p))
                            .map(str::to_owned),
                        installed: r["installed"].as_bool(),
                        id,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether `detect status` says the recording agent's detection is off.
pub fn detection_off(v: &serde_json::Value) -> bool {
    v["enabled"].as_bool() == Some(false)
}

/// The line shown above the Meeting apps rule list, if any: detection being
/// off takes precedence over an empty list.
pub fn meeting_apps_notice(apps: &[MeetingApp], detection_off: bool) -> Option<&'static str> {
    if detection_off {
        Some(
            "Meeting detection is off, so calls aren't noticed yet. These choices apply once it's on.",
        )
    } else if apps.is_empty() {
        Some("No supported meeting app is configured.")
    } else {
        None
    }
}

/// `detect policy <app> <policy>` (the CLI saves it and reloads the agent).
pub fn detect_policy_args(app: &str, policy: &str) -> Vec<OsString> {
    args(["detect", "policy", app, policy])
}

// ---------------------------------------------------------------------------
// The view
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HelperRoute {
    Native,
    Running,
    Stopped,
}

fn helper_route(data_dir: &Path) -> std::result::Result<HelperRoute, String> {
    let socket = Endpoint::new(data_dir);
    let method = Call::Ping.method();
    // Setup may ask right after it started the agent: allow its first answer.
    if let Some(mut client) = Client::connect_daemon(&socket, ANSWER_TIMEOUT)
        .map_err(|e| crate::daemon::connect_error_text(method, &e))?
    {
        crate::daemon::require_operational_agent(&mut client)
            .map_err(|e| crate::daemon::agent_failure_text(method, &e))?;
        return Ok(HelperRoute::Running);
    }
    let exe = std::env::current_exe().map_err(|e| io_user_text(&e))?;
    NativePlatform::autostart(exe)
        .is_enabled()
        .map(|installed| {
            if installed {
                HelperRoute::Stopped
            } else {
                HelperRoute::Native
            }
        })
        .map_err(|e| {
            tracing::warn!(code = e.code(), "checking whether the agent is installed");
            core_user_text(&e)
        })
}

pub(crate) fn helper_call(
    data_dir: &Path,
    call: Call,
    timeout: Duration,
) -> std::result::Result<Value, String> {
    let socket = Endpoint::new(data_dir);
    let method = call.method();
    if call.is_mutating() {
        Client::call_mutating(&socket, call, timeout)
            .map_err(|e| crate::daemon::call_error_text(method, e))
    } else {
        Client::connect_daemon(&socket, timeout)
            .map_err(|e| crate::daemon::connect_error_text(method, &e))?
            .ok_or_else(|| "BroScribe Agent isn't running. Start it to continue.".to_owned())?
            .call(call)
            .map_err(|e| crate::daemon::call_error_text(method, e))
    }
}

impl PcmSink for Clip {
    fn format(&mut self, sample_rate: u32) -> broscribe_core::Result<()> {
        self.sample_rate = sample_rate;
        Ok(())
    }
    fn samples(&mut self, mono: &[f32]) -> broscribe_core::Result<()> {
        self.clipped += mono.iter().filter(|s| s.abs() >= 0.99).count();
        self.samples.extend_from_slice(mono);
        Ok(())
    }
}

/// A capture in the helper: the clip and the input device the helper
/// opened (`device_id`: the pinned microphone, `None` for the system
/// default input).
fn capture_helper_sample(
    purpose: CapturePurpose,
    session: String,
    cache_dir: &Path,
    request: impl FnOnce(Call, Duration) -> std::result::Result<Value, String>,
) -> std::result::Result<(Clip, Option<String>), String> {
    request(
        Call::CaptureSample {
            session,
            seconds: purpose.limit().as_secs_f64(),
            purpose: match purpose {
                CapturePurpose::MicCheck => "audio_check",
                CapturePurpose::TryIt => "try_it",
                CapturePurpose::Passage | CapturePurpose::Confirmation => "enrollment",
            }
            .into(),
        },
        purpose.limit() + Duration::from_secs(10),
    )
    .and_then(|value| {
        let device = value["device_id"].as_str().map(str::to_owned);
        helper_clip(value, cache_dir).map(|clip| (clip, device))
    })
}

fn helper_clip(value: Value, cache_dir: &Path) -> std::result::Result<Clip, String> {
    let path = PathBuf::from(
        value["path"]
            .as_str()
            .ok_or("BroScribe Agent didn't return the recording. Try again.")?,
    );
    let dir = path
        .parent()
        .ok_or("BroScribe Agent didn't return a recording BroScribe could open. Try again.")?;
    if path.file_name().is_none_or(|name| name != "sample.wav")
        || dir.parent() != Some(cache_dir.join("helper-capture").as_path())
        || dir
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| !name.starts_with(broscribe_platform::paths::HELPER_SAMPLE_PREFIX))
    {
        return Err(
            "BroScribe Agent didn't return a recording BroScribe could open. Try again.".into(),
        );
    }
    let mut clip = Clip {
        samples: Vec::new(),
        sample_rate: 0,
        clipped: 0,
    };
    let result = NativePlatform::decoder_chain()
        .decode(&path, &mut clip)
        .map_err(|e| {
            tracing::warn!(code = e.code(), "decoding the agent's sample");
            core_user_text(&e)
        });
    if let Some(dir) = path.parent() {
        remove_dir(dir);
    }
    result.map(|_| clip)
}

/// The helper waits up to 120 s for the user to answer a permission prompt
/// (`MacDeviceInfo::request_permission`); the app waits longer, so a slow
/// answer is not reported as an error.
const PERMISSION_WAIT: Duration = Duration::from_secs(130);
/// A permission status read prompts nobody and answers at once.
const PERMISSION_STATUS_WAIT: Duration = Duration::from_secs(5);

/// The helper call for a permission refresh: a request (`Some`) may wait on
/// the user's answer, a status read must not.
pub(crate) fn permission_call(permission: Option<&str>) -> (Call, Duration) {
    match permission {
        Some(permission) => (
            Call::PermissionsRequest {
                permission: permission.into(),
            },
            PERMISSION_WAIT,
        ),
        None => (Call::PermissionsStatus, PERMISSION_STATUS_WAIT),
    }
}

/// After "Allow Accessibility" the grant arrives later, in System Settings:
/// step 2 re-reads it this often while it is shown, for at most
/// [`ACCESSIBILITY_POLL_LIMIT`] (and whenever the window becomes active).
const ACCESSIBILITY_POLL_INTERVAL: Duration = Duration::from_secs(1);
const ACCESSIBILITY_POLL_LIMIT: Duration = Duration::from_secs(180);

/// Step 2's "Install BroScribe Agent": the CLI registers it as a login
/// item, then starts it (design §4.1 step 2).
fn helper_install_commands() -> [Vec<OsString>; 2] {
    [args(["daemon", "install"]), args(["daemon", "start"])]
}

/// launchd starts the agent asynchronously: Install waits this long, at most,
/// for it to answer on its socket.
const HELPER_START_WAIT: Duration = Duration::from_secs(5);
const HELPER_START_POLL: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, PartialEq)]
enum HelperInstall {
    Idle,
    Running,
    Failed(String),
}

/// Length of the audio check's test sound (as `broscribe audio-check`).
const AUDIO_CHECK_SECONDS: f64 = 4.0;

/// Echo handling names (`broscribe_dsp::EchoHandling`, snake case) and labels.
const ECHO_HANDLINGS: [(&str, &str, &str); 3] = [
    ("none", "Leave as is", "Leaves your track as recorded."),
    (
        "residue_gating",
        "Mute leftover echo",
        "Quiets your track where it only picks up the other side.",
    ),
    (
        "full_aec",
        "Cancel echo fully",
        "Removes the other side's sound from your track as you speak.",
    ),
];

/// Step 5: what the helper measured, and on which devices.
#[derive(Clone, Debug, PartialEq)]
struct AudioCheckResult {
    delay_ms: Option<f64>,
    leakage_db: Option<f64>,
    recommendation: String,
    bluetooth_call_profile: bool,
    /// The setup a remote meeting on these devices is recorded with.
    setup: Option<String>,
    input: broscribe_platform::DeviceDetails,
    output: broscribe_platform::DeviceDetails,
}

impl AudioCheckResult {
    /// The helper measured the echo path; otherwise the recommendation is a
    /// default, not a finding about these devices.
    fn measured(&self) -> bool {
        self.delay_ms.is_some() && self.leakage_db.is_some()
    }
}

/// Runs the helper's `audio_check` (it plays the synthetic test sound and
/// listens) and ties the report to the default device pair, which must not
/// change while the check runs.
fn run_audio_check(
    devices: &dyn broscribe_platform::DeviceInfo,
    request: impl FnOnce(Call, Duration) -> std::result::Result<Value, String>,
) -> std::result::Result<AudioCheckResult, String> {
    let pair = || -> std::result::Result<_, String> {
        let input = devices
            .default_input()
            .map_err(|e| core_user_text(&e))?
            .ok_or_else(|| "No default microphone.".to_owned())?;
        let output = devices
            .default_output()
            .map_err(|e| core_user_text(&e))?
            .ok_or_else(|| "No default output device.".to_owned())?;
        Ok((input, output))
    };
    let (input, output) = pair()?;
    let value = request(
        Call::AudioCheck {
            seconds: AUDIO_CHECK_SECONDS,
        },
        Duration::from_secs_f64(AUDIO_CHECK_SECONDS) + Duration::from_secs(20),
    )?;
    let (after_input, after_output) = pair()?;
    if after_input.id != input.id
        || after_output.id != output.id
        || after_output.output_data_source != output.output_data_source
    {
        return Err("The microphone or output changed during the check. Run it again.".into());
    }
    let recommendation = value["recommendation"]
        .as_str()
        .filter(|r| ECHO_HANDLINGS.iter().any(|(id, _, _)| id == r))
        .ok_or_else(|| {
            "BroScribe Agent didn't suggest echo settings. Try the audio check again.".to_owned()
        })?
        .to_owned();
    Ok(AudioCheckResult {
        delay_ms: value["delay_ms"].as_f64(),
        leakage_db: value["leakage_db"].as_f64(),
        recommendation,
        bluetooth_call_profile: value["bluetooth_call_profile"].as_bool().unwrap_or(false)
            || after_output.is_bluetooth_call_profile(),
        setup: value["setup"].as_str().map(str::to_owned),
        input: after_input,
        output: after_output,
    })
}

/// Labels of the setups the helper detects (`broscribe_dsp::Setup`, snake case).
const SETUP_LABELS: [(&str, &str); 6] = [
    ("headphones", "Headphones"),
    ("speakerphone_aec", "Speakerphone with echo cancellation"),
    ("speakers_mic", "Speakers and microphone"),
    ("bluetooth_call", "Bluetooth headset in call mode"),
    ("hybrid_room", "Hybrid room"),
    ("in_person", "In person"),
];

fn audio_check_measurements(result: &AudioCheckResult) -> String {
    let delay = result.delay_ms.map_or_else(
        || "Echo delay not measurable".into(),
        |ms| format!("Echo arrives after {ms:.0} ms"),
    );
    let leakage = result.leakage_db.map_or_else(
        || "echo level not measurable".into(),
        |db| format!("echo level {db:.0} dB").replace('-', "−"),
    );
    format!("{delay} · {leakage}")
}

fn setup_label(setup: &str) -> &str {
    SETUP_LABELS
        .iter()
        .find(|(id, _)| *id == setup)
        .map_or(setup, |(_, label)| label)
}

/// The config entry the record pipeline reads for this device pair.
fn audio_check_choice(
    result: &AudioCheckResult,
    handling: &str,
) -> broscribe_core::config::AudioCheckChoice {
    broscribe_core::config::AudioCheckChoice {
        input_uid: result.input.id.clone(),
        output_uid: result.output.id.clone(),
        output_data_source: result.output.output_data_source.clone(),
        echo_handling: handling.to_owned(),
    }
}

/// What Continue saves for the checked pair: only a real measurement
/// replaces the pair's choice.
fn audio_check_to_save(
    result: &AudioCheckResult,
    echo_choice: Option<&str>,
) -> Option<broscribe_core::config::AudioCheckChoice> {
    result
        .measured()
        .then(|| audio_check_choice(result, echo_choice.unwrap_or(&result.recommendation)))
}

fn progress_failure(cause: &str) -> String {
    failure_text(
        "Couldn't save setup progress.",
        &sentence(cause),
        "The assistant stays on this step.",
    )
}

#[derive(Clone, Debug, PartialEq)]
enum Outcome {
    Ok(String),
    Err(String),
}

#[derive(Clone, Debug, Default)]
struct MachineInfo {
    memory: u64,
    free_disk: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
struct Passage {
    lang: String,
    text: String,
    confirmation: String,
}

const VOICE_LANGUAGES: [(&str, &str); 3] = [
    ("en", "English"),
    ("pt-BR", "Português (Brasil)"),
    ("pt-PT", "Português (Portugal)"),
];

/// Where the Your voice step is; the step's prompt and actions follow it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum VoiceStage {
    /// Recording 1 or 2 is being captured.
    Recording(CapturePurpose),
    /// The enrollment child runs.
    Processing,
    /// Recording 1 is being checked.
    Checking,
    /// Recording 1 passed its check: recording 2 (optional) comes next.
    Phrase,
    /// Recording 1 starts (again) from here.
    Start,
}

impl VoiceStage {
    /// Whether the confirmation reading (recording 2) is shown instead of the
    /// first reading.
    fn shows_phrase(self) -> bool {
        matches!(
            self,
            VoiceStage::Phrase | VoiceStage::Recording(CapturePurpose::Confirmation)
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
enum TryState<T> {
    Idle,
    Running,
    Cancelled,
    Done(Result<T, String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TryMe {
    Named,
    Suggested,
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct TryResult {
    text: String,
    me: TryMe,
    meeting: Option<MeetingId>,
    saved_before_cancel: bool,
}

impl TryResult {
    fn from_reply(reply: Value) -> Result<Self, String> {
        if reply["committed"].as_bool() == Some(false) {
            return Err("The setup session ended before saving.".into());
        }
        let text = reply["transcript"]
            .as_str()
            .ok_or("The transcription result was incomplete.")?
            .to_owned();
        let me = match reply["me"].as_str() {
            Some("named") => TryMe::Named,
            Some("suggested") => TryMe::Suggested,
            // Older CLI replies still have the boolean.
            None if reply["recognised_me"] == true => TryMe::Named,
            _ => TryMe::None,
        };
        Ok(Self {
            text,
            me,
            meeting: reply["meeting_id"].as_str().and_then(|id| id.parse().ok()),
            saved_before_cancel: false,
        })
    }
}

/// Computed only for a Try it recording the user asked to keep.
struct RecordingFingerprint {
    sha256: String,
    bytes: u64,
}

const TEST_CANCELLED: &str = "Cancelled. Nothing was saved.";

pub struct SetupAssistant {
    state: Entity<AppState>,
    deps: SetupDeps,
    system_settings: Arc<dyn SystemSettingsOpener>,
    step: Step,
    heading_focus: FocusHandle,
    nav_focus: FocusHandle,
    nav_cursor: usize,
    all_steps_focus: FocusHandle,
    body_scroll: gpui::ScrollHandle,
    steps_scroll: gpui::ScrollHandle,
    continue_focus: FocusHandle,
    back_focus: FocusHandle,
    close_focus: FocusHandle,
    keep_focus: FocusHandle,
    discard: Option<DiscardConfirmation>,
    narrow: bool,
    /// Bumped by navigation and by every Continue/Finish: a progress write
    /// moves on or closes only while it is still the newest, on its step.
    advance_generation: u64,
    /// A Continue/Finish progress write is in flight.
    advancing: bool,
    /// Shown on screen; a hidden assistant captures and polls nothing.
    visible: bool,

    // 1 Languages
    language: Entity<InputState>,
    progress_error: Option<String>,
    language_error: Option<String>,
    capture_error: Option<String>,
    settings_error: Option<ElementId>,
    agent_start_error: Option<String>,
    legacy_outcome: Option<Outcome>,

    // capture (2, 4, 13)
    capture_generation: u64,
    capture: Option<ActiveCapture>,
    devices: Option<std::result::Result<Vec<AudioDevice>, String>>,
    device: Option<String>,
    /// The device of the capture just handed to [`Self::captured`].
    captured_device: Option<String>,
    mic_check: Option<Outcome>,
    // 5 Audio check
    audio_check: Option<std::result::Result<AudioCheckResult, String>>,
    /// Live meters of the running audio check.
    audio_check_meters: HelperMeters,
    audio_check_clipped: bool,
    audio_checking: bool,
    measurements_open: bool,
    echo_choice: Option<String>,
    helper: Option<std::result::Result<HelperRoute, String>>,
    permissions: Option<std::result::Result<Value, String>>,
    /// Only the newest permission read is shown.
    permissions_generation: u64,
    permission_request_pending: bool,
    notifier: Arc<dyn broscribe_platform::Notifier>,
    notification_authorization: Option<NotificationAuthorization>,
    notifications_generation: u64,
    accessibility_answered: bool,
    agent_location_advisory: bool,
    recording_devices: Option<Result<Vec<super::settings::InputDevice>, String>>,
    devices_generation: u64,
    accessibility_trusted: bool,
    accessibility: Arc<dyn broscribe_platform::AppUiReader>,
    /// Re-reads the Accessibility grant after a request (dropping it stops).
    accessibility_poll: Option<Task<()>>,
    helper_install: HelperInstall,

    // 3 Models
    machine: Option<MachineInfo>,
    models: Option<std::result::Result<ModelsList, String>>,
    models_expanded: bool,
    profile_notice: Option<Outcome>,
    models_generation: u64,
    sample: TryState<String>,
    sample_cancelled: Option<Arc<AtomicBool>>,
    /// The example-audio test's run is over.
    sample_ended: Option<RunEnd>,

    // 4 Your voice
    name: Entity<InputState>,
    emails: Entity<InputState>,
    saved_identity: (String, String),
    identity_error: Option<IdentityError>,
    identity_save_error: Option<String>,
    voice_saved: Option<bool>,
    voice_details: bool,
    passage: Option<std::result::Result<Passage, String>>,
    /// An explicit reading-language choice is local to this assistant.
    passage_language: Option<String>,
    passage_generation: u64,
    /// Recording 1, held only once its check said `ready`.
    passage_clip: Option<Clip>,
    /// The input device recording 1 was made on, recorded with the voice.
    passage_device: Option<String>,
    /// The running check of recording 1 (its clip travels with the task).
    reading_check: Option<ReadingCheckJob>,
    reading_feedback: Option<ReadingFeedback>,
    voice_job: Option<VoiceJob>,
    voice_outcome: Option<VoiceOutcome>,
    /// Whether a Me person exists, loaded off GPUI's loop with the step
    /// (`None` until known).
    me_exists: Option<bool>,
    /// The phrase recorded while the Name/Email fields were not valid; kept
    /// with [`Self::passage_clip`] until they are fixed and submitted.
    confirmation_clip: Option<Clip>,

    // 6 Summaries: the shared provider view, LLM providers only. Leaving the
    // step or closing drops its API-key draft and consent question.
    providers: Entity<ProvidersView>,

    // 7 Words and people
    words: Entity<TextareaState>,
    /// Distinct terms in the field, kept by its change events.
    words_count: usize,

    // 8 Meeting apps
    meeting_apps: Option<std::result::Result<Vec<MeetingApp>, String>>,
    /// `detect status` answered `enabled: false`.
    detection_off: bool,
    policy_errors: BTreeMap<String, String>,

    // 9 Storage
    retention_days: Entity<InputState>,
    retention: RetentionGuard,
    calendars: Option<Entity<crate::views::integrations::IntegrationsView>>,

    // 11 Agents: the shared view Settings shows too.
    agents: Entity<AgentsView>,

    // 13 Try it
    try_state: TryState<TryResult>,
    save_try_as_meeting: bool,
    try_job: Option<VoiceJob>,
    try_recording: Option<Arc<OnceLock<RecordingFingerprint>>>,
    pending_cancel: Option<futures::future::Shared<Task<()>>>,

    busy: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SetupEvent> for SetupAssistant {}

impl SetupAssistant {
    pub fn new(
        state: Entity<AppState>,
        deps: SetupDeps,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let config = state.read(cx).config().clone();
        let language = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("en")
                .default_value(config.default_language.clone().unwrap_or_default())
        });
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Your name")
                .default_value(config.setup.name.clone().unwrap_or_default())
        });
        let emails = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("ana@example.com")
                .default_value(config.setup.emails.join(", "))
        });
        let words = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(6)
                .placeholder("Product names, jargon, people's names")
        });
        let days = match config.retention {
            Retention::DeleteAfterDays { days } => days,
            _ => 30,
        };
        let retention_days = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Days")
                .default_value(days.to_string())
        });
        let downloads = state.read(cx).model_downloads().clone();
        let mut subscriptions = vec![
            cx.observe(&downloads, |this: &mut Self, downloads, cx| {
                if !downloads.read(cx).running() && this.step != Step::Welcome {
                    this.load_models(cx);
                }
                cx.notify();
            }),
            cx.subscribe_in(&retention_days, window, |this, _, event, window, cx| {
                if this.step == Step::Storage
                    && matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur)
                {
                    this.save_retention_days(window, cx);
                }
            }),
            cx.observe(&state, |_, _, cx| cx.notify()),
            // Quitting: invalidate an enrollment token under the lock, then
            // kill the setup children (plan item 22). `AppState::on_quit`
            // leaves the setup children alone while an assistant is alive.
            // Helper-capture cancellations and device stops this release
            // starts are awaited too (bounded); recordings are not touched.
            cx.on_app_quit(|this, cx| {
                this.release(cx);
                let pending = this.pending_cancel.take();
                let runs = SetupRuns::quit(cx);
                let children = this.state.read(cx).cli().children().clone();
                let executor = cx.background_executor().clone();
                let admission = this.state.read(cx).cli().admission();
                async move {
                    let _admission = admission;
                    finish_setup_quit(
                        async move {
                            let pending = async move {
                                if let Some(pending) = pending {
                                    pending.await;
                                }
                            };
                            futures::join!(pending, runs);
                        },
                        executor.timer(QUIT_CANCEL_TIMEOUT),
                        move || children.cancel_setup_children(),
                    )
                    .await;
                }
            }),
            cx.on_release(|this, cx| {
                this.release(cx);
                this.state.update(cx, |s, _| s.setup_assistant_closed());
            }),
            // Back from System Settings: the Accessibility grant may have changed.
            cx.observe_window_activation(window, |this, window, cx| {
                if this.visible && window.is_window_active() {
                    if this.step == Step::MeetingApps {
                        this.load_meeting_apps(cx);
                    } else {
                        this.refresh_recording_access(cx);
                    }
                }
            }),
            cx.subscribe_in(&words, window, |this, words, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    let count = terms(&words.read(cx).value()).len();
                    if count != this.words_count {
                        this.words_count = count;
                        cx.notify();
                    }
                }
            }),
        ];
        for (input, name_field) in [(&name, true), (&emails, false)] {
            subscriptions.push(
                cx.subscribe_in(input, window, move |this, _, event, _, cx| {
                    if matches!(event, InputEvent::Change) {
                        if matches!(
                            (&this.identity_error, name_field),
                            (Some(IdentityError::Name(_)), true)
                                | (Some(IdentityError::Email(_)), false)
                        ) {
                            this.identity_error = None;
                        }
                        this.identity_save_error = None;
                        cx.notify();
                    }
                }),
            );
        }
        if let Some(agent) = state.read(cx).agent_starter() {
            subscriptions.push(cx.observe(&agent, |this: &mut Self, _, cx| {
                if this.step == Step::Permissions {
                    this.load_devices(cx);
                    if !this.permission_request_pending {
                        this.refresh_permissions(None, cx);
                    }
                    cx.notify();
                }
            }));
        }
        // Live levels of captures running in the helper (design §4.1).
        if let Some(daemon) = state.read(cx).daemon().cloned() {
            subscriptions.push(cx.subscribe(&daemon, |this, _, event: &DaemonEvent, cx| {
                this.helper_event(event, cx)
            }));
        }
        state.update(cx, |s, _| s.setup_assistant_opened());
        let cache = state.read(cx).paths().cache_dir.clone();
        state
            .read(cx)
            .cli()
            .spawn(cx.background_executor(), async move {
                remove_orphan_capture_dirs(&cache)
            })
            .detach();
        let accessibility: Arc<dyn broscribe_platform::AppUiReader> =
            Arc::from(NativePlatform::app_ui_reader());
        let providers =
            cx.new(|_| ProvidersView::new(state.clone(), ProviderScope::Llm).in_setup());
        let agents = cx.new(|_| AgentsView::new(state.clone()).in_setup());
        let mut retention = RetentionGuard::new(cx);
        retention.bind_days_input(&retention_days, cx);
        let step = resume_step(&config);
        let paths = state.read(cx).paths();
        let notifier = Arc::from(NativePlatform::notifier(
            paths
                .data_dir_overridden
                .then_some(paths.data_dir.as_path()),
        ));
        let heading_focus = cx.focus_handle();
        heading_focus.focus(window, cx);
        let mut this = Self {
            step,
            heading_focus,
            nav_focus: cx.focus_handle(),
            nav_cursor: step.position().unwrap_or(0),
            all_steps_focus: cx.focus_handle(),
            body_scroll: gpui::ScrollHandle::new(),
            steps_scroll: gpui::ScrollHandle::new(),
            continue_focus: cx.focus_handle(),
            back_focus: cx.focus_handle(),
            close_focus: cx.focus_handle(),
            keep_focus: cx.focus_handle(),
            discard: None,
            narrow: Self::is_narrow(window),
            advance_generation: 0,
            advancing: false,
            visible: true,
            state,
            deps,
            system_settings: Arc::new(NativePlatform::system_settings()),
            language,
            progress_error: None,
            language_error: None,
            capture_error: None,
            settings_error: None,
            agent_start_error: None,
            legacy_outcome: None,
            capture_generation: 0,
            capture: None,
            devices: None,
            device: None,
            captured_device: None,
            mic_check: None,
            audio_check: None,
            audio_check_meters: HelperMeters::default(),
            audio_check_clipped: false,
            audio_checking: false,
            measurements_open: false,
            echo_choice: None,
            helper: None,
            permissions: None,
            permissions_generation: 0,
            permission_request_pending: false,
            notifier,
            notification_authorization: None,
            notifications_generation: 0,
            accessibility_answered: false,
            agent_location_advisory: false,
            recording_devices: None,
            devices_generation: 0,
            accessibility_trusted: accessibility.is_trusted(),
            accessibility,
            accessibility_poll: None,
            helper_install: HelperInstall::Idle,
            machine: None,
            models: None,
            models_expanded: false,
            profile_notice: None,
            models_generation: 0,
            sample: TryState::Idle,
            sample_cancelled: None,
            sample_ended: None,
            name,
            emails,
            saved_identity: (
                config.setup.name.unwrap_or_default(),
                config.setup.emails.join(", "),
            ),
            identity_error: None,
            identity_save_error: None,
            voice_saved: None,
            voice_details: false,
            passage: None,
            passage_language: None,
            passage_generation: 0,
            passage_clip: None,
            passage_device: None,
            reading_check: None,
            reading_feedback: None,
            voice_job: None,
            voice_outcome: None,
            me_exists: None,
            confirmation_clip: None,
            providers,
            words,
            words_count: 0,
            meeting_apps: None,
            detection_off: false,
            policy_errors: BTreeMap::new(),
            retention_days,
            retention,
            calendars: None,
            agents,
            try_state: TryState::Idle,
            save_try_as_meeting: false,
            try_job: None,
            try_recording: None,
            pending_cancel: None,
            busy: false,
            _subscriptions: subscriptions,
        };
        this.enter_step(cx);
        this
    }

    #[cfg(test)]
    pub(crate) fn step(&self) -> Step {
        self.step
    }

    // ---- navigation ----

    pub(crate) fn is_capturing(&self) -> bool {
        self.capture.is_some()
    }

    fn go_to(&mut self, step: Step, cx: &mut Context<Self>) {
        if step == self.step || (self.busy && self.step == Step::AudioCheck) {
            return;
        }
        // A Continue still saving belongs to the step being left.
        self.advance_generation += 1;
        self.advancing = false;
        self.retention.invalidate();
        if self.step == Step::Summaries {
            self.providers
                .update(cx, |providers, cx| providers.reset_drafts(cx));
        }
        if self.step == Step::Calendars {
            self.deactivate_calendars(cx);
        }
        self.accessibility_poll = None;
        self.cancel_capture(cx);
        if self.voice_job.is_some() {
            self.cancel_voice(cx);
        }
        self.discard_try(cx);
        self.cancel_sample(cx);
        self.discard_recordings(cx);
        if let Some(pending) = self.pending_cancel.clone()
            && pending.peek().is_none()
        {
            // The step does not leave until token invalidation and child
            // reaping finish. A commit that already owns the lock finishes
            // while its step is still visible, never after navigation.
            let generation = self.capture_generation;
            cx.spawn(async move |this, cx| {
                pending.await;
                this.update(cx, |this, cx| {
                    if this.capture_generation != generation {
                        return;
                    }
                    this.step = step;
                    this.nav_cursor = step.position().unwrap_or(this.nav_cursor);
                    this.enter_step(cx);
                    cx.notify();
                })
                .ok();
            })
            .detach();
            cx.notify();
            return;
        }
        self.step = step;
        self.nav_cursor = step.position().unwrap_or(self.nav_cursor);
        self.enter_step(cx);
        cx.notify();
    }

    fn enter_step(&mut self, cx: &mut Context<Self>) {
        self.progress_error = None;
        self.capture_error = None;
        self.settings_error = None;
        self.legacy_outcome = None;
        self.body_scroll
            .set_offset(gpui::point(gpui::px(0.), gpui::px(0.)));
        match self.step {
            Step::Permissions => self.refresh_recording_access(cx),
            Step::AudioCheck => self.load_devices(cx),
            Step::TryIt => {
                self.load_devices(cx);
                self.load_models(cx);
                self.load_me_exists(cx);
            }
            Step::Ready | Step::Overview => {
                self.load_models(cx);
                self.load_devices(cx);
                self.load_me_exists(cx);
                self.load_meeting_apps(cx);
            }
            Step::Languages | Step::Models => {
                self.load_machine(cx);
                self.load_models(cx);
            }
            Step::Voice => {
                self.load_devices(cx);
                self.load_models(cx);
                self.load_me_exists(cx);
                if self.passage.is_none() {
                    self.load_passage(cx);
                }
            }
            Step::Summaries => self
                .providers
                .update(cx, |providers, cx| providers.reload(cx)),
            Step::Words => self.load_models(cx),
            Step::MeetingApps => self.load_meeting_apps(cx),
            Step::Agents => self.agents.update(cx, |agents, cx| agents.reload(cx)),
            _ => {}
        }
    }

    fn skip(&mut self, cx: &mut Context<Self>) {
        self.go_to(self.step.next(), cx);
    }

    /// Marks the current step done and moves on (Languages saves first).
    /// Moving on waits for the progress write; a failed write stays on the
    /// step and says so.
    fn continue_step(&mut self, cx: &mut Context<Self>) {
        if self.advancing {
            return;
        }
        match self.step {
            Step::Welcome => self.skip(cx),
            Step::Ready | Step::Overview => self.finish(cx),
            Step::Languages | Step::Models => self.save_language(true, cx),
            Step::Permissions => self.continue_recording_access(cx),
            Step::AudioCheck => self.save_audio_check(cx),
            Step::TryIt if !matches!(&self.try_state, TryState::Done(Ok(result)) if !result.text.trim().is_empty()) => {
                self.skip(cx)
            }
            Step::Voice if self.identity_dirty(cx) => self.save_me(cx),
            Step::Voice
                if !matches!(
                    self.voice_outcome,
                    Some(VoiceOutcome::Enrolled { .. } | VoiceOutcome::CompletedBeforeCancel)
                ) =>
            {
                // Not enrolled: moving on is a skip.
                self.skip(cx);
            }
            step => {
                let task = mark_step(&self.state, step.id(), cx);
                self.advance_after(task, false, cx);
            }
        }
    }

    /// Marks the assistant finished in one write, then closes.
    fn finish(&mut self, cx: &mut Context<Self>) {
        if self.advancing {
            return;
        }
        let task = mark_step(&self.state, ASSISTANT_DONE, cx);
        self.advance_after(task, true, cx);
    }

    /// Moves on (or closes, with `close`) once `progress` is saved, only if
    /// no navigation or newer Continue happened meanwhile; a failure stays
    /// on the step with its error.
    fn advance_after(
        &mut self,
        progress: Task<std::result::Result<(), String>>,
        close: bool,
        cx: &mut Context<Self>,
    ) {
        self.advance_generation += 1;
        let generation = self.advance_generation;
        let step = self.step;
        self.advancing = true;
        self.progress_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = progress.await;
            this.update(cx, |this, cx| {
                if this.advance_generation != generation || this.step != step {
                    if result.is_err() {
                        tracing::warn!("saving setup progress after leaving its step");
                    }
                    return;
                }
                this.advancing = false;
                match result {
                    Ok(()) if close => {
                        this.close(cx);
                        this.state
                            .update(cx, |_, cx| cx.emit(crate::state::AppEvent::SetupFinished));
                    }
                    Ok(()) => {
                        if step == Step::Models
                            && this.wants_download(cx)
                            && !this.download_profile(cx)
                        {
                            cx.notify();
                            return;
                        }
                        this.skip(cx);
                    }
                    Err(e) => {
                        this.progress_error = Some(progress_failure(&e));
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Closes without marking anything; the assistant resumes next time.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.release(cx);
        if let Some(pending) = self.pending_cancel.clone()
            && pending.peek().is_none()
        {
            cx.spawn(async move |this, cx| {
                pending.await;
                this.update(cx, |_, cx| cx.emit(SetupEvent::Closed)).ok();
            })
            .detach();
        } else {
            cx.emit(SetupEvent::Closed);
        }
    }

    pub(crate) fn clear_deferral(&mut self, cx: &mut Context<Self>) {
        if !self.state.read(cx).config().setup.defer_assistant {
            return;
        }
        self.advancing = true;
        let task = update_config(
            &self.state,
            |config| config.setup.defer_assistant = false,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.advancing = false;
                if let Err(text) = result {
                    this.progress_error = Some(progress_failure(&text));
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn set_up_later(&mut self, cx: &mut Context<Self>) {
        if self.advancing {
            return;
        }
        if !should_show_first_run(self.state.read(cx).config()) {
            self.close(cx);
            return;
        }
        self.advancing = true;
        cx.notify();
        let task = update_config(
            &self.state,
            |config| config.setup.defer_assistant = true,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.advancing = false;
                match result {
                    Ok(()) => this.close(cx),
                    Err(text) => {
                        this.progress_error = Some(progress_failure(&text));
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Stops capture, drops the held recordings (and an inconclusive phrase
    /// report) at once, and cancels a running enrollment.
    fn release(&mut self, cx: &mut App) {
        self.capture_generation += 1;
        self.advance_generation += 1;
        self.advancing = false;
        self.retention.invalidate();
        self.accessibility_poll = None;
        self.discard_capture(cx);
        self.discard_try(cx);
        self.cancel_sample(cx);
        // At once, not when `Closed` is delivered: that may wait for a
        // pending cancellation.
        self.discard_recordings(cx);
        self.providers
            .update(cx, |providers, cx| providers.reset_drafts(cx));
        self.deactivate_calendars(cx);
        if let Some(task) = self.cancel_voice_job_task(cx) {
            self.pending_cancel = Some(task.clone());
            cx.spawn(async move |_| task.await).detach();
        } else if let Some(pending) = self.pending_cancel.clone() {
            cx.spawn(async move |_| pending.await).detach();
        }
    }

    // ---- 1 Languages ----

    fn save_language(&mut self, advance: bool, cx: &mut Context<Self>) {
        let text = self.language.read(cx).value().trim().to_owned();
        if !text.is_empty() && !valid_language_tag(&text) {
            self.language_error = Some(format!(
                "\u{201c}{text}\u{201d} is not a language tag. Use a tag like pt-BR."
            ));
            cx.notify();
            return;
        }
        self.language_error = None;
        let value = (!text.is_empty()).then_some(text);
        let ready = self.step == Step::Models
            && self
                .models
                .as_ref()
                .and_then(|list| required_models(list.as_ref().ok()?).ok())
                .is_some_and(|rows| rows.iter().all(|row| row.installed()));
        let saved = update_config(
            &self.state,
            move |c| {
                c.default_language = value;
                mark_setup_completed(c, Step::Languages.id());
                if ready {
                    mark_setup_completed(c, Step::Models.id());
                }
            },
            cx,
        );
        let task = cx.spawn(async move |this, cx| {
            let result = saved.await;
            if result.is_ok() {
                this.update(cx, |this, cx| {
                    // A manual reading-language choice is independent of meetings.
                    if this.passage_language.is_none() {
                        this.invalidate_passage();
                        if this.step == Step::Voice {
                            this.load_passage(cx);
                        }
                    }
                    cx.notify();
                })
                .ok();
            }
            result
        });
        if advance {
            // Continue moves on only from Languages and only if no other
            // navigation or Continue came after it.
            self.advance_after(task, false, cx);
        } else {
            self.report(task, None, cx);
        }
    }

    fn refresh_recording_access(&mut self, cx: &mut Context<Self>) {
        if self.step != Step::Permissions || !self.visible {
            return;
        }
        if let Some(agent) = self.state.read(cx).agent_starter() {
            agent.update(cx, |agent, cx| agent.refresh_service(cx));
        }
        self.load_devices(cx);
        self.recheck_accessibility(cx);
        if !self.permission_request_pending {
            self.refresh_permissions(None, cx);
        }
        self.read_notification_authorization(false, cx);
    }

    fn read_notification_authorization(&mut self, request: bool, cx: &mut Context<Self>) {
        if request {
            self.permission_request_pending = true;
            cx.notify();
        } else if self.permission_request_pending {
            return;
        }
        self.notifications_generation += 1;
        let generation = self.notifications_generation;
        let notifier = self.notifier.clone();
        let task = cx.background_executor().spawn(async move {
            if request {
                notifier.request_authorization()
            } else {
                notifier.authorization()
            }
        });
        cx.spawn(async move |this, cx| {
            let status = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.notifications_generation != generation {
                    return;
                }
                if request {
                    this.permission_request_pending = false;
                }
                if request || this.notification_authorization != Some(status) {
                    this.notification_authorization = Some(status);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn agent_status(&self, cx: &App) -> Option<AgentStartStatus> {
        if let Some(daemon) = self.state.read(cx).daemon()
            && let Some(agent) = self.state.read(cx).agent_starter()
        {
            let unavailable =
                AgentStartStatus::unavailable(daemon.read(cx), agent.read(cx).status());
            if unavailable.is_none() || !matches!(unavailable, Some(AgentStartStatus::Idle)) {
                return unavailable;
            }
        }
        match &self.helper {
            Some(Ok(HelperRoute::Running)) => None,
            Some(Ok(HelperRoute::Stopped)) => Some(AgentStartStatus::Stopped),
            Some(Ok(HelperRoute::Native)) => Some(AgentStartStatus::NotInstalled),
            Some(Err(error)) => Some(AgentStartStatus::Failed(error.clone())),
            None => Some(AgentStartStatus::Connecting),
        }
    }

    fn permission_state(&self, key: &str) -> Option<&str> {
        self.permissions.as_ref()?.as_ref().ok()?[key].as_str()
    }

    fn next_permission(&self, cx: &App) -> Option<RecordingPermission> {
        if self.agent_status(cx).is_some() {
            return None;
        }
        match (
            self.permission_state("microphone"),
            self.permission_state("system_audio"),
        ) {
            (Some("not_determined"), _) => return Some(RecordingPermission::Microphone),
            (_, Some("not_determined"))
                if audio_permission_answered(self.permission_state("microphone")) =>
            {
                return Some(RecordingPermission::SystemAudio);
            }
            (mic, system)
                if audio_permission_answered(mic) && audio_permission_answered(system) => {}
            _ => return None,
        }
        if self.notification_authorization == Some(NotificationAuthorization::NotDetermined) {
            return Some(RecordingPermission::Notifications);
        }
        if !self.accessibility_trusted && !self.accessibility_answered {
            return Some(RecordingPermission::MeetingNames);
        }
        None
    }

    fn recording_access_can_continue(&self, cx: &App) -> bool {
        !self.permission_request_pending
            && self.agent_status(cx).is_none()
            && (self.next_permission(cx).is_some() || self.recording_audio_answered())
    }

    fn recording_audio_answered(&self) -> bool {
        audio_permission_answered(self.permission_state("microphone"))
            && audio_permission_answered(self.permission_state("system_audio"))
    }

    fn continue_recording_access(&mut self, cx: &mut Context<Self>) {
        if !self.recording_access_can_continue(cx) {
            return;
        }
        match self.next_permission(cx) {
            Some(RecordingPermission::Microphone) => {
                self.refresh_permissions(Some("microphone"), cx)
            }
            Some(RecordingPermission::SystemAudio) => {
                self.refresh_permissions(Some("system_audio"), cx)
            }
            Some(RecordingPermission::Notifications) => {
                self.read_notification_authorization(true, cx)
            }
            Some(RecordingPermission::MeetingNames) => {
                self.accessibility_answered = true;
                self.request_accessibility(cx);
            }
            None => {
                let task = mark_step(&self.state, Step::Permissions.id(), cx);
                self.advance_after(task, false, cx);
            }
        }
    }

    // ---- capture ----
    /// Reads (or, with `permission`, requests) the helper's audio
    /// permissions; only the newest read is shown, an error included.
    fn refresh_permissions(&mut self, permission: Option<&'static str>, cx: &mut Context<Self>) {
        if permission.is_some() {
            self.permission_request_pending = true;
            cx.notify();
        }
        self.permissions_generation += 1;
        let generation = self.permissions_generation;
        let data_dir = self.state.read(cx).paths().data_dir.clone();
        let task = self
            .state
            .read(cx)
            .cli()
            .spawn(cx.background_executor(), async move {
                match helper_route(&data_dir)? {
                    HelperRoute::Running => {
                        let (call, timeout) = permission_call(permission);
                        helper_call(&data_dir, call, timeout)
                    }
                    HelperRoute::Stopped => Err(
                        "Start BroScribe Agent to check or request its audio permissions.".into(),
                    ),
                    HelperRoute::Native => {
                        Err("Install BroScribe Agent to check recording access.".into())
                    }
                }
            });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.permissions_generation != generation {
                    return;
                }
                if permission.is_some() {
                    this.permission_request_pending = false;
                }
                if permission.is_some() || this.permissions.as_ref() != Some(&result) {
                    this.permissions = Some(result);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    // ---- helper levels ----

    /// Feeds the helper's `setup_levels` events to the meters of the running
    /// helper capture or audio check; redraws only when what a meter shows
    /// changed (the helper sends at most 10 events a second).
    fn helper_event(&mut self, event: &DaemonEvent, cx: &mut Context<Self>) {
        let DaemonEvent::Received(event) = event else {
            return;
        };
        let changed = match self.capture.as_mut() {
            Some(capture) => {
                let applied = match capture.helper_session.as_deref() {
                    Some(session) => capture.helper_meters.apply(Some(session), event),
                    None => false,
                };
                applied && capture.redraw_needed()
            }
            None if self.audio_checking => {
                let before = self.audio_check_view();
                let clipped = self.audio_check_clipped;
                if self.audio_check_meters.apply(None, event) {
                    self.audio_check_clipped |=
                        self.audio_check_meters.me.is_some_and(MeterLevel::clipping);
                    self.audio_check_view() != before || self.audio_check_clipped != clipped
                } else {
                    false
                }
            }
            None => false,
        };
        if changed {
            cx.notify();
        }
    }

    /// What the audio check's Me and Others meters show.
    fn audio_check_view(&self) -> [Option<(usize, bool)>; 2] {
        [self.audio_check_meters.me, self.audio_check_meters.others]
            .map(|level| level.map(MeterLevel::readout))
    }

    // ---- 5 Audio check ----
    fn start_audio_check(&mut self, cx: &mut Context<Self>) {
        if self.audio_checking || self.busy {
            return;
        }
        if self.helper != Some(Ok(HelperRoute::Running)) {
            self.audio_check = Some(Err("Start BroScribe Agent to run the audio check.".into()));
            self.load_devices(cx);
            cx.notify();
            return;
        }
        // The test sound must not compete with app playback or a capture.
        self.cancel_capture(cx);
        self.state.update(cx, |s, _| {
            if let Some(player) = s.player_mut() {
                player.pause();
            }
        });
        self.audio_checking = true;
        self.audio_check = None;
        self.audio_check_meters = HelperMeters::default();
        self.audio_check_clipped = false;
        self.measurements_open = false;
        self.echo_choice = None;
        self.progress_error = None;
        let data_dir = self.state.read(cx).paths().data_dir.clone();
        let task = self
            .state
            .read(cx)
            .cli()
            .spawn(cx.background_executor(), async move {
                let devices = NativePlatform::device_info();
                run_audio_check(devices.as_ref(), |call, timeout| {
                    helper_call(&data_dir, call, timeout)
                })
            });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.audio_checking = false;
                this.echo_choice = result
                    .as_ref()
                    .ok()
                    .filter(|r| r.measured())
                    .map(|r| r.recommendation.clone());
                this.audio_check = Some(result);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Saves the chosen echo handling for the checked device pair, marks the
    /// step and moves on; without a measurement, moving on is a skip that
    /// leaves the pair's saved choice alone.
    fn save_audio_check(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.audio_checking {
            return;
        }
        let Some(Ok(result)) = &self.audio_check else {
            self.skip(cx);
            return;
        };
        let choice = audio_check_to_save(result, self.echo_choice.as_deref());
        self.busy = true;
        self.progress_error = None;
        let task = update_config(
            &self.state,
            move |config| {
                if let Some(choice) = choice {
                    config.set_audio_check(choice);
                }
                mark_setup_completed(config, Step::AudioCheck.id());
            },
            cx,
        );
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(()) if this.step == Step::AudioCheck => this.skip(cx),
                    Ok(()) => cx.notify(),
                    Err(e) => {
                        this.progress_error = Some(progress_failure(&e));
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Live Me and Others meters while the helper's check runs: what the
    /// microphone hears and the test sound it plays.
    fn render_audio_check_meters(&self, cx: &App) -> AnyElement {
        let meters = self.audio_check_meters;
        let Some(me) = meters.me else {
            return muted(
                "audio-check-waiting",
                "Waiting for the microphone level…",
                cx,
            )
            .into_any_element();
        };
        let others = meters.others.unwrap_or_default();
        kit::stack()
            .child(kit::level_meter(
                "audio-check-me",
                kit::Track::Me,
                me.rms_dbfs(),
                kit::MeterState::Recording,
                cx,
            ))
            .child(kit::level_meter(
                "audio-check-others",
                kit::Track::Others,
                others.rms_dbfs(),
                kit::MeterState::Recording,
                cx,
            ))
            .when(self.audio_check_clipped, |el| {
                el.child(clipping_warning(cx))
            })
            .into_any_element()
    }

    fn render_audio_check(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let checking = self.audio_checking;
        let helper_ready = self.helper == Some(Ok(HelperRoute::Running));
        let mut el = div().flex().flex_col().gap_2().child(text_line(
            "setup-intro",
            Role::Label,
            "Use the speakers or headphones you have in meetings. BroScribe plays a short \
                 synthetic test sound and measures how much of it your microphone picks up, so \
                 it can remove the other side's voices from your track. Nothing is kept.",
        ));
        el = el.child(match &self.helper {
            Some(Ok(HelperRoute::Native)) => muted(
                "audio-check-native",
                "The audio check needs BroScribe Agent.",
                cx,
            )
            .into_any_element(),
            _ => self.render_device_picker(cx),
        });
        el = el
            .when(!checking && !helper_ready, |el| {
                el.child(self.step_link(Step::Permissions, "Go to Recording access", cx))
            })
            .child(
                Button::new("setup-audio-check")
                    .label(if checking {
                        "Playing the test sound…"
                    } else {
                        "Run the audio check"
                    })
                    .disabled(checking || self.busy || !helper_ready)
                    .when(checking || self.busy || !helper_ready, |button| {
                        button.tooltip(if !helper_ready {
                            "The audio check needs BroScribe Agent. Install it in Permissions."
                        } else {
                            "The current setup action is still running."
                        })
                    })
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.start_audio_check(cx)),
                    ),
            );
        if checking {
            el = el
                .child(crate::ui::kit::status_line(
                    "audio-check-running",
                    crate::ui::kit::StatusKind::Pending,
                    "Playing the test sound…",
                    cx,
                ))
                .child(self.render_audio_check_meters(cx));
        }
        match &self.audio_check {
            None => {}
            Some(Err(e)) => el = el.child(error_line("audio-check-result", e.clone(), cx)),
            Some(Ok(result)) => {
                let output = match &result.output.output_data_source {
                    Some(source) => format!("{} ({source})", result.output.name),
                    None => result.output.name.clone(),
                };
                let headline = result.setup.as_ref().map_or_else(
                    || "Check complete".to_owned(),
                    |setup| format!("Check complete · Detected setup: {}", setup_label(setup)),
                );
                el = el.child(kit::status_line(
                    "audio-check-result",
                    kit::StatusKind::Done,
                    headline,
                    cx,
                ));
                if result.bluetooth_call_profile {
                    el = el.child(crate::ui::kit::status_line(
                        "audio-check-bluetooth",
                        crate::ui::kit::StatusKind::Attention,
                        "Bluetooth call mode is active: audio quality is reduced. Use the \
                         headset's media mode or another microphone if you can.",
                        cx,
                    ));
                }
                if !result.measured() {
                    el = el.child(crate::ui::kit::status_line(
                        "audio-check-unmeasured",
                        crate::ui::kit::StatusKind::Attention,
                        "Check completed, but the echo path was not measurable. Headphones or \
                         built-in echo cancellation can prevent measurement; an inaudible test \
                         sound or muted microphone can also do this. Continue completes this \
                         step without saving calibration or changing echo handling.",
                        cx,
                    ));
                } else {
                    el = el
                        .child(self.echo_choices(&result.recommendation, cx))
                        .child(muted(
                            "audio-check-continue-note",
                            format!(
                                "Continue saves this choice for {} with {output}.",
                                result.input.name
                            ),
                            cx,
                        ));
                }
                el = el
                    .child(kit::disclosure(
                        "audio-check-measurements",
                        "Measurements",
                        self.measurements_open,
                        cx,
                        |this, _, cx| {
                            this.measurements_open = !this.measurements_open;
                            cx.notify();
                        },
                    ))
                    .when(self.measurements_open, |el| {
                        el.child(muted(
                            "audio-check-devices",
                            format!("Microphone: {} · Output: {output}", result.input.name),
                            cx,
                        ))
                        .child(muted(
                            "audio-check-delay",
                            audio_check_measurements(result),
                            cx,
                        ))
                    });
            }
        }
        el.into_any_element()
    }

    /// The echo handlings after a measured audio check, as one radio group
    /// with `recommendation` marked.
    fn echo_choices(&self, recommendation: &str, cx: &mut Context<Self>) -> ChoiceGroup {
        let mut choices = ChoiceGroup::new(
            kit::stack().child(text_line(
                "setup-echo-label",
                Role::Label,
                "How to handle echo",
            )),
            "echo-handling",
            "How to handle echo",
        );
        for (id, label, description) in ECHO_HANDLINGS {
            let recommended = id == recommendation;
            let option = choice(
                SharedString::from(format!("echo-{id}")),
                label,
                self.echo_choice.as_deref() == Some(id),
                cx.listener(move |this, _: &bool, _, cx| {
                    this.echo_choice = Some(id.to_owned());
                    cx.notify();
                }),
            )
            .accessibility_label(if recommended {
                format!("{label}, recommended, {description}")
            } else {
                format!("{label}, {description}")
            })
            .details(muted(format!("echo-description-{id}"), description, cx));
            choices = choices.option(if recommended {
                option.trailing(kit::badge(kit::BadgeTone::Neutral, "Recommended", cx))
            } else {
                option
            });
        }
        choices
    }

    fn start_helper(&mut self, cx: &mut Context<Self>) {
        let task = self
            .state
            .read(cx)
            .cli()
            .spawn(cx.background_executor(), async {
                let exe = std::env::current_exe().map_err(|e| {
                    failure_text("Couldn't start BroScribe Agent.", &io_user_text(&e), "")
                })?;
                NativePlatform::autostart(exe).start().map_err(|e| {
                    tracing::warn!(code = e.code(), "starting the agent");
                    failure_text("Couldn't start BroScribe Agent.", &core_user_text(&e), "")
                })
            });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.helper = None;
                this.agent_start_error = result.err();
                this.load_devices(cx);
                this.refresh_permissions(None, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Asks the helper to stop `session` off the UI thread: keeping the
    /// sample for the capture's own reply, or discarding it. A discard is
    /// tracked in [`SetupRuns`] until the capture request itself has ended
    /// (`done`), so quitting waits for it; it never targets a recording.
    fn stop_helper_capture(
        &self,
        session: String,
        discard: bool,
        done: Option<Shared<oneshot::Receiver<()>>>,
        cx: &mut App,
    ) {
        let data_dir = self.state.read(cx).paths().data_dir.clone();
        let task = self
            .state
            .read(cx)
            .cli()
            .spawn(cx.background_executor(), async move {
                if let Err(e) = helper_call(
                    &data_dir,
                    Call::CaptureSampleStop { session, discard },
                    Duration::from_secs(5),
                ) {
                    tracing::warn!("stopping setup capture: {e}");
                }
                if let Some(done) = done {
                    let _ = done.await;
                }
            });
        if discard {
            SetupRuns::track(cx, task.shared());
        } else {
            task.detach();
        }
    }

    /// Ends the current capture and drops its audio: a helper capture is
    /// stopped with discard, an in-app device is stopped off the UI thread.
    fn discard_capture(&mut self, cx: &mut App) -> bool {
        let Some(capture) = self.capture.take() else {
            return false;
        };
        capture.shared.closed.store(true, Ordering::Release);
        capture.shared.ended.store(true, Ordering::SeqCst);
        if let Some(session) = capture.helper_session {
            self.stop_helper_capture(session, true, capture.helper_done, cx);
        }
        if let Some(session) = capture.session {
            stop_device(session, self.state.read(cx).cli().admission(), cx);
        }
        true
    }

    fn request_accessibility(&mut self, cx: &mut Context<Self>) {
        self.accessibility_trusted = self.accessibility.request_trust();
        cx.notify();
        if self.accessibility_trusted {
            self.accessibility_poll = None;
            return;
        }
        let ticks = (ACCESSIBILITY_POLL_LIMIT.as_millis() / ACCESSIBILITY_POLL_INTERVAL.as_millis())
            as usize;
        self.accessibility_poll = Some(cx.spawn(async move |this, cx| {
            for _ in 0..ticks {
                cx.background_executor()
                    .timer(ACCESSIBILITY_POLL_INTERVAL)
                    .await;
                let keep = this
                    .update(cx, |this, cx| this.recheck_accessibility(cx))
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        }));
    }

    /// Re-reads the Accessibility grant while step 2 is shown; redraws only
    /// when it changed. Returns whether a poll should keep going.
    fn recheck_accessibility(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.visible || self.step != Step::Permissions {
            return false;
        }
        let trusted = self.accessibility.is_trusted();
        if trusted != self.accessibility_trusted {
            self.accessibility_trusted = trusted;
            cx.notify();
        }
        !trusted
    }

    /// Installs the recording agent as a login item and starts it through
    /// the CLI (`daemon install`, `daemon start`), then refreshes the route.
    fn install_helper(&mut self, cx: &mut Context<Self>) {
        if self.helper_install == HelperInstall::Running
            || self
                .state
                .read(cx)
                .daemon()
                .is_some_and(|daemon| daemon.read(cx).panel.recording.is_some())
        {
            return;
        }
        self.helper_install = HelperInstall::Running;
        cx.notify();
        let cli = self.state.read(cx).cli().clone();
        let data_dir = self.state.read(cx).paths().data_dir.clone();
        let executor = cx.background_executor().clone();
        let task = cx.background_executor().spawn(async move {
            for command in helper_install_commands() {
                cli.run_blocking(&command, ChildKind::Command)
                    .map_err(|e| e.user_text())?;
            }
            let mut route = helper_route(&data_dir);
            let polls = HELPER_START_WAIT.as_millis() / HELPER_START_POLL.as_millis();
            for _ in 0..polls {
                if route != Ok(HelperRoute::Stopped) {
                    break;
                }
                executor.timer(HELPER_START_POLL).await;
                route = helper_route(&data_dir);
            }
            Ok::<_, String>(route)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(route) => {
                        this.helper_install = HelperInstall::Idle;
                        this.helper = Some(route);
                    }
                    Err(e) => {
                        this.helper_install = HelperInstall::Failed(e);
                        this.helper = None;
                    }
                }
                this.load_devices(cx);
                this.refresh_permissions(None, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_agent_access(&self, cx: &Context<Self>) -> Div {
        use kit::StatusKind::{Attention, Done, Failed, Pending};
        let status = self.agent_status(cx);
        let busy = self.helper_install == HelperInstall::Running;
        let (kind, text) = match &status {
            _ if busy => (Pending, "Installing and starting…".into()),
            None => (Done, "Running".into()),
            Some(status) => (
                if status.is_pending() {
                    Pending
                } else if status.is_failure() {
                    Failed
                } else {
                    Attention
                },
                match status {
                    AgentStartStatus::Stopped => "Stopped".into(),
                    AgentStartStatus::NotInstalled => "Not installed".into(),
                    _ => status.message().unwrap_or_default(),
                },
            ),
        };
        let recording = self
            .state
            .read(cx)
            .daemon()
            .is_some_and(|daemon| daemon.read(cx).panel.recording.is_some());
        let mut body = kit::stack()
            .items_start()
            .child(crate::ui::heading("setup-agent-heading", "BroScribe Agent"))
            .child(kit::status_line("setup-agent-status", kind, text, cx));
        if status.is_some() {
            body = body.child(text_line(
                "setup-agent-purpose",
                Role::Label,
                "Records meetings even while this window is closed, and starts when you log in.",
            ));
        }
        if let HelperInstall::Failed(error) = &self.helper_install {
            body = body.child(kit::status_line(
                "setup-helper-install-error",
                Failed,
                failure_text("Couldn't install BroScribe Agent.", &sentence(error), ""),
                cx,
            ));
        }
        if let Some(status) = status
            && !busy
        {
            if let Some(details) = status.copy_details()
                && matches!(status, AgentStartStatus::Outdated { .. })
            {
                body = body.child(muted("setup-agent-details", details, cx));
            }
            match status.recovery() {
                Some(AgentRecovery::Setup | AgentRecovery::Reinstall) => {
                    if self.agent_location_advisory {
                        body = body.child(kit::status_line("setup-agent-location", Attention,
                            "Move BroScribe to Applications before installing BroScribe Agent, or it may stop working after a restart.", cx));
                    }
                    body = body.child(
                        Button::new("setup-install-helper")
                            .outline()
                            .tab_stop(true)
                            .label(if matches!(status, AgentStartStatus::Outdated { .. }) {
                                "Reinstall"
                            } else {
                                "Install and start"
                            })
                            .disabled(recording)
                            .on_click(cx.listener(|this, _, _, cx| this.install_helper(cx))),
                    );
                }
                Some(AgentRecovery::LoginItems) => {
                    body = body.children(self.system_settings_button(
                        "setup-open-login-items",
                        SettingsPane::LoginItems,
                        cx,
                    ));
                }
                Some(AgentRecovery::Start | AgentRecovery::Retry) => {
                    body = body.child(
                        Button::new("setup-start-agent")
                            .outline()
                            .tab_stop(true)
                            .label(
                                if matches!(
                                    status,
                                    AgentStartStatus::Stopped | AgentStartStatus::Idle
                                ) {
                                    "Start"
                                } else {
                                    "Try again"
                                },
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(agent) = this.state.read(cx).agent_starter() {
                                    agent.update(cx, |agent, cx| {
                                        agent.start(cx);
                                    });
                                } else {
                                    this.start_helper(cx);
                                }
                            })),
                    );
                }
                None => {}
            }
        }
        body.children(
            self.agent_start_error
                .as_ref()
                .map(|e| error_line("setup-agent-start-error", e.clone(), cx)),
        )
    }

    fn load_devices(&mut self, cx: &mut Context<Self>) {
        let mic = self.deps.mic.clone();
        let data_dir = self.state.read(cx).paths().data_dir.clone();
        self.devices_generation += 1;
        let generation = self.devices_generation;
        let recording_access = self.step == Step::Permissions;
        let known = (!recording_access).then(|| self.helper.clone()).flatten();
        let device_list = recording_access.then(|| {
            self.state
                .read(cx)
                .run_cli(args(["audio", "devices"]), ChildKind::Command, cx)
        });
        let task = self
            .state
            .read(cx)
            .cli()
            .spawn(cx.background_executor(), async move {
                let route = known.unwrap_or_else(|| helper_route(&data_dir));
                let devices = match &route {
                    Ok(HelperRoute::Native) => mic.devices().map_err(|e| core_user_text(&e)),
                    Ok(_) => Ok(Vec::new()),
                    Err(e) => Err(e.clone()),
                };
                let advisory = recording_access
                    && std::env::current_exe().ok().is_some_and(|executable| {
                        NativePlatform::autostart(executable).agent_install_location_advisory()
                    });
                (route, devices, advisory)
            });
        cx.spawn(async move |this, cx| {
            let (route, result, advisory) = task.await;
            let recording_devices = match device_list {
                Some(task) => Some(
                    task.await
                        .map(|reply| super::settings::parse_input_devices(&reply))
                        .map_err(|error| error.user_text()),
                ),
                None => None,
            };
            this.update(cx, |this, cx| {
                if this.devices_generation != generation {
                    return;
                }
                let changed = this.helper.as_ref() != Some(&route)
                    || this.devices.as_ref() != Some(&result)
                    || this.agent_location_advisory != advisory
                    || (recording_devices.is_some() && this.recording_devices != recording_devices);
                this.helper = Some(route);
                this.agent_location_advisory = advisory;
                if recording_devices.is_some() {
                    this.recording_devices = recording_devices;
                }
                if this.device.is_none()
                    && let Ok(devices) = &result
                {
                    this.device = devices.iter().find(|d| d.is_default).map(|d| d.id.clone());
                }
                this.devices = Some(result);
                if changed {
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Tests elsewhere in the crate: starts an in-process capture.
    #[cfg(test)]
    pub(crate) fn start_native_capture_for_test(&mut self, cx: &mut Context<Self>) {
        self.helper = Some(Ok(HelperRoute::Native));
        self.start_capture(CapturePurpose::TryIt, cx);
    }

    pub fn start_capture(&mut self, purpose: CapturePurpose, cx: &mut Context<Self>) {
        if matches!(self.step, Step::Voice | Step::TryIt) && !self.models_ready(purpose, cx) {
            return;
        }
        if !matches!(
            self.helper,
            Some(Ok(HelperRoute::Native | HelperRoute::Running))
        ) {
            self.capture_failed_for(
                purpose,
                match &self.helper {
                    Some(Ok(HelperRoute::Stopped)) => {
                        "Start BroScribe Agent before recording.".into()
                    }
                    Some(Err(e)) => failure_text("Couldn't start recording.", &sentence(e), ""),
                    _ => "Checking BroScribe Agent. Try again in a moment.".into(),
                },
            );
            self.load_devices(cx);
            cx.notify();
            return;
        }
        // Supersede any previous capture and pause app playback.
        self.cancel_capture(cx);
        self.state.update(cx, |s, _| {
            if let Some(player) = s.player_mut() {
                player.pause();
            }
        });
        self.capture_generation += 1;
        let generation = self.capture_generation;
        let helper_session = (self.helper == Some(Ok(HelperRoute::Running)))
            .then(|| unique_name("sample", generation));
        // The helper records on the recorder's microphone and names it in
        // its reply; an in-app capture records on the chosen device.
        let device = helper_session
            .is_none()
            .then(|| self.device.clone())
            .flatten();
        let capture =
            ActiveCapture::new(generation, purpose, device.clone(), helper_session.clone());
        let shared = capture.shared.clone();
        self.capture = Some(capture);
        self.capture_error = None;
        cx.notify();
        if let Some(session) = helper_session {
            let data_dir = self.state.read(cx).paths().data_dir.clone();
            let cache_dir = self.state.read(cx).paths().cache_dir.clone();
            let (done, ended) = oneshot::channel::<()>();
            let task = self
                .state
                .read(cx)
                .cli()
                .spawn(cx.background_executor(), async move {
                    let result =
                        capture_helper_sample(purpose, session, &cache_dir, |call, timeout| {
                            helper_call(&data_dir, call, timeout)
                        });
                    // The request has ended: a pending discard may finish now.
                    drop(done);
                    result
                });
            let ticker = self.meter_ticker(generation, cx);
            if let Some(capture) = self.capture.as_mut() {
                capture.running = true;
                capture.helper_done = Some(ended.shared());
                capture._ticker = Some(ticker);
            }
            cx.spawn(async move |this, cx| {
                let result = task.await;
                this.update(cx, |this, cx| {
                    if !this
                        .capture
                        .as_ref()
                        .is_some_and(|c| c.generation == generation)
                    {
                        return;
                    }
                    this.capture = None;
                    this.capture_generation += 1;
                    match result {
                        Ok((clip, device)) => {
                            this.captured_device = device;
                            this.captured(purpose, clip, cx);
                        }
                        Err(e) => this.capture_failed_for(
                            purpose,
                            failure_text("Couldn't record.", &sentence(&e), ""),
                        ),
                    }
                    cx.notify();
                })
                .ok();
            })
            .detach();
            return;
        }
        let mic = self.deps.mic.clone();
        let stop_admission = self.state.read(cx).cli().admission();
        let admission = stop_admission.clone();
        let task = self
            .state
            .read(cx)
            .cli()
            .spawn(cx.background_executor(), async move {
                let guard = SinkGuard {
                    shared,
                    _admission: admission,
                };
                mic.start(
                    device.as_deref(),
                    Box::new(move |chunk: MicChunk| guard.push(&chunk)),
                )
            });
        cx.spawn(async move |this, cx| {
            // The opened device passes to the assistant only when it takes
            // the result; once the assistant is gone, it is stopped off the
            // UI thread like any other (dropping it here would join the
            // capture on GPUI's thread).
            let mut result = Some(task.await);
            this.update(cx, |this, cx| {
                if let Some(result) = result.take() {
                    this.capture_started(generation, result, cx);
                }
            })
            .ok();
            if let Some(Ok(session)) = result {
                cx.update(|cx| stop_device(session, stop_admission, cx));
            }
        })
        .detach();
    }

    /// Redraws the running capture's meter at most every
    /// [`METER_INTERVAL`], only when what it shows changed; ends an in-app
    /// capture at its limit or when its device stopped.
    fn meter_ticker(&self, generation: u64, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(METER_INTERVAL).await;
                let keep = this
                    .update(cx, |this, cx| this.tick(generation, cx))
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        })
    }

    fn capture_started(
        &mut self,
        generation: u64,
        result: broscribe_core::Result<Box<dyn CaptureSession>>,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .capture
            .as_ref()
            .is_some_and(|c| c.generation == generation);
        match result {
            Ok(session) if current => {
                let ticker = self.meter_ticker(generation, cx);
                if let Some(c) = self.capture.as_mut() {
                    c.session = Some(session);
                    c.running = true;
                    c.started = Instant::now();
                    c._ticker = Some(ticker);
                }
            }
            Ok(session) => {
                // Superseded while the device was opening.
                stop_device(session, self.state.read(cx).cli().admission(), cx);
            }
            Err(e) if current => {
                let purpose = self.capture.take().map(|c| c.purpose);
                tracing::warn!(code = e.code(), "opening the microphone");
                let text = failure_text(
                    "Couldn't record from the microphone.",
                    &core_user_text(&e),
                    microphone_instructions(self.system_settings.host_platform()),
                );
                if let Some(purpose) = purpose {
                    self.capture_failed_for(purpose, text);
                }
            }
            Err(_) => {}
        }
        cx.notify();
    }

    /// One meter check: ends an in-app capture at its limit or when the
    /// device stopped, else redraws if what the meter shows changed.
    /// Returns whether to keep ticking.
    fn tick(&mut self, generation: u64, cx: &mut Context<Self>) -> bool {
        let Some(c) = self.capture.as_mut().filter(|c| c.generation == generation) else {
            return false;
        };
        if c.helper_session.is_none() {
            let full = c.shared.full.load(Ordering::Acquire);
            let ended = c.shared.ended.load(Ordering::SeqCst);
            if full || c.started.elapsed() >= c.shared.limit + Duration::from_millis(500) {
                self.stop_capture(cx);
                return false;
            }
            if ended {
                self.capture_failed(cx);
                return false;
            }
        }
        if c.redraw_needed() {
            cx.notify();
        }
        true
    }

    fn capture_failed(&mut self, cx: &mut Context<Self>) {
        self.capture_generation += 1;
        let Some(capture) = self.capture.take() else {
            return;
        };
        if let Some(session) = capture.session {
            stop_device(session, self.state.read(cx).cli().admission(), cx);
        }
        let text = "The microphone stopped (was it disconnected?). Try again or pick another \
                    microphone."
            .to_owned();
        self.capture_failed_for(capture.purpose, text);
        cx.notify();
    }

    fn capture_failed_for(&mut self, purpose: CapturePurpose, text: String) {
        if purpose == CapturePurpose::MicCheck {
            self.mic_check = Some(Outcome::Err(text));
        } else {
            self.capture_error = Some(text);
        }
    }

    /// Stops the current capture and uses what was recorded.
    pub fn stop_capture(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.capture.as_ref().and_then(|c| c.helper_session.clone()) {
            self.stop_helper_capture(session, false, None, cx);
            return;
        }
        let Some(c) = self.capture.take() else {
            return;
        };
        self.capture_generation += 1;
        let clip = c.shared.take();
        if let Some(session) = c.session {
            stop_device(session, self.state.read(cx).cli().admission(), cx);
        }
        self.captured_device = c.device;
        self.captured(c.purpose, clip, cx);
        cx.notify();
    }

    /// Stops the current capture and discards it.
    pub fn cancel_capture(&mut self, cx: &mut Context<Self>) {
        self.capture_generation += 1;
        if self.discard_capture(cx) {
            cx.notify();
        }
    }

    /// Shows (`true`) or hides the assistant, as the route or window holding
    /// it changes. Hidden, it stops and discards its capture, cancels the
    /// Calendars step's running browser sign-ins and stops re-reading the
    /// Accessibility grant; processing already submitted (checks,
    /// enrollment, Try it) carries on and its result is shown when it
    /// comes back. Shown again, step 2 re-reads the grant once.
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            self.recheck_accessibility(cx);
        } else {
            self.accessibility_poll = None;
            self.cancel_capture(cx);
            self.deactivate_calendars(cx);
        }
        cx.notify();
    }

    /// The Calendars page stops being shown (its step left, the assistant
    /// hidden, closed or released): its running browser sign-ins are
    /// cancelled, so none completes unseen
    /// ([`IntegrationsView::deactivate`]).
    ///
    /// [`IntegrationsView::deactivate`]: crate::views::integrations::IntegrationsView::deactivate
    fn deactivate_calendars(&self, cx: &mut App) {
        if let Some(calendars) = &self.calendars {
            calendars.update(cx, |calendars, cx| calendars.deactivate(cx));
        }
    }

    /// A finished capture. Its device ([`Self::captured_device`]) follows
    /// recording 1 into the enrollment; other captures do not record one.
    fn captured(&mut self, purpose: CapturePurpose, clip: Clip, cx: &mut Context<Self>) {
        let device = self.captured_device.take();
        match purpose {
            CapturePurpose::MicCheck => {
                self.mic_check = Some(match broscribe_platform::mic_check(&clip.samples).0 {
                    "no_audio" => Outcome::Err(format!(
                        "No audio arrived. {}",
                        microphone_instructions(self.system_settings.host_platform())
                    )),
                    "silence" => Outcome::Err("The microphone works but recorded silence. Is it muted?".into()),
                    "quiet" => Outcome::Ok("The microphone works, but it's quiet. Move closer or raise its input level. Nothing was kept.".into()),
                    _ => Outcome::Ok("The microphone works · good level. Nothing was kept.".into()),
                });
            }
            CapturePurpose::Passage => self.check_reading(clip, device, cx),
            CapturePurpose::Confirmation => self.submit_recordings(Some(clip), cx),
            CapturePurpose::TryIt => self.run_try(clip, cx),
        }
    }

    // ---- 3 Models ----

    fn load_machine(&mut self, cx: &mut Context<Self>) {
        let system = self.deps.system.clone();
        let data_dir = self.state.read(cx).paths().data_dir.clone();
        let task = self
            .state
            .read(cx)
            .cli()
            .spawn(cx.background_executor(), async move {
                MachineInfo {
                    memory: system.memory_bytes(),
                    free_disk: system.free_disk_bytes(&data_dir).ok(),
                }
            });
        cx.spawn(async move |this, cx| {
            let info = task.await;
            this.update(cx, |this, cx| {
                this.machine = Some(info);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_models(&mut self, cx: &mut Context<Self>) {
        self.models_generation += 1;
        let generation = self.models_generation;
        let task = self
            .state
            .read(cx)
            .run_cli(args(["models", "list"]), ChildKind::Command, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.models_generation != generation {
                    return;
                }
                this.models = Some(
                    result
                        .map(|v| parse_models_list(&v))
                        .map_err(|e| e.user_text()),
                );
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn choose_profile(&mut self, profile: String, cx: &mut Context<Self>) {
        if self.busy || self.download(cx).read(cx).running() {
            return;
        }
        let model_name = self
            .models
            .as_ref()
            .and_then(|list| list.as_ref().ok())
            .and_then(|list| {
                list.profiles
                    .iter()
                    .find(|choice| choice.id == profile)
                    .map(|choice| list.model_name(&choice.asr))
            })
            .unwrap_or_else(|| "the selected speech model".into());
        self.profile_notice = None;
        self.cancel_sample(cx);
        self.sample = TryState::Idle;
        self.busy = true;
        cx.notify();
        let task = self.state.read(cx).run_cli(
            args(["setup", "--profile", profile.as_str(), "--non-interactive"]),
            ChildKind::Command,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let Ok(reload) = this.update(cx, |this, cx| {
                this.profile_notice = Some(match result {
                    Ok(_) => Outcome::Ok(format!("Now using {model_name}.")),
                    Err(e) => Outcome::Err(failure_text(
                        &format!("Couldn't switch to {model_name}."),
                        &e.user_text(),
                        "",
                    )),
                });
                reload_config(&this.state, cx)
            }) else {
                return;
            };
            reload.await;
            this.update(cx, |this, cx| {
                this.busy = false;
                this.load_models(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn download<'a>(&self, cx: &'a App) -> &'a Entity<ModelDownloads> {
        self.state.read(cx).model_downloads()
    }

    fn download_profile(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(Ok(list)) = &self.models else {
            return false;
        };
        let rows = match required_models(list) {
            Ok(rows) => rows,
            Err(error) => {
                self.profile_notice = Some(Outcome::Err(error));
                cx.notify();
                return false;
            }
        };
        let needed = rows
            .iter()
            .filter(|row| !row.installed())
            .map(|row| row.bytes)
            .sum();
        if self.low_disk(needed)
            || rows
                .iter()
                .any(|row| !row.installed() && !row.license_verified)
        {
            return false;
        }
        let models = rows
            .iter()
            .map(|row| DownloadModel {
                id: row.id.clone(),
                name: list.model_name(&row.id),
                bytes: row.bytes,
                installed: row.installed(),
            })
            .collect();
        let state = self.state.clone();
        self.download(cx)
            .clone()
            .update(cx, |downloads, cx| downloads.start(state, models, cx))
    }

    fn low_disk(&self, needed: u64) -> bool {
        broscribe_platform::model_download_low_disk(
            needed,
            self.machine.as_ref().and_then(|info| info.free_disk),
        )
    }

    fn models_ready(&self, purpose: CapturePurpose, cx: &App) -> bool {
        if purpose == CapturePurpose::MicCheck {
            return true;
        }
        let Some(Ok(list)) = &self.models else {
            return false;
        };
        let profile = &self.state.read(cx).config().profile;
        match purpose {
            CapturePurpose::Passage | CapturePurpose::Confirmation => {
                [&profile.vad, &profile.speaker_embedding]
                    .into_iter()
                    .all(|id| list.rows.iter().any(|row| &row.id == id && row.installed()))
            }
            CapturePurpose::TryIt => {
                required_models(list).is_ok_and(|rows| rows.iter().all(|row| row.installed()))
            }
            CapturePurpose::MicCheck => true,
        }
    }

    fn model_wait(&self, purpose: CapturePurpose, cx: &Context<Self>) -> Option<Div> {
        if self.models_ready(purpose, cx) {
            return None;
        }
        if let Some(percent) = self.download(cx).read(cx).percent() {
            return Some(row().child(kit::status_line(
                "setup-models-wait",
                kit::StatusKind::Pending,
                format!(
                    "Waiting for the speech models · {}",
                    crate::ui::format::percent(percent)
                ),
                cx,
            )));
        }
        Some(
            row()
                .flex_wrap()
                .child(kit::status_line(
                    "setup-models-wait",
                    kit::StatusKind::Attention,
                    "Download the speech models first.",
                    cx,
                ))
                .child(
                    Button::new("setup-go-models")
                        .label("Go to the language step")
                        .outline()
                        .tab_stop(true)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.request_departure(Departure::Step(Step::Models), window, cx)
                        })),
                ),
        )
    }

    /// Transcribes the active ASR model's example audio (see
    /// [`find_sample_wav`]) through the CLI child using a temporary copy.
    /// Without `--save-as-meeting`, the child saves nothing.
    /// The microphone is not used.
    fn test_transcription(&mut self, cx: &mut Context<Self>) {
        if self.busy || self.sample == TryState::Running {
            return;
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        self.sample_cancelled = Some(cancelled.clone());
        let (over, ended) = run_end();
        self.sample_ended = Some(ended);
        let child_cancelled = cancelled.clone();
        self.sample = TryState::Running;
        cx.notify();
        let (data_dir, cache_dir, asr, cli) = {
            let s = self.state.read(cx);
            (
                s.paths().data_dir.clone(),
                s.paths().cache_dir.clone(),
                s.config().profile.asr.clone(),
                s.cli().clone(),
            )
        };
        let work = async move {
            let sample =
                broscribe_core::fsutil::find_sample_wav(&data_dir.join("models").join(&asr))
                    .ok_or_else(|| format!("{asr} has no example audio; download it first."))?;
            let session = unique_name("sample", 0);
            let dir = private_dir(&cache_dir, &session).map_err(|e| io_user_text(&e))?;
            let copy = dir.join("sample.wav");
            let result = std::fs::read(&sample)
                .and_then(|bytes| write_private_file(&copy, &bytes))
                .map_err(|e| io_user_text(&e))
                .and_then(|()| {
                    cli.run_blocking_cancellable(
                        &[
                            OsString::from("setup"),
                            "try".into(),
                            "--from-file".into(),
                            copy.clone().into(),
                            "--session".into(),
                            session.into(),
                        ],
                        ChildKind::Setup,
                        child_cancelled,
                    )
                    .map_err(|e: CliError| e.user_text())
                });
            remove_dir(&dir);
            // This run's child is reaped and its copy removed.
            drop(over);
            result.map(|v| v["transcript"].as_str().unwrap_or("").to_owned())
        };
        let previous = self.pending_cancel.clone();
        let executor = cx.background_executor().clone();
        let task = cx.spawn(async move |_, _| {
            if let Some(previous) = previous {
                previous.await;
            }
            executor.spawn(work).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if cancelled.load(Ordering::SeqCst) {
                    return;
                }
                this.sample_cancelled = None;
                this.sample_ended = None;
                this.sample = TryState::Done(result);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Stops the example-audio test: only its own child is killed and
    /// reaped (its flag), and later work waits for that.
    fn cancel_sample(&mut self, cx: &mut App) {
        let Some(cancelled) = self.sample_cancelled.take() else {
            return;
        };
        cancelled.store(true, Ordering::SeqCst);
        self.sample = TryState::Idle;
        let ended = self.sample_ended.take();
        let previous = self.pending_cancel.clone();
        let task = cx
            .spawn(async move |_| {
                if let Some(previous) = previous {
                    previous.await;
                }
                if let Some(ended) = ended {
                    let _ = ended.await;
                }
            })
            .shared();
        self.pending_cancel = Some(task.clone());
        cx.spawn(async move |_| task.await).detach();
    }

    // ---- 4 Your voice ----

    fn invalidate_passage(&mut self) {
        self.passage_generation = self.passage_generation.wrapping_add(1);
        self.passage = None;
    }

    fn can_change_passage_language(&self) -> bool {
        self.capture.is_none()
            && self.passage_clip.is_none()
            && self.confirmation_clip.is_none()
            && self.reading_check.is_none()
            && self.voice_job.is_none()
            && !self.busy
    }

    fn choose_passage_language(&mut self, lang: &str, cx: &mut Context<Self>) {
        if !self.can_change_passage_language() || !VOICE_LANGUAGES.iter().any(|(id, _)| *id == lang)
        {
            return;
        }
        self.passage_language = Some(lang.to_owned());
        self.load_passage(cx);
        cx.notify();
    }

    fn load_passage(&mut self, cx: &mut Context<Self>) {
        self.invalidate_passage();
        let generation = self.passage_generation;
        let mut a = args(["setup", "voice", "--show-passage"]);
        if let Some(lang) = self
            .passage_language
            .clone()
            .or_else(|| self.state.read(cx).config().default_language.clone())
        {
            a.push("--lang".into());
            a.push(lang.into());
        }
        let task = self.state.read(cx).run_cli(a, ChildKind::Command, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.passage_loaded(generation, result, cx);
            })
            .ok();
        })
        .detach();
    }

    fn passage_loaded(
        &mut self,
        generation: u64,
        result: std::result::Result<Value, CliError>,
        cx: &mut Context<Self>,
    ) {
        if self.passage_generation != generation {
            return;
        }
        self.passage = Some(
            result
                .map(|v| Passage {
                    lang: v["lang"].as_str().unwrap_or("").to_owned(),
                    text: v["passage"].as_str().unwrap_or("").to_owned(),
                    confirmation: v["confirmation_phrase"].as_str().unwrap_or("").to_owned(),
                })
                .map_err(|e| e.user_text()),
        );
        cx.notify();
    }

    /// The Name and Email fields: trimmed name, and the emails (an invalid
    /// one is an error message).
    fn me_fields(&self, cx: &App) -> Result<(String, Vec<String>), IdentityError> {
        me_fields(
            self.name.read(cx).value().as_ref(),
            self.emails.read(cx).value().as_ref(),
        )
    }

    /// The fields as an enrollment sends them: an invalid email is an
    /// error, and so is an empty name while nothing else names Me (no saved
    /// setup name, no Me person, or not known yet).
    fn voice_identity(&self, cx: &App) -> Result<(String, Vec<String>), IdentityError> {
        let (name, emails) = self.me_fields(cx)?;
        let named = !name.is_empty()
            || self
                .state
                .read(cx)
                .config()
                .setup
                .name
                .as_deref()
                .is_some_and(|n| !n.trim().is_empty())
            || self.me_exists == Some(true);
        if !named {
            return Err(IdentityError::Name(
                "Enter your name to save your voice.".into(),
            ));
        }
        Ok((name, emails))
    }

    /// Reads whether a Me person exists on the background executor.
    fn load_me_exists(&mut self, cx: &mut Context<Self>) {
        let library = self.state.read(cx).library().clone();
        let task = cx.background_executor().spawn(async move {
            let me = library.self_person()?;
            let saved = if let Some(person) = &me {
                !library.load_voiceprints(&person.id)?.embeddings.is_empty()
            } else {
                false
            };
            Ok::<_, broscribe_core::Error>((me.is_some(), saved))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                match result {
                    Ok((exists, saved)) => {
                        this.me_exists = Some(exists);
                        this.voice_saved = Some(saved);
                    }
                    Err(e) => tracing::warn!(code = e.code(), "checking for Me"),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn identity_inputs(&self, cx: &App) -> (String, String) {
        (
            self.name.read(cx).value().to_string(),
            self.emails.read(cx).value().to_string(),
        )
    }

    fn identity_dirty(&self, cx: &App) -> bool {
        self.identity_inputs(cx) != self.saved_identity
    }

    fn save_me(&mut self, cx: &mut Context<Self>) {
        let (name, emails) = match self.me_fields(cx) {
            Ok(fields) => fields,
            Err(error) => {
                self.identity_error = Some(error);
                cx.notify();
                return;
            }
        };
        let inputs = self.identity_inputs(cx);
        self.identity_error = None;
        self.identity_save_error = None;
        self.advancing = true;
        self.advance_generation += 1;
        let generation = self.advance_generation;
        // The name and emails go on stdin, never in argv.
        let task = self.state.read(cx).cli().run_with_stdin(
            args(["setup", "--non-interactive", "--spec-stdin"]),
            ChildKind::Command,
            identity_spec(&name, &emails),
            cx.background_executor(),
        );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let Ok(reload) = this.update(cx, |this, cx| reload_config(&this.state, cx)) else {
                return;
            };
            reload.await;
            this.update(cx, |this, cx| {
                if this.advance_generation != generation || this.step != Step::Voice {
                    return;
                }
                this.advancing = false;
                match result {
                    Ok(_) => {
                        this.saved_identity = inputs;
                        this.skip(cx);
                    }
                    Err(e) => {
                        this.identity_save_error = Some(failure_text(
                            "Couldn't save your details.",
                            &e.user_text(),
                            "",
                        ))
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Starts recording 1 (again) once the Name/Email fields can identify
    /// Me; checked before recording, so no reading is lost to it. Drops the
    /// held recordings and any running check of recording 1.
    fn start_reading(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.voice_identity(cx) {
            self.identity_error = Some(error);
            cx.notify();
            return;
        }
        self.identity_error = None;
        self.discard_recordings(cx);
        self.voice_outcome = None;
        self.voice_details = false;
        self.start_capture(CapturePurpose::Passage, cx)
    }

    fn discard_and_read_again(&mut self, cx: &mut Context<Self>) {
        // Discard is unconditional, even if edited identity fields prevent
        // starting a replacement recording.
        self.discard_recordings(cx);
        self.start_reading(cx);
    }

    /// Whether recording 1 passed its check and is held for the enrollment.
    fn first_recording_ready(&self) -> bool {
        self.reading_check.is_none()
            && self.passage_clip.is_some()
            && matches!(self.reading_feedback, Some(ReadingFeedback::Ready { .. }))
    }

    fn voice_stage(&self) -> VoiceStage {
        match self.capture.as_ref().map(|c| c.purpose) {
            Some(purpose @ (CapturePurpose::Passage | CapturePurpose::Confirmation)) => {
                VoiceStage::Recording(purpose)
            }
            _ if self.voice_job.is_some() => VoiceStage::Processing,
            _ if self.reading_check.is_some() => VoiceStage::Checking,
            _ if self.first_recording_ready() => VoiceStage::Phrase,
            _ => VoiceStage::Start,
        }
    }

    /// Records the optional phrase (recording 2), only after recording 1
    /// passed its check. After an inconclusive phrase check this is "Retry
    /// confirmation only": recording 1 stays held and is not recorded again.
    fn record_phrase(&mut self, cx: &mut Context<Self>) {
        if self.first_recording_ready() {
            self.start_capture(CapturePurpose::Confirmation, cx);
            if self.capture.is_some() {
                self.clear_unconfirmed();
            }
        }
    }

    /// Drops both recordings, the check of recording 1 and its result, and
    /// an inconclusive phrase report that offered choices for them.
    fn discard_recordings(&mut self, cx: &mut App) {
        self.cancel_reading_check(cx);
        self.passage_clip = None;
        self.passage_device = None;
        self.confirmation_clip = None;
        self.reading_feedback = None;
        self.clear_unconfirmed();
    }

    fn clear_unconfirmed(&mut self) {
        if matches!(self.voice_outcome, Some(VoiceOutcome::Unconfirmed(_))) {
            self.voice_outcome = None;
        }
    }

    /// Checks recording 1 with `setup voice --check-recording` off the UI
    /// thread. The clip moves into the task and comes back with the reply;
    /// it is held only when the reply says `ready`.
    fn check_reading(&mut self, clip: Clip, device: Option<String>, cx: &mut Context<Self>) {
        self.discard_recordings(cx);
        self.voice_outcome = None;
        if clip.samples.is_empty() || clip.sample_rate == 0 {
            self.reading_feedback = Some(ReadingFeedback::NotReady {
                warnings: Vec::new(),
            });
            cx.notify();
            return;
        }
        self.capture_generation += 1;
        let generation = self.capture_generation;
        let session = unique_name("check", generation);
        let (cache_dir, cli) = {
            let s = self.state.read(cx);
            (s.paths().cache_dir.clone(), s.cli().clone())
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let (over, ended) = run_end();
        self.reading_check = Some(ReadingCheckJob {
            generation,
            dir: cache_dir.join(CAPTURE_DIR).join(&session),
            cancelled: cancelled.clone(),
            ended,
            device,
        });
        // An earlier cancellation's cleanup finishes before this child starts.
        let previous = self.pending_cancel.clone();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let (clip, result) = executor
                .spawn(async move {
                    let checked = run_reading_check(&cli, &cache_dir, &session, clip, cancelled);
                    // The child (if any) is reaped and the copy removed.
                    drop(over);
                    checked
                })
                .await;
            this.update(cx, |this, cx| {
                this.reading_checked(generation, clip, result, cx)
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// The check's reply: ignored once the check was cancelled or replaced.
    fn reading_checked(
        &mut self,
        generation: u64,
        clip: Clip,
        result: std::result::Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(job) = self.reading_check.take_if(|c| c.generation == generation) else {
            return;
        };
        self.reading_feedback = Some(match result {
            Ok(v) => {
                let check = reading_check(&v);
                if check.ready {
                    self.passage_clip = Some(clip);
                    self.passage_device = job.device;
                    ReadingFeedback::Ready {
                        warnings: check.warnings,
                    }
                } else {
                    ReadingFeedback::NotReady {
                        warnings: check.warnings,
                    }
                }
            }
            Err(e) => ReadingFeedback::Failed(e),
        });
        cx.notify();
    }

    /// Drops the running check: its reply is ignored, then (after any
    /// earlier cancel) its child is killed and reaped and its session
    /// folder removed.
    fn cancel_reading_check(&mut self, cx: &mut App) {
        let Some(check) = self.reading_check.take() else {
            return;
        };
        // Stops only this check's child; the folder goes once it is reaped.
        check.cancelled.store(true, Ordering::SeqCst);
        let previous = self.pending_cancel.clone();
        let executor = cx.background_executor().clone();
        let cli = self.state.read(cx).cli().clone();
        let task = cx
            .spawn(async move |_| {
                if let Some(previous) = previous {
                    previous.await;
                }
                cli.spawn(&executor, stop_reading_check(check.ended, check.dir))
                    .await;
            })
            .shared();
        self.pending_cancel = Some(task.clone());
        cx.spawn(async move |_| task.await).detach();
    }

    /// Submits the recorded passage (and phrase) once the Name/Email fields
    /// are valid again: they may have been edited while reading. Otherwise
    /// shows the field error and keeps both recordings until it is fixed.
    fn submit_recordings(&mut self, confirmation: Option<Clip>, cx: &mut Context<Self>) {
        if !self.first_recording_ready() {
            return;
        }
        match self.voice_identity(cx) {
            Ok(me) => {
                self.confirmation_clip = None;
                if let Some(passage) = self.passage_clip.take() {
                    self.submit_voice(passage, confirmation, me, cx);
                }
            }
            Err(error) => {
                self.confirmation_clip = confirmation;
                self.identity_error = Some(error);
                cx.notify();
            }
        }
    }

    /// Hands the passage (and the confirmation phrase) to `setup voice`,
    /// with a fresh session id written under the library lock first. `me`
    /// is the validated Name and Emails, fixed at submission. With the
    /// phrase, recording 1 itself moves through preparation into the job
    /// (RAM only) so an inconclusive phrase check can give it back.
    fn submit_voice(
        &mut self,
        passage: Clip,
        confirmation: Option<Clip>,
        me: (String, Vec<String>),
        cx: &mut Context<Self>,
    ) {
        // A new submission supersedes a running one; its child is killed
        // before the new one can start.
        let previous = self.cancel_voice_job_task(cx);
        self.capture_generation += 1;
        let generation = self.capture_generation;
        let session = unique_name("vs", generation);
        let (library, data_dir, cache_dir) = {
            let s = self.state.read(cx);
            (
                s.library().clone(),
                s.paths().data_dir.clone(),
                s.paths().cache_dir.clone(),
            )
        };
        let dir_name = session.clone();
        let token = session.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let job_cancelled = cancelled.clone();
        self.voice_outcome = None;
        self.reading_feedback = None;
        self.identity_error = None;
        self.capture_error = None;
        // Recording 1's own device, whatever the picker shows now.
        let device = self.passage_device.clone();
        let prep = async move {
            let dir = private_dir(&cache_dir, &dir_name).map_err(|e| io_user_text(&e))?;
            let passage_path = dir.join("passage.wav");
            let confirm_path = dir.join("confirmation.wav");
            let written = write_private_file(
                &passage_path,
                &wav_bytes(&passage.samples, passage.sample_rate),
            )
            .and_then(|()| match &confirmation {
                Some(c) => write_private_file(&confirm_path, &wav_bytes(&c.samples, c.sample_rate)),
                None => Ok(()),
            });
            if let Err(e) = written {
                remove_dir(&dir);
                return Err(io_user_text(&e));
            }
            let prepared = library.write(|_| {
                let before = enrollment_ids(&library)?;
                if cancelled.load(Ordering::SeqCst) {
                    return Err(broscribe_core::Error::InvalidInput(
                        "Cancelled. Nothing was saved.".into(),
                    ));
                }
                Config::update(&data_dir, |config| {
                    config.setup.voice_session = Some(token.clone());
                    Ok(())
                })?;
                Ok(before)
            });
            match prepared {
                Ok(before) => Ok(PreparedVoice {
                    dir,
                    passage: passage_path,
                    confirm: confirmation.is_some().then_some(confirm_path),
                    before,
                    reading: passage,
                    device,
                }),
                Err(e) => {
                    remove_dir(&dir);
                    Err(core_user_text(&e))
                }
            }
        };
        // Registered before preparing so cancel works meanwhile.
        self.voice_job = Some(VoiceJob {
            generation,
            session: session.clone(),
            dir: PathBuf::new(),
            before: None,
            cancelled: job_cancelled,
            stop: Arc::new(AtomicBool::new(false)),
            ended: None,
            reading: None,
        });
        cx.notify();
        let executor = cx.background_executor().clone();
        let cli = self.state.read(cx).cli().clone();
        cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let prepared = executor.spawn(prep).await;
            let abandoned_dir = prepared.as_ref().ok().map(|p| p.dir.clone());
            if this
                .update(cx, |this, cx| {
                    this.voice_prepared(generation, session, me, prepared, cx)
                })
                .is_err()
                && let Some(dir) = abandoned_dir
            {
                cli.spawn(&executor, async move { remove_dir(&dir) })
                    .detach();
            }
        })
        .detach();
    }

    fn voice_prepared(
        &mut self,
        generation: u64,
        session: String,
        me: (String, Vec<String>),
        prepared: std::result::Result<PreparedVoice, String>,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .voice_job
            .as_ref()
            .is_some_and(|j| j.generation == generation);
        let PreparedVoice {
            dir,
            passage,
            confirm,
            before,
            reading,
            device,
        } = match prepared {
            Ok(p) => p,
            Err(e) => {
                if current {
                    self.voice_job = None;
                    self.voice_outcome = Some(VoiceOutcome::Failed(e));
                    cx.notify();
                }
                return;
            }
        };
        if !current {
            // Cancelled while preparing: make sure the token is gone.
            let (library, data_dir) = {
                let s = self.state.read(cx);
                (s.library().clone(), s.paths().data_dir.clone())
            };
            cx.background_executor()
                .spawn(async move {
                    if let Err(e) = invalidate_session(&library, &data_dir, &session, None) {
                        tracing::warn!(code = e.code(), "clearing a cancelled voice session");
                    }
                    remove_dir(&dir);
                })
                .detach();
            return;
        }
        if let Some(job) = self.voice_job.as_mut() {
            job.dir = dir;
            job.before = Some(before);
            // Only a submission with the phrase can come back inconclusive.
            job.reading = confirm.is_some().then_some(reading);
        }
        // The Name / Email validated at submission, saved or not: the child
        // upserts Me under its commit lock, so a first enrollment never
        // fails for lack of it.
        let (name, emails) = me;
        let a = voice_args(&passage, confirm.as_deref(), &session, device);
        let spec = identity_spec(&name, &emails);
        let (over, ended) = run_end();
        let job = self.voice_job.as_mut().expect("prepared voice job");
        job.ended = Some(ended);
        let stop = job.stop.clone();
        let cli = self.state.read(cx).cli().clone();
        let task = cx.background_executor().spawn(async move {
            let result = cli.run_blocking_cancellable_stdin(&a, ChildKind::Setup, spec, stop);
            // This run's child is reaped.
            drop(over);
            result
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| this.voice_done(generation, result, cx))
                .ok();
        })
        .detach();
    }

    fn voice_done(
        &mut self,
        generation: u64,
        result: std::result::Result<Value, CliError>,
        cx: &mut Context<Self>,
    ) {
        let Some(mut job) = self.voice_job.take_if(|j| j.generation == generation) else {
            // Cancelled or superseded: the cancel path reported it.
            return;
        };
        let reading = job.reading.take();
        let outcome = match &result {
            Ok(v) => voice_outcome(v),
            Err(e) => VoiceOutcome::Failed(e.user_text()),
        };
        let outcome = match (outcome, reading) {
            // Nothing was saved: recording 1 is held again, as ready as its
            // check left it, until an explicit choice.
            (VoiceOutcome::Unconfirmed(u), Some(reading)) => {
                self.passage_clip = Some(reading);
                self.confirmation_clip = None;
                self.reading_feedback = Some(ReadingFeedback::Ready {
                    warnings: u.reading_warnings.clone(),
                });
                self.capture_error = None;
                VoiceOutcome::Unconfirmed(u)
            }
            (VoiceOutcome::Unconfirmed(_), None) => {
                VoiceOutcome::Failed(MALFORMED_UNCONFIRMED.into())
            }
            (outcome, _) => outcome,
        };
        if matches!(outcome, VoiceOutcome::Enrolled { .. }) {
            self.saved_identity = self.identity_inputs(cx);
            self.voice_saved = Some(true);
            let task = mark_step(&self.state, Step::Voice.id(), cx);
            self.report(task, None, cx);
            self.load_me_exists(cx);
        }
        self.voice_outcome = Some(outcome);
        // Clear the token (the child may have left it) and the temp dir,
        // after any earlier cancellation and before the next job prepares.
        // The child is already reaped, so nothing is killed or discarded.
        let (library, data_dir) = {
            let s = self.state.read(cx);
            (s.library().clone(), s.paths().data_dir.clone())
        };
        let previous = self.pending_cancel.clone();
        let executor = cx.background_executor().clone();
        let cleanup = cx
            .spawn(async move |_, _| {
                if let Some(previous) = previous {
                    previous.await;
                }
                executor
                    .spawn(async move {
                        if let Err(e) = invalidate_session(
                            &library,
                            &data_dir,
                            &job.session,
                            job.before.as_ref(),
                        ) {
                            tracing::warn!(code = e.code(), "clearing the voice session");
                        }
                        remove_dir(&job.dir);
                    })
                    .await;
            })
            .shared();
        self.pending_cancel = Some(cleanup.clone());
        cx.spawn(async move |this, cx| {
            cleanup.await;
            this.update(cx, |this, cx| reload_config(&this.state, cx).detach())
                .ok();
        })
        .detach();
        cx.notify();
    }

    /// Background part of a cancel: clears the token under the library lock
    /// first (so the child can no longer commit), then stops only this
    /// job's run and waits until its child is reaped, then discards its
    /// staged import and removes the temp dir. Resolves to whether the child
    /// had already committed.
    fn spawn_cancel_job(&self, mut job: VoiceJob, cx: &App) -> Task<broscribe_core::Result<bool>> {
        let (library, data_dir, cli) = {
            let s = self.state.read(cx);
            (
                s.library().clone(),
                s.paths().data_dir.clone(),
                s.cli().clone(),
            )
        };
        // A cancelled job never gives its recording back, and its
        // preparation writes no token from now on.
        job.reading = None;
        job.cancelled.store(true, Ordering::SeqCst);
        cx.background_executor().spawn(async move {
            let committed =
                invalidate_session(&library, &data_dir, &job.session, job.before.as_ref());
            job.stop.store(true, Ordering::SeqCst);
            if let Some(ended) = job.ended {
                let _ = ended.await;
            }
            // The stopped child's staged Try-it import (job and audio) goes
            // now, not at the next import.
            if let Err(e) = cli.run_blocking(
                &args(["queue", "discard-setup", "--session", &job.session]),
                ChildKind::Command,
            ) {
                tracing::warn!(code = %e.code, "discarding the setup import");
            }
            if !job.dir.as_os_str().is_empty() {
                remove_dir(&job.dir);
            }
            committed
        })
    }

    /// Cancels the running enrollment (supersede, close, quit). `None` when
    /// no enrollment runs.
    fn cancel_voice_job_task(&mut self, cx: &mut App) -> Option<futures::future::Shared<Task<()>>> {
        let Some(job) = self.voice_job.take() else {
            return self.pending_cancel.clone();
        };
        let task = self.spawn_cancel_job(job, cx);
        let task = cx
            .spawn(async move |_| {
                if let Err(e) = task.await {
                    tracing::warn!(code = e.code(), "cancelling the voice enrollment");
                }
            })
            .shared();
        self.pending_cancel = Some(task.clone());
        Some(task)
    }

    /// The user's Cancel: reports "completed" when the child had committed.
    fn cancel_voice(&mut self, cx: &mut Context<Self>) {
        let Some(job) = self.voice_job.take() else {
            return;
        };
        let inputs = self.identity_inputs(cx);
        let generation = self.capture_generation;
        let task = self
            .spawn_cancel_job(job, cx)
            .map(|result| result.map_err(|e| core_user_text(&e)))
            .shared();
        let completion = task.clone();
        self.pending_cancel = Some(
            cx.spawn(async move |_, _| {
                let _ = completion.await;
            })
            .shared(),
        );
        cx.spawn(async move |this, cx| {
            let committed = task.await;
            this.update(cx, |this, cx| {
                if this.capture_generation != generation {
                    return;
                }
                this.voice_outcome = Some(match committed {
                    Ok(true) => {
                        this.saved_identity = inputs;
                        this.voice_saved = Some(true);
                        let task = mark_step(&this.state, Step::Voice.id(), cx);
                        this.report(task, None, cx);
                        VoiceOutcome::CompletedBeforeCancel
                    }
                    Ok(false) => VoiceOutcome::Cancelled,
                    Err(e) => {
                        VoiceOutcome::Failed(failure_text("Cancelling didn't finish.", &e, ""))
                    }
                });
                reload_config(&this.state, cx).detach();
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    // ---- 7 Words and people ----

    fn save_retention_days(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.retention.checking_or_confirming() {
            return;
        }
        let text = self.retention_days.read(cx).value().to_string();
        match super::settings::parse_retention_days(&text) {
            Some(days) => {
                let retention = Retention::DeleteAfterDays { days };
                if self.state.read(cx).config().retention != retention {
                    self.request_retention(retention, RetentionTrigger::DaysSave, None, window, cx);
                }
            }
            None => {
                self.legacy_outcome = Some(Outcome::Err(
                    "Use a whole number of days from 1 to 3,650.".into(),
                ));
                cx.notify();
            }
        }
    }

    fn save_words(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.words.read(cx).value().to_string();
        let Some(task) = save_word_list(&self.state, &text, cx) else {
            self.legacy_outcome = Some(Outcome::Err("Enter at least one word.".into()));
            cx.notify();
            return;
        };
        self.busy = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                this.legacy_outcome = Some(match result {
                    Ok(v) => {
                        // Saved: the field empties unless it was edited since.
                        if *this.words.read(cx).value() == *text {
                            this.words
                                .update(cx, |words, cx| words.set_value(String::new(), window, cx));
                            this.words_count = 0;
                        }
                        let count = v["words_added"].as_u64().unwrap_or(0);
                        Outcome::Ok(format!(
                            "Added {count} new {}.",
                            plural(count, "word", "words")
                        ))
                    }
                    Err(e) => Outcome::Err(e),
                });
                reload_config(&this.state, cx).detach();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---- 8 Meeting apps ----

    fn load_meeting_apps(&mut self, cx: &mut Context<Self>) {
        let task = self
            .state
            .read(cx)
            .run_cli(args(["detect", "status"]), ChildKind::Command, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.detection_off = result.as_ref().is_ok_and(detection_off);
                this.meeting_apps = Some(
                    result
                        .map(|v| parse_meeting_apps(&v))
                        .map_err(|e| e.user_text()),
                );
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_detect_policy(&mut self, app: String, policy: &'static str, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.policy_errors.remove(&app);
        cx.notify();
        let task =
            self.state
                .read(cx)
                .run_cli(detect_policy_args(&app, policy), ChildKind::Command, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(_) => {
                        if let Some(Ok(apps)) = this.meeting_apps.as_mut()
                            && let Some(row) = apps.iter_mut().find(|a| a.id == app)
                        {
                            row.policy = Some(policy.to_owned());
                        }
                    }
                    Err(e) => {
                        this.policy_errors.insert(
                            app,
                            failure_text("Couldn't change this app's setting.", &e.user_text(), ""),
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---- 12 Existing recordings ----

    fn choose_recordings(&mut self, cx: &mut Context<Self>) {
        let rx = crate::views::import_sheet::pick_import_paths(
            PathPromptOptions {
                files: true,
                directories: true,
                multiple: true,
                prompt: Some("Import".into()),
            },
            cx,
        );
        cx.spawn(async move |this, cx| {
            let Ok(Some(paths)) = rx.await else {
                return;
            };
            if paths.is_empty() {
                return;
            }
            this.update(cx, |this, cx| {
                crate::views::import_sheet::ImportRuns::request_dialog(paths, cx);
                this.state
                    .update(cx, |_, cx| cx.emit(crate::state::AppEvent::OpenMainWindow));
            })
            .ok();
        })
        .detach();
    }

    fn import_manifest(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import manifest".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |this, cx| {
                let spec = match manifest_import_spec(&path) {
                    Ok(spec) => spec,
                    Err(message) => {
                        this.legacy_outcome = Some(Outcome::Err(message));
                        cx.notify();
                        return;
                    }
                };
                this.busy = true;
                cx.notify();
                // The manifest's path goes on stdin, never in argv; the child reads it
                // where it is, so the batch keeps its identity across runs.
                let task = this.state.read(cx).cli().run_with_stdin(
                    manifest_import_args(),
                    ChildKind::Command,
                    spec,
                    cx.background_executor(),
                );
                cx.spawn(async move |this, cx| {
                    let result = task.await;
                    this.update(cx, |this, cx| {
                        this.busy = false;
                        this.legacy_outcome = Some(match result {
                            Ok(_) => {
                                let task = mark_step(&this.state, Step::Existing.id(), cx);
                                this.report(task, None, cx);
                                this.state.update(cx, |s, cx| s.kick_jobs(cx));
                                Outcome::Ok(
                                    "The manifest's meetings are queued; progress shows at the \
                                     bottom of the window."
                                        .into(),
                                )
                            }
                            Err(e) => Outcome::Err(failure_text(
                                "Couldn't import the manifest.",
                                &e.user_text(),
                                "",
                            )),
                        });
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
            })
            .ok();
        })
        .detach();
    }

    // ---- 13 Try it ----

    fn cancel_try(
        &mut self,
        cx: &mut App,
    ) -> Option<Task<broscribe_core::Result<Option<MeetingId>>>> {
        let job = self.try_job.take()?;
        self.try_state = TryState::Idle;
        let cleanup = self.spawn_cancel_job(job, cx);
        let recording = self.try_recording.take();
        let library = self.state.read(cx).library().clone();
        let task = cx.background_executor().spawn(async move {
            cleanup.await?;
            // Publication and token invalidation take the same library lock.
            // If publication won, retain the user's explicitly saved meeting.
            let Some(fingerprint) = recording.as_ref().and_then(|recording| recording.get()) else {
                return Ok(None);
            };
            for directory in library.list_meeting_dirs()? {
                let meeting = library.load_meeting_at(&directory)?;
                if meeting
                    .recording
                    .is_some_and(|r| r.sha256 == fingerprint.sha256 && r.bytes == fingerprint.bytes)
                {
                    return Ok(Some(meeting.id));
                }
            }
            Ok(None)
        });
        Some(task)
    }

    fn stop_try_transcription(&mut self, cx: &mut Context<Self>) {
        let Some(cancelled) = self.cancel_try(cx) else {
            return;
        };
        let generation = self.capture_generation;
        let task = cx
            .spawn(async move |this, cx| {
                let result = cancelled.await;
                let _ = this.update(cx, |this, cx| {
                    if this.capture_generation != generation || this.step != Step::TryIt {
                        return;
                    }
                    this.try_state = match result {
                        Ok(Some(meeting)) => {
                            this.state.update(cx, |s, cx| s.refresh_all(cx));
                            TryState::Done(Ok(TryResult {
                                meeting: Some(meeting),
                                text: String::new(),
                                me: TryMe::None,
                                saved_before_cancel: true,
                            }))
                        }
                        Ok(None) => TryState::Cancelled,
                        Err(error) => {
                            tracing::warn!(code = error.code(), "cancelling Try it");
                            TryState::Done(Err(
                                "Couldn't verify what the test saved. Check the Library.".into(),
                            ))
                        }
                    };
                    cx.notify();
                });
            })
            .shared();
        self.pending_cancel = Some(task.clone());
        cx.spawn(async move |_, _| task.await).detach();
        // Keep the panel in its processing state until the child is reaped.
        // Without a job there is no second cancellation action.
        self.try_state = TryState::Running;
        cx.notify();
    }

    fn discard_try(&mut self, cx: &mut App) {
        if self.try_job.is_none() && self.try_state == TryState::Running {
            self.try_state = TryState::Idle;
        }
        if let Some(cancelled) = self.cancel_try(cx) {
            let task = cx
                .spawn(async move |_| {
                    if let Err(error) = cancelled.await {
                        tracing::warn!(code = error.code(), "cancelling Try it");
                    }
                })
                .shared();
            self.pending_cancel = Some(task.clone());
            cx.spawn(async move |_| task.await).detach();
        }
    }

    fn run_try(&mut self, clip: Clip, cx: &mut Context<Self>) {
        self.discard_try(cx);
        if clip.samples.is_empty() {
            self.try_state = TryState::Done(Err("Nothing was recorded.".into()));
            cx.notify();
            return;
        }
        self.try_state = TryState::Running;
        let generation = self.capture_generation;
        let session = unique_name("try", generation);
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (over, ended) = run_end();
        self.try_job = Some(VoiceJob {
            generation,
            session: session.clone(),
            dir: PathBuf::new(),
            before: None,
            cancelled: cancelled.clone(),
            stop: stop.clone(),
            ended: Some(ended),
            reading: None,
        });
        let save = self.save_try_as_meeting;
        let recording = save.then(|| Arc::new(OnceLock::new()));
        self.try_recording = recording.clone();
        let previous = self.pending_cancel.clone();
        let (cache_dir, data_dir, library, cli, lang) = {
            let s = self.state.read(cx);
            (
                s.paths().cache_dir.clone(),
                s.paths().data_dir.clone(),
                s.library().clone(),
                s.cli().clone(),
                s.config().default_language.clone(),
            )
        };
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            if let Some(previous) = previous {
                previous.await;
            }
            let task = executor.spawn(async move {
                // Dropped when this run is over (its child reaped).
                let _over = over;
                let dir = private_dir(&cache_dir, &session).map_err(|e| io_user_text(&e))?;
                let file = dir.join("try.wav");
                let result = (|| {
                    let wav = wav_bytes(&clip.samples, clip.sample_rate);
                    write_private_file(&file, &wav).map_err(|e| io_user_text(&e))?;
                    library
                        .write(|_| {
                            if cancelled.load(Ordering::SeqCst) {
                                return Err(broscribe_core::Error::InvalidInput(
                                    "Try it cancelled".into(),
                                ));
                            }
                            Config::update(&data_dir, |config| {
                                config.setup.voice_session = Some(session.clone());
                                Ok(())
                            })
                            .map(|_| ())
                        })
                        .map_err(|e| core_user_text(&e))?;
                    if cancelled.load(Ordering::SeqCst) {
                        return Err("It was cancelled.".into());
                    }
                    let mut a = args(["setup", "try", "--from-file"]);
                    a.push(file.into());
                    a.extend(args(["--session", &session]));
                    if let Some(lang) = lang {
                        a.extend(args(["--lang", &lang]));
                    }
                    if save {
                        a.push("--save-as-meeting".into());
                    }
                    if let Some(recording) = recording {
                        let _ = recording.set(RecordingFingerprint {
                            sha256: broscribe_core::schema::sha256_hex(&wav),
                            bytes: wav.len() as u64,
                        });
                    }
                    cli.run_blocking_cancellable(&a, ChildKind::Setup, stop)
                        .map_err(|e| e.user_text())
                })();
                let _ = invalidate_session(&library, &data_dir, &session, None);
                remove_dir(&dir);
                result
            });
            let result = task.await;
            this.update(cx, |this, cx| {
                if this
                    .try_job
                    .take_if(|j| j.generation == generation)
                    .is_none()
                {
                    return;
                }
                this.try_recording = None;
                this.try_state = TryState::Done(result.and_then(TryResult::from_reply));
                if let TryState::Done(Ok(result)) = &this.try_state {
                    if !result.text.trim().is_empty() {
                        let task = mark_step(&this.state, Step::TryIt.id(), cx);
                        this.report(task, None, cx);
                    }
                    if save {
                        this.state.update(cx, |s, cx| s.refresh_all(cx));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    // ---- rendering ----

    fn render_meter(&self, cx: &App) -> Option<AnyElement> {
        let c = self.capture.as_ref()?;
        let Some((level, seconds)) = c.meter() else {
            return Some(
                muted("setup-meter-opening", "Opening the microphone…", cx).into_any_element(),
            );
        };
        let seconds = seconds.round() as u64;
        let limit = c.shared.limit.as_secs();
        let shown = meter_time_text(c.purpose, seconds, limit, crate::ui::format::seconds);
        let spoken = meter_time_text(c.purpose, seconds, limit, crate::ui::format::spoken_seconds);
        let maximum = if matches!(
            c.purpose,
            CapturePurpose::Passage | CapturePurpose::Confirmation
        ) {
            format!(
                "Recording time, maximum {}",
                crate::ui::format::spoken_seconds(limit)
            )
        } else {
            "Recording time".into()
        };
        let clock = crate::ui::timer_line("setup-meter-time", maximum, spoken, shown)
            .text_role(TextRole::Caption)
            .text_color(cx.theme().muted_foreground);
        Some(
            kit::stack()
                .child(
                    row()
                        .when_some(level, |el, level| {
                            el.child(kit::level_meter(
                                "setup-level",
                                kit::Track::Me,
                                level.rms_dbfs(),
                                kit::MeterState::Recording,
                                cx,
                            ))
                        })
                        .child(clock),
                )
                .when(c.clipped, |el| el.child(clipping_warning(cx)))
                .into_any_element(),
        )
    }

    fn render_device_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        match &self.helper {
            Some(Ok(HelperRoute::Stopped)) => {
                return row()
                    .child(muted(
                        "setup-helper-stopped",
                        "BroScribe Agent isn't running.",
                        cx,
                    ))
                    .child(
                        Button::new("setup-start-helper")
                            .label("Start BroScribe Agent")
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.start_helper(cx)),
                            ),
                    )
                    .children(
                        self.agent_start_error
                            .as_ref()
                            .map(|e| error_line("setup-agent-start-error", e.clone(), cx)),
                    )
                    .into_any_element();
            }
            Some(Ok(HelperRoute::Running)) => {
                return muted(
                    "setup-helper-default-mic",
                    "BroScribe records with your default microphone.",
                    cx,
                )
                .into_any_element();
            }
            Some(Err(e)) => {
                return crate::ui::kit::retry_line(
                    "setup-helper-error",
                    e.clone(),
                    cx,
                    |this, _, cx| this.retry_device_list(cx),
                )
                .into_any_element();
            }
            _ => {}
        }
        match &self.devices {
            None => crate::ui::kit::status_line(
                "setup-mic-loading",
                crate::ui::kit::StatusKind::Pending,
                "Looking for microphones…",
                cx,
            )
            .into_any_element(),
            Some(Err(e)) => crate::ui::kit::retry_line(
                "setup-mic-error",
                failure_text("Couldn't list the microphones.", &sentence(e), ""),
                cx,
                |this, _, cx| this.retry_device_list(cx),
            )
            .into_any_element(),
            Some(Ok(devices)) if devices.is_empty() => crate::ui::kit::retry_line(
                "setup-mic-error",
                "No microphone found.",
                cx,
                |this, _, cx| this.retry_device_list(cx),
            )
            .into_any_element(),
            Some(Ok(devices)) => self.mic_choices(devices, cx).into_any_element(),
        }
    }

    fn retry_device_list(&mut self, cx: &mut Context<Self>) {
        self.helper = None;
        self.devices = None;
        self.load_devices(cx);
        cx.notify();
    }

    /// The in-app capture's microphones as one radio group (the system
    /// default is chosen when the list loads).
    fn mic_choices(&self, devices: &[AudioDevice], cx: &Context<Self>) -> impl IntoElement {
        let options = devices
            .iter()
            .map(|device| {
                crate::ui::select::OptionItem::new(device.id.clone(), device.name.clone())
            })
            .collect();
        crate::ui::kit::field(
            "setup-microphone-label",
            "Microphone",
            None,
            |label| {
                crate::ui::select::Picker::new(
                    "setup-mic",
                    label,
                    options,
                    self.device.clone().unwrap_or_default(),
                    cx,
                    |this, id, _, cx| {
                        this.device = Some(id.to_owned());
                        cx.notify();
                    },
                )
                .disabled(self.capture.is_some())
            },
            cx,
        )
    }

    fn system_settings_button(
        &self,
        id: impl Into<ElementId>,
        pane: SettingsPane,
        cx: &Context<Self>,
    ) -> Option<Div> {
        let id = id.into();
        let settings = self.system_settings.clone();
        let label = match settings.host_platform() {
            HostPlatform::MacOs if pane == SettingsPane::LoginItems => "Open Login Items",
            HostPlatform::MacOs => "Open System Settings",
            HostPlatform::Windows if pane == SettingsPane::Taskbar => "Open taskbar settings",
            HostPlatform::Windows => "Open Windows Settings",
            HostPlatform::Linux => return None,
        };
        settings.supports(pane).then(|| {
            let error_id = id.clone();
            kit::stack()
                .items_start()
                .child(
                    Button::new(id.clone())
                        .label(label)
                        .outline()
                        .self_start()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.settings_error = settings.open(pane).err().map(|error| {
                                tracing::warn!(code = error.code(), "opening permission settings");
                                id.clone()
                            });
                            cx.notify();
                        })),
                )
                .children(self.settings_error_line(
                    error_id,
                    "Couldn't open system settings. You can open them manually.",
                    cx,
                ))
        })
    }

    fn permission_settings_line(
        &self,
        key: &'static str,
        pane: SettingsPane,
        cx: &Context<Self>,
    ) -> Option<Div> {
        let state = self.permissions.as_ref()?.as_ref().ok()?[key].as_str()?;
        if state != "denied" {
            return None;
        }
        let detail = match pane {
            SettingsPane::Microphone => {
                microphone_instructions(self.system_settings.host_platform())
            }
            _ => system_audio_instructions(self.system_settings.host_platform()),
        };
        Some(
            row()
                .child(crate::ui::kit::status_line(
                    format!("setup-{key}-denied"),
                    crate::ui::kit::StatusKind::Attention,
                    "Denied",
                    cx,
                ))
                .child(muted(format!("setup-{key}-denied-help"), detail, cx))
                .children(self.system_settings_button(
                    format!("setup-open-{key}-settings"),
                    pane,
                    cx,
                )),
        )
    }

    fn render_recording_access(&self, cx: &mut Context<Self>) -> Div {
        use kit::StatusKind::{Attention, Done, Pending};
        let active = self.next_permission(cx);
        let running = self.agent_status(cx).is_none();
        let host = self.system_settings.host_platform();
        let mut checklist = kit::stack().items_start()
            .child(text_line("setup-intro", Role::Label,
                "BroScribe needs these to record meetings. Each one is explained before your system asks."))
            .child(self.render_agent_access(cx));
        let audio_unsupported = self.permission_state("microphone") == Some("unsupported")
            && self.permission_state("system_audio") == Some("unsupported");
        if audio_unsupported && host != HostPlatform::MacOs {
            checklist = checklist.child(text_line("setup-audio-not-needed", Role::Label, match host {
                HostPlatform::Windows => "Windows doesn't ask desktop apps for permission to record, so there's nothing to allow here.",
                _ => "Linux doesn't ask apps for permission to record, so there's nothing to allow here.",
            }));
        } else {
            for (item, key, pane) in [
                (
                    RecordingPermission::Microphone,
                    "microphone",
                    SettingsPane::Microphone,
                ),
                (
                    RecordingPermission::SystemAudio,
                    "system_audio",
                    SettingsPane::SystemAudio,
                ),
            ] {
                let state = self.permission_state(key);
                let (kind, words) = if running {
                    (permission_kind(state), permission_words(state))
                } else {
                    (Pending, "Waiting for BroScribe Agent")
                };
                checklist = checklist
                    .child(crate::ui::heading(
                        format!("setup-{key}-heading"),
                        item.title(),
                    ))
                    .child(kit::status_without_wait_clock(
                        format!("setup-{key}-state"),
                        kind,
                        words,
                        cx,
                    ))
                    .when(active == Some(item), |el| {
                        el.child(text_line(
                            format!("setup-{key}-purpose"),
                            Role::Label,
                            item.purpose(),
                        ))
                    })
                    .children(self.permission_settings_line(key, pane, cx));
            }
        }
        checklist = checklist.child(self.recording_microphone(cx));
        if running && self.permission_state("microphone") == Some("granted") {
            let checking = self.capture.is_some();
            checklist = checklist
                .child(
                    Button::new("mic-check")
                        .outline()
                        .tab_stop(true)
                        .label(if checking {
                            "Listening…"
                        } else {
                            "Test the microphone"
                        })
                        .disabled(checking)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.mic_check = None;
                            this.start_capture(CapturePurpose::MicCheck, cx);
                        })),
                )
                .children(self.render_meter(cx));
        }
        if let Some(outcome) = &self.mic_check {
            checklist =
                checklist.child(outcome_line("mic-check-result", Role::Status, outcome, cx));
            if matches!(outcome, Outcome::Err(_))
                && self.permission_state("microphone") != Some("denied")
            {
                checklist = checklist.children(self.system_settings_button(
                    "setup-mic-test-settings",
                    SettingsPane::Microphone,
                    cx,
                ));
            }
        }
        if running && let Some(Err(error)) = &self.permissions {
            checklist = checklist.child(kit::retry_line(
                "setup-permissions-error",
                error.clone(),
                cx,
                |this, _, cx| this.refresh_permissions(None, cx),
            ));
        }
        let notification = match self.notification_authorization {
            Some(NotificationAuthorization::Granted) => Some("granted"),
            Some(NotificationAuthorization::Denied) => Some("denied"),
            Some(NotificationAuthorization::NotDetermined) => Some("not_determined"),
            Some(NotificationAuthorization::Unsupported) => Some("unsupported"),
            _ => None,
        };
        checklist = checklist
            .child(crate::ui::heading(
                "setup-notifications-heading",
                RecordingPermission::Notifications.title(),
            ))
            .child(kit::status_without_wait_clock(
                "setup-notifications-state",
                permission_kind(notification),
                permission_words(notification),
                cx,
            ))
            .when(active == Some(RecordingPermission::Notifications), |el| {
                el.child(text_line(
                    "setup-notifications-purpose",
                    Role::Label,
                    RecordingPermission::Notifications.purpose(),
                ))
            });
        if notification == Some("denied") {
            let place = if host == HostPlatform::Windows {
                "notification area"
            } else {
                "menu bar"
            };
            checklist = checklist.child(kit::status_line("setup-notifications-off", Attention,
                format!("Off · BroScribe asks in the recording controls and from its icon in the {place} instead."), cx))
                .children(self.system_settings_button("setup-open-notification-settings", SettingsPane::Notifications, cx));
        }
        checklist
            .child(crate::ui::heading(
                "setup-meeting-names-heading",
                RecordingPermission::MeetingNames.title(),
            ))
            .child(kit::status_without_wait_clock(
                "setup-accessibility-state",
                if self.accessibility_trusted {
                    Done
                } else {
                    Pending
                },
                if self.accessibility_trusted {
                    "Allowed"
                } else if self.accessibility_answered {
                    "Not allowed"
                } else {
                    "Not asked yet"
                },
                cx,
            ))
            .when(active == Some(RecordingPermission::MeetingNames), |el| {
                el.child(text_line(
                    "setup-accessibility-note",
                    Role::Label,
                    RecordingPermission::MeetingNames.purpose(),
                ))
                .child(
                    Button::new("setup-skip-meeting-names")
                        .outline()
                        .label("Skip")
                        .tab_stop(true)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.accessibility_answered = true;
                            this.continue_recording_access(cx);
                        })),
                )
            })
            .children(
                (!self.accessibility_trusted && self.accessibility_answered)
                    .then(|| {
                        self.system_settings_button(
                            "setup-open-accessibility-settings",
                            SettingsPane::Accessibility,
                            cx,
                        )
                    })
                    .flatten(),
            )
    }

    fn recording_microphone(&self, cx: &Context<Self>) -> impl IntoElement {
        let pinned = self.state.read(cx).config().audio.pinned_mic.as_deref();
        let words = match &self.recording_devices {
            None => "Looking for microphones…".into(),
            Some(Err(error)) => {
                failure_text("Couldn't list the microphones.", &sentence(error), "")
            }
            Some(Ok(devices)) => active_microphone(devices, pinned),
        };
        kit::stack()
            .items_start()
            .child(muted("setup-recording-microphone", words, cx))
            .child(text_line(
                "setup-recording-mic-settings",
                Role::Label,
                "Choose a microphone to use for future recordings.",
            ))
            .child(self.settings_link(
                crate::state::SettingsPage::Recording,
                "Choose microphone…",
                cx,
            ))
    }

    fn render_step(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let content = match self.step {
            Step::Welcome | Step::Ready | Step::Overview => self.render_landing(cx),
            Step::Languages | Step::Models => crate::ui::kit::stack()
                .child(text_line(
                    "setup-intro",
                    Role::Label,
                    "What language are most of your meetings in?",
                ))
                .child(crate::ui::kit::field(
                    "setup-language-label",
                    "Meeting language",
                    Some((
                        "setup-language-note".into(),
                        "BroScribe uses it when a recording doesn't say, and for the text you read in Your voice. Choose Detect automatically to detect each recording's language."
                            .into(),
                    )),
                    |label| {
                        crate::ui::select::Picker::languages(
                            "setup-language-picker",
                            label,
                            &[],
                            &self.language.read(cx).value(),
                            cx,
                            |this, value, window, cx| {
                                this.language.update(cx, |input, cx| {
                                    input.set_value(value.to_owned(), window, cx)
                                });
                                this.language_error = None;
                                cx.notify();
                            },
                        )
                    },
                    cx,
                ))
                .children(self.language_error.as_ref().map(|e| error_line("setup-language-error", e.clone(), cx)))
                .child(self.render_models(cx))
                .into_any_element(),
            Step::Permissions => self.render_recording_access(cx).into_any_element(),
            Step::Voice => self.render_voice(cx),
            Step::AudioCheck => self.render_audio_check(cx),
            Step::Summaries => self.render_summaries(),
            Step::MeetingApps => self.render_meeting_apps(cx),
            Step::Words => self.render_words(cx),
            Step::Storage => {
                let config = self.state.read(cx).config().clone();
                let days = self.retention_days.clone();
                storage_section(
                    &config,
                    &days,
                    &self.retention,
                    cx,
                    |this: &mut Self, h: Handling, cx| {
                        let task = update_config(&this.state, move |c| c.import_handling = h, cx);
                        this.report(task, None, cx);
                    },
                    |this: &mut Self, r: Retention, trigger: RetentionTrigger, window, cx| {
                        this.request_retention(r, trigger, None, window, cx);
                    },
                    None,
                )
            }
            Step::Calendars => {
                let calendars = match &self.calendars {
                    Some(calendars) => calendars.clone(),
                    None => {
                        let state = self.state.clone();
                        let calendars = cx.new(|cx| {
                            crate::views::integrations::IntegrationsView::new(state, window, cx)
                                .in_setup()
                        });
                        self.calendars = Some(calendars.clone());
                        calendars
                    }
                };
                calendars.into_any_element()
            }
            Step::Agents => self.render_agents(cx),
            Step::Existing => crate::ui::kit::stack()
                .child(text_line(
                    "setup-intro",
                    Role::Label,
                    "Bring in recordings you already have. Choose files or a folder (you will \
                     confirm language, title, date and participants), or a manifest describing \
                     many meetings. You can also drop files anywhere on the window later.",
                ))
                .child(
                    row()
                        .child(
                            Button::new("choose-recordings")
                                .label("Choose recordings or a folder…")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.choose_recordings(cx)
                                })),
                        )
                        .child(
                            Button::new("choose-manifest")
                                .label("Import a manifest…")
                                .map(|button| {
                                    crate::ui::kit::disable_with_reason(
                                        button,
                                        self.busy,
                                        "The current setup action is still running.",
                                        cx,
                                    )
                                })
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.import_manifest(cx)
                                })),
                        ),
                )
                .into_any_element(),
            Step::TryIt => self.render_try(cx),
        };
        if let Some(outcome) = &self.legacy_outcome
            && matches!(self.step, Step::Words | Step::Storage | Step::Existing)
        {
            kit::stack()
                .child(content)
                .child(outcome_line("setup-tool-result", Role::Status, outcome, cx))
                .into_any_element()
        } else {
            content
        }
    }

    fn request_retention(
        &mut self,
        retention: Retention,
        trigger: RetentionTrigger,
        ok: Option<&'static str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let state = self.state.clone();
        request_retention_change(
            self,
            state,
            |this| &mut this.retention,
            retention,
            trigger,
            move |this, cx| {
                if let Some(text) = ok {
                    this.legacy_outcome = Some(Outcome::Ok(text.into()));
                }
                cx.notify();
            },
            window,
            cx,
        );
    }

    fn report(
        &mut self,
        task: Task<Result<(), String>>,
        ok: Option<&'static str>,
        cx: &mut Context<Self>,
    ) {
        let step = self.step;
        let generation = self.advance_generation;
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if this.step != step || this.advance_generation != generation {
                    return;
                }
                match result {
                    Err(e) if matches!(step, Step::Words | Step::Storage | Step::Existing) => {
                        this.legacy_outcome = Some(Outcome::Err(e));
                    }
                    Err(e) => this.progress_error = Some(progress_failure(&e)),
                    Ok(()) => {
                        if let Some(text) = ok {
                            this.legacy_outcome = Some(Outcome::Ok(text.into()));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_models(&self, cx: &mut Context<Self>) -> AnyElement {
        use kit::StatusKind::{Attention, Failed, Pending};
        let mut body = kit::stack().items_start();
        let Some(models) = &self.models else {
            return body
                .child(kit::status_line(
                    "setup-models-state",
                    Pending,
                    "Checking models…",
                    cx,
                ))
                .into_any_element();
        };
        let list = match models {
            Ok(list) => list,
            Err(error) => {
                return kit::retry_line(
                    "setup-models-state",
                    failure_text("Couldn't list the models.", &sentence(error), ""),
                    cx,
                    |this: &mut Self, _, cx| this.load_models(cx),
                )
                .into_any_element();
            }
        };
        let download = self.download(cx).read(cx);
        body = body.child(kit::field(
            "setup-profiles-caption", "Speech model", None,
            |_| super::settings::speech_model_choices(
                "setup-profile-choices", list, Some(self.language.read(cx).value().as_ref()).filter(|value| !value.is_empty()), self.busy || download.running(), cx,
                |this, profile, cx| this.choose_profile(profile, cx),
            ), cx,
        )).child(muted("setup-profiles-note",
            "These options use the same speaker recognition model, so switching keeps your saved voices.", cx))
            .children(self.profile_notice.as_ref().map(|notice| outcome_line("setup-profile-status", Role::Status, notice, cx)));
        let rows = match required_models(list) {
            Ok(rows) => rows,
            Err(error) => {
                return body
                    .child(error_line("setup-profile-models-error", error, cx))
                    .into_any_element();
            }
        };
        let needed = rows
            .iter()
            .filter(|row| !row.installed())
            .map(|row| row.bytes)
            .sum::<u64>();
        let free = self.machine.as_ref().and_then(|info| info.free_disk);
        let bytes = crate::ui::format::bytes;
        if self
            .machine
            .as_ref()
            .is_some_and(|m| broscribe_platform::model_download_low_memory(Some(m.memory)))
        {
            body = body.child(kit::status_line(
                "setup-machine-memory",
                Attention,
                "Transcription may be slow with this amount of memory.",
                cx,
            ));
        }
        body = body.children(self.render_download(cx));
        if needed > 0 && !download.running() {
            let space = free.map_or_else(
                || "free disk space unavailable".into(),
                |n| format!("{} free on this disk", bytes(n)),
            );
            body = body.child(text_line(
                "setup-download-size",
                Role::Label,
                format!("{} to download · {space}", bytes(needed)),
            ));
        }
        if self.low_disk(needed)
            && let Some(free) = free
        {
            body = body.child(kit::status_line(
                "setup-download-space",
                Failed,
                format!(
                    "Not enough free space. {} needed, {} free on this disk.",
                    bytes(needed),
                    bytes(free)
                ),
                cx,
            ));
        }
        for model in rows
            .iter()
            .filter(|row| !row.installed() && !row.license_verified)
        {
            body = body.child(
                row()
                    .flex_wrap()
                    .child(kit::status_line(
                        format!("setup-licence-{}", model.id),
                        Attention,
                        format!(
                            "The licence of {} isn't verified yet.",
                            list.model_name(&model.id)
                        ),
                        cx,
                    ))
                    .child(self.settings_link(
                        crate::state::SettingsPage::Transcription,
                        "Open Transcription settings",
                        cx,
                    )),
            );
        }
        body = body.child(kit::disclosure(
            "setup-model-disclosure",
            format!("Models ({})", rows.len()),
            self.models_expanded,
            cx,
            |this, _, cx| {
                this.models_expanded = !this.models_expanded;
                cx.notify();
            },
        ));
        if self.models_expanded {
            body = body.children(rows.iter().map(|model| {
                text_line(
                    format!("setup-model-{}", model.id),
                    Role::Label,
                    format!(
                        "{} · {} · {} · {}",
                        model
                            .roles
                            .iter()
                            .map(|role| role_label(role))
                            .collect::<Vec<_>>()
                            .join(", "),
                        list.model_name(&model.id),
                        bytes(model.bytes),
                        model.state_label()
                    ),
                )
            }));
        }
        let asr = &self.state.read(cx).config().profile.asr;
        let ready = list
            .rows
            .iter()
            .any(|row| &row.id == asr && row.installed());
        let testing = self.sample == TryState::Running;
        body = body.when(!testing, |body| body.child(row().child(
            Button::new("test-transcription").outline().tab_stop(true)
                .label("Test the speech model")
                .disabled(self.busy || !ready)
                .on_click(cx.listener(|this, _, _, cx| this.test_transcription(cx))),
        ).when(!ready, |el| el.child(muted("setup-sample-unavailable",
            "Available once the speech model is downloaded.", cx)))))
        .child(muted("setup-sample-note",
            "Transcribes a short example recording that came with the model. Your microphone isn't used and nothing is saved.", cx));
        match &self.sample {
            TryState::Done(Ok(text)) => {
                body = body
                    .child(kit::status_line(
                        "sample-result",
                        kit::StatusKind::Done,
                        "The speech model transcribed the example.",
                        cx,
                    ))
                    .child(transcript_box(
                        "sample-transcript",
                        "Example transcription",
                        text,
                        cx,
                    ))
            }
            TryState::Running => {
                body = body.child(
                    row()
                        .child(kit::status_line(
                            "sample-result",
                            Pending,
                            "Transcribing the example…",
                            cx,
                        ))
                        .child(
                            Button::new("sample-transcribe-cancel")
                                .outline()
                                .label("Cancel")
                                .tab_stop(true)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.cancel_sample(cx);
                                    this.sample = TryState::Cancelled;
                                    cx.notify();
                                })),
                        ),
                )
            }
            TryState::Cancelled => {
                body = body.child(kit::status_line(
                    "sample-result",
                    kit::StatusKind::Attention,
                    TEST_CANCELLED,
                    cx,
                ))
            }
            TryState::Done(Err(error)) => {
                body = body.child(kit::retry_line(
                    "sample-result",
                    failure_text(
                        "Couldn't transcribe the example audio.",
                        &sentence(error),
                        "",
                    ),
                    cx,
                    |this: &mut Self, _, cx| this.test_transcription(cx),
                ))
            }
            TryState::Idle => {}
        }
        body.into_any_element()
    }

    fn render_download(&self, cx: &Context<Self>) -> Option<Div> {
        use kit::StatusKind::{Attention, Done, Failed, Pending};
        let downloads = self.download(cx).read(cx);
        let bytes = crate::ui::format::bytes;
        let mut body = kit::stack().items_start();
        match &downloads.status {
            DownloadStatus::Idle => {
                let rows = required_models(self.models.as_ref()?.as_ref().ok()?).ok()?;
                if rows.iter().any(|model| !model.installed()) {
                    return None;
                }
                let total = rows.iter().map(|row| row.bytes).sum();
                body = body.child(kit::status_line(
                    "setup-download-state",
                    Done,
                    format!("Speech models downloaded · {}", bytes(total)),
                    cx,
                ));
            }
            DownloadStatus::Running { percent, total } => {
                let stopping = downloads.stopping();
                let current = (*total as u128 * *percent as u128 / 100) as u64;
                body = body
                    .child(
                        row()
                            .flex_wrap()
                            .child(kit::status_line(
                                "setup-download-state",
                                Pending,
                                format!(
                                    "Downloading speech models · {} of {} · {}",
                                    bytes(current),
                                    bytes(*total),
                                    crate::ui::format::percent(*percent)
                                ),
                                cx,
                            ))
                            .child(
                                Button::new("cancel-download-profile")
                                    .outline()
                                    .tab_stop(true)
                                    .label(if stopping {
                                        "Cancelling…"
                                    } else {
                                        "Cancel download"
                                    })
                                    .disabled(stopping)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.download(cx).clone().update(cx, ModelDownloads::cancel)
                                    })),
                            ),
                    )
                    .child(
                        kit::progress(
                            "setup-download",
                            "Speech model download",
                            f32::from(*percent) / 100.,
                        )
                        .max_w(gpui::rems(30.)),
                    );
            }
            DownloadStatus::Done(total) => {
                body = body.child(kit::status_line(
                    "setup-download-state",
                    Done,
                    format!("Speech models downloaded · {}", bytes(*total)),
                    cx,
                ))
            }
            DownloadStatus::Cancelled => {
                body = body.child(kit::status_line(
                    "setup-download-state",
                    Attention,
                    "Download cancelled. Models already downloaded stay installed.",
                    cx,
                ))
            }
            DownloadStatus::Failed(message) => {
                body = body.child(kit::status_line(
                    "setup-download-state",
                    Failed,
                    message.clone(),
                    cx,
                ));
                if self.step == Step::Models {
                    body = body.child(
                        Button::new("setup-download-retry")
                            .outline()
                            .label("Try again")
                            .tab_stop(true)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.download_profile(cx);
                            })),
                    );
                }
            }
        }
        Some(body)
    }

    /// The voice sample's reading language as one radio group: the loaded
    /// passage's language is selected.
    fn voice_language_choices(&self, cx: &mut Context<Self>) -> ChoiceGroup {
        let mut languages = ChoiceGroup::new(
            div().flex().items_start().gap_2().min_w_0(),
            "voice-language",
            "Reading language",
        )
        .fit_options();
        let active_language = self
            .passage
            .as_ref()
            .and_then(|p| p.as_ref().ok())
            .map(|p| p.lang.as_str());
        for (lang, label) in VOICE_LANGUAGES {
            languages = languages.option(
                choice(
                    SharedString::from(format!("voice-language-{lang}")),
                    label,
                    active_language == Some(lang),
                    cx.listener(move |this, _: &bool, _, cx| {
                        this.choose_passage_language(lang, cx);
                    }),
                )
                .disabled(!self.can_change_passage_language()),
            );
        }
        languages
    }

    fn render_voice(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let stage = self.voice_stage();
        let recording = self.capture.as_ref().map(|c| c.purpose);
        let meter = self.render_meter(cx);
        let languages = self.voice_language_choices(cx);
        let mut el = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(text_line(
                "setup-intro",
                Role::Label,
                "BroScribe learns your voice, not your pronunciation. Use the language you \
                 speak most comfortably, at your normal pace.",
            ))
            .child(muted(
                "voice-alternative",
                "Rather not read aloud? Skip this step, then name yourself in a meeting's Speakers tab. BroScribe learns your voice from that speaker.",
                cx,
            ))
            .children(self.model_wait(CapturePurpose::Passage, cx))
            .child(self.render_voice_identity(cx))
            .child(self.render_device_picker(cx))
            .child(text_line("setup-voice-language-label", Role::Label, "Reading language").text_role(TextRole::Label))
            .child(languages)
            .child(muted(
                "voice-language-note",
                "This changes only the reading, not your meeting language.",
                cx,
            ));
        let mut passage = None;
        match &self.passage {
            None => {
                el = el.child(crate::ui::kit::status_line(
                    "passage-state",
                    crate::ui::kit::StatusKind::Pending,
                    "Loading the passage…",
                    cx,
                ))
            }
            Some(Err(e)) => {
                el = el.child(crate::ui::kit::retry_line(
                    "passage-state",
                    failure_text("Couldn't load the passage.", &sentence(e), ""),
                    cx,
                    |this, _, cx| this.load_passage(cx),
                ))
            }
            Some(Ok(p)) => {
                let phrase = stage.shows_phrase();
                let (label, instruction, text, aria) = if phrase {
                    (
                        PHRASE_LABEL,
                        phrase_instruction(),
                        p.confirmation.clone(),
                        format!(
                            "Confirmation reading ({})",
                            crate::ui::format::language_name(&p.lang)
                        ),
                    )
                } else {
                    (
                        FIRST_RECORDING_LABEL,
                        FIRST_RECORDING_INSTRUCTION.to_owned(),
                        p.text.clone(),
                        format!(
                            "Passage to read ({})",
                            crate::ui::format::language_name(&p.lang)
                        ),
                    )
                };
                el = el
                    .child(
                        text_line("passage-label", Role::Label, label)
                            .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(muted("passage-instruction", instruction, cx));
                passage = Some(
                    kit::inset_box("passage", cx)
                        .role(Role::Article)
                        .aria_label(aria)
                        .max_w(gpui::rems(36.))
                        .text_role(TextRole::Reading)
                        .child(text_line("passage-text", Role::Label, text)),
                );
            }
        }
        let passage_ready =
            matches!(self.passage, Some(Ok(_))) && self.models_ready(CapturePurpose::Passage, cx);
        let mut buttons = row();
        match stage {
            VoiceStage::Recording(_) => {
                buttons = buttons
                    .child(
                        Button::new("voice-stop")
                            .map(|button| kit::primary(button, false, cx))
                            .label("Done reading")
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.stop_capture(cx)),
                            ),
                    )
                    .child(
                        Button::new("voice-cancel-capture")
                            .outline()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.cancel_capture(cx);
                                this.discard_recordings(cx);
                                cx.notify();
                            })),
                    );
            }
            VoiceStage::Processing => {
                buttons = buttons
                    .child(kit::status_line(
                        "voice-processing",
                        kit::StatusKind::Pending,
                        "Analysing your voice…",
                        cx,
                    ))
                    .child(Button::new("voice-cancel").label("Cancel").on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.cancel_voice(cx)),
                    ));
            }
            VoiceStage::Checking => {
                buttons =
                    buttons
                        .child(kit::status_line(
                            "voice-checking",
                            kit::StatusKind::Pending,
                            "Checking the first reading…",
                            cx,
                        ))
                        .child(Button::new("voice-check-cancel").label("Cancel").on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.discard_recordings(cx);
                                cx.notify();
                            }),
                        ))
                        .child(
                            Button::new("voice-check-restart")
                                .disabled(!self.models_ready(CapturePurpose::Passage, cx))
                                .label("Record again")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.start_reading(cx)
                                })),
                        );
            }
            VoiceStage::Phrase
                if matches!(self.voice_outcome, Some(VoiceOutcome::Unconfirmed(_))) =>
            {
                // Inconclusive phrase check: recording 1 is held and nothing
                // is saved until one of these explicit choices.
                buttons = buttons
                    .child(
                        Button::new("voice-retry-confirm")
                            .disabled(!self.models_ready(CapturePurpose::Passage, cx))
                            .map(|button| {
                                kit::primary(
                                    button,
                                    !self.models_ready(CapturePurpose::Passage, cx),
                                    cx,
                                )
                            })
                            .label("Read confirmation again")
                            .on_click(
                                cx.listener(|this, _: &ClickEvent, _, cx| this.record_phrase(cx)),
                            ),
                    )
                    .child(
                        Button::new("voice-save-unconfirmed")
                            .outline()
                            .label("Save first reading without confirmation")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.submit_recordings(None, cx)
                            })),
                    )
                    .child(
                        Button::new("voice-discard-unconfirmed")
                            .outline()
                            .disabled(!self.models_ready(CapturePurpose::Passage, cx))
                            .label("Discard and read again")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.discard_and_read_again(cx)
                            })),
                    );
            }
            VoiceStage::Phrase => {
                buttons = if self.confirmation_clip.is_some() {
                    // Both recordings are kept while the fields are fixed.
                    buttons.child(
                        Button::new("voice-submit")
                            .label("Submit the recordings")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                let confirmation = this.confirmation_clip.take();
                                this.submit_recordings(confirmation, cx);
                            })),
                    )
                } else {
                    buttons
                        .child(
                            Button::new("voice-confirm")
                                .map(|button| {
                                    kit::primary(
                                        button,
                                        !self.models_ready(CapturePurpose::Passage, cx),
                                        cx,
                                    )
                                })
                                .disabled(!self.models_ready(CapturePurpose::Passage, cx))
                                .label("Record the confirmation reading")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.record_phrase(cx)
                                })),
                        )
                        .child(
                            Button::new("voice-skip-confirm")
                                .outline()
                                .label("Skip the confirmation reading")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.submit_recordings(None, cx)
                                })),
                        )
                };
                buttons = buttons.child(
                    Button::new("voice-restart")
                        .disabled(!self.models_ready(CapturePurpose::Passage, cx))
                        .label("Read again")
                        .on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| this.start_reading(cx)),
                        ),
                );
            }
            VoiceStage::Start => {
                let redo = self.voice_saved == Some(true)
                    || matches!(
                        self.voice_outcome,
                        Some(VoiceOutcome::Enrolled { .. } | VoiceOutcome::CompletedBeforeCancel)
                    );
                buttons = buttons.child(
                    Button::new("voice-record")
                        .map(|button| {
                            kit::primary(
                                button,
                                !passage_ready || recording.is_some() || self.advancing,
                                cx,
                            )
                        })
                        .label(if redo {
                            "Record your voice again"
                        } else {
                            "Start reading"
                        })
                        .disabled(!passage_ready || recording.is_some() || self.advancing)
                        .when(!passage_ready || recording.is_some(), |button| {
                            button.tooltip(if !passage_ready {
                                "Wait for the reading passage to load."
                            } else {
                                "The microphone is already recording."
                            })
                        })
                        .on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| this.start_reading(cx)),
                        ),
                );
            }
        }
        el = el
            .child(buttons.id("voice-reading-actions"))
            .children(meter)
            .children(
                self.capture_error
                    .as_ref()
                    .map(|e| error_line("voice-capture-error", e.clone(), cx)),
            );
        if let Some(feedback) = &self.reading_feedback
            && matches!(stage, VoiceStage::Start | VoiceStage::Phrase)
        {
            for (i, (error, line)) in reading_feedback_lines(feedback).into_iter().enumerate() {
                el = el.child(if error {
                    error_line(("voice-feedback", i), line, cx)
                } else {
                    crate::ui::kit::status_line(
                        ("voice-feedback", i),
                        crate::ui::kit::StatusKind::Attention,
                        line,
                        cx,
                    )
                });
            }
        }
        match &self.voice_outcome {
            Some(VoiceOutcome::Enrolled {
                kept,
                warnings,
                confirmation,
                ..
            }) => {
                el = el
                    .child(kit::status_line(
                        "voice-outcome",
                        kit::StatusKind::Done,
                        "Your voice is saved.",
                        cx,
                    ))
                    .when(*kept, |el| {
                        el.child(muted(
                            "voice-kept",
                            "Your first reading is kept with your voice.",
                            cx,
                        ))
                        .child(self.settings_link(
                            SettingsPage::Transcription,
                            "Open Transcription settings",
                            cx,
                        ))
                    })
                    .child(muted(
                        "voice-confirmation",
                        enrolled_confirmation_line(confirmation.as_ref()),
                        cx,
                    ));
                if let Some((visual, spoken)) =
                    confirmation.as_ref().and_then(PhraseComparison::details)
                {
                    el = el
                        .child(kit::disclosure(
                            "voice-details",
                            "Details",
                            self.voice_details,
                            cx,
                            |this, _, cx| {
                                this.voice_details = !this.voice_details;
                                cx.notify();
                            },
                        ))
                        .when(self.voice_details, |el| {
                            el.child(muted("voice-comparison", visual, cx).aria_label(spoken))
                        });
                }
                for (i, w) in warnings.iter().enumerate() {
                    el = el.child(crate::ui::kit::status_line(
                        ("voice-warning", i),
                        crate::ui::kit::StatusKind::Attention,
                        warning_text(w),
                        cx,
                    ));
                }
            }
            Some(VoiceOutcome::Unconfirmed(u)) => {
                for (i, (error, line)) in unconfirmed_lines(u).into_iter().enumerate() {
                    el = el.child(kit::status_line(
                        ("voice-unconfirmed", i),
                        if error {
                            kit::StatusKind::Failed
                        } else {
                            kit::StatusKind::Attention
                        },
                        line,
                        cx,
                    ));
                }
                if let ConfirmationIssue::BelowThreshold(comparison) = &u.issue
                    && let Some((visual, spoken)) = comparison.details()
                {
                    el = el.child(muted("voice-comparison", visual, cx).aria_label(spoken));
                }
            }
            Some(VoiceOutcome::Cancelled) => {
                el = el.child(kit::status_line(
                    "voice-outcome",
                    kit::StatusKind::Attention,
                    TEST_CANCELLED,
                    cx,
                ));
            }
            Some(VoiceOutcome::CompletedBeforeCancel) => {
                el = el.child(kit::status_line(
                    "voice-outcome",
                    kit::StatusKind::Done,
                    "Your voice had already been saved before it could be cancelled.",
                    cx,
                ));
            }
            Some(VoiceOutcome::Failed(e)) => {
                el = el.child(error_line(
                    "voice-outcome",
                    failure_text("Couldn't save your voice.", &sentence(e), ""),
                    cx,
                ));
            }
            None => {}
        }
        el.children(passage).into_any_element()
    }

    fn render_voice_identity(&self, cx: &Context<Self>) -> Div {
        let locked = self.advancing || self.voice_job.is_some();
        let name_error = match &self.identity_error {
            Some(IdentityError::Name(error)) => Some(error),
            _ => None,
        };
        let email_error = match &self.identity_error {
            Some(IdentityError::Email(error)) => Some(error),
            _ => None,
        };
        kit::stack().gap_1()
            .child(kit::field(
                "voice-name-label", "Your name",
                Some(("voice-name-help".into(), "Shown for Me in transcripts and summaries.".into())),
                |label| div().w_full().max_w(gpui::rems(20.)).child(Input::new(&self.name).aria_label(label).disabled(locked)),
                cx,
            ).children(name_error.map(|e| error_line("voice-name-error", e.clone(), cx))))
            .child(kit::field(
                "voice-emails-label", "Email addresses",
                Some(("voice-emails-help".into(),
                    "Optional. Separate them with commas. Helps BroScribe recognise you in calendar attendees and imported participant lists.".into())),
                |label| div().w_full().max_w(gpui::rems(28.)).child(Input::new(&self.emails).aria_label(label).disabled(locked)),
                cx,
            ).children(email_error.map(|e| error_line("voice-emails-error", e.clone(), cx))))
            .children(self.identity_save_error.as_ref().map(|e| error_line("voice-identity-save", e.clone(), cx)))
    }

    fn render_words(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let config = self.state.read(cx).config();
        let saved = config.hotwords.len();
        let note = match &self.models {
            Some(Ok(list)) => hotword_note(list, &config.profile.asr),
            _ => None,
        };
        let pending = self.words_count;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(text_line(
                "setup-intro",
                Role::Label,
                "Words BroScribe should recognise (names, products, jargon) help \
                 transcription models that accept hints.",
            ))
            .child(muted(
                "words-saved",
                format!("{saved} {} saved.", plural(saved as u64, "word", "words")),
                cx,
            ))
            .when_some(note, |el, note| {
                el.child(
                    div()
                        .id("words-model-note")
                        .spoken(Role::Note, note.text())
                        .debug_selector(|| "words-model-note".into())
                        .text_role(TextRole::Caption)
                        .child(note.text()),
                )
            })
            .child(crate::ui::kit::field(
                "setup-terms-hint",
                "One word per line",
                None,
                |label| {
                    Textarea::new(&self.words)
                        .h(gpui::rems(7.5))
                        .aria_label(label)
                },
                cx,
            ))
            .child(muted(
                "words-pending",
                format!(
                    "{pending} {} to add",
                    plural(pending as u64, "word", "words")
                ),
                cx,
            ))
            .child(
                Button::new("save-words")
                    .label("Add words")
                    .disabled(self.busy || pending == 0)
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.save_words(window, cx)),
                    ),
            )
            .into_any_element()
    }

    fn render_agents(&mut self, _cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(text_line(
                "setup-intro",
                Role::Label,
                "AI tools (Claude Code, Codex, Gemini CLI…) can search your meetings with \
                 BroScribe: through its command-line skill, or through its MCP server.",
            ))
            .child(self.agents.clone())
            .into_any_element()
    }

    fn render_summaries(&mut self) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(text_line(
                "setup-intro",
                Role::Label,
                "Summaries, decisions and action items are written by an AI provider you turn on. \
                 BroScribe sends it the meeting transcript only. Audio and voice data stay on this computer. \
                 Every provider is off until you turn it on and agree to its terms.",
            ))
            .child(self.providers.clone())
            .into_any_element()
    }

    fn render_meeting_apps(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mut el = div().flex().flex_col().gap_2().child(text_line(
            "setup-intro",
            Role::Label,
            "BroScribe notices when a supported meeting app starts a call. Choose what happens for each app.",
        ));
        match &self.meeting_apps {
            None => {
                el = el.child(crate::ui::kit::status_line(
                    "meeting-apps-state",
                    crate::ui::kit::StatusKind::Pending,
                    "Reading meeting apps…",
                    cx,
                ))
            }
            Some(Err(e)) => {
                el = el.child(crate::ui::kit::retry_line(
                    "meeting-apps-state",
                    failure_text("Couldn't load the meeting apps.", &sentence(e), ""),
                    cx,
                    |this, _, cx| {
                        this.meeting_apps = None;
                        this.load_meeting_apps(cx);
                        cx.notify();
                    },
                ))
            }
            Some(Ok(apps)) => {
                if let Some(notice) = meeting_apps_notice(apps, self.detection_off) {
                    el = el
                        .child(kit::status_line(
                            "meeting-apps-notice",
                            kit::StatusKind::Attention,
                            notice,
                            cx,
                        ))
                        .when(self.detection_off, |el| {
                            el.child(self.settings_link(
                                SettingsPage::Recording,
                                "Recording settings",
                                cx,
                            ))
                        });
                }
                for app in apps.clone() {
                    el = el.child(self.detect_choices(&app, cx)).children(
                        self.policy_errors
                            .get(&app.id)
                            .map(|e| error_line(format!("detect-error-{}", app.id), e.clone(), cx)),
                    );
                }
            }
        }
        el.into_any_element()
    }

    /// One meeting app's detection policy (auto, ask or off) as one radio
    /// group named after the app.
    fn detect_choices(&self, app: &MeetingApp, cx: &mut Context<Self>) -> ChoiceGroup {
        let mut line = ChoiceGroup::new(
            row().child(div().w(gpui::rems(10.)).child(app.name.clone())),
            SharedString::from(format!("detect-{}", app.id)),
            format!("When {} starts a call", app.name),
        );
        for (policy, label) in
            DETECT_POLICIES
                .into_iter()
                .zip(["Record automatically", "Ask first", "Don't record"])
        {
            let id = app.id.clone();
            line = line.option(
                choice(
                    SharedString::from(format!("detect-{}-{policy}", app.id)),
                    label,
                    app.policy.as_deref() == Some(policy),
                    cx.listener(move |this, _: &bool, _, cx| {
                        this.set_detect_policy(id.clone(), policy, cx)
                    }),
                )
                .disabled(self.busy),
            );
        }
        line
    }

    fn render_try(&mut self, cx: &mut Context<Self>) -> AnyElement {
        use kit::StatusKind::{Attention, Done, Pending};
        let recording = self
            .capture
            .as_ref()
            .is_some_and(|capture| capture.purpose == CapturePurpose::TryIt);
        let running = self.try_state == TryState::Running;
        let mut el = kit::stack()
            .w_full()
            .child(text_line(
                "try-intro",
                Role::Label,
                "Say a few sentences, as you would in a meeting. BroScribe transcribes them on this computer and should label you as Me. The test is deleted afterwards unless you save it.",
            ))
            .when(!recording && !running && !matches!(self.try_state, TryState::Done(Ok(_))) && self.voice_saved == Some(false), |el| {
                el.child(self.try_voice_status("try-no-voice", "Your voice isn't set up, so this test can't label you as Me.", cx))
            })
            .children(self.model_wait(CapturePurpose::TryIt, cx))
            .child(self.render_device_picker(cx))
            .child(
                gpui::component::checkbox::Checkbox::new("try-save")
                    .label("Keep as a meeting")
                    .checked(self.save_try_as_meeting)
                    .disabled(recording || running)
                    .on_change(cx.listener(|this, checked, _, cx| {
                        this.save_try_as_meeting = *checked;
                        cx.notify();
                    })),
            );
        if recording {
            el = el
                .child(
                    row()
                        .child(
                            Button::new("try-stop")
                                .map(|button| kit::primary(button, false, cx))
                                .icon(crate::ui::icons::STOP)
                                .label("Stop")
                                .on_click(cx.listener(|this, _, _, cx| this.stop_capture(cx))),
                        )
                        .child(
                            Button::new("try-cancel")
                                .outline()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.cancel_capture(cx);
                                    this.try_state = TryState::Cancelled;
                                    cx.notify();
                                })),
                        ),
                )
                .children(self.render_meter(cx));
        } else if !running {
            let models_ready = self.models_ready(CapturePurpose::TryIt, cx);
            let words = "Start test recording";
            el = el.child(
                kit::disable_with_reason(
                    Button::new("try-record")
                        .map(|button| kit::primary(button, !models_ready, cx))
                        .accessibility_label(words)
                        .child(kit::record_dot(true, !models_ready, cx))
                        .child(words)
                        .self_start(),
                    !models_ready,
                    "Download the speech models first.",
                    cx,
                )
                .on_click(cx.listener(|this, _, _, cx| this.start_try_recording(cx))),
            );
        }
        el = el.children(
            self.capture_error
                .as_ref()
                .map(|e| error_line("try-capture-error", e.clone(), cx)),
        );
        match &self.try_state {
            TryState::Idle => {}
            TryState::Running => {
                let can_cancel = self.try_job.is_some();
                el = el.child(
                    row()
                        .child(kit::status_line(
                            "try-result",
                            Pending,
                            if can_cancel {
                                "Transcribing the test…"
                            } else {
                                "Cancelling the test…"
                            },
                            cx,
                        ))
                        .when(can_cancel, |el| {
                            el.child(
                                Button::new("try-transcribe-cancel")
                                    .outline()
                                    .label("Cancel")
                                    .tab_stop(true)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.stop_try_transcription(cx)
                                    })),
                            )
                        }),
                );
            }
            TryState::Cancelled => {
                el = el.child(kit::status_line(
                    "try-result",
                    Attention,
                    TEST_CANCELLED,
                    cx,
                ));
            }
            TryState::Done(Ok(result)) => {
                el = if result.saved_before_cancel {
                    el
                } else if result.text.trim().is_empty() {
                    el.child(kit::status_line("try-result", Attention,
                        "No speech was recognised. Speak a little closer to the microphone and try again.", cx))
                } else {
                    let status = if result.me == TryMe::Named {
                        kit::status_line("try-result", Done, "Recognised you as Me.", cx)
                            .into_any_element()
                    } else {
                        self.try_voice_status(
                            "try-result",
                            "This test wasn't labelled as Me. Recording your voice again can help.",
                            cx,
                        )
                        .into_any_element()
                    };
                    el.child(status).child(transcript_box(
                        "try-transcript",
                        "Transcript",
                        &result.text,
                        cx,
                    ))
                };
                if let Some(id) = result.meeting.clone() {
                    el = el.child(
                        row()
                            .child(kit::status_line(
                                "try-saved",
                                Done,
                                if result.saved_before_cancel {
                                    "The test was saved before it could be cancelled."
                                } else {
                                    "Saved as a meeting."
                                },
                                cx,
                            ))
                            .child(
                                Button::new("try-open-meeting")
                                    .outline()
                                    .label("Open meeting")
                                    .tab_stop(true)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.state.update(cx, |state, cx| {
                                            state.navigate(
                                                Route::Library {
                                                    selected: Some(id.clone()),
                                                },
                                                cx,
                                            );
                                            cx.emit(AppEvent::OpenMainWindow);
                                        });
                                    })),
                            ),
                    );
                }
            }
            TryState::Done(Err(e)) => {
                el = el.child(kit::retry_line(
                    "try-result",
                    try_failure(&sentence(e)),
                    cx,
                    |this, _, cx| this.start_try_recording(cx),
                ));
            }
        }
        el.into_any_element()
    }

    fn start_try_recording(&mut self, cx: &mut Context<Self>) {
        self.try_state = TryState::Idle;
        self.start_capture(CapturePurpose::TryIt, cx);
    }

    fn try_voice_status(&self, id: &'static str, text: &'static str, cx: &Context<Self>) -> Div {
        row()
            .child(kit::status_line(id, kit::StatusKind::Attention, text, cx))
            .child(self.step_link(Step::Voice, "Go to Your voice", cx))
    }

    fn step_name(&self, step: Step, cx: &App) -> String {
        let state = if step.completed(self.state.read(cx).config()) {
            "done"
        } else {
            "not done"
        };
        let current = if step == self.step {
            ", current step"
        } else {
            ""
        };
        format!("{}, {state}{current}", step.title())
    }

    fn render_nav(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut list = kit::stack()
            .id("setup-steps-list")
            .role(Role::List)
            .gap_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.steps_scroll);
        for (i, step) in Step::DISPLAYED.into_iter().enumerate() {
            if i == 0 || i == ESSENTIAL_SETUP_COUNT {
                list = list.child(
                    text_line(
                        ("setup-group", i),
                        Role::Heading,
                        if i == 0 { "Essentials" } else { "Recommended" },
                    )
                    .text_role(TextRole::Caption)
                    .text_color(theme.muted_foreground)
                    .px_3(),
                );
            }
            let done = step.completed(self.state.read(cx).config());
            let focused = self.nav_focus.is_focused(window) && i == self.nav_cursor;
            list = list.child(
                kit::sidebar_item(
                    div()
                        .id(SharedString::from(format!("setup-step-{}", step.id())))
                        .w_full(),
                    step == self.step,
                    focused,
                    cx,
                )
                .role(Role::ListItem)
                .aria_selected(step == self.step)
                .when(step == self.step, |row| {
                    row.font_weight(FontWeight::SEMIBOLD)
                })
                .aria_label(self.step_name(step, cx))
                .aria_position_in_set(i + 1)
                .aria_size_of_set(Step::DISPLAYED.len())
                .min_h(gpui::rems(2.))
                .px_3()
                .py_1()
                .flex()
                .items_center()
                .gap_2()
                .child(kit::glyph(
                    if done {
                        assets::STATUS_DONE
                    } else {
                        assets::STATUS_PENDING
                    },
                    if done {
                        theme.success
                    } else {
                        theme.muted_foreground
                    },
                ))
                .child(div().min_w_0().flex_1().child(step.title()))
                .when_some(
                    (step == Step::Models)
                        .then(|| self.download(cx).read(cx).percent())
                        .flatten(),
                    |el, percent| {
                        el.child(
                            div()
                                .text_role(TextRole::Caption)
                                .child(crate::ui::format::percent(percent)),
                        )
                    },
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.nav_cursor = i;
                    this.nav_focus.focus(window, cx);
                    this.request_departure(Departure::Step(step), window, cx);
                })),
            );
        }
        div()
            .id("setup-steps")
            .role(Role::Navigation)
            .aria_label("Setup steps")
            .track_focus(&self.nav_focus.clone().tab_stop(true))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                let last = Step::DISPLAYED.len() - 1;
                match event.keystroke.key.as_str() {
                    "up" => this.nav_cursor = this.nav_cursor.saturating_sub(1),
                    "down" => this.nav_cursor = (this.nav_cursor + 1).min(last),
                    "home" => this.nav_cursor = 0,
                    "end" => this.nav_cursor = last,
                    "enter" | "space" => this.request_departure(
                        Departure::Step(Step::DISPLAYED[this.nav_cursor]),
                        window,
                        cx,
                    ),
                    _ => return,
                }
                let index = this.nav_cursor
                    + if this.nav_cursor < ESSENTIAL_SETUP_COUNT {
                        1
                    } else {
                        2
                    };
                this.steps_scroll.scroll_to_item(index);
                cx.stop_propagation();
                cx.notify();
            }))
            .flex()
            .flex_col()
            .flex_none()
            .w(gpui::rems(12.5))
            .p_2()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(list)
    }

    fn all_steps(&self, cx: &Context<Self>) -> impl IntoElement {
        let view = cx.weak_entity();
        let rows = Step::DISPLAYED.map(|step| (step, self.step_name(step, cx)));
        Button::new("setup-all-steps")
            .label("All steps")
            .outline()
            .track_focus(&self.all_steps_focus)
            .tab_stop(true)
            .dropdown_menu(move |menu, _, _| {
                rows.iter().fold(
                    menu.item(PopupMenuItem::label("Setup steps")),
                    |menu, (step, label)| {
                        let step = *step;
                        let view = view.clone();
                        menu.item(PopupMenuItem::new(label.clone()).on_click(
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.request_departure(Departure::Step(step), window, cx)
                                });
                            },
                        ))
                    },
                )
            })
    }

    fn render_landing(&self, cx: &mut Context<Self>) -> AnyElement {
        use kit::StatusKind::{Attention, Done, Pending};
        let mut body = kit::stack();
        if self.step == Step::Welcome {
            return body.children([
                ("setup-welcome-recording", "BroScribe records each meeting as two tracks, your microphone (Me) and your computer's sound (Others), and transcribes them on this computer."),
                ("setup-welcome-voice", "It learns your voice so your lines are labelled Me, and recognises other people after you name them once."),
                ("setup-welcome-privacy", "Recordings and transcripts stay on this computer. A transcript goes to an AI provider only if you turn one on. Audio and voice data always stay local."),
                ("setup-welcome-download", "Setup takes a few minutes. The speech models download while you continue."),
            ].map(|(id, text)| text_line(id, Role::Label, text))).into_any_element();
        }
        let config = self.state.read(cx).config().clone();
        if self.step == Step::Ready {
            let running = self.agent_status(cx).is_none();
            let voice = Step::Voice.completed(&config);
            let download = self.render_download(cx);
            body = body
                .children(download.or_else(|| {
                    Some(kit::stack().child(kit::status_without_wait_clock(
                        "setup-ready-models",
                        Pending,
                        "Speech models · not downloaded",
                        cx,
                    )))
                }))
                .child(kit::status_line(
                    "setup-ready-agent",
                    if running { Done } else { Attention },
                    if running {
                        "BroScribe Agent · running"
                    } else {
                        "BroScribe Agent · not running"
                    },
                    cx,
                ))
                .child(
                    row()
                        .child(kit::status_line(
                            "setup-ready-voice",
                            if voice { Done } else { Attention },
                            if voice {
                                "Your voice · saved"
                            } else {
                                "Your voice · not set up"
                            },
                            cx,
                        ))
                        .when(!voice, |el| {
                            el.child(self.step_link(Step::Voice, "Set up", cx))
                        }),
                )
                .when(Step::TryIt.completed(&config), |el| {
                    el.child(kit::status_line(
                        "setup-ready-test",
                        Done,
                        if matches!(&self.try_state, TryState::Done(Ok(result)) if result.me == TryMe::Named) {
                            "Test · recognised you as Me"
                        } else {
                            "Test · completed"
                        },
                        cx,
                    ))
                });
        }
        let recommendations = &Step::DISPLAYED[ESSENTIAL_SETUP_COUNT..];
        let listed = if self.step == Step::Ready {
            recommendations
        } else {
            &Step::DISPLAYED
        };
        if self.step == Step::Ready {
            body = body.child(crate::ui::heading("setup-ready-recommended", "Recommended"));
        }
        for &step in listed {
            let completed = step.completed(&config);
            body = body.child(
                row()
                    .flex_wrap()
                    .child(kit::status_without_wait_clock(
                        format!("setup-overview-{}", step.id()),
                        if completed { Done } else { Pending },
                        format!(
                            "{} · {}",
                            step.title(),
                            if completed { "done" } else { "not done" }
                        ),
                        cx,
                    ))
                    .child(self.step_link(step, if completed { "Redo" } else { "Set up" }, cx)),
            );
        }
        if self.step == Step::Ready {
            use broscribe_platform::StatusIconPlace::*;
            let place = crate::ui::status_icon_place(cx);
            let start = match place {
                MenuBar => {
                    "Start a recording with Start recording in BroScribe's window, or from the BroScribe icon in the menu bar."
                }
                NotificationArea => {
                    "Start a recording with Start recording in BroScribe's window, or from the BroScribe icon in the notification area."
                }
                SystemTray => {
                    "Start a recording with Start recording in BroScribe's window, or from the BroScribe icon in the system tray."
                }
                NoTrayHost => {
                    "Start a recording with Start recording in BroScribe's window. Your desktop doesn't show tray icons."
                }
            };
            body = body.child(text_line("setup-ready-record", Role::Label, start));
            if self.system_settings.host_platform() == HostPlatform::Windows {
                body = body
                    .child(text_line("setup-tray-tip", Role::Note, crate::ui::TRAY_TIP))
                    .children(self.system_settings_button(
                        "setup-open-taskbar-settings",
                        SettingsPane::Taskbar,
                        cx,
                    ));
            }
            for (key, track) in [
                ("microphone", "Microphone"),
                ("system_audio", "System audio"),
            ] {
                if self.permission_state(key) == Some("denied") {
                    body = body.child(kit::status_line(
                        format!("setup-ready-{key}"),
                        Attention,
                        format!("{track} · denied"),
                        cx,
                    ));
                }
            }
            if !self.detection_off {
                let policy = self
                    .meeting_apps
                    .as_ref()
                    .and_then(|apps| apps.as_ref().ok())
                    .and_then(|apps| {
                        ["auto", "ask"].into_iter().find(|policy| {
                            apps.iter()
                                .any(|app| app.policy.as_deref() == Some(*policy))
                        })
                    });
                if let Some(policy) = policy {
                    body = body.child(text_line(
                        "setup-ready-detection",
                        Role::Label,
                        if policy == "auto" {
                            "BroScribe also starts recording when a meeting app joins a call."
                        } else {
                            "BroScribe also asks to record when a meeting app joins a call."
                        },
                    ));
                }
            }
        } else {
            use crate::state::SettingsPage::*;
            body = body.child(
                Button::new("setup-import-recordings")
                    .label("Import recordings…")
                    .outline()
                    .on_click(cx.listener(|this, _, _, cx| this.choose_recordings(cx))),
            );
            body = body.child(muted("setup-overview-settings", "Also in Settings", cx));
            body = body.child(
                row().flex_wrap().children(
                    [
                        (Transcription, "Add words…"),
                        (Storage, "Open Storage settings"),
                        (Calendars, "Connect an account…"),
                        (AiTools, "Open AI tools settings"),
                    ]
                    .map(|(page, label)| self.settings_link(page, label, cx)),
                ),
            );
        }
        body.into_any_element()
    }

    fn settings_link(&self, page: SettingsPage, label: &'static str, cx: &Context<Self>) -> Div {
        kit::stack()
            .items_start()
            .child(
                Button::new(label)
                    .label(label)
                    .outline()
                    .tab_stop(true)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let result = match label {
                            "Add words…" => crate::ui::settings_window::show_word_list(
                                &this.state,
                                &this.deps,
                                cx,
                            ),
                            "Open Transcription settings" if this.step == Step::Voice => {
                                crate::ui::settings_window::show_your_voice(
                                    &this.state,
                                    &this.deps,
                                    cx,
                                )
                            }
                            _ => crate::ui::settings_window::show(
                                &this.state,
                                &this.deps,
                                Some(page),
                                cx,
                            ),
                        };
                        this.settings_error = result.err().map(|_| {
                            tracing::warn!("opening setup's Settings link failed");
                            label.into()
                        });
                        cx.notify();
                    })),
            )
            .children(self.settings_error_line(label, "Couldn't open Settings. Try again.", cx))
    }

    fn settings_error_line(
        &self,
        id: impl Into<ElementId>,
        text: &'static str,
        cx: &App,
    ) -> Option<Stateful<Div>> {
        (self.settings_error.as_ref() == Some(&id.into()))
            .then(|| error_line("setup-settings-error", text, cx))
    }

    fn step_link(&self, step: Step, label: &'static str, cx: &Context<Self>) -> Button {
        Button::new(format!("setup-open-step-{}", step.id()))
            .label(label)
            .outline()
            .tab_stop(true)
            .when(matches!(label, "Redo" | "Set up"), |button| {
                button.accessibility_label(format!("{label} {}", step.title()))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.request_departure(Departure::Step(step), window, cx);
            }))
    }

    fn is_narrow(window: &Window) -> bool {
        window.viewport_size().width < window.rem_size() * 45.
    }

    fn skip_primary(&self, cx: &App) -> bool {
        !self.step.completed(self.state.read(cx).config())
            && match self.step {
                Step::Voice => {
                    !self.identity_dirty(cx)
                        && !matches!(
                            self.voice_outcome,
                            Some(
                                VoiceOutcome::Enrolled { .. } | VoiceOutcome::CompletedBeforeCancel
                            )
                        )
                }
                Step::TryIt => {
                    !matches!(&self.try_state, TryState::Done(Ok(result)) if !result.text.trim().is_empty())
                }
                Step::AudioCheck => !matches!(self.audio_check, Some(Ok(_))),
                _ => false,
            }
    }

    fn wants_download(&self, cx: &App) -> bool {
        self.step == Step::Models
            && !self.download(cx).read(cx).running()
            && self
                .models
                .as_ref()
                .and_then(|list| required_models(list.as_ref().ok()?).ok())
                .is_some_and(|rows| rows.iter().any(|row| !row.installed()))
    }

    fn primary_enabled(&self, cx: &App) -> bool {
        let model_can_continue = self.step != Step::Models
            || !self.busy
                && (self.download(cx).read(cx).running()
                    || self
                        .models
                        .as_ref()
                        .and_then(|list| required_models(list.as_ref().ok()?).ok())
                        .is_some_and(|rows| {
                            let missing = rows.iter().filter(|row| !row.installed());
                            !self.low_disk(missing.clone().map(|row| row.bytes).sum())
                                && missing.clone().all(|row| row.license_verified)
                        }));
        model_can_continue
            && (self.step != Step::Permissions || self.recording_access_can_continue(cx))
            && !self.advancing
            && !(self.step == Step::AudioCheck && (self.busy || self.audio_checking))
    }

    fn activate_primary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.primary_enabled(cx) || self.discard.is_some() {
            return;
        }
        self.continue_focus.focus(window, cx);
        if self.skip_primary(cx) || self.has_unsaved_capture() {
            self.request_departure(Departure::Step(self.step.next()), window, cx);
        } else if self.step.completed(self.state.read(cx).config())
            && self.step == Step::AudioCheck
            && !matches!(self.audio_check, Some(Ok(_)))
        {
            self.skip(cx);
        } else {
            self.continue_step(cx);
        }
    }

    fn previous_step(&self, cx: &App) -> Option<Step> {
        self.step.prev().filter(|step| {
            *step != Step::Welcome
                || self
                    .state
                    .read(cx)
                    .config()
                    .setup
                    .completed_steps
                    .is_empty()
        })
    }

    fn back_disabled(&self) -> bool {
        self.discard.is_some() || (self.step == Step::AudioCheck && self.busy)
    }

    fn activate_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.back_disabled()
            && let Some(step) = self.previous_step(cx)
        {
            if step == Step::Welcome {
                self.heading_focus.focus(window, cx);
            } else {
                self.back_focus.focus(window, cx);
            }
            self.request_departure(Departure::Step(step), window, cx);
        }
    }

    fn has_unsaved_capture(&self) -> bool {
        self.capture.is_some()
            || self.passage_clip.is_some()
            || self.confirmation_clip.is_some()
            || self.reading_check.is_some()
            || self.voice_job.is_some()
            || self.try_job.is_some()
    }

    fn request_departure(
        &mut self,
        destination: Departure,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.discard.is_some()
            || matches!(destination, Departure::Step(step) if step == self.step)
            || (self.step == Step::AudioCheck && self.busy)
        {
            return;
        }
        if self.has_unsaved_capture() {
            self.discard = Some(DiscardConfirmation {
                destination,
                opener: window.focused(cx),
                try_it: self.step == Step::TryIt,
                seconds: self
                    .passage_clip
                    .as_ref()
                    .map(|clip| clip.samples.len() as f32 / clip.sample_rate as f32),
            });
            self.keep_focus.focus(window, cx);
            cx.notify();
        } else {
            self.depart(destination, cx);
        }
    }

    pub(crate) fn request_step(&mut self, step: Step, window: &mut Window, cx: &mut Context<Self>) {
        self.request_departure(Departure::Step(step), window, cx);
    }

    pub(crate) fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.request_departure(Departure::Later, window, cx);
    }

    fn cancel_transient(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.discard.is_some() {
            self.keep_working(window, cx);
        } else {
            self.request_close(window, cx);
        }
        cx.stop_propagation();
    }

    fn depart(&mut self, destination: Departure, cx: &mut Context<Self>) {
        match destination {
            Departure::Step(step) => self.go_to(step, cx),
            Departure::Later => self.set_up_later(cx),
        }
    }

    fn keep_working(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(confirm) = self.discard.take()
            && let Some(opener) = confirm.opener
        {
            opener.focus(window, cx);
        }
        cx.notify();
    }

    fn render_discard(
        &self,
        confirmation: &DiscardConfirmation,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let (title, action, detail) = if confirmation.try_it {
            (
                "Leave without the test result?",
                "Keep testing",
                "The test recording will be deleted.".into(),
            )
        } else if self.step == Step::Permissions {
            (
                "Leave the microphone test?",
                "Keep testing",
                "The test recording will be deleted.".into(),
            )
        } else {
            let duration = confirmation
                .seconds
                .map_or(String::new(), |seconds| format!(" ({seconds:.0} s)"));
            (
                "Leave without saving your voice?",
                "Keep reading",
                format!("Your first reading{duration} hasn't been saved and will be deleted."),
            )
        };
        kit::confirm_box(kit::ConfirmTone::Warning, cx)
            .id("setup-discard")
            .role(Role::AlertDialog)
            .aria_label(title)
            .child(crate::ui::heading("setup-discard-title", title))
            .child(text_line(
                "setup-discard-detail",
                Role::Label,
                format!("{detail} Steps you finished stay done."),
            ))
            .child(kit::confirm_actions(
                Button::new("setup-keep-working")
                    .label(action)
                    .map(|button| kit::primary(button, false, cx))
                    .track_focus(&self.keep_focus)
                    .tab_stop(true)
                    .on_click(cx.listener(|this, _, window, cx| this.keep_working(window, cx))),
                Button::new("setup-discard-leave")
                    .label("Discard and leave")
                    .outline()
                    .tab_stop(true)
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some(confirmation) = this.discard.take() {
                            if let Some(opener) = confirmation.opener {
                                opener.focus(window, cx);
                            }
                            this.depart(confirmation.destination, cx);
                            cx.notify();
                        }
                    })),
                cx,
            ))
    }

    fn render_footer(&self, cx: &Context<Self>) -> impl IntoElement {
        let skipping = self.skip_primary(cx);
        let label = match (self.step, self.advancing, skipping) {
            (_, true, _) => "Saving…",
            (Step::Welcome, _, _) => "Get started",
            (Step::Ready | Step::Overview, _, _) => "Done",
            (Step::Permissions, _, _) if self.permission_request_pending => {
                "Waiting for your system…"
            }
            (Step::Models, _, _) if self.wants_download(cx) => "Download and continue",
            (_, _, true) => "Skip this step",
            _ => "Continue",
        };
        let disabled = self.discard.is_some() || !self.primary_enabled(cx);
        let previous = self.previous_step(cx);
        let theme = cx.theme();
        kit::stack()
            .id("setup-footer")
            .role(Role::Group)
            .aria_label("Setup actions")
            .flex_none()
            .gap_2()
            .px_4()
            .py_3()
            .debug_selector(|| "setup-footer".into())
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.background)
            .when_some(self.discard.as_ref(), |el, pending| {
                el.child(self.render_discard(pending, cx))
            })
            .child(
                row()
                    .child(
                        Button::new("setup-close")
                            .ghost()
                            .label("Set up later")
                            .tooltip(if self.download(cx).read(cx).running() {
                                "Your progress is saved. The speech models will keep downloading."
                            } else {
                                "Your progress is saved."
                            })
                            .track_focus(&self.close_focus)
                            .tab_stop(true)
                            .disabled(self.advancing || self.discard.is_some())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.request_close(window, cx)),
                            ),
                    )
                    .child(
                        div().flex_1().min_w_0().children(
                            self.progress_error
                                .as_ref()
                                .map(|text| error_line("setup-message", text.clone(), cx)),
                        ),
                    )
                    .when(previous.is_some(), |el| {
                        el.child(
                            Button::new("setup-back")
                                .outline()
                                .label("Back")
                                .track_focus(&self.back_focus)
                                .tab_stop(true)
                                .disabled(self.back_disabled())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.activate_back(window, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new("setup-continue")
                            .label(label)
                            .track_focus(&self.continue_focus)
                            .tab_stop(true)
                            .disabled(disabled)
                            .map(|button| {
                                if skipping {
                                    button.outline()
                                } else {
                                    kit::primary(button, disabled, cx)
                                }
                            })
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.activate_primary(window, cx)
                                }),
                            ),
                    ),
            )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecordingPermission {
    Microphone,
    SystemAudio,
    Notifications,
    MeetingNames,
}

impl RecordingPermission {
    fn title(self) -> &'static str {
        match self {
            Self::Microphone => "Microphone",
            Self::SystemAudio => "System audio",
            Self::Notifications => "Notifications",
            Self::MeetingNames => "Meeting app names (optional)",
        }
    }

    fn purpose(self) -> &'static str {
        match self {
            Self::Microphone => {
                "Records your side of each meeting as the Me track, plus your voice samples and tests during setup."
            }
            Self::SystemAudio => {
                "Records what the other participants say, from your computer's sound, as the Others track. The two tracks are never mixed."
            }
            Self::Notifications => {
                "Tells you when a transcript is ready and asks before recording a meeting it detects."
            }
            Self::MeetingNames => {
                "Reads participant names from meeting app windows. Recording works without it."
            }
        }
    }
}

fn active_microphone(devices: &[super::settings::InputDevice], pinned: Option<&str>) -> String {
    if let Some(device) = pinned.and_then(|id| devices.iter().find(|device| device.id == id)) {
        format!("Microphone: {} (chosen in Settings)", device.name)
    } else if let Some(device) = devices.iter().find(|device| device.is_default) {
        let fallback = if pinned.is_some() {
            ". The microphone chosen in Settings is unavailable."
        } else {
            ""
        };
        format!("Microphone: {} (system default){fallback}", device.name)
    } else {
        "No microphone found.".into()
    }
}

fn permission_words(state: Option<&str>) -> &'static str {
    match state {
        Some("granted") => "Allowed",
        Some("denied") => "Denied",
        Some("not_determined") => "Not asked yet",
        Some("restricted") => "Restricted by the system",
        Some("unsupported") => "No permission needed",
        _ => "Unknown",
    }
}

fn audio_permission_answered(state: Option<&str>) -> bool {
    matches!(
        state,
        Some("granted" | "denied" | "restricted" | "unsupported")
    )
}

fn permission_kind(state: Option<&str>) -> kit::StatusKind {
    match state {
        Some("granted" | "unsupported") => kit::StatusKind::Done,
        Some("denied" | "restricted") => kit::StatusKind::Attention,
        _ => kit::StatusKind::Pending,
    }
}

fn microphone_instructions(host: HostPlatform) -> &'static str {
    match host {
        HostPlatform::MacOs => {
            "Allow BroScribe Agent in System Settings › Privacy & Security › Microphone, then try again."
        }
        HostPlatform::Windows => {
            "Check that Let desktop apps access your microphone is on in Settings › Privacy & security › Microphone, then try again."
        }
        HostPlatform::Linux => {
            "Check that the microphone isn't muted in your sound settings, then try again."
        }
    }
}

fn system_audio_instructions(host: HostPlatform) -> &'static str {
    match host {
        HostPlatform::MacOs => {
            "Allow BroScribe Agent in System Settings › Privacy & Security › Screen & System Audio Recording, then try again."
        }
        HostPlatform::Windows => {
            "Check that system audio plays through the selected output device, then try again."
        }
        HostPlatform::Linux => {
            "Check that the output device and its monitor are available, then try again."
        }
    }
}

/// One outcome as a single line: `ok_role` (a `Label`, or the `Status`
/// of the assistant's own message) on success, an alert on failure.
fn outcome_line(id: impl Into<ElementId>, ok_role: Role, o: &Outcome, cx: &App) -> Stateful<Div> {
    match o {
        Outcome::Ok(t) => crate::ui::kit::status_line_role(
            id,
            crate::ui::kit::StatusKind::Done,
            ok_role,
            t.clone(),
            cx,
        ),
        Outcome::Err(t) => error_line(id, t.clone(), cx),
    }
}

/// A transcript in a labelled box; each non-empty line is read as itself.
fn transcript_box(id: &'static str, label: &'static str, text: &str, cx: &App) -> impl IntoElement {
    div()
        .id(id)
        .role(Role::Article)
        .aria_label(label)
        .p_3()
        .rounded(cx.theme().radius)
        .border_1()
        .border_color(cx.theme().border)
        .text_role(TextRole::Reading)
        .children(text.lines().enumerate().map(|(i, l)| {
            if l.trim().is_empty() {
                div().child(l.to_owned()).into_any_element()
            } else {
                text_line(
                    (SharedString::from(format!("{id}-line")), i),
                    Role::Label,
                    l.to_owned(),
                )
                .into_any_element()
            }
        }))
}

/// The distinct non-empty lines of the Words field, in order.
pub(crate) fn save_word_list(
    state: &Entity<AppState>,
    text: &str,
    cx: &App,
) -> Option<Task<std::result::Result<Value, String>>> {
    let words = terms(text);
    if words.is_empty() {
        return None;
    }
    let body = words.join("\n") + "\n";
    let state = state.read(cx);
    let cache_dir = state.paths().cache_dir.clone();
    let cli = state.cli().clone();
    Some(cx.background_executor().spawn(async move {
        let dir =
            private_dir(&cache_dir, &unique_name("words", 0)).map_err(|e| io_user_text(&e))?;
        let file = dir.join("words.txt");
        let result = write_private_file(&file, body.as_bytes())
            .map_err(|e| io_user_text(&e))
            .and_then(|()| {
                let mut a = args(["setup", "--non-interactive", "--words-file"]);
                a.push(file.into());
                cli.run_blocking(&a, ChildKind::Command)
                    .map_err(|e| e.user_text())
            });
        remove_dir(&dir);
        result
    }))
}

fn terms(text: &str) -> Vec<&str> {
    let mut seen = BTreeSet::new();
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && seen.insert(*l))
        .collect()
}

impl Render for SetupAssistant {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let narrow = Self::is_narrow(window);
        if narrow != self.narrow {
            if narrow && self.nav_focus.is_focused(window) {
                self.all_steps_focus.focus(window, cx);
            } else if !narrow && self.all_steps_focus.is_focused(window) {
                self.nav_focus.focus(window, cx);
            }
            self.narrow = narrow;
        }
        let step = self.step;
        let landing = matches!(step, Step::Welcome | Step::Ready);
        let display = TextRole::Display;
        let content = self.render_step(window, cx);
        let position = step.position().map(|i| {
            if i < ESSENTIAL_SETUP_COUNT {
                format!("Step {} of {}", i + 1, ESSENTIAL_SETUP_COUNT)
            } else {
                "Recommended".into()
            }
        });
        div()
            .id("setup-assistant")
            .key_context("SetupAssistant")
            .capture_action(cx.listener(
                |this, _: &crate::ui::keymap::CancelTransient, window, cx| {
                    this.cancel_transient(window, cx);
                },
            ))
            .capture_action(
                cx.listener(|this, _: &gpui::component::input::Escape, window, cx| {
                    let owns_focus = [
                        &this.language,
                        &this.name,
                        &this.emails,
                        &this.retention_days,
                    ]
                    .into_iter()
                    .any(|input| input.focus_handle(cx).is_focused(window))
                        || this.words.focus_handle(cx).is_focused(window);
                    if owns_focus {
                        this.cancel_transient(window, cx);
                    }
                }),
            )
            .on_action(
                cx.listener(|this, _: &gpui::base::actions::Cancel, window, cx| {
                    this.cancel_transient(window, cx);
                }),
            )
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" if !event.keystroke.modifiers.modified() => {
                        this.cancel_transient(window, cx);
                        return;
                    }
                    "enter" if this.discard.is_some() && this.keep_focus.is_focused(window) => {
                        this.keep_working(window, cx);
                        window.prevent_default();
                    }
                    "enter" if this.back_focus.is_focused(window) => {
                        this.activate_back(window, cx);
                        window.prevent_default();
                    }
                    "enter" if this.close_focus.is_focused(window) => {
                        this.request_close(window, cx);
                        window.prevent_default();
                    }
                    "enter"
                        if this.heading_focus.is_focused(window)
                            || this.continue_focus.is_focused(window)
                            || window.focused(cx).is_none() =>
                    {
                        this.activate_primary(window, cx);
                        window.prevent_default();
                    }
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .on_drop(cx.listener(|this, paths: &gpui::ExternalPaths, _, cx| {
                crate::views::import_sheet::ImportRuns::request_dialog(
                    crate::views::import_sheet::dropped_paths(paths),
                    cx,
                );
                this.state
                    .update(cx, |_, cx| cx.emit(crate::state::AppEvent::OpenMainWindow));
            }))
            .role(Role::Main)
            .aria_label("Setup assistant")
            .size_full()
            .flex()
            .when(!narrow && step != Step::Welcome, |el| {
                el.child(self.render_nav(window, cx))
            })
            .child(
                div()
                    .id("setup-step")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .bg(cx.theme().background)
                    .debug_selector(|| "setup-step".into())
                    .child(
                        div()
                            .id("setup-step-body")
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .debug_selector(|| "setup-step-body".into())
                            .overflow_y_scroll()
                            .track_scroll(&self.body_scroll)
                            .gap_3()
                            .p(kit::DESK_PADDING)
                            .when(
                                step != Step::Welcome && (narrow || position.is_some()),
                                |el| {
                                    el.child(
                                        row()
                                            .flex_wrap()
                                            .when_some(position, |el, position| {
                                                el.child(
                                                    text_line(
                                                        "setup-step-position",
                                                        Role::Status,
                                                        format!("{position} · {}", step.title()),
                                                    )
                                                    .text_role(TextRole::Caption),
                                                )
                                            })
                                            .when(narrow, |el| {
                                                el.child(
                                                    div()
                                                        .id("setup-narrow-navigation")
                                                        .role(Role::Navigation)
                                                        .aria_label("Setup steps")
                                                        .child(self.all_steps(cx)),
                                                )
                                            }),
                                    )
                                },
                            )
                            .when(landing, |el| {
                                el.child(
                                    div()
                                        .id("setup-mascot")
                                        .debug_selector(|| "setup-mascot".into())
                                        .child(kit::mascot(assets::MASCOT_IDLE, 160.)),
                                )
                            })
                            .child(
                                crate::ui::title("setup-step-title", step.title())
                                    .track_focus(&self.heading_focus)
                                    .tab_stop(false)
                                    .when(landing, |heading| heading.text_role(display)),
                            )
                            // The step's content is a card on the desk.
                            .child(kit::card("setup-card", cx).child(content)),
                    )
                    .child(self.render_footer(cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ps_path, ps_str, write_cli};

    use broscribe_platform::fakes::{FakeMicCapture, FakePaths, FakeSystemInfo};
    use broscribe_platform::resolve_paths;
    use gpui::TestAppContext;

    #[gpui::test]
    fn welcome_footer_steps_and_narrow_focus_follow_the_setup_flow(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        cx.update(gpui::init);
        let (setup, visual) =
            cx.add_window_view(|window, cx| SetupAssistant::new(state, fake_deps(), window, cx));
        visual.simulate_resize(gpui::size(px(760.), px(600.)));
        redraw(visual);
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("setup-step-title")),
                ("Heading", "Welcome to BroScribe")
            );
            assert_eq!(
                tree.read(tree.find("setup-continue")),
                ("Button", "Get started")
            );
            tree.find("setup-welcome-privacy");
            assert!(tree.all("setup-back").is_empty());
            assert!(tree.all("setup-steps").is_empty());
            assert!(setup.read(cx).heading_focus.is_focused(window));
        });
        assert!(visual.debug_bounds("setup-mascot").is_some());
        visual.simulate_keystrokes("enter");
        visual.run_until_parked();
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("setup-step-position")),
                ("Status", "Step 1 of 4 · Language and speech model")
            );
            assert_eq!(tree.all("setup-step-position").len(), 1);
            tree.find("setup-language-label");
            assert_eq!(
                tree.read(tree.find("setup-step-models")),
                (
                    "ListItem",
                    "Language and speech model, not done, current step"
                )
            );
            assert!(tree.all("setup-step-words").is_empty());
            assert!(tree.all("setup-step-existing").is_empty());
            assert!(setup.read(cx).continue_focus.is_focused(window));
            setup.read(cx).nav_focus.clone().focus(window, cx);
        });
        assert!(visual.debug_bounds("setup-mascot").is_none());
        visual.simulate_keystrokes("down enter");
        visual.run_until_parked();
        visual.update(|window, cx| {
            assert_eq!(setup.read(cx).step, Step::Permissions);
            assert!(setup.read(cx).nav_focus.is_focused(window));
        });
        visual.simulate_resize(gpui::size(px(640.), px(480.)));
        redraw(visual);
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert!(tree.all("setup-steps").is_empty());
            tree.find("setup-all-steps");
            assert!(setup.read(cx).all_steps_focus.is_focused(window));
        });
        let footer = visual.debug_bounds("setup-footer").unwrap();
        let body = visual.debug_bounds("setup-step-body").unwrap();
        assert!(body.bottom() <= footer.top());
        assert!(footer.bottom() <= px(480.));
    }

    #[gpui::test]
    fn discard_prompt_preserves_reading_until_the_user_chooses_to_leave(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        cx.update(gpui::init);
        let (setup, visual) =
            cx.add_window_view(|window, cx| SetupAssistant::new(state, fake_deps(), window, cx));
        visual.update(|window, cx| {
            setup.update(cx, |this, cx| {
                this.step = Step::Voice;
                this.passage_clip = Some(synthetic_reading());
                this.close_focus.focus(window, cx);
                this.request_close(window, cx);
                assert!(this.keep_focus.is_focused(window));
                assert!(this.passage_clip.is_some());
            })
        });
        redraw(visual);
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("setup-discard-title")),
                ("Heading", "Leave without saving your voice?")
            );
        });
        visual.simulate_keystrokes("enter");
        visual.update(|window, cx| {
            setup.update(cx, |this, cx| {
                assert!(this.discard.is_none());
                assert!(this.passage_clip.is_some());
                assert!(this.close_focus.is_focused(window));
                this.request_departure(Departure::Step(Step::TryIt), window, cx);
            })
        });
        redraw(visual);
        let leave = visual.update(|window, cx| {
            crate::ui::a11y_tree::draw_and_read(window, cx);
            gpui::base::test_support::snapshots(window)
                .iter()
                .find(|node| node.label() == Some("Discard and leave"))
                .unwrap()
                .bounds()
                .center()
        });
        visual.simulate_click(leave, Default::default());
        visual.run_until_parked();
        assert_eq!(setup.read_with(visual, |this, _| this.step), Step::TryIt);
        assert!(setup.read_with(visual, |this, _| this.passage_clip.is_none()));
    }

    #[gpui::test]
    fn setup_static_rows_never_gain_a_wait_clock(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        cx.update(gpui::init);
        let (setup, visual) = cx.add_window_view(|window, cx| {
            let mut view = SetupAssistant::new(state.clone(), fake_deps(), window, cx);
            view.step = Step::Ready;
            view.helper = Some(Ok(HelperRoute::Running));
            view.permissions = Some(Ok(serde_json::json!({
                "microphone": "not_determined", "system_audio": "not_determined"
            })));
            view.accessibility_trusted = false;
            view.notification_authorization = Some(NotificationAuthorization::NotDetermined);
            view
        });
        visual.simulate_resize(gpui::size(px(1200.), px(1250.)));
        for step in [Step::Ready, Step::Permissions] {
            setup.update(visual, |view, cx| {
                view.step = step;
                cx.notify();
            });
            visual.update(|window, cx| {
                let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
                if step == Step::Ready {
                    tree.find("setup-ready-models");
                    tree.find("setup-overview-audio_check");
                } else {
                    assert_eq!(
                        tree.read(tree.find("setup-microphone-state")).1,
                        "Not asked yet"
                    );
                    assert_eq!(
                        tree.read(tree.find("setup-notifications-state")).1,
                        "Not asked yet"
                    );
                    assert_eq!(
                        tree.read(tree.find("setup-accessibility-state")).1,
                        "Not asked yet"
                    );
                }
            });
            visual.executor().advance_clock(Duration::from_secs(31));
            visual.run_until_parked();
            visual.update(|window, cx| {
                let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
                assert!(tree.all("pending-elapsed").is_empty());
                assert!(tree.all("pending-help").is_empty());
            });
        }
    }

    #[gpui::test]
    fn recording_access_explains_each_prompt_and_saves_answered_audio(cx: &mut TestAppContext) {
        use broscribe_platform::fakes::FakeNotifier;
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let notifier = Arc::new(FakeNotifier::default());
        *notifier.authorization.lock().unwrap() = NotificationAuthorization::NotDetermined;
        *notifier.authorization_answer.lock().unwrap() = Some(NotificationAuthorization::Denied);
        cx.update(gpui::init);
        let (setup, visual) = cx.add_window_view(|window, cx| {
            let mut view = SetupAssistant::new(state.clone(), fake_deps(), window, cx);
            view.step = Step::Permissions;
            view.helper = Some(Ok(HelperRoute::Running));
            view.permissions = Some(Ok(serde_json::json!({
                "microphone": "not_determined", "system_audio": "not_determined"
            })));
            view.accessibility_trusted = false;
            view.notifier = notifier.clone();
            view.notification_authorization = Some(NotificationAuthorization::NotDetermined);
            view
        });
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("setup-agent-heading")),
                ("Heading", "BroScribe Agent")
            );
            assert_eq!(
                tree.read(tree.find("setup-microphone-purpose")),
                ("Label", RecordingPermission::Microphone.purpose())
            );
            assert!(tree.all("setup-system_audio-purpose").is_empty());
            assert!(tree.all("setup-refresh-permissions").is_empty());
            assert_eq!(notifier.authorization_requests.load(Ordering::SeqCst), 0);
            assert!(!Step::Permissions.completed(state.read(cx).config()));
        });
        setup.update(visual, |this, cx| {
            this.permissions = Some(Ok(
                serde_json::json!({"microphone": "granted", "system_audio": "not_determined"}),
            ));
            assert_eq!(
                this.next_permission(cx),
                Some(RecordingPermission::SystemAudio)
            );
            cx.notify();
        });
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("setup-system_audio-purpose")),
                ("Label", RecordingPermission::SystemAudio.purpose())
            );
            tree.find("mic-check");
        });
        visual.update(|window, cx| {
            setup.update(cx, |this, cx| {
                this.permissions = Some(Ok(
                    serde_json::json!({"microphone": "granted", "system_audio": "denied"}),
                ));
                assert_eq!(
                    this.next_permission(cx),
                    Some(RecordingPermission::Notifications)
                );
                this.activate_primary(window, cx);
                assert!(this.permission_request_pending);
            })
        });
        visual.run_until_parked();
        assert_eq!(notifier.authorization_requests.load(Ordering::SeqCst), 1);
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            tree.find("setup-notifications-off");
            tree.find("setup-accessibility-note");
            setup.update(cx, |this, cx| {
                assert_eq!(
                    this.next_permission(cx),
                    Some(RecordingPermission::MeetingNames)
                );
                this.accessibility_answered = true;
                this.activate_primary(window, cx);
            });
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            assert_eq!(setup.read(cx).step, Step::Voice);
            assert!(Step::Permissions.completed(state.read(cx).config()));
            assert!(setup.read(cx).continue_focus.is_focused(window));
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("setup-step-position")),
                ("Status", "Step 3 of 4 · Your voice")
            );
        });
        visual.update(|window, cx| {
            setup.update(cx, |this, cx| {
                this.activate_back(window, cx);
                assert_eq!(this.step, Step::Permissions);
                assert!(this.back_focus.is_focused(window));
            })
        });
    }

    #[gpui::test]
    fn recording_access_uses_shared_agent_states_and_the_install_advisory(cx: &mut TestAppContext) {
        use crate::daemon::{DaemonClient, EventStream, Transport};
        use broscribe_platform::AgentServiceStatus;
        use broscribe_platform::fakes::{FakeAutostart, FakeSystemSettings};

        struct AgentFixture;

        impl Transport for AgentFixture {
            fn subscribe(&self) -> Result<Box<dyn EventStream>, broscribe_ipc::IpcError> {
                Err(broscribe_ipc::IpcError::new(
                    "unreachable",
                    "synthetic agent",
                ))
            }

            fn call(&self, _: Call) -> Result<Value, broscribe_ipc::IpcError> {
                Err(broscribe_ipc::IpcError::new(
                    "unreachable",
                    "synthetic agent",
                ))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        cx.update(gpui::init);
        let daemon = cx.update(|cx| DaemonClient::detached(Arc::new(AgentFixture), cx));
        let service = Arc::new(FakeAutostart::default());
        let agent = cx.new(|cx| {
            crate::launch::AgentStarter::with_autostart(
                state.clone(),
                daemon.clone(),
                Some(service.clone()),
                cx,
            )
        });
        state.update(cx, |state, cx| {
            state.attach_daemon(daemon.clone(), cx);
            state.attach_agent_starter(&agent);
        });
        let (setup, visual) = cx.add_window_view(|window, cx| {
            let mut view = SetupAssistant::new(state.clone(), fake_deps(), window, cx);
            view.step = Step::Permissions;
            view.system_settings = Arc::new(FakeSystemSettings {
                host: HostPlatform::MacOs,
                supported: [SettingsPane::LoginItems].into(),
                opened: Default::default(),
                notifications_permission: None,
            });
            view
        });
        for (observed, words, action) in [
            (
                AgentServiceStatus::NotInstalled,
                "Not installed",
                "Install and start",
            ),
            (AgentServiceStatus::Stopped, "Stopped", "Start"),
            (
                AgentServiceStatus::NeedsApproval,
                "Waiting for your approval in Login Items",
                "Open Login Items",
            ),
        ] {
            *service.agent_status.lock().unwrap() = Some(observed);
            agent.update(visual, |agent, cx| agent.refresh_service(cx));
            visual.run_until_parked();
            visual.update(|window, cx| {
                let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
                assert_eq!(tree.read(tree.find("setup-agent-status")).1, words);
                assert!(
                    tree.nodes
                        .iter()
                        .any(|node| node.role == "Button" && node.name == action)
                );
                assert!(!setup.read(cx).recording_access_can_continue(cx));
            });
        }
        visual.update(|window, cx| {
            daemon.update(cx, |agent, cx| {
                agent.incompatible_agent = Some(Some(1));
                cx.notify();
            });
            setup.update(cx, |this, cx| {
                this.agent_location_advisory = true;
                cx.notify();
            });
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert!(
                tree.read(tree.find("setup-agent-status"))
                    .1
                    .contains("older")
            );
            tree.find("setup-agent-details");
            tree.find("setup-agent-location");
            assert_eq!(
                tree.read(tree.find("setup-install-helper")),
                ("Button", "Reinstall")
            );
        });
        visual.update(|window, cx| {
            daemon.update(cx, |agent, cx| {
                agent.connected = true;
                agent.incompatible_agent = None;
                cx.notify();
            });
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("setup-agent-status")),
                ("Status", "Running")
            );
        });
    }

    #[gpui::test]
    fn unsupported_audio_and_pinned_microphone_are_named_by_host(cx: &mut TestAppContext) {
        use broscribe_platform::fakes::FakeSystemSettings;
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        cx.update(gpui::init);
        let (setup, visual) = cx.add_window_view(|window, cx| {
            let mut view = SetupAssistant::new(state.clone(), fake_deps(), window, cx);
            view.step = Step::Permissions;
            view.helper = Some(Ok(HelperRoute::Running));
            view.permissions = Some(Ok(serde_json::json!({
                "microphone": "unsupported", "system_audio": "unsupported"
            })));
            view.notification_authorization = Some(NotificationAuthorization::Unsupported);
            view.accessibility_trusted = true;
            view.recording_devices = Some(Ok(super::super::settings::parse_input_devices(
                &serde_json::json!({
                    "devices": [
                        {"id": "built-in", "name": "Acorn microphone", "is_default": true},
                        {"id": "usb", "name": "Clover microphone", "is_default": false},
                    ]
                }),
            )));
            view
        });
        for (host, platform) in [
            (HostPlatform::Windows, "Windows"),
            (HostPlatform::Linux, "Linux"),
        ] {
            setup.update(visual, |this, cx| {
                this.system_settings = Arc::new(FakeSystemSettings {
                    host,
                    supported: Default::default(),
                    opened: Default::default(),
                    notifications_permission: None,
                });
                let mut config = state.read(cx).config().clone();
                config.audio.pinned_mic = Some("usb".into());
                state.update(cx, |state, cx| state.set_config(config, cx));
                assert!(this.recording_access_can_continue(cx));
                cx.notify();
            });
            visual.update(|window, cx| {
                let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
                assert!(tree.read(tree.find("setup-audio-not-needed")).1.starts_with(platform));
                assert_eq!(tree.read(tree.find("setup-notifications-state")), ("Status", "No permission needed"));
                assert_eq!(tree.read(tree.find("setup-recording-microphone")), ("Label", "Microphone: Clover microphone (chosen in Settings)"));
                assert!(tree.all("setup-open-microphone-settings").is_empty());
                let devices = setup.read(cx).recording_devices.as_ref().unwrap().as_ref().unwrap();
                assert_eq!(active_microphone(devices, Some("unplugged")),
                    "Microphone: Acorn microphone (system default). The microphone chosen in Settings is unavailable.");
            });
        }
    }

    #[gpui::test]
    fn model_disclosure_download_states_and_capture_gate_use_registry_rows(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        cx.update(gpui::init);
        let (setup, visual) = cx.add_window_view(|window, cx| {
            let mut view = SetupAssistant::new(state.clone(), fake_deps(), window, cx);
            view.step = Step::Models;
            let profile = &state.read(cx).config().profile;
            view.models = Some(Ok(parse_models_list(&serde_json::json!({
                "models": [
                    {"id": profile.asr, "display_name": "Acorn Speech", "roles": ["asr"], "state": "missing", "license_verified": true, "bytes": 570000000},
                    {"id": profile.vad, "display_name": "Acorn Detection", "roles": ["vad"], "state": "installed", "bytes": 2000000},
                    {"id": profile.speaker_embedding, "display_name": "Acorn Voices", "roles": ["speaker_embedding"], "state": "installed", "bytes": 97000000},
                ],
                "profiles": [{"id": "acorn", "asr": profile.asr, "active": true, "languages": ["en"]}],
                "profile_models": [profile.asr, profile.vad, profile.speaker_embedding],
            }))));
            view.machine = Some(MachineInfo { memory: 7_000_000_000, free_disk: Some(420_000_000) });
            view
        });
        let asr = state.read_with(visual, |state, _| state.config().profile.asr.clone());
        let row_id = format!("setup-model-{asr}");
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(tree.read(tree.find("setup-profile-choices")), ("RadioGroup", "Speech model"));
            assert!(tree.nodes.iter().any(|node| node.role == "RadioButton" && node.name.starts_with("Acorn Speech")));
            assert_eq!(tree.read(tree.find("setup-continue")), ("Button", "Download and continue"));
            assert_eq!(tree.read(tree.find("setup-sample-unavailable")).1,
                "Available once the speech model is downloaded.");
            assert!(tree.read(tree.find("setup-download-space")).1.contains("420 MB"));
            assert!(tree.all(&row_id).is_empty());
            assert!(!setup.read(cx).primary_enabled(cx));
            assert!(setup.read(cx).models_ready(CapturePurpose::Passage, cx));
            assert!(!setup.read(cx).models_ready(CapturePurpose::TryIt, cx));
        });
        setup.update(visual, |this, cx| {
            this.models_expanded = true;
            this.download(cx).clone().update(cx, |downloads, cx| {
                downloads.set_status(
                    DownloadStatus::Running {
                        percent: 55,
                        total: 669_000_000,
                    },
                    cx,
                )
            });
            cx.notify();
        });
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("setup-download")),
                ("ProgressIndicator", "Speech model download")
            );
            assert!(
                tree.read(tree.find("setup-download-state"))
                    .1
                    .ends_with("55%")
            );
            assert_eq!(tree.all("setup-download").len(), 1);
            assert_eq!(
                tree.read(tree.find(&row_id)).1,
                "Transcription · Acorn Speech · 570 MB · Not downloaded"
            );
            assert_eq!(
                tree.read(tree.find("setup-continue")),
                ("Button", "Continue")
            );
        });
        for (status, expected) in [
            (
                DownloadStatus::Cancelled,
                "Download cancelled. Models already downloaded stay installed.",
            ),
            (
                DownloadStatus::Failed(
                    "Couldn't download Acorn Speech. Network unavailable.".into(),
                ),
                "Couldn't download Acorn Speech. Network unavailable.",
            ),
        ] {
            // Draw the terminal result before allowing the synthetic registry reload.
            visual.update(|window, cx| {
                setup.update(cx, |this, cx| {
                    this.download(cx)
                        .clone()
                        .update(cx, |downloads, cx| downloads.set_status(status, cx));
                    cx.notify();
                });
                let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
                assert!(tree.all("setup-download").is_empty());
                assert_eq!(tree.read(tree.find("setup-download-state")).1, expected);
            });
        }
        assert_eq!(download_percent(200, 800, 50, 1000), 60);
    }

    #[gpui::test]
    fn selected_profile_download_keeps_running_after_setup_closes(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = write_cli(
            &dir.path().join("synthetic-cli"),
            "echo '{}'",
            "fx_echo '{}'",
        );
        let state = state_with_cli(cx, dir.path(), cli);
        let data_dir = state.read_with(cx, |state, _| state.paths().data_dir.clone());
        cx.update(gpui::init);
        let main = cx.add_window(|window, cx| {
            crate::ui::MainWindow::new(state.clone(), fake_deps(), window, cx)
        });
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state.clone(), fake_deps(), window, cx));
        window
            .update(cx, |setup, window, cx| {
                setup.step = Step::Models;
                setup.models = Some(Ok(parse_models_list(&serde_json::json!({
                    "models": [{"id": "test-speech", "display_name": "Acorn", "state": "missing",
                        "license_verified": true, "bytes": 1_000_000}],
                    "profile_models": ["test-speech"],
                }))));
                assert!(setup.download_profile(cx));
                assert!(setup.download(cx).read(cx).running());
                assert!(!Step::Models.completed(state.read(cx).config()));
                setup.close(cx);
                window.remove_window();
            })
            .unwrap();
        cx.update_window(main.into(), |_, window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("main-model-download")),
                ("Status", "Downloading speech models · 0%")
            );
            tree.find("main-model-download-cancel");
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(cx.windows().len(), 1);
        assert_eq!(
            Config::load(&data_dir).unwrap().setup.completed_steps,
            ["models"]
        );
        state.read_with(cx, |state, cx| {
            assert_eq!(
                state.model_downloads().read(cx).status,
                DownloadStatus::Done(1_000_000)
            );
        });
    }

    #[test]
    fn permission_states_read_as_words_and_only_answered_audio_can_complete() {
        for (state, words, answered) in [
            (Some("granted"), "Allowed", true),
            (Some("unsupported"), "No permission needed", true),
            (Some("denied"), "Denied", true),
            (Some("restricted"), "Restricted by the system", true),
            (Some("not_determined"), "Not asked yet", false),
            (Some("unrecognised"), "Unknown", false),
            (None, "Unknown", false),
        ] {
            assert_eq!(permission_words(state), words);
            assert_eq!(audio_permission_answered(state), answered);
        }
    }

    #[gpui::test]
    fn windows_setup_explains_the_tray_and_opens_taskbar_settings(cx: &mut TestAppContext) {
        use broscribe_platform::fakes::FakeSystemSettings;

        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        cx.update(gpui::init);
        let settings = Arc::new(FakeSystemSettings {
            host: HostPlatform::Windows,
            supported: [SettingsPane::Taskbar].into(),
            opened: Default::default(),
            notifications_permission: None,
        });
        let (_, visual) = cx.add_window_view(|window, cx| {
            let mut assistant = SetupAssistant::new(state, fake_deps(), window, cx);
            assistant.step = Step::Ready;
            assistant.system_settings = settings.clone();
            assistant
        });
        let point = visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("setup-tray-tip")),
                ("Note", crate::ui::TRAY_TIP)
            );
            assert_eq!(
                tree.read(tree.find("setup-open-taskbar-settings")),
                ("Button", "Open taskbar settings")
            );
            gpui::base::test_support::snapshots(window)
                .iter()
                .find(|node| node.label() == Some("Open taskbar settings"))
                .unwrap()
                .bounds()
                .center()
        });
        visual.simulate_click(point, Default::default());
        assert_eq!(*settings.opened.lock().unwrap(), [SettingsPane::Taskbar]);
    }

    #[gpui::test]
    fn denied_microphone_renders_and_opens_only_the_hosts_supported_pane(cx: &mut TestAppContext) {
        use broscribe_platform::fakes::FakeSystemSettings;

        for (host, expected) in [
            (HostPlatform::MacOs, Some("Open System Settings")),
            (HostPlatform::Windows, Some("Open Windows Settings")),
            (HostPlatform::Linux, None),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let state = new_state(cx, dir.path());
            cx.update(gpui::init);
            let settings = Arc::new(FakeSystemSettings {
                host,
                supported: expected
                    .map(|_| SettingsPane::Microphone)
                    .into_iter()
                    .collect(),
                opened: Default::default(),
                notifications_permission: None,
            });
            let (view, visual) = cx.add_window_view(|window, cx| {
                let mut assistant = SetupAssistant::new(state, fake_deps(), window, cx);
                assistant.step = Step::Permissions;
                assistant.system_settings = settings.clone();
                assistant.permissions = Some(Ok(
                    serde_json::json!({ "microphone": "denied", "system_audio": "denied" }),
                ));
                assistant
            });
            let id = "setup-open-microphone-settings";
            let position =
                visual.update(|window, cx| {
                    let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
                    assert_eq!(
                        tree.read(tree.find("setup-microphone-denied")),
                        ("Status", "Denied")
                    );
                    assert_eq!(
                        tree.read(tree.find("setup-microphone-denied-help")),
                        ("Label", microphone_instructions(host))
                    );
                    assert!(tree.nodes.iter().all(|node| node.element_id.as_deref()
                        != Some("setup-open-system_audio-settings")));
                    match expected {
                        Some(label) => {
                            assert_eq!(tree.read(tree.find(id)), ("Button", label));
                            gpui::base::test_support::snapshots(window)
                                .iter()
                                .find(|node| {
                                    node.role() == Some(Role::Button) && node.label() == Some(label)
                                })
                                .map(|node| node.bounds().center())
                        }
                        None => {
                            assert!(
                                tree.nodes
                                    .iter()
                                    .all(|node| node.element_id.as_deref() != Some(id))
                            );
                            None
                        }
                    }
                });
            if expected.is_some() {
                visual.simulate_click(
                    position.expect("painted settings button"),
                    Default::default(),
                );
            }
            assert_eq!(
                *settings.opened.lock().unwrap(),
                expected
                    .map(|_| SettingsPane::Microphone)
                    .into_iter()
                    .collect::<Vec<_>>()
            );
            view.update(visual, |view, cx| {
                view.permissions = Some(Ok(
                    serde_json::json!({ "microphone": "granted", "system_audio": "unsupported" }),
                ));
                cx.notify();
            });
            visual.update(|window, cx| {
                let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
                assert!(
                    tree.nodes
                        .iter()
                        .all(|node| node.element_id.as_deref() != Some(id))
                );
            });
        }
    }

    #[test]
    fn microphone_instructions_follow_the_native_platform() {
        assert_eq!(
            microphone_instructions(HostPlatform::MacOs),
            "Allow BroScribe Agent in System Settings › Privacy & Security › Microphone, then try again."
        );
        assert_eq!(
            microphone_instructions(HostPlatform::Windows),
            "Check that Let desktop apps access your microphone is on in Settings › Privacy & security › Microphone, then try again."
        );
        assert_eq!(
            microphone_instructions(HostPlatform::Linux),
            "Check that the microphone isn't muted in your sound settings, then try again."
        );
        assert!(
            system_audio_instructions(HostPlatform::MacOs)
                .contains("Screen & System Audio Recording")
        );
        assert!(system_audio_instructions(HostPlatform::Windows).contains("output device"));
        assert!(system_audio_instructions(HostPlatform::Linux).contains("monitor"));
    }

    #[test]
    fn a_failed_helper_call_is_logged_by_its_method_and_code() {
        let dir = tempfile::tempdir().unwrap();
        let (log, _guard) = crate::test_support::LogCapture::start();
        let call = Call::KickQueue;
        let method = call.method();
        assert!(call.is_mutating());
        let error = helper_call(dir.path(), call, Duration::from_millis(500)).unwrap_err();
        assert_eq!(error, "BroScribe Agent isn't running.");
        let text = log.text();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(
            text.contains("agent request failed")
                && text.contains("method=")
                && text.contains(method)
                && text.contains("code=daemon_unavailable"),
            "{text}"
        );
        let data = dir.path().to_string_lossy().into_owned();
        for raw in [data.as_str(), "os error", "Daemon not reached"] {
            assert!(!text.contains(raw), "{raw} logged: {text}");
        }
    }

    #[test]
    fn resume_and_first_run() {
        let mut config = Config::default();
        assert_eq!(resume_step(&config), Step::Welcome);
        assert!(should_show_first_run(&config));
        assert_eq!(
            Step::DISPLAYED.map(Step::id),
            [
                "models",
                "permissions",
                "voice",
                "try",
                "audio_check",
                "summaries",
                "meeting_apps",
            ]
        );
        assert_eq!(Step::Welcome.next(), Step::Models);
        assert_eq!(Step::Models.next(), Step::Permissions);
        assert_eq!(Step::Permissions.next(), Step::Voice);
        assert_eq!(Step::Voice.next(), Step::TryIt);
        assert_eq!(Step::TryIt.next(), Step::Ready);
        assert_eq!(Step::AudioCheck.next(), Step::Summaries);
        assert_eq!(Step::Summaries.next(), Step::MeetingApps);
        assert_eq!(Step::MeetingApps.next(), Step::Ready);
        config.setup.completed_steps.push("languages".into());
        assert_eq!(resume_step(&config), Step::Models);
        config.setup.defer_assistant = true;
        assert!(!should_show_first_run(&config));
        for (i, step) in Step::DISPLAYED.into_iter().enumerate() {
            assert_eq!(resume_step(&config), step);
            config.setup.completed_steps.push(step.id().into());
            if i + 1 == ESSENTIAL_SETUP_COUNT {
                assert_eq!(resume_step(&config), Step::AudioCheck);
            }
        }
        assert_eq!(resume_step(&config), Step::Overview);
        config.setup.completed_steps.retain(|id| id == "languages");
        config.setup.completed_steps.push(ASSISTANT_DONE.into());
        assert_eq!(resume_step(&config), Step::Overview);
        assert_eq!(Step::ALL.len(), 13);
    }

    #[test]
    fn meeting_apps_step_reads_the_cli() {
        // Daemon-down answer (names and policies) and daemon answer (ids only).
        let apps = parse_meeting_apps(&serde_json::json!({"running": false, "apps": [
            {"app_id": "synthetic-meet", "name": "Synthetic Meet", "policy": "ask"},
            {"app_id": "other", "state": "idle"},
            {"app_id": "bad", "policy": "sometimes"}
        ]}));
        assert_eq!(apps[0].policy.as_deref(), Some("ask"));
        assert_eq!(
            (apps[1].name.as_str(), apps[1].policy.as_deref()),
            ("other", None)
        );
        assert_eq!(apps[2].policy, None);
        // Detection off: the rule list stays, with an "off" note instead of
        // "no supported app".
        let off = serde_json::json!({"enabled": false, "apps": [
            {"app_id": "synthetic-meet", "name": "Synthetic Meet", "policy": "auto"}
        ]});
        assert!(detection_off(&off));
        assert!(!detection_off(
            &serde_json::json!({"running": false, "apps": []})
        ));
        let off_apps = parse_meeting_apps(&off);
        assert_eq!(off_apps.len(), 1);
        for apps in [&off_apps[..], &[]] {
            let notice = meeting_apps_notice(apps, true).unwrap();
            assert!(notice.contains("detection is off"), "{notice}");
        }
        assert_eq!(
            meeting_apps_notice(&[], false),
            Some("No supported meeting app is configured.")
        );
        assert_eq!(meeting_apps_notice(&off_apps, false), None);
        let a: Vec<String> = detect_policy_args("synthetic-meet", "off")
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        assert_eq!(a, ["detect", "policy", "synthetic-meet", "off"]);
    }

    /// Summaries enables a provider inline: the registry consent text first,
    /// then the canonical `enable --accept`, with AppState's config and the
    /// provider status reloaded, while the assistant keeps its route, step
    /// and typed input. Back still works, and leaving the step drops an
    /// API-key draft and an open consent question.
    #[gpui::test]
    fn summaries_enable_a_provider_inline_after_consent(cx: &mut TestAppContext) {
        use std::cell::Cell;
        use std::rc::Rc;

        use crate::views::providers::tests::{
            AGENT, agent_consented, args_log, providers_state, stage_enabled_agent,
        };

        let dir = tempfile::tempdir().unwrap();
        stage_enabled_agent(dir.path());
        let state = providers_state(cx, dir.path());
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state.clone(), fake_deps(), window, cx));
        let assistant = window.root(cx).unwrap();
        let closed = Rc::new(Cell::new(false));
        let _closed_sub = cx.update(|cx| {
            let closed = closed.clone();
            cx.subscribe(&assistant, move |_, _: &SetupEvent, _| closed.set(true))
        });
        window
            .update(cx, |this, window, cx| {
                this.language
                    .update(cx, |input, cx| input.set_value("pt-BR", window, cx));
                this.go_to(Step::Summaries, cx);
            })
            .unwrap();
        cx.run_until_parked();
        let providers = window
            .update(cx, |this, _, _| this.providers.clone())
            .unwrap();
        providers.read_with(cx, |p, _| {
            assert_eq!(p.scope(), ProviderScope::Llm);
            let rows: Vec<_> = p
                .rows()
                .map(|r| (r.id.as_str(), r.name.as_str(), r.status.as_deref()))
                .collect();
            assert_eq!(
                rows,
                [
                    (AGENT, "Synthetic Agent", Some("not_found")),
                    ("remote", "remote", None)
                ]
            );
        });
        // Without the human accepting the consent text nothing is enabled.
        providers.update(cx, |p, cx| p.accept_consent(AGENT.into(), cx));
        cx.run_until_parked();
        assert!(!args_log(dir.path()).contains("providers enable"));
        assert!(!agent_consented(&state, cx));
        let listings = |log: &str| {
            log.lines()
                .filter(|l| l.ends_with("providers list"))
                .count()
        };
        let listed_before = listings(&args_log(dir.path()));
        providers.update(cx, |p, cx| {
            p.request_consent(AGENT.into(), cx);
            p.accept_consent(AGENT.into(), cx);
        });
        cx.run_until_parked();
        let log = args_log(dir.path());
        assert_eq!(
            log.matches(&format!("--json providers enable {AGENT} --accept\n"))
                .count(),
            1,
            "{log}"
        );
        assert!(
            agent_consented(&state, cx),
            "AppState took in the new config"
        );
        assert_eq!(listings(&log), listed_before + 1, "status relisted");
        providers.read_with(cx, |p, _| {
            assert_eq!(
                p.result(AGENT, crate::views::providers::ProviderControl::Consent),
                Some(&Ok("Enabled.".to_owned()))
            );
        });
        // Still the same assistant, step, input and route.
        assert!(!closed.get());
        assert!(state.read_with(cx, |s, _| *s.route() == Route::library()));
        window
            .update(cx, |this, window, cx| {
                assert_eq!(this.step(), Step::Summaries);
                assert_eq!(this.language.read(cx).value().to_string(), "pt-BR");
                this.providers.update(cx, |p, cx| {
                    p.open_key_form("remote".into(), window, cx);
                    p.request_consent("remote".into(), cx);
                });
                // Back.
                if let Some(prev) = this.step.prev() {
                    this.go_to(prev, cx);
                }
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                assert_eq!(this.step(), Step::AudioCheck);
                let p = this.providers.read(cx);
                assert_eq!((p.key_form_provider(), p.pending_consent()), (None, None));
            })
            .unwrap();
        assert!(!closed.get());
        assert!(!args_log(dir.path()).contains("set-key"));
    }

    #[test]
    fn permission_wait_outlasts_helper_prompt() {
        // The helper waits up to 120 s for the user's answer.
        const { assert!(PERMISSION_WAIT.as_secs() > 120) };
    }

    /// The Voice step refuses, before anything runs, a typed name the
    /// library refuses: more than one line, control or bidirectional
    /// characters, or more than 200 characters. An empty name still leaves
    /// Me's name alone, and a valid one is kept trimmed.
    #[test]
    fn me_fields_refuse_a_name_the_library_refuses() {
        for bad in [
            "Synthetic\nMe",
            "Synthetic\u{0007}Me",
            "Synthetic \u{202E}eM",
            "Synthetic \u{2066}Me\u{2069}",
        ] {
            let err = me_fields(bad, "me@example.org").unwrap_err();
            assert!(
                matches!(&err, IdentityError::Name(text) if text.contains("control characters")),
                "{bad:?}: {err:?}"
            );
        }
        let long = "S".repeat(201);
        let err = me_fields(&long, "").unwrap_err();
        assert!(
            matches!(&err, IdentityError::Name(text) if text.contains("at most 200 characters")),
            "{err:?}"
        );
        assert_eq!(
            me_fields(&"S".repeat(200), "").unwrap(),
            ("S".repeat(200), vec![])
        );
        assert_eq!(
            me_fields("   ", "me@example.org").unwrap(),
            (String::new(), vec!["me@example.org".to_owned()])
        );
        assert_eq!(
            me_fields(" Synthetic Me ", "").unwrap(),
            ("Synthetic Me".to_owned(), vec![])
        );
    }

    #[test]
    fn enrollment_sends_the_typed_name_and_emails_on_stdin_only() {
        assert!(me_fields("Synthetic Me", "not-an-email").is_err());
        let (name, emails) =
            me_fields(" Synthetic Me ", "me@example.org; alt@example.org").unwrap();
        let a: Vec<String> = voice_args(Path::new("/tmp/passage.wav"), None, "session-1", None)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        assert_eq!(
            a,
            [
                "setup",
                "voice",
                "--from-file",
                "/tmp/passage.wav",
                "--session",
                "session-1",
                "--keep-as-enrollment",
                "--spec-stdin",
            ]
        );
        let spec: serde_json::Value =
            serde_json::from_slice(&identity_spec(&name, &emails)).unwrap();
        assert_eq!(
            spec,
            serde_json::json!({
                "name": "Synthetic Me",
                "emails": ["me@example.org", "alt@example.org"]
            })
        );
        // Nothing typed: no name, so the CLI keeps the saved Me. Without the
        // phrase there is nothing to report as unconfirmed.
        let spec: serde_json::Value = serde_json::from_slice(&identity_spec("", &[])).unwrap();
        assert_eq!(spec, serde_json::json!({ "emails": [] }));
        let a = voice_args(Path::new("/p.wav"), None, "s", None);
        assert!(!a.iter().any(|a| a == "--report-unconfirmed"));
        let a = voice_args(Path::new("/p.wav"), Some(Path::new("/c.wav")), "s", None);
        assert!(a.iter().any(|a| a == "--report-unconfirmed"));
    }

    #[test]
    fn permission_status_does_not_wait_like_a_prompt() {
        let (call, timeout) = permission_call(Some("microphone"));
        assert_eq!(
            call,
            Call::PermissionsRequest {
                permission: "microphone".into()
            }
        );
        assert_eq!(timeout, PERMISSION_WAIT);
        let (call, timeout) = permission_call(None);
        assert_eq!(call, Call::PermissionsStatus);
        assert!(timeout < Duration::from_secs(30));
    }

    fn acoustic_report(delay_ms: Option<f64>, leakage_db: Option<f64>) -> AudioCheckResult {
        use broscribe_platform::{DeviceTransport, devices::FakeDeviceInfo};
        let mut output = FakeDeviceInfo::device("out", DeviceTransport::BuiltIn, false, true);
        output.output_data_source = Some("Speakers".into());
        let devices = FakeDeviceInfo::new(vec![
            FakeDeviceInfo::device("mic", DeviceTransport::BuiltIn, true, false),
            output,
        ]);
        run_audio_check(&devices, |_, _| {
            Ok(serde_json::json!({
                "delay_ms": delay_ms, "leakage_db": leakage_db,
                "recommendation": "none", "bluetooth_call_profile": false
            }))
        })
        .unwrap()
    }

    fn acoustic_window(
        cx: &mut TestAppContext,
        root: &Path,
        report: AudioCheckResult,
    ) -> (Entity<AppState>, gpui::WindowHandle<SetupAssistant>) {
        let state = new_state(cx, root);
        let data = state.read_with(cx, |s, _| s.paths().data_dir.clone());
        let mut config = Config::default();
        config.setup.completed_steps = ["languages", "permissions", "models", "voice"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        config.set_audio_check(audio_check_choice(&report, "full_aec"));
        config.save(&data).unwrap();
        state.update(cx, |s, cx| s.set_config(config, cx));
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state.clone(), fake_deps(), window, cx));
        window
            .update(cx, |this, _, _| {
                this.step = Step::AudioCheck;
                this.audio_check = Some(Ok(report));
            })
            .unwrap();
        (state, window)
    }

    #[gpui::test]
    fn unmeasured_check_continue_completes_without_recalibrating(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (state, window) = acoustic_window(cx, dir.path(), acoustic_report(None, None));
        let data = state.read_with(cx, |s, _| s.paths().data_dir.clone());
        let old = Config::load(&data).unwrap().audio_checks;
        window
            .update(cx, |this, _, cx| this.continue_step(cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| assert_eq!(this.step, Step::Summaries))
            .unwrap();
        let config = Config::load(&data).unwrap();
        assert_eq!(config.audio_checks, old);
        assert_eq!(resume_step(&config), Step::TryIt);
        window
            .update(cx, |this, _, cx| {
                this.step = Step::AudioCheck;
                this.continue_step(cx);
            })
            .unwrap();
        cx.run_until_parked();
        let config = Config::load(&data).unwrap();
        assert_eq!(config.audio_checks, old);
        assert_eq!(
            config
                .setup
                .completed_steps
                .iter()
                .filter(|s| *s == "audio_check")
                .count(),
            1
        );
    }

    #[gpui::test]
    fn measured_check_continue_saves_choice_and_progress(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (state, window) =
            acoustic_window(cx, dir.path(), acoustic_report(Some(12.5), Some(-30.0)));
        let data = state.read_with(cx, |s, _| s.paths().data_dir.clone());
        window
            .update(cx, |this, _, cx| {
                this.echo_choice = Some("residue_gating".into());
                this.continue_step(cx);
            })
            .unwrap();
        cx.run_until_parked();
        let config = Config::load(&data).unwrap();
        assert_eq!(config.audio_checks.len(), 1);
        assert_eq!(config.audio_checks[0].echo_handling, "residue_gating");
        assert_eq!(resume_step(&config), Step::TryIt);
    }

    /// The assistant's echo handling, microphone, voice-sample language and
    /// meeting-app policy choices read as labelled radio groups whose
    /// options say which one is selected, following the assistant's state
    /// (UI26).
    #[gpui::test]
    fn setup_choices_read_as_radio_groups_with_their_selection(cx: &mut TestAppContext) {
        use crate::views::settings::{ChoiceNodes, choice_nodes};

        let dir = tempfile::tempdir().unwrap();
        let (_state, window) =
            acoustic_window(cx, dir.path(), acoustic_report(Some(12.5), Some(-30.0)));
        let view = window.root(cx).unwrap();
        let set = |cx: &mut TestAppContext, echo: &str, device: &str, lang: &str, policy: &str| {
            window
                .update(cx, |this, _, _| {
                    this.echo_choice = Some(echo.to_owned());
                    this.device = Some(device.to_owned());
                    this.passage = Some(Ok(Passage {
                        lang: lang.to_owned(),
                        ..synthetic_passage()
                    }));
                    this.meeting_apps = Some(Ok(vec![MeetingApp {
                        id: "synthetic-meet".into(),
                        name: "Synthetic Meet".into(),
                        policy: Some(policy.to_owned()),
                        installed: Some(true),
                    }]));
                })
                .unwrap();
        };
        let read = |cx: &mut TestAppContext| {
            let view = view.clone();
            choice_nodes(cx, move |_, cx| {
                view.update(cx, |this, cx| {
                    let recommendation = match &this.audio_check {
                        Some(Ok(result)) => result.recommendation.clone(),
                        _ => panic!("a measured check"),
                    };
                    let app = match &this.meeting_apps {
                        Some(Ok(apps)) => apps[0].clone(),
                        _ => panic!("a meeting app"),
                    };
                    vec![
                        this.echo_choices(&recommendation, cx),
                        this.voice_language_choices(cx),
                        this.detect_choices(&app, cx),
                    ]
                })
            })
        };
        let expected = |echo: [bool; 3], lang: [bool; 3], policy: [bool; 3]| {
            vec![
                ChoiceNodes::radios(
                    "echo-handling",
                    "How to handle echo",
                    &[
                        (
                            "echo-none",
                            "Leave as is, recommended, Leaves your track as recorded.",
                            echo[0],
                        ),
                        (
                            "echo-residue_gating",
                            "Mute leftover echo, Quiets your track where it only picks up the other side.",
                            echo[1],
                        ),
                        (
                            "echo-full_aec",
                            "Cancel echo fully, Removes the other side's sound from your track as you speak.",
                            echo[2],
                        ),
                    ],
                ),
                ChoiceNodes::radios(
                    "voice-language",
                    "Reading language",
                    &[
                        ("voice-language-en", "English", lang[0]),
                        ("voice-language-pt-BR", "Português (Brasil)", lang[1]),
                        ("voice-language-pt-PT", "Português (Portugal)", lang[2]),
                    ],
                ),
                ChoiceNodes::radios(
                    "detect-synthetic-meet",
                    "When Synthetic Meet starts a call",
                    &[
                        (
                            "detect-synthetic-meet-auto",
                            "Record automatically",
                            policy[0],
                        ),
                        ("detect-synthetic-meet-ask", "Ask first", policy[1]),
                        ("detect-synthetic-meet-off", "Don't record", policy[2]),
                    ],
                ),
            ]
        };

        set(cx, "none", "synthetic-builtin", "en", "ask");
        assert_eq!(
            read(cx),
            expected(
                [true, false, false],
                [true, false, false],
                [false, true, false]
            )
        );
        set(cx, "full_aec", "synthetic-usb", "pt-PT", "off");
        assert_eq!(
            read(cx),
            expected(
                [false, false, true],
                [false, false, true],
                [false, false, true]
            )
        );
    }

    #[gpui::test]
    fn missing_or_failed_check_never_marks_completion(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (state, window) = acoustic_window(cx, dir.path(), acoustic_report(None, None));
        let data = state.read_with(cx, |s, _| s.paths().data_dir.clone());
        for report in [None, Some(Err("synthetic capture error".into()))] {
            window
                .update(cx, |this, _, cx| {
                    this.step = Step::AudioCheck;
                    this.audio_check = report;
                    this.continue_step(cx);
                })
                .unwrap();
            cx.run_until_parked();
            assert!(
                !Config::load(&data)
                    .unwrap()
                    .setup
                    .completed_steps
                    .iter()
                    .any(|s| s == "audio_check")
            );
        }
    }

    #[gpui::test]
    fn pending_audio_save_cannot_navigate_or_advance_another_step(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (_state, window) = acoustic_window(cx, dir.path(), acoustic_report(None, None));
        window
            .update(cx, |this, _, cx| {
                this.audio_checking = true;
                this.continue_step(cx);
                assert!(!this.busy);
                assert_eq!(this.step, Step::AudioCheck);
                this.audio_checking = false;
                this.continue_step(cx);
                assert!(this.busy);
                this.go_to(Step::Words, cx);
                this.skip(cx);
                this.start_audio_check(cx);
                assert_eq!(this.step, Step::AudioCheck);
                assert!(!this.audio_checking);
                // An external route change must not be advanced by the old callback.
                this.step = Step::Words;
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert_eq!(this.step, Step::Words);
                assert!(!this.busy);
            })
            .unwrap();
    }

    #[gpui::test]
    fn failed_audio_progress_write_keeps_step_and_calibration(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (state, window) = acoustic_window(cx, dir.path(), acoustic_report(None, None));
        let data = state.read_with(cx, |s, _| s.paths().data_dir.clone());
        std::fs::remove_file(data.join("config.toml")).unwrap();
        std::fs::create_dir(data.join("config.toml")).unwrap();
        window
            .update(cx, |this, _, cx| this.continue_step(cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                assert_eq!(this.step, Step::AudioCheck);
                assert!(!this.busy);
                assert!(this.progress_error.is_some());
                let config = this.state.read(cx).config();
                assert!(
                    !config
                        .setup
                        .completed_steps
                        .iter()
                        .any(|s| s == "audio_check")
                );
                assert_eq!(config.audio_checks[0].echo_handling, "full_aec");
            })
            .unwrap();
    }

    #[test]
    fn unmeasured_audio_check_saves_nothing() {
        use broscribe_platform::DeviceTransport;
        use broscribe_platform::devices::FakeDeviceInfo;
        let devices = FakeDeviceInfo::new(vec![
            FakeDeviceInfo::device("mic", DeviceTransport::BuiltIn, true, false),
            FakeDeviceInfo::device("out", DeviceTransport::BuiltIn, false, true),
        ]);
        let report = |delay: Value, leakage: Value| {
            serde_json::json!({
                "delay_ms": delay, "leakage_db": leakage, "correlation_peak": null,
                "noise_floor_dbfs": -70.0, "recommendation": "residue_gating",
                "bluetooth_call_profile": false
            })
        };
        for (delay, leakage) in [
            (Value::Null, Value::Null),
            (12.5.into(), Value::Null),
            (Value::Null, (-30.0).into()),
        ] {
            let value = report(delay, leakage);
            let result = run_audio_check(&devices, |_, _| Ok(value)).unwrap();
            assert!(!result.measured());
            assert!(audio_check_to_save(&result, Some("full_aec")).is_none());
            assert!(audio_check_to_save(&result, None).is_none());
        }
        // A real measurement saves the user's choice, else the recommendation.
        let measured = report(12.5.into(), (-30.0).into());
        let result = run_audio_check(&devices, |_, _| Ok(measured)).unwrap();
        assert_eq!(
            audio_check_to_save(&result, Some("full_aec")).map(|c| c.echo_handling),
            Some("full_aec".to_owned())
        );
        assert_eq!(
            audio_check_to_save(&result, None).map(|c| c.echo_handling),
            Some("residue_gating".to_owned())
        );
    }

    #[test]
    fn audio_check_ties_recommendation_to_device_pair_and_saves_choice() {
        use broscribe_platform::DeviceTransport;
        use broscribe_platform::devices::FakeDeviceInfo;
        let mut speakers = FakeDeviceInfo::device("out", DeviceTransport::BuiltIn, false, true);
        speakers.output_data_source = Some("Speakers".into());
        let devices = FakeDeviceInfo::new(vec![
            FakeDeviceInfo::device("mic", DeviceTransport::BuiltIn, true, false),
            speakers.clone(),
        ]);
        let report = serde_json::json!({
            "delay_ms": 12.5, "leakage_db": -30.0, "correlation_peak": 0.4,
            "noise_floor_dbfs": -70.0, "recommendation": "residue_gating",
            "bluetooth_call_profile": false, "setup": "speakers_mic"
        });
        let mut asked = None;
        let result = run_audio_check(&devices, |call, timeout| {
            asked = Some((call, timeout));
            Ok(report.clone())
        })
        .unwrap();
        let (call, timeout) = asked.unwrap();
        assert_eq!(
            call,
            Call::AudioCheck {
                seconds: AUDIO_CHECK_SECONDS
            }
        );
        assert!(timeout > Duration::from_secs_f64(AUDIO_CHECK_SECONDS));
        assert_eq!(result.recommendation, "residue_gating");
        assert_eq!(result.delay_ms, Some(12.5));
        assert_eq!(result.setup.as_deref(), Some("speakers_mic"));
        assert_eq!(setup_label("speakers_mic"), "Speakers and microphone");
        assert_eq!(setup_label("future_setup"), "future_setup");
        // The saved choice is what the daemon looks up at record start.
        let mut config = Config::default();
        config.set_audio_check(audio_check_choice(&result, "full_aec"));
        assert_eq!(
            config.audio_check_for("mic", "out", Some("Speakers")),
            Some("full_aec")
        );
        assert_eq!(
            config.audio_check_for("mic", "out", Some("Headphones")),
            None
        );
        // Devices changing mid-check void the measurement.
        let mut headphones = speakers.clone();
        headphones.output_data_source = Some("Headphones".into());
        let error = run_audio_check(&devices, |_, _| {
            devices.set(vec![
                FakeDeviceInfo::device("mic", DeviceTransport::BuiltIn, true, false),
                headphones,
            ]);
            Ok(report.clone())
        })
        .unwrap_err();
        assert!(error.contains("changed"));
        // An unknown recommendation is refused rather than saved.
        let mut bogus = report.clone();
        bogus["recommendation"] = "bogus".into();
        assert!(run_audio_check(&devices, |_, _| Ok(bogus)).is_err());
        // Helper errors surface as-is.
        assert_eq!(
            run_audio_check(&devices, |_, _| Err("busy".into())).unwrap_err(),
            "busy"
        );
    }

    #[test]
    fn wav_header_and_meter() {
        let bytes = wav_bytes(&[0.0, 1.0, -1.0], 16_000);
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(bytes.len(), 44 + 6);
        assert_eq!(i16::from_le_bytes([bytes[46], bytes[47]]), i16::MAX);
        assert_eq!(kit::meter_fraction(MeterLevel::default().rms_dbfs()), 0.0);
        assert!(
            (kit::meter_fraction(MeterLevel { peak: 1., rms: 1. }.rms_dbfs()) - 1.0).abs() < 1e-6
        );
    }

    fn setup_levels(
        session: Option<&str>,
        me_dbfs: f32,
        others: Option<f32>,
    ) -> broscribe_ipc::Event {
        let level = |dbfs| broscribe_ipc::SetupLevel {
            peak_dbfs: dbfs,
            rms_dbfs: dbfs - 6.0,
        };
        broscribe_ipc::Event::SetupLevels {
            session: session.map(str::to_owned),
            me: level(me_dbfs),
            others: others.map(level),
        }
    }

    #[test]
    fn helper_level_events_map_to_meter_state() {
        let mut meters = HelperMeters::default();
        let event = setup_levels(Some("sample-1"), -6.0, None);
        assert!(meters.apply(Some("sample-1"), &event));
        let me = meters.me.unwrap();
        assert!((me.peak - 0.501).abs() < 0.01, "{me:?}");
        assert!((kit::meter_fraction(me.rms_dbfs()) - 0.8).abs() < 0.01);
        assert_eq!(meters.others, None);
        // The same levels again: nothing to redraw.
        assert!(!meters.apply(Some("sample-1"), &event));
        // Another session's levels, the audio check's, or other events are ignored.
        for other in [
            setup_levels(Some("sample-2"), -1.0, None),
            setup_levels(None, -1.0, Some(-3.0)),
            broscribe_ipc::Event::Lagged { dropped: 1 },
        ] {
            assert!(!meters.apply(Some("sample-1"), &other));
        }
        assert_eq!(meters.me, Some(me));
        // Silence empties the meter; a clipped peak trips the warning level.
        assert!(meters.apply(
            Some("sample-1"),
            &setup_levels(Some("sample-1"), -120.0, None)
        ));
        assert_eq!(kit::meter_fraction(meters.me.unwrap().rms_dbfs()), 0.0);
        assert!(meters.apply(Some("sample-1"), &setup_levels(Some("sample-1"), 0.0, None)));
        assert!(meters.me.unwrap().peak >= 0.99);
        // The audio check takes only untagged levels, with the test sound as Others.
        let mut check = HelperMeters::default();
        assert!(!check.apply(None, &setup_levels(Some("sample-1"), -6.0, None)));
        assert!(check.apply(None, &setup_levels(None, -20.0, Some(-6.0))));
        assert!(check.others.unwrap().rms > check.me.unwrap().rms);
    }

    #[gpui::test]
    fn helper_levels_reach_only_the_running_capture_or_check(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let deps = SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        cx.update(gpui::init);
        let window = cx.add_window(|window, cx| SetupAssistant::new(state, deps, window, cx));
        window
            .update(cx, |this, _, cx| {
                let received = DaemonEvent::Received;
                // Nothing running: levels are dropped.
                this.helper_event(&received(setup_levels(None, -6.0, Some(-6.0))), cx);
                assert_eq!(this.audio_check_meters, HelperMeters::default());
                // A helper capture shows its own session's levels.
                let mut capture =
                    ActiveCapture::new(1, CapturePurpose::Passage, None, Some("sample-1".into()));
                capture.running = true;
                this.capture = Some(capture);
                this.helper_event(&received(setup_levels(Some("sample-2"), -6.0, None)), cx);
                assert_eq!(this.capture.as_ref().unwrap().helper_meters.me, None);
                this.helper_event(&received(setup_levels(Some("sample-1"), -6.0, None)), cx);
                assert!(this.capture.as_ref().unwrap().helper_meters.me.is_some());
                assert!(this.render_meter(cx).is_some());
                this.capture = None;
                // The audio check shows Me and Others.
                this.audio_checking = true;
                this.helper_event(&received(setup_levels(None, -20.0, Some(-6.0))), cx);
                assert!(this.audio_check_meters.me.is_some());
                assert!(this.audio_check_meters.others.is_some());
                this.audio_checking = false;
            })
            .unwrap();
    }

    #[test]
    fn recorder_is_bounded() {
        let mut r = Recorder::default();
        let chunk = MicChunk::new(vec![0.5; 1000], 1000);
        for _ in 0..5 {
            r.push(&chunk, Duration::from_secs(2));
        }
        assert_eq!(r.samples.len(), 2000);
        assert!(r.full());
    }

    #[test]
    fn terms_are_the_distinct_non_empty_lines() {
        assert_eq!(
            terms(" Synthetica \n\nKubernetes\nSynthetica\n  \n"),
            ["Synthetica", "Kubernetes"]
        );
        assert!(terms("\n \n").is_empty());
    }

    fn words_window<'a>(
        cx: &'a mut TestAppContext,
        dir: &Path,
    ) -> (Entity<SetupAssistant>, &'a mut gpui::VisualTestContext) {
        let cli = crate::views::agents::tests::synthetic_agents_cli(dir);
        let state = state_with_cli(cx, dir, cli);
        cx.update(gpui::init);
        let (view, cx) =
            cx.add_window_view(|window, cx| SetupAssistant::new(state, fake_deps(), window, cx));
        cx.simulate_resize(gpui::size(px(800.), px(900.)));
        (view, cx)
    }

    /// Draws the window's current state.
    fn redraw(cx: &mut gpui::VisualTestContext) {
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    #[gpui::test]
    fn words_count_live_and_the_field_clears_after_saving(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = words_window(cx, dir.path());
        view.update(cx, |this, cx| this.go_to(Step::Words, cx));
        cx.run_until_parked();
        view.update_in(cx, |this, window, cx| {
            assert_eq!(this.words_count, 0);
            this.words.update(cx, |words, cx| {
                words.replace_all("Synthetica\n\n Kubernetes \nSynthetica\n", window, cx)
            });
        });
        // The change event updated the count.
        view.read_with(cx, |this, _| assert_eq!(this.words_count, 2));
        view.update_in(cx, |this, window, cx| {
            this.save_words(window, cx);
            assert!(this.busy);
        });
        cx.run_until_parked();
        view.read_with(cx, |this, cx| {
            assert!(!this.busy);
            assert!(matches!(this.legacy_outcome, Some(Outcome::Ok(_))));
            assert!(this.words.read(cx).value().is_empty());
            assert_eq!(this.words_count, 0);
        });
        let commands = crate::views::agents::tests::commands(dir.path());
        assert!(
            commands
                .iter()
                .any(|c| c.starts_with("setup --non-interactive --words-file ")),
            "{commands:?}"
        );
        // A failed save keeps what was typed.
        std::fs::remove_file(crate::test_support::native_cli_path(
            &dir.path().join("synthetic-cli"),
        ))
        .unwrap();
        view.update_in(cx, |this, window, cx| {
            this.words
                .update(cx, |words, cx| words.replace_all("Synthetica", window, cx));
            this.save_words(window, cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |this, cx| {
            assert!(matches!(this.legacy_outcome, Some(Outcome::Err(_))));
            assert_eq!(&*this.words.read(cx).value(), "Synthetica");
            assert_eq!(this.words_count, 1);
        });
    }

    /// CL02: the contacts file and the manifest the user picks reach their CLI
    /// children only as the stdin spec; argv names neither.
    #[gpui::test]
    fn picked_contacts_and_manifest_paths_reach_their_children_only_on_stdin(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display();
        let cli = write_cli(
            &dir.path().join("synthetic-cli"),
            &format!(
                "printf '%s\\n' \"$*\" >> '{d}/args.log'\n\
                 case \"$*\" in\n\
                 *' --json setup --non-interactive --spec-stdin') cat > '{d}/people-spec.json'; \
                   echo '{{\"people\":{{\"contacts\":2,\"created\":1,\"updated\":1}}}}' ;;\n\
                 *' --json import --spec-stdin') cat > '{d}/import-spec.json'; echo '{{}}' ;;\n\
                 *) echo '{{}}' ;;\n\
                 esac\n"
            ),
            &format!(
                "fx_log_args {log};if($all -clike '* --json setup --non-interactive --spec-stdin'){{fx_save_stdin {people};fx_echo {reply}}}elseif($all -clike '* --json import --spec-stdin'){{fx_save_stdin {manifest};fx_echo '{{}}'}}else{{fx_echo '{{}}'}}",
                log = ps_path(&dir.path().join("args.log")),
                people = ps_path(&dir.path().join("people-spec.json")),
                manifest = ps_path(&dir.path().join("import-spec.json")),
                reply = ps_str(r#"{"people":{"contacts":2,"created":1,"updated":1}}"#),
            ),
        );
        let state = state_with_cli(cx, dir.path(), cli);
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state.clone(), fake_deps(), window, cx));
        let people =
            cx.add_window(|window, cx| super::super::people::PeopleView::new(state, window, cx));
        cx.run_until_parked();
        let contacts = dir
            .path()
            .join("synthetic.owner@example.invalid contacts.vcf");
        let manifest = dir.path().join("synthetic-private-manifest.json");

        people
            .update(cx, |this, _, cx| this.import_contacts(cx))
            .unwrap();
        assert!(cx.did_prompt_for_paths());
        let chosen = contacts.clone();
        cx.simulate_path_prompt_response(move |_| Some(vec![chosen]));
        cx.run_until_parked();
        people
            .update(cx, |this, _, _| {
                assert_eq!(
                    this.import_contacts_result.as_ref().unwrap().as_deref(),
                    Ok("Imported 2 contacts · 1 new · 1 updated")
                );
            })
            .unwrap();

        window
            .update(cx, |this, _, cx| this.import_manifest(cx))
            .unwrap();
        assert!(cx.did_prompt_for_paths());
        let chosen = manifest.clone();
        cx.simulate_path_prompt_response(move |_| Some(vec![chosen]));
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert!(!this.busy);
                assert!(
                    matches!(&this.legacy_outcome, Some(Outcome::Ok(text)) if text.starts_with("The manifest's meetings are queued")),
                    "{:?}",
                    this.legacy_outcome
                );
            })
            .unwrap();

        let commands = crate::views::agents::tests::commands(dir.path());
        assert!(
            commands
                .iter()
                .any(|c| c == "setup --non-interactive --spec-stdin"),
            "{commands:?}"
        );
        assert!(
            commands.iter().any(|c| c == "import --spec-stdin"),
            "{commands:?}"
        );
        let argv = crate::views::agents::tests::args_log(dir.path());
        assert!(
            !argv.contains("synthetic.owner@example.invalid")
                && !argv.contains("synthetic-private-manifest")
                && !argv.contains("--people-file")
                && !argv.contains("--manifest"),
            "{argv}"
        );
        let spec = |name: &str| -> serde_json::Value {
            serde_json::from_slice(&std::fs::read(dir.path().join(name)).unwrap()).unwrap()
        };
        assert_eq!(
            spec("people-spec.json"),
            serde_json::json!({"people_file": contacts.to_str().unwrap()})
        );
        assert_eq!(
            spec("import-spec.json"),
            serde_json::json!({"manifest": manifest.to_str().unwrap()})
        );
    }

    #[gpui::test]
    fn words_note_follows_the_registry_data(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let models = |active: &str| {
            let row = |id: &str, hints: bool| {
                let active_for: Vec<&str> = if id == active { vec!["asr"] } else { vec![] };
                serde_json::json!({"id": id, "display_name": if hints { "Clover Voice" } else { "Acorn Voice" }, "roles": ["asr"], "accepts_hotwords": hints,
                                   "active_for": active_for})
            };
            let profile = |id: &str, asr: &str| serde_json::json!({"id": id, "asr": asr, "active": asr == active});
            serde_json::json!({
                "models": [row("synthetic-asr-plain", false), row("synthetic-asr-hints", true)],
                "profiles": [profile("synthetic-plain", "synthetic-asr-plain"),
                             profile("synthetic-hints", "synthetic-asr-hints")]
            })
            .to_string()
        };
        std::fs::write(
            dir.path().join("models.json"),
            models("synthetic-asr-plain"),
        )
        .unwrap();
        let (view, cx) = words_window(cx, dir.path());
        view.update(cx, |this, cx| this.go_to(Step::Words, cx));
        cx.run_until_parked();
        view.read_with(cx, |this, cx| {
            let Some(Ok(list)) = &this.models else {
                panic!("models not listed");
            };
            let note = hotword_note(list, &this.state.read(cx).config().profile.asr).unwrap();
            assert_eq!(note.model, "Acorn Voice");
            assert_eq!(note.profiles, ["Clover Voice"]);
        });
        redraw(cx);
        assert!(cx.debug_bounds("words-model-note").is_some());

        // A model that takes hints: no note.
        std::fs::write(
            dir.path().join("models.json"),
            models("synthetic-asr-hints"),
        )
        .unwrap();
        view.update(cx, |this, cx| {
            this.go_to(Step::MeetingApps, cx);
            this.go_to(Step::Words, cx);
        });
        cx.run_until_parked();
        redraw(cx);
        assert!(cx.debug_bounds("words-model-note").is_none());
    }

    #[gpui::test]
    fn agents_step_shows_the_shared_view(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, cx) = words_window(cx, dir.path());
        view.update(cx, |this, cx| this.go_to(Step::Agents, cx));
        cx.run_until_parked();
        assert_eq!(
            crate::views::agents::tests::commands(dir.path()),
            ["agents status"]
        );
        view.read_with(cx, |this, cx| {
            assert!(matches!(this.agents.read(cx).status(), Some(Ok(_))));
        });
        redraw(cx);
        assert!(cx.debug_bounds("agents-actions").is_some());
        assert!(cx.debug_bounds("mcp-actions").is_some());
    }

    #[test]
    fn invalidating_a_session_clears_only_its_own_token() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let mut c = Config::default();
        c.setup.voice_session = Some("newer".into());
        c.save(&data).unwrap();
        assert!(!invalidate_session(&lib, &data, "older", Some(&BTreeSet::new())).unwrap());
        assert_eq!(
            Config::load(&data).unwrap().setup.voice_session.as_deref(),
            Some("newer")
        );
        assert!(!invalidate_session(&lib, &data, "newer", Some(&BTreeSet::new())).unwrap());
        assert_eq!(Config::load(&data).unwrap().setup.voice_session, None);
    }

    #[gpui::test]
    fn voice_language_is_independent_and_rejects_stale_replies(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let cli = write_cli(
            &dir.path().join("synthetic-cli"),
            "lang=\"${8:-en}\"\nprintf '{\"lang\":\"%s\",\"passage\":\"synthetic reading\",\"confirmation_phrase\":\"synthetic phrase\"}\\n' \"$lang\"\n",
            "$lang=fx_arg 8;if(-not $lang){$lang='en'};fx_echo ([ordered]@{lang=$lang;passage='synthetic reading';confirmation_phrase='synthetic phrase'}|ConvertTo-Json -Compress)",
        );
        let state = state_with_cli(cx, dir.path(), cli);
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state.clone(), fake_deps(), window, cx));
        window
            .update(cx, |this, _, cx| {
                this.step = Step::Voice;
                this.load_passage(cx);
                let old = this.passage_generation;
                this.choose_passage_language("pt-BR", cx);
                this.passage_loaded(old, Ok(serde_json::json!({
                "lang": "en", "passage": "stale reading", "confirmation_phrase": "stale phrase"
            })), cx);
                assert!(this.passage.is_none());
                this.passage_loaded(
                    old,
                    Err(CliError::answered("synthetic", "stale failure")),
                    cx,
                );
                assert!(this.passage.is_none());
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, window, cx| {
                assert_eq!(
                    this.passage.as_ref().unwrap().as_ref().unwrap().lang,
                    "pt-BR"
                );
                assert_eq!(this.state.read(cx).config().default_language, None);
                this.language
                    .update(cx, |input, cx| input.set_value("en", window, cx));
                this.save_language(false, cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, window, cx| {
                assert_eq!(
                    this.state.read(cx).config().default_language.as_deref(),
                    Some("en")
                );
                assert_eq!(
                    this.passage.as_ref().unwrap().as_ref().unwrap().lang,
                    "pt-BR"
                );
                // Without a manual choice, a changed meeting default invalidates
                // in-flight replies and loads the new default.
                this.passage_language = None;
                this.load_passage(cx);
                this.language
                    .update(cx, |input, cx| input.set_value("pt-PT", window, cx));
                this.save_language(false, cx);
            })
            .unwrap();
        let old = window
            .update(cx, |this, _, _| this.passage_generation)
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                this.passage_loaded(
                    old,
                    Err(CliError::answered(
                        "synthetic",
                        "late failure before saving language",
                    )),
                    cx,
                );
                assert_eq!(
                    this.passage.as_ref().unwrap().as_ref().unwrap().lang,
                    "pt-PT"
                );
                assert!(this.capture.is_none());
            })
            .unwrap();
        assert_eq!(
            Config::load(&dir.path().join("data"))
                .unwrap()
                .default_language
                .as_deref(),
            Some("pt-PT")
        );
    }

    #[gpui::test]
    fn voice_language_cannot_change_during_capture_or_with_pending_recordings(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state, fake_deps(), window, cx));
        let clip = || Clip {
            samples: vec![0.1; 16_000 * 12],
            sample_rate: 16_000,
            clipped: 0,
        };
        window
            .update(cx, |this, _, cx| {
                this.step = Step::Voice;
                this.helper = Some(Ok(HelperRoute::Native));
                installed_voice_models(this, cx);
                this.start_capture(CapturePurpose::Passage, cx);
                assert!(this.capture.is_some());
                this.choose_passage_language("pt-BR", cx);
                assert_eq!(this.passage_language, None);
                this.cancel_capture(cx);
                // A first recording being checked locks the language too.
                this.captured(CapturePurpose::Passage, clip(), cx);
                assert!(this.reading_check.is_some());
                this.choose_passage_language("pt-BR", cx);
                assert_eq!(this.passage_language, None);
                this.discard_recordings(cx);
                assert!(this.reading_check.is_none());
                accept_reading(this, clip(), cx);
                this.choose_passage_language("pt-BR", cx);
                assert_eq!(this.passage_language, None);
                this.discard_recordings(cx);
                this.confirmation_clip = Some(clip());
                this.choose_passage_language("pt-BR", cx);
                assert_eq!(this.passage_language, None);
                this.confirmation_clip = None;
                this.submit_voice(clip(), None, ("Robin Example".into(), vec![]), cx);
                assert!(this.voice_job.is_some());
                this.choose_passage_language("pt-BR", cx);
                assert_eq!(this.passage_language, None);
                this.cancel_voice(cx);
            })
            .unwrap();
        cx.run_until_parked();
    }

    /// Holds `clip` as recording 1 through the check's own acceptance path,
    /// as a `ready` reply does.
    fn accept_reading(this: &mut SetupAssistant, clip: Clip, cx: &mut Context<SetupAssistant>) {
        this.capture_generation += 1;
        let generation = this.capture_generation;
        this.reading_check = Some(ReadingCheckJob {
            generation,
            dir: PathBuf::new(),
            cancelled: Arc::default(),
            ended: run_end().1,
            device: None,
        });
        this.reading_checked(
            generation,
            clip,
            Ok(serde_json::json!({
                "ready": true, "usable_parts": 4, "required_parts": 4,
                "checks": {"warnings": []}
            })),
            cx,
        );
        assert!(this.first_recording_ready());
    }

    #[gpui::test]
    fn every_setup_step_has_at_most_one_primary_in_its_footer(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (state, _, _) = crate::views::person_picker::fixtures::state_with_cli(
            cx,
            dir.path(),
            dir.path().join("unused-cli"),
        );
        cx.update(crate::ui::theme::init);
        let mut entity = None;
        let window = cx.add_window(|window, cx| {
            let view = cx.new(|cx| SetupAssistant::new(state, fake_deps(), window, cx));
            entity = Some(view.clone());
            gpui::component::Root::new(view, window, cx)
        });
        let view = entity.unwrap();
        for step in Step::ALL {
            view.update(cx, |view, cx| {
                view.step = step;
                cx.notify();
            });
            cx.update_window(window.into(), |_, window, cx| {
                let actions = crate::ui::kit::painted_primary_actions(window, cx);
                assert!(actions.len() <= 1, "{step:?}: {actions:?}");
                assert!(
                    actions.iter().all(|action| action == "setup-continue"),
                    "{step:?}: {actions:?}"
                );
            })
            .unwrap();
        }
    }

    /// Every step reaches assistive technology by its words: its heading
    /// as "N. Title", its body lines as labels, the assistant's own
    /// message once, as a Status on success and an Alert on failure. In
    /// each step every named node has its own ID and a non-empty name.
    #[test]
    fn capture_times_speak_the_elapsed_time_and_target() {
        assert_eq!(
            meter_time_text(CapturePurpose::Passage, 1, 45, crate::ui::format::seconds),
            "1 s of about 30 s",
        );
        assert_eq!(
            meter_time_text(
                CapturePurpose::Passage,
                1,
                45,
                crate::ui::format::spoken_seconds
            ),
            "1 second of about 30 seconds",
        );
    }

    #[gpui::test]
    fn setup_steps_are_read_by_their_heading_lines_and_messages(cx: &mut TestAppContext) {
        use crate::ui::{a11y_name, spoken_log};

        assert_eq!(
            a11y_name(&crate::ui::title("setup-step-title", Step::Models.title())),
            Some((
                "setup-step-title".to_owned(),
                Role::Heading,
                "Language and speech model".to_owned()
            ))
        );
        cx.update(gpui::init);
        let text = progress_failure("Synthetic failure.");
        let failure = cx.update(|cx| a11y_name(&error_line("setup-message", text.clone(), cx)));
        assert_eq!(
            failure,
            Some(("setup-message".to_owned(), Role::Alert, text.clone()))
        );

        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state.clone(), fake_deps(), window, cx));
        cx.run_until_parked();
        for step in Step::ALL {
            window
                .update(cx, |this, _, cx| {
                    this.step = step;
                    cx.notify();
                })
                .unwrap();
            cx.update_window(window.into(), |_, window, cx| {
                crate::ui::a11y_tree::draw_and_read(window, cx).assert_title("setup-step-title");
            })
            .unwrap();
        }
        window
            .update(cx, |this, window, cx| {
                for step in Step::ALL {
                    {
                        this.step = step;
                        this.progress_error = Some(text.clone());
                        let (_, named) =
                            spoken_log::while_building(|| Render::render(this, window, cx));
                        let ids: BTreeSet<&str> =
                            named.iter().map(|(id, ..)| id.as_str()).collect();
                        assert_eq!(
                            ids.len(),
                            named.len(),
                            "{step:?}: IDs are unique: {named:?}"
                        );
                        assert!(
                            named.iter().all(|(_, _, name)| !name.trim().is_empty()),
                            "{step:?}: {named:?}"
                        );
                        let find = |id: &str| {
                            named
                                .iter()
                                .find(|(i, ..)| i == id)
                                .unwrap_or_else(|| panic!("{step:?}: no {id} in {named:?}"))
                        };
                        assert_eq!(
                            find("setup-step-title"),
                            &(
                                "setup-step-title".to_owned(),
                                Role::Heading,
                                step.title().to_owned()
                            )
                        );
                        let (_, shown_role, shown) = find("setup-message");
                        assert_eq!((*shown_role, shown), (Role::Alert, &text), "{step:?}");
                    }
                }
                this.step = Step::Languages;
                this.progress_error = None;
                let (_, named) = spoken_log::while_building(|| Render::render(this, window, cx));
                let (_, role, first) = named
                    .iter()
                    .find(|(id, ..)| id == "setup-intro")
                    .expect("the step's first line is read");
                assert_eq!(*role, Role::Label);
                assert_eq!(first, "What language are most of your meetings in?");
                let (_, role, help) = named
                    .iter()
                    .find(|(id, ..)| id == "setup-language-note")
                    .expect("the language hint is read");
                assert_eq!(*role, Role::Label);
                assert_eq!(help, "BroScribe uses it when a recording doesn't say, and for the text you read in Your voice. Choose Detect automatically to detect each recording's language.");
                assert!(named.iter().all(|(id, ..)| id != "setup-message"));
            })
            .unwrap();
    }

    fn new_state(cx: &mut TestAppContext, dir: &Path) -> Entity<AppState> {
        state_with_cli(cx, dir, dir.join("no-such-cli"))
    }

    fn state_with_cli(cx: &mut TestAppContext, dir: &Path, cli: PathBuf) -> Entity<AppState> {
        let library = Library::open(dir.join("lib")).unwrap();
        let paths = resolve_paths(&FakePaths::new(dir)).unwrap();
        cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(paths, Config::default(), library, cli),
                cx,
            )
        })
    }

    fn installed_voice_models(this: &mut SetupAssistant, cx: &App) {
        let profile = &this.state.read(cx).config().profile;
        let models = [&profile.vad, &profile.speaker_embedding].map(|id| {
            serde_json::json!({
                "id": id, "state": "installed",
            })
        });
        this.models = Some(Ok(parse_models_list(
            &serde_json::json!({"models": models}),
        )));
    }

    fn fake_deps() -> SetupDeps {
        SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        }
    }

    /// Grants Accessibility only when the test says so, as System Settings
    /// does some time after the request; counts the status reads.
    #[derive(Default)]
    struct LateGrantReader {
        granted: AtomicBool,
        checks: std::sync::atomic::AtomicUsize,
    }

    impl broscribe_platform::AppUiReader for LateGrantReader {
        fn is_trusted(&self) -> bool {
            self.checks.fetch_add(1, Ordering::SeqCst);
            self.granted.load(Ordering::SeqCst)
        }
        fn request_trust(&self) -> bool {
            self.granted.load(Ordering::SeqCst)
        }
        fn snapshot(
            &self,
            _: u32,
            _: broscribe_platform::AxLimits,
        ) -> broscribe_platform::Result<broscribe_platform::AxTree> {
            Err(broscribe_platform::Error::Unsupported("synthetic".into()))
        }
        fn observe(
            &self,
            _: u32,
            _: &[broscribe_platform::AxChange],
            _: broscribe_platform::AxChangeCallback,
        ) -> broscribe_platform::Result<Box<dyn broscribe_platform::AxObservation>> {
            Err(broscribe_platform::Error::Unsupported("synthetic".into()))
        }
        fn manual_accessibility(&self, _: u32) -> broscribe_platform::Result<Option<bool>> {
            Ok(None)
        }
        fn set_manual_accessibility(&self, _: u32, _: bool) -> broscribe_platform::Result<()> {
            Ok(())
        }
    }

    #[gpui::test]
    fn accessibility_grant_appears_without_another_click(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let reader = Arc::new(LateGrantReader::default());
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state, fake_deps(), window, cx));
        window
            .update(cx, |this, _, cx| {
                this.step = Step::Permissions;
                this.accessibility = reader.clone();
                this.accessibility_trusted = false;
                this.request_accessibility(cx);
                assert!(!this.accessibility_trusted);
            })
            .unwrap();
        let tick = |cx: &mut TestAppContext| {
            cx.executor().advance_clock(ACCESSIBILITY_POLL_INTERVAL);
            cx.run_until_parked();
        };
        tick(cx);
        assert!(reader.checks.load(Ordering::SeqCst) >= 1);
        assert!(
            !window
                .update(cx, |this, _, _| this.accessibility_trusted)
                .unwrap()
        );
        // The user grants it in System Settings: no second click needed.
        reader.granted.store(true, Ordering::SeqCst);
        tick(cx);
        assert!(
            window
                .update(cx, |this, _, _| this.accessibility_trusted)
                .unwrap()
        );
        // Polling stopped once granted.
        let checks = reader.checks.load(Ordering::SeqCst);
        for _ in 0..5 {
            tick(cx);
        }
        assert_eq!(reader.checks.load(Ordering::SeqCst), checks);
    }

    #[gpui::test]
    fn accessibility_poll_stops_when_leaving_the_step(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let reader = Arc::new(LateGrantReader::default());
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state, fake_deps(), window, cx));
        window
            .update(cx, |this, _, cx| {
                this.step = Step::Permissions;
                this.accessibility = reader.clone();
                this.request_accessibility(cx);
                this.go_to(Step::Storage, cx);
                assert!(this.accessibility_poll.is_none());
            })
            .unwrap();
        let checks = reader.checks.load(Ordering::SeqCst);
        for _ in 0..3 {
            cx.executor().advance_clock(ACCESSIBILITY_POLL_INTERVAL);
            cx.run_until_parked();
        }
        assert_eq!(reader.checks.load(Ordering::SeqCst), checks);
    }

    #[gpui::test]
    fn recording_access_rechecks_agent_devices_and_permissions_on_activation(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let reply = r#"{"devices":[{"id":"acorn","name":"Acorn microphone","is_default":true}]}"#;
        let cli = write_cli(
            &dir.path().join("synthetic-cli"),
            &format!("echo '{reply}'"),
            &format!("fx_echo {}", ps_str(reply)),
        );
        let state = state_with_cli(cx, dir.path(), cli);
        let notifier = Arc::new(broscribe_platform::fakes::FakeNotifier::default());
        *notifier.authorization.lock().unwrap() = NotificationAuthorization::Granted;
        let reader = Arc::new(LateGrantReader::default());
        cx.update(gpui::init);
        let (view, cx) =
            cx.add_window_view(|window, cx| SetupAssistant::new(state, fake_deps(), window, cx));
        view.update(cx, |this, _| {
            this.step = Step::Permissions;
            this.accessibility = reader.clone();
            this.notifier = notifier.clone();
            this.helper = Some(Ok(HelperRoute::Running));
            this.permissions = Some(Ok(serde_json::json!({"microphone":"granted"})));
            this.notification_authorization = Some(NotificationAuthorization::Denied);
            this.accessibility_trusted = false;
        });
        cx.deactivate_window();
        reader.granted.store(true, Ordering::SeqCst);
        cx.update(|window, _| window.activate_window());
        cx.run_until_parked();
        view.read_with(cx, |this, _| {
            assert!(this.accessibility_trusted);
            assert_eq!(
                this.notification_authorization,
                Some(NotificationAuthorization::Granted)
            );
            assert_eq!(notifier.authorization_requests.load(Ordering::SeqCst), 0);
            assert!(this.permissions_generation > 0 && this.devices_generation > 0);
            assert_ne!(
                this.permissions,
                Some(Ok(serde_json::json!({"microphone":"granted"})))
            );
            let devices = this.recording_devices.as_ref().unwrap().as_ref().unwrap();
            assert_eq!(
                active_microphone(devices, None),
                "Microphone: Acorn microphone (system default)"
            );
        });
    }

    #[gpui::test]
    fn step_text_wraps_inside_a_narrow_window(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        cx.update(gpui::init);
        let (view, cx) =
            cx.add_window_view(|window, cx| SetupAssistant::new(state, fake_deps(), window, cx));
        assert_eq!(view.read_with(cx, |this, _| this.step), Step::Welcome);
        let width = px(640.);
        cx.simulate_resize(gpui::size(width, px(480.)));
        cx.run_until_parked();
        let column = cx.debug_bounds("setup-step").expect("step column drawn");
        assert!(
            column.right() <= width,
            "the step column overflows the window: {column:?}"
        );
    }

    /// A synthetic `broscribe` that logs its argv and fails `daemon <fail>`.
    fn daemon_cli(dir: &Path, fail: &str) -> PathBuf {
        write_cli(
            &dir.join("synthetic-cli"),
            &format!(
                "printf '%s %s\\n' \"$4\" \"$5\" >> '{}'\n\
                 if [ \"$5\" = '{fail}' ]; then echo 'synthetic failure' >&2; exit 3; fi\n\
                 echo '{{\"installed\":true,\"running\":false}}'\n",
                dir.join("args.log").display()
            ),
            &format!(
                "fx_append {log} ((fx_words 4,5)+[char]10);if((fx_arg 5) -ceq {fail}){{fx_eecho 'synthetic failure';exit 3}};fx_echo {reply}",
                log = ps_path(&dir.join("args.log")),
                fail = ps_str(fail),
                reply = ps_str(r#"{"installed":true,"running":false}"#),
            ),
        )
    }

    fn run_install(cx: &mut TestAppContext, dir: &Path, fail: &str) -> HelperInstall {
        let state = state_with_cli(cx, dir, daemon_cli(dir, fail));
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state, fake_deps(), window, cx));
        window
            .update(cx, |this, _, cx| {
                this.step = Step::Permissions;
                this.helper = Some(Ok(HelperRoute::Native));
                this.install_helper(cx);
                assert_eq!(this.helper_install, HelperInstall::Running);
                // A second click while installing does nothing.
                this.install_helper(cx);
            })
            .unwrap();
        // The bounded wait for the started agent's socket.
        for _ in 0..=(HELPER_START_WAIT.as_millis() / HELPER_START_POLL.as_millis()) {
            cx.run_until_parked();
            cx.executor().advance_clock(HELPER_START_POLL);
        }
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| this.helper_install.clone())
            .unwrap()
    }

    #[gpui::test]
    fn install_agent_runs_install_then_start(cx: &mut TestAppContext) {
        assert_eq!(
            helper_install_commands(),
            [args(["daemon", "install"]), args(["daemon", "start"])]
        );
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(run_install(cx, dir.path(), "none"), HelperInstall::Idle);
        let log = std::fs::read_to_string(dir.path().join("args.log")).unwrap();
        assert_eq!(log, "daemon install\ndaemon start\naudio devices\n");
    }

    #[gpui::test]
    fn install_agent_failure_is_shown_and_skips_start(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let outcome = run_install(cx, dir.path(), "install");
        assert!(matches!(outcome, HelperInstall::Failed(e) if !e.is_empty()));
        let log = std::fs::read_to_string(dir.path().join("args.log")).unwrap();
        assert_eq!(log, "daemon install\naudio devices\n");
    }

    #[test]
    fn example_audio_of_each_asr_layout_and_never_another_models() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models");
        // Parakeet: the archive's test_wavs folder.
        let wavs = models.join("parakeet").join("root").join("test_wavs");
        std::fs::create_dir_all(&wavs).unwrap();
        std::fs::write(wavs.join("en.wav"), b"RIFF").unwrap();
        assert_eq!(
            broscribe_core::fsutil::find_sample_wav(&models.join("parakeet")),
            Some(wavs.join("en.wav"))
        );
        // Whisper: the model file alone has none; the pinned WAV installed
        // next to it is used, and another model's sample never is.
        let whisper = models.join("whisper");
        std::fs::create_dir_all(&whisper).unwrap();
        std::fs::write(whisper.join("ggml-model.bin"), b"ggml").unwrap();
        assert_eq!(broscribe_core::fsutil::find_sample_wav(&whisper), None);
        std::fs::write(whisper.join("jfk.wav"), b"RIFF").unwrap();
        assert_eq!(
            broscribe_core::fsutil::find_sample_wav(&whisper),
            Some(whisper.join("jfk.wav"))
        );
    }

    #[gpui::test]
    fn example_audio_test_completes_without_the_microphone(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args.log");
        let cli = write_cli(
            &dir.path().join("synthetic-cli"),
            &format!(
                "printf '%s %s %s\\n' \"$4\" \"$5\" \"$6\" >> '{}'\n\
                 [ -f \"$7\" ] || exit 9\n\
                 echo '{{\"transcript\":\"synthetic example words\"}}'\n",
                log.display()
            ),
            &format!(
                "fx_append {log} ((fx_words 4,5,6)+[char]10);if(-not (fx_exists (fx_arg 7))){{exit 9}};fx_echo {reply}",
                log = ps_path(&log),
                reply = ps_str(r#"{"transcript":"synthetic example words"}"#),
            ),
        );
        let state = state_with_cli(cx, dir.path(), cli);
        let data_dir = state.read_with(cx, |s, _| s.paths().data_dir.clone());
        let asr = state.read_with(cx, |s, _| s.config().profile.asr.clone());
        let wavs = data_dir.join("models").join(asr).join("test_wavs");
        std::fs::create_dir_all(&wavs).unwrap();
        std::fs::write(wavs.join("en.wav"), wav_bytes(&[0.0; 160], 16_000)).unwrap();
        let mic = Arc::new(FakeMicCapture::new(16_000));
        let deps = SetupDeps {
            mic: mic.clone(),
            system: Arc::new(FakeSystemInfo::default()),
        };
        cx.update(gpui::init);
        let window = cx.add_window(|window, cx| SetupAssistant::new(state, deps, window, cx));
        window
            .update(cx, |this, _, cx| {
                this.step = Step::Models;
                this.test_transcription(cx);
                assert_eq!(this.sample, TryState::Running);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert_eq!(
                    this.sample,
                    TryState::Done(Ok("synthetic example words".into()))
                );
                assert!(this.capture.is_none());
            })
            .unwrap();
        assert!(mic.started.lock().unwrap().is_empty());
        // `setup try` on a copy, never saved as a meeting.
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "setup try --from-file\n"
        );
    }

    #[gpui::test]
    fn choosing_a_profile_runs_setup_profile_then_relists(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args.log");
        let cli = write_cli(
            &dir.path().join("synthetic-cli"),
            &format!(
                "printf '%s %s %s\\n' \"$4\" \"$5\" \"$6\" >> '{}'\n\
                 echo '{{\"models\":[],\"profiles\":[{{\"id\":\"multilingual\",\"asr\":\"synthetic-asr\",\"active\":true}}]}}'\n",
                log.display()
            ),
            &format!(
                "fx_append {log} ((fx_words 4,5,6)+[char]10);fx_echo {reply}",
                log = ps_path(&log),
                reply = ps_str(
                    r#"{"models":[],"profiles":[{"id":"multilingual","asr":"synthetic-asr","active":true}]}"#
                ),
            ),
        );
        let state = state_with_cli(cx, dir.path(), cli);
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state, fake_deps(), window, cx));
        window
            .update(cx, |this, _, cx| {
                this.step = Step::Models;
                this.sample = TryState::Done(Ok("previous model's example".into()));
                this.choose_profile("multilingual".into(), cx);
                assert!(this.busy);
                assert_eq!(this.sample, TryState::Idle);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert!(!this.busy);
                assert!(matches!(this.profile_notice, Some(Outcome::Ok(_))));
                let Some(Ok(list)) = &this.models else {
                    panic!("models not relisted");
                };
                assert!(list.profiles[0].active);
            })
            .unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        window
            .update(cx, |this, _, cx| {
                this.sample = TryState::Running;
                this.sample_cancelled = Some(cancelled.clone());
                this.choose_profile("default".into(), cx);
                assert!(cancelled.load(Ordering::SeqCst));
                assert_eq!(this.sample, TryState::Idle);
            })
            .unwrap();
        let log = std::fs::read_to_string(&log).unwrap();
        let mut lines = log.lines();
        assert_eq!(lines.next(), Some("setup --profile multilingual"));
        assert!(lines.any(|l| l.starts_with("models list")), "{log}");
    }

    #[gpui::test]
    fn profile_download_pulls_every_model_the_registry_resolved_including_live(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("args.log");
        let listing = dir.path().join("list.json");
        let cli = write_cli(
            &dir.path().join("synthetic-cli"),
            &format!(
                "printf '%s %s %s\\n' \"$4\" \"$5\" \"$6\" >> '{}'\n\
                 if [ \"$5\" = list ]; then cat '{}'; else echo '{{}}'; fi\n",
                log.display(),
                listing.display()
            ),
            &format!(
                "fx_append {log} ((fx_words 4,5,6)+[char]10);if((fx_arg 5) -ceq 'list'){{fx_cat {listing}}}else{{fx_echo '{{}}'}}",
                log = ps_path(&log),
                listing = ps_path(&listing),
            ),
        );
        let row = |id: &str, state: &str| {
            serde_json::json!({"id": id, "roles": [], "license": "MIT",
                "license_verified": true, "bytes": 1024, "state": state})
        };
        let rows = serde_json::json!([
            row("synthetic-vad", "installed"),
            row("synthetic-asr", "installed"),
            row("synthetic-live", "missing"),
            row("synthetic-diar", "installed"),
            row("synthetic-emb", "missing"),
            row("synthetic-unrelated", "missing"),
        ]);
        // The live model is not one of the config's four IDs: only the
        // registry's resolution names it.
        std::fs::write(
            &listing,
            serde_json::json!({"models": rows, "profiles": [],
                "profile_models": ["synthetic-vad", "synthetic-asr", "synthetic-live",
                                   "synthetic-diar", "synthetic-emb"]})
            .to_string(),
        )
        .unwrap();
        let state = state_with_cli(cx, dir.path(), cli);
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state, fake_deps(), window, cx));
        window
            .update(cx, |this, _, cx| {
                this.step = Step::Models;
                this.load_models(cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, window, cx| {
                assert!(this.wants_download(cx));
                this.activate_primary(window, cx);
                assert!(this.advancing);
                assert!(!this.download(cx).read(cx).running());
                assert!(!Step::Models.completed(this.state.read(cx).config()));
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                assert_eq!(this.step, Step::Permissions);
                assert!(Step::Models.completed(this.state.read(cx).config()));
                assert!(!this.download(cx).read(cx).running());
                assert!(matches!(
                    this.download(cx).read(cx).status,
                    DownloadStatus::Done(_)
                ));
            })
            .unwrap();
        let pulls: Vec<String> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter(|l| l.starts_with("models pull"))
            .map(str::to_owned)
            .collect();
        assert_eq!(
            pulls,
            ["models pull synthetic-live", "models pull synthetic-emb"]
        );

        // The registry cannot resolve the profile: an error, and nothing is
        // pulled from a list guessed here.
        std::fs::write(&log, "").unwrap();
        std::fs::write(
            &listing,
            serde_json::json!({"models": rows, "profiles": [],
                "profile_models_error": "ASR model synthetic-asr declares no live_model"})
            .to_string(),
        )
        .unwrap();
        window
            .update(cx, |this, _, cx| this.load_models(cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                this.download_profile(cx);
                assert!(!this.download(cx).read(cx).running());
                let Some(Outcome::Err(e)) = &this.profile_notice else {
                    panic!("no profile error: {:?}", this.profile_notice);
                };
                assert!(e.contains("declares no live_model"), "{e}");
            })
            .unwrap();
        cx.run_until_parked();
        assert!(
            !std::fs::read_to_string(&log)
                .unwrap()
                .contains("models pull"),
            "nothing pulled"
        );
    }

    #[gpui::test]
    fn superseded_capture_is_discarded(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let mic = Arc::new(FakeMicCapture::new(16_000));
        let deps = SetupDeps {
            mic: mic.clone(),
            system: Arc::new(FakeSystemInfo::default()),
        };
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state.clone(), deps, window, cx));
        window
            .update(cx, |this, _, cx| {
                this.helper = Some(Ok(HelperRoute::Native));
                this.start_capture(CapturePurpose::TryIt, cx);
                // A second start supersedes the first before it opened.
                this.start_capture(CapturePurpose::TryIt, cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                let c = this.capture.as_ref().unwrap();
                assert_eq!(c.generation, this.capture_generation);
                assert!(c.running);
                // Leaving the step stops and discards the capture.
                this.go_to(Step::Words, cx);
                assert!(this.capture.is_none());
            })
            .unwrap();
        // Both sessions were stopped, off the UI thread: the superseded one
        // at once, the other on leaving.
        cx.run_until_parked();
        assert_eq!(mic.stopped.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn cancel_waiting_for_commit_lock_reports_completed_without_kept_audio() {
        use broscribe_core::schema::{
            self, Person, Strength, Voiceprint, VoiceprintSource, Voiceprints,
        };
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let library = Library::open(dir.path().join("lib")).unwrap();
        let mut config = Config::default();
        config.setup.voice_session = Some("enrollment-session".into());
        config.save(&data).unwrap();
        let mut me = Person::new(library.new_person_id(), "Synthetic Person");
        me.is_self = true;
        let print = Voiceprint {
            id: "synthetic-enrollment".into(),
            model: "synthetic".into(),
            dim: 2,
            vector_b64: schema::encode_vector(&[1.0, 0.0]),
            source: VoiceprintSource {
                kind: VoiceprintKind::Enrollment,
                meeting_id: None,
                sha256: Some("a".repeat(64)),
                start: 0.0,
                end: 12.0,
                ranges: Vec::new(),
                speaker_id: None,
                speaker_namespace: String::new(),
            },
            device: None,
            strength: Strength::Confirmed,
            created_at: schema::now(),
        };
        let cancel_library = library.clone();
        let cancel_data = data.clone();
        let (started, receive) = std::sync::mpsc::channel();
        let cancellation = library
            .write(|tx| {
                tx.save_person(&me)?;
                tx.save_voiceprints(&Voiceprints {
                    schema_version: schema::VOICEPRINTS_SCHEMA_VERSION,
                    person_id: me.id.clone(),
                    embeddings: vec![print],
                })?;
                let cancellation = std::thread::spawn(move || {
                    started.send(()).unwrap();
                    invalidate_session(
                        &cancel_library,
                        &cancel_data,
                        "enrollment-session",
                        Some(&BTreeSet::new()),
                    )
                });
                receive.recv().unwrap();
                Ok(cancellation)
            })
            .unwrap();
        assert!(cancellation.join().unwrap().unwrap());
        assert_eq!(Config::load(&data).unwrap().setup.voice_session, None);
        let before = enrollment_ids(&library).unwrap();
        assert!(!invalidate_session(&library, &data, "enrollment-session", Some(&before)).unwrap());
    }

    #[gpui::test]
    fn quit_cancels_setup_runs_after_150ms_when_token_clear_cannot_finish(cx: &mut TestAppContext) {
        let killed = Arc::new(AtomicBool::new(false));
        let observed = killed.clone();
        let dir = tempfile::tempdir().unwrap();
        let runner =
            crate::test_support::runner(dir.path().join("missing"), dir.path().join("library"));
        let task = cx.update(|cx| {
            let deadline = cx.background_executor().timer(QUIT_CANCEL_TIMEOUT);
            cx.spawn(async move |_| {
                finish_setup_quit(futures::future::pending(), deadline, move || {
                    observed.store(true, Ordering::SeqCst);
                    runner.children().cancel_setup_children()
                })
                .await;
            })
        });
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(149));
        cx.run_until_parked();
        assert!(!killed.load(Ordering::SeqCst));
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        assert!(killed.load(Ordering::SeqCst));
        task.detach();
    }

    #[gpui::test]
    fn leaving_voice_and_try_steps_cancels_before_preparation(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let deps = SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state.clone(), deps, window, cx));
        let clip = || Clip {
            samples: vec![0.1; 16_000],
            sample_rate: 16_000,
            clipped: 0,
        };
        window
            .update(cx, |this, _, cx| {
                this.step = Step::Voice;
                this.submit_voice(clip(), None, (String::new(), Vec::new()), cx);
                let cancelled = this.voice_job.as_ref().unwrap().cancelled.clone();
                this.go_to(Step::Words, cx);
                assert!(cancelled.load(Ordering::SeqCst));
                assert!(this.voice_job.is_none());
                assert_eq!(this.step, Step::Voice);
                this.step = Step::TryIt;
                this.save_try_as_meeting = true;
                this.run_try(clip(), cx);
                let cancelled = this.try_job.as_ref().unwrap().cancelled.clone();
                this.go_to(Step::Words, cx);
                assert!(cancelled.load(Ordering::SeqCst));
                assert!(this.try_job.is_none());
                assert_eq!(this.try_state, TryState::Idle);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            Config::load(&dir.path().join("data"))
                .unwrap()
                .setup
                .voice_session,
            None
        );
        state.read_with(cx, |state, _| {
            assert!(state.cli().children().is_empty());
            assert!(state.library().list_meeting_dirs().unwrap().is_empty());
        });
    }

    #[gpui::test]
    fn enrollment_needs_a_name_and_valid_fields_at_submit(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let deps = SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state.clone(), deps, window, cx));
        let clip = || Clip {
            samples: vec![0.1; 16_000 * 12],
            sample_rate: 16_000,
            clipped: 0,
        };
        let set = |input: &Entity<InputState>, text: &str, window: &mut Window, cx: &mut App| {
            input.update(cx, |i, cx| i.set_value(text.to_owned(), window, cx));
        };
        window
            .update(cx, |this, window, cx| {
                this.helper = Some(Ok(HelperRoute::Native));
                this.step = Step::Voice;
                set(&this.name.clone(), "  ", window, cx);
                this.load_me_exists(cx);
            })
            .unwrap();
        cx.run_until_parked();
        // No typed name, no saved setup name, no Me: reading is refused.
        window
            .update(cx, |this, window, cx| {
                assert_eq!(this.me_exists, Some(false));
                installed_voice_models(this, cx);
                this.start_reading(cx);
                assert_eq!(
                    this.identity_error,
                    Some(IdentityError::Name(
                        "Enter your name to save your voice.".into()
                    ))
                );
                assert!(this.capture.is_none());
                set(&this.name.clone(), "Synthetic Me", window, cx);
                this.identity_error = None;
                this.start_reading(cx);
                assert_eq!(this.identity_error, None);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, window, cx| {
                assert!(this.capture.is_some());
                this.cancel_capture(cx);
                // An email made invalid while reading: the submission is
                // refused with the field error and both recordings are kept.
                accept_reading(this, clip(), cx);
                set(&this.emails.clone(), "not-an-email", window, cx);
                this.captured(CapturePurpose::Confirmation, clip(), cx);
                assert!(matches!(&this.identity_error, Some(IdentityError::Email(m)) if m.contains("email")));
                assert!(this.voice_job.is_none());
                assert!(this.passage_clip.is_some() && this.confirmation_clip.is_some());
                // Fixed: the kept recordings go out with the name.
                set(&this.emails.clone(), "me@example.org", window, cx);
                let confirmation = this.confirmation_clip.take();
                this.submit_recordings(confirmation, cx);
                assert!(this.voice_job.is_some());
                assert!(this.passage_clip.is_none() && this.confirmation_clip.is_none());
                this.go_to(Step::Words, cx);
            })
            .unwrap();
        cx.run_until_parked();
        // A Me person names the enrollment even with the field empty.
        state.read_with(cx, |state, _| {
            let mut me = broscribe_core::schema::Person::new(
                state.library().new_person_id(),
                "Synthetic Person",
            );
            me.is_self = true;
            state.library().write(|tx| tx.save_person(&me)).unwrap();
        });
        window
            .update(cx, |this, window, cx| {
                this.step = Step::Voice;
                set(&this.name.clone(), "", window, cx);
                set(&this.emails.clone(), "", window, cx);
                this.load_me_exists(cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                assert_eq!(this.me_exists, Some(true));
                this.identity_error = None;
                this.start_reading(cx);
                assert_eq!(this.identity_error, None);
                this.cancel_capture(cx);
            })
            .unwrap();
        cx.run_until_parked();
    }

    #[gpui::test]
    fn stopped_helper_never_falls_back_to_native_capture(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let mic = Arc::new(FakeMicCapture::new(16_000));
        let deps = SetupDeps {
            mic: mic.clone(),
            system: Arc::new(FakeSystemInfo::default()),
        };
        cx.update(gpui::init);
        let window = cx.add_window(|window, cx| SetupAssistant::new(state, deps, window, cx));
        window
            .update(cx, |this, _, cx| {
                this.helper = Some(Ok(HelperRoute::Stopped));
                this.start_capture(CapturePurpose::TryIt, cx);
                assert!(this.capture.is_none());
                assert!(this.capture_error.is_some());
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(mic.stopped.load(Ordering::SeqCst), 0);
        assert!(mic.started.lock().unwrap().is_empty());
    }

    #[gpui::test]
    fn optional_accessibility_requests_trust_only_on_action(cx: &mut TestAppContext) {
        use broscribe_platform::fakes::FakeAppUiReader;
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let deps = SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        let reader = Arc::new(FakeAppUiReader::default());
        cx.update(gpui::init);
        let window = cx.add_window(|window, cx| SetupAssistant::new(state, deps, window, cx));
        window
            .update(cx, |this, _, cx| {
                this.accessibility = reader.clone();
                this.accessibility_trusted = false;
                assert_eq!(reader.trust_requests.load(Ordering::SeqCst), 0);
                this.request_accessibility(cx);
                assert!(this.accessibility_trusted);
                assert_eq!(reader.trust_requests.load(Ordering::SeqCst), 1);
            })
            .unwrap();
    }

    #[test]
    fn helper_capture_routes_purpose_and_bound_then_consumes_private_sample() {
        let cache = tempfile::tempdir().unwrap();
        for (index, (purpose, wire_purpose, seconds)) in [
            (CapturePurpose::MicCheck, "audio_check", 2),
            (CapturePurpose::Passage, "enrollment", 45),
            (
                CapturePurpose::Confirmation,
                "enrollment",
                CONFIRM_MAX_CAPTURE_S,
            ),
            (CapturePurpose::TryIt, "try_it", 25),
        ]
        .into_iter()
        .enumerate()
        {
            let dir = cache
                .path()
                .join("helper-capture")
                .join(format!("sample-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("sample.wav");
            write_private_file(&path, &wav_bytes(&[0.0, 0.5, -0.5], 16_000)).unwrap();
            // The helper names the pinned device it opened, or null for the
            // system default input.
            let pinned = (index % 2 == 1).then(|| format!("synthetic-usb-{index}"));
            let (clip, device) = capture_helper_sample(
                purpose,
                "synthetic".into(),
                cache.path(),
                |call, timeout| {
                    assert_eq!(
                        call,
                        Call::CaptureSample {
                            session: "synthetic".into(),
                            seconds: seconds as f64,
                            purpose: wire_purpose.into(),
                        }
                    );
                    assert_eq!(timeout, Duration::from_secs(seconds + 10));
                    Ok(serde_json::json!({
                        "path": path,
                        "sample_rate": 16_000,
                        "device_id": pinned,
                        "device_name": null,
                    }))
                },
            )
            .unwrap();
            assert_eq!(device, pinned);
            assert_eq!(clip.sample_rate, 16_000);
            assert_eq!(clip.samples.len(), 3);
            assert!((clip.samples[1] - 0.5).abs() < 0.001);
            assert!(!dir.exists());
        }
    }

    #[test]
    fn invalid_helper_path_never_removes_unrelated_files() {
        let cache = tempfile::tempdir().unwrap();
        let path = cache.path().join("unrelated.wav");
        write_private_file(&path, &wav_bytes(&[0.0], 16_000)).unwrap();
        assert!(helper_clip(serde_json::json!({"path": path}), cache.path()).is_err());
        assert!(path.is_file());
    }

    #[test]
    fn recording_readiness_requires_boolean_success() {
        for ready in [
            serde_json::json!("true"),
            serde_json::json!(1),
            serde_json::Value::Null,
        ] {
            let result = reading_check(&serde_json::json!({
                "ready": ready,
                "usable_parts": 5,
                "required_parts": 4,
                "checks": {"warnings": []}
            }));
            assert!(!result.ready);
        }
    }

    /// A synthetic `broscribe` for `setup voice --check-recording`: it takes
    /// a private copy under `setup-capture/<session>` (0600 on Unix), logs
    /// the flags and answers with `answer.json`.
    fn reading_check_cli(dir: &Path) -> PathBuf {
        write_cli(
            &dir.join("synthetic-cli"),
            &format!(
                "case \"$8\" in */setup-capture/\"${{10}}\"/passage.wav) ;; *) exit 9 ;; esac\n\
                 case \"$(ls -l \"$8\")\" in -rw-------*) ;; *) exit 9 ;; esac\n\
                 printf '%s %s %s %s %s\\n' \"$4\" \"$5\" \"$6\" \"$7\" \"$9\" >> '{}'\n\
                 cat '{}'\n",
                dir.join("args.log").display(),
                dir.join("answer.json").display(),
            ),
            &format!(
                "$copy=fx_arg 8;$session=fx_arg 10;$parent=[IO.Path]::GetDirectoryName($copy);if([IO.Path]::GetFileName($copy) -cne 'passage.wav' -or [IO.Path]::GetFileName($parent) -cne $session -or [IO.Path]::GetFileName([IO.Path]::GetDirectoryName($parent)) -cne 'setup-capture'){{exit 9}};fx_append {log} ((fx_words 4,5,6,7,9)+[char]10);fx_cat {answer}",
                log = ps_path(&dir.join("args.log")),
                answer = ps_path(&dir.join("answer.json")),
            ),
        )
    }

    fn synthetic_passage() -> Passage {
        Passage {
            lang: "en".into(),
            text: "synthetic reading".into(),
            confirmation: "synthetic phrase".into(),
        }
    }

    const CHECK_LOG_LINE: &str = "setup voice --check-recording --from-file --session\n";

    #[gpui::test]
    fn first_reading_check_gates_the_phrase_until_ready(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_cli(cx, dir.path(), reading_check_cli(dir.path()));
        let cache = state.read_with(cx, |s, _| s.paths().cache_dir.join(CAPTURE_DIR));
        let answer = |v: serde_json::Value| {
            std::fs::write(dir.path().join("answer.json"), v.to_string()).unwrap();
        };
        let mic = Arc::new(FakeMicCapture::new(16_000));
        let deps = SetupDeps {
            mic: mic.clone(),
            system: Arc::new(FakeSystemInfo::default()),
        };
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state.clone(), deps, window, cx));
        // The launch sweep of orphaned capture folders runs first.
        cx.run_until_parked();
        let clip = || Clip {
            samples: vec![0.1; 16_000 * 20],
            sample_rate: 16_000,
            clipped: 0,
        };
        let quiet_short = vec!["low_level".to_owned(), "short_speech".to_owned()];
        answer(serde_json::json!({
            "ready": false, "usable_parts": 2, "required_parts": 4,
            "checks": {
                "duration_s": 20.0, "speech_s": 9.0, "speech_rms_dbfs": -44.0,
                "noise_rms_dbfs": -62.0, "noise_reference_s": 2.0,
                "snr_db": 18.0, "clipped_fraction": 0.0,
                "warnings": quiet_short
            }
        }));
        window
            .update(cx, |this, _, cx| {
                this.step = Step::Voice;
                this.helper = Some(Ok(HelperRoute::Native));
                this.passage = Some(Ok(synthetic_passage()));
                this.captured(CapturePurpose::Passage, clip(), cx);
                // Checking: still recording 1, no phrase and no way past it.
                assert_eq!(this.voice_stage(), VoiceStage::Checking);
                assert!(!this.voice_stage().shows_phrase());
                assert!(this.passage_clip.is_none());
                this.record_phrase(cx);
                assert!(this.capture.is_none());
                this.submit_recordings(None, cx);
                assert!(this.voice_job.is_none());
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                assert_eq!(
                    this.reading_feedback,
                    Some(ReadingFeedback::NotReady {
                        warnings: quiet_short.clone()
                    })
                );
                assert_eq!(this.voice_stage(), VoiceStage::Start);
                assert!(!this.voice_stage().shows_phrase());
                assert!(this.passage_clip.is_none());
                // Actionable, with the translated warnings and the quiet hint.
                let lines = reading_feedback_lines(this.reading_feedback.as_ref().unwrap());
                assert_eq!(
                    lines,
                    [
                        (true, READING_NOT_READY.to_owned()),
                        (true, warning_text("low_level").to_owned()),
                        (true, warning_text("short_speech").to_owned()),
                        (false, QUIET_HINT.to_owned()),
                    ]
                );
                this.record_phrase(cx);
                this.submit_recordings(None, cx);
                assert!(this.capture.is_none() && this.voice_job.is_none());
                // Ready but quiet: the phrase comes next, the warning stays
                // non-fatal and still suggests recording 1 again.
                answer(serde_json::json!({
                    "ready": true, "usable_parts": 4, "required_parts": 4,
                    "checks": {"warnings": ["low_level"]}
                }));
                this.captured(CapturePurpose::Passage, clip(), cx);
                assert_eq!(this.voice_stage(), VoiceStage::Checking);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                assert_eq!(this.voice_stage(), VoiceStage::Phrase);
                assert!(this.voice_stage().shows_phrase());
                assert!(this.first_recording_ready());
                assert_eq!(this.passage_clip, Some(clip()));
                assert_eq!(
                    reading_feedback_lines(this.reading_feedback.as_ref().unwrap()),
                    [
                        (true, warning_text("low_level").to_owned()),
                        (false, QUIET_HINT.to_owned()),
                    ]
                );
                // An operational failure keeps the user on recording 1.
                answer(serde_json::json!({
                    "error": {"code": "synthetic", "message": "synthetic check failure"}
                }));
                this.captured(CapturePurpose::Passage, clip(), cx);
                assert!(this.passage_clip.is_none());
                assert_eq!(this.voice_stage(), VoiceStage::Checking);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                assert_eq!(
                    this.reading_feedback,
                    Some(ReadingFeedback::Failed("Synthetic check failure.".into()))
                );
                assert_eq!(this.voice_stage(), VoiceStage::Start);
                assert!(this.passage_clip.is_none());
                this.record_phrase(cx);
                this.submit_recordings(None, cx);
                assert!(this.capture.is_none() && this.voice_job.is_none());
            })
            .unwrap();
        // Three checks, each on a private copy whose folder is gone again.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("args.log")).unwrap(),
            CHECK_LOG_LINE.repeat(3)
        );
        assert_eq!(std::fs::read_dir(&cache).map(|d| d.count()).unwrap_or(0), 0);
        state.read_with(cx, |s, _| assert!(s.cli().children().is_empty()));
        assert!(mic.started.lock().unwrap().is_empty());
        // The check never writes an enrollment token.
        assert_eq!(
            Config::load(&dir.path().join("data"))
                .unwrap()
                .setup
                .voice_session,
            None
        );
    }

    #[gpui::test]
    fn cancelled_or_replaced_reading_checks_never_answer(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_cli(cx, dir.path(), reading_check_cli(dir.path()));
        let cache = state.read_with(cx, |s, _| s.paths().cache_dir.join(CAPTURE_DIR));
        let ready = serde_json::json!({
            "ready": true, "usable_parts": 4, "required_parts": 4, "checks": {"warnings": []}
        });
        std::fs::write(dir.path().join("answer.json"), ready.to_string()).unwrap();
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state.clone(), fake_deps(), window, cx));
        cx.run_until_parked();
        let clip = || Clip {
            samples: vec![0.1; 16_000 * 20],
            sample_rate: 16_000,
            clipped: 0,
        };
        window
            .update(cx, |this, _, cx| {
                this.step = Step::Voice;
                this.passage = Some(Ok(synthetic_passage()));
                this.captured(CapturePurpose::Passage, clip(), cx);
                let first = this.reading_check.as_ref().unwrap().generation;
                let first_cancelled = this.reading_check.as_ref().unwrap().cancelled.clone();
                // Cancel while checking: a late success is ignored.
                this.discard_recordings(cx);
                assert!(first_cancelled.load(Ordering::SeqCst));
                assert_eq!(this.voice_stage(), VoiceStage::Start);
                this.reading_checked(first, clip(), Ok(ready.clone()), cx);
                assert!(this.passage_clip.is_none() && this.reading_feedback.is_none());
                // A new recording replaces a check still running.
                this.captured(CapturePurpose::Passage, clip(), cx);
                let second = this.reading_check.as_ref().unwrap().generation;
                this.captured(CapturePurpose::Passage, clip(), cx);
                this.reading_checked(second, clip(), Ok(ready.clone()), cx);
                assert_eq!(this.voice_stage(), VoiceStage::Checking);
                assert!(this.passage_clip.is_none() && this.reading_feedback.is_none());
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                assert_eq!(this.voice_stage(), VoiceStage::Phrase);
                // Leaving the step while checking: the check is dropped and
                // the step changes only after its child is gone.
                this.captured(CapturePurpose::Passage, clip(), cx);
                this.go_to(Step::Words, cx);
                assert!(this.reading_check.is_none() && this.passage_clip.is_none());
                assert_eq!(this.step, Step::Voice);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert_eq!(this.step, Step::Words);
                assert!(this.passage_clip.is_none() && this.reading_feedback.is_none());
            })
            .unwrap();
        // Cancelled checks never started a child; only the kept one ran.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("args.log")).unwrap(),
            CHECK_LOG_LINE
        );
        assert_eq!(std::fs::read_dir(&cache).map(|d| d.count()).unwrap_or(0), 0);
        state.read_with(cx, |s, _| assert!(s.cli().children().is_empty()));
    }

    #[test]
    fn killed_reading_check_child_is_reaped_before_its_copy_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        // A check that runs until its contained process tree is killed.
        let cli_path = write_cli(
            &dir.path().join("synthetic-cli"),
            "exec sleep 60\n",
            "Start-Sleep 60",
        );
        let cli = crate::test_support::runner(cli_path, dir.path().join("lib"));
        let cache = dir.path().join("cache");
        let session = unique_name("check", 1);
        let session_dir = cache.join(CAPTURE_DIR).join(&session);
        let cancelled = Arc::new(AtomicBool::new(false));
        let clip = Clip {
            samples: vec![0.1; 1_600],
            sample_rate: 16_000,
            clipped: 0,
        };
        let worker = {
            let (cli, cache, session, clip, cancelled) = (
                cli.clone(),
                cache.clone(),
                session.clone(),
                clip.clone(),
                cancelled.clone(),
            );
            std::thread::spawn(move || run_reading_check(&cli, &cache, &session, clip, cancelled))
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while cli.children().is_empty() {
            assert!(Instant::now() < deadline, "the check child never started");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Unix permission bits are platform-specific; cancellation, reaping
        // and removal of the actual copy are checked on every platform.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&session_dir), 0o700);
            assert_eq!(mode(&session_dir.join("passage.wav")), 0o600);
        }
        cancelled.store(true, Ordering::SeqCst);
        // Its own flag stops it: the run kills and reaps only its child,
        // then removes the copy, before returning.
        let (returned, result) = worker.join().unwrap();
        assert!(cli.children().is_empty());
        assert!(!session_dir.exists());
        // The original clip comes back; the stopped child's reply is an error.
        assert_eq!(returned, clip);
        assert!(result.is_err());
    }

    fn unconfirmed_reply() -> serde_json::Value {
        serde_json::json!({
            "committed": false,
            "reason": "confirmation_unconfirmed",
            "confirmation_issue": "below_threshold",
            "confirmation": {
                "score": 0.8, "threshold": 0.9, "method": "calibrated", "passed": false
            },
            "checks": {"warnings": ["low_level"]},
            "confirmation_checks": {"speech_s": 12.5, "warnings": ["noisy", "short_speech"]},
            "confirmation_min_speech_s": 8.0,
            "model": "synthetic-embedder",
            "voiceprints": 0,
            "enrollment_file": null
        })
    }

    #[test]
    fn unconfirmed_reports_parse_only_when_typed_and_complete() {
        let below = PhraseComparison {
            score: Some(0.8),
            threshold: Some(0.9),
            method: Some("calibrated".into()),
        };
        assert_eq!(
            voice_outcome(&unconfirmed_reply()),
            VoiceOutcome::Unconfirmed(Unconfirmed {
                issue: ConfirmationIssue::BelowThreshold(below.clone()),
                reading_warnings: vec!["low_level".into()],
                // The phrase never gets the reading's ten-second warning.
                phrase_warnings: vec!["noisy".into()],
            })
        );
        // Too short to compare: the reply's own detected speech and minimum,
        // no score at all, and none is invented.
        let mut short = unconfirmed_reply();
        short["confirmation_issue"] = "too_short".into();
        short["confirmation"] = serde_json::Value::Null;
        short["confirmation_checks"]["speech_s"] = serde_json::json!(7.96);
        short["confirmation_checks"]["warnings"] = serde_json::json!(["short_speech", "low_level"]);
        let VoiceOutcome::Unconfirmed(u) = voice_outcome(&short) else {
            panic!("too short is a typed report");
        };
        assert_eq!(
            u.issue,
            ConfirmationIssue::TooShort {
                speech_s: 7.96,
                min_speech_s: CONFIRM_MIN_SPEECH_S
            }
        );
        assert_eq!(u.phrase_warnings, ["low_level"]);
        assert_eq!(u.reading_warnings, ["low_level"]);
        // The only numbers shown are the detected speech (rounded down, so
        // it never reads as the minimum), the minimum and the retry target.
        let numbers = |u: &Unconfirmed| -> BTreeSet<String> {
            unconfirmed_lines(u)
                .iter()
                .flat_map(|(_, l)| {
                    l.split(|c: char| !c.is_ascii_digit() && c != '.')
                        .map(|n| n.trim_matches('.').to_owned())
                        .filter(|n| !n.is_empty())
                        .collect::<Vec<_>>()
                })
                .collect()
        };
        let target = CONFIRM_TARGET_CAPTURE_S.to_string();
        assert_eq!(
            numbers(&u),
            BTreeSet::from(["7.9".to_owned(), "8".to_owned(), target.clone()])
        );
        // Exactly the minimum is enough speech: never too short.
        let mut exact = short.clone();
        exact["confirmation_checks"]["speech_s"] = CONFIRM_MIN_SPEECH_S.into();
        assert_eq!(
            voice_outcome(&exact),
            VoiceOutcome::Failed(MALFORMED_UNCONFIRMED.into())
        );
        // Enough speech the model could not use: distinct, still no score.
        exact["confirmation_issue"] = "unusable".into();
        let VoiceOutcome::Unconfirmed(u) = voice_outcome(&exact) else {
            panic!("unusable is a typed report");
        };
        assert_eq!(
            u.issue,
            ConfirmationIssue::Unusable {
                speech_s: CONFIRM_MIN_SPEECH_S,
                min_speech_s: CONFIRM_MIN_SPEECH_S
            }
        );
        assert_eq!(
            numbers(&u),
            BTreeSet::from(["8.0".to_owned(), "8".to_owned(), target])
        );
        // A missing score reads as missing, never as zero.
        let mut unscored = unconfirmed_reply();
        unscored["confirmation"]
            .as_object_mut()
            .unwrap()
            .remove("score");
        let VoiceOutcome::Unconfirmed(u) = voice_outcome(&unscored) else {
            panic!("a missing score keeps the typed report");
        };
        let ConfirmationIssue::BelowThreshold(c) = &u.issue else {
            panic!("below threshold");
        };
        assert_eq!(c.score, None);
        let summary = &unconfirmed_lines(&u)[0].1;
        assert!(summary.contains("confirmation reading"), "{summary}");
        assert_eq!(c.details(), None);
        assert!(!summary.contains("0.00"), "{summary}");
        // Incomplete or contradictory reports offer no recovery.
        for edit in [
            ("committed", serde_json::json!(true)),
            ("checks", serde_json::json!({})),
            ("confirmation_checks", serde_json::Value::Null),
            ("confirmation_issue", serde_json::json!("other")),
            ("confirmation", serde_json::Value::Null),
            (
                "confirmation",
                serde_json::json!({"score": 0.95, "threshold": 0.9, "passed": true}),
            ),
            ("confirmation_min_speech_s", serde_json::Value::Null),
            ("confirmation_min_speech_s", serde_json::json!("8")),
            // Another minimum than the one the instruction names.
            ("confirmation_min_speech_s", serde_json::json!(5.0)),
        ] {
            let mut v = unconfirmed_reply();
            v[edit.0] = edit.1;
            assert_eq!(
                voice_outcome(&v),
                VoiceOutcome::Failed(MALFORMED_UNCONFIRMED.into()),
                "{v}"
            );
        }
        // Missing, invalid or contradictory detected speech, for every issue.
        for (issue, speech) in [
            ("below_threshold", serde_json::Value::Null),
            ("below_threshold", serde_json::json!("12.5")),
            ("below_threshold", serde_json::json!(-1.0)),
            // A comparison never runs on less than the minimum.
            ("below_threshold", serde_json::json!(7.9)),
            ("too_short", serde_json::Value::Null),
            ("too_short", serde_json::json!(-0.5)),
            ("too_short", serde_json::json!(12.5)),
            ("unusable", serde_json::Value::Null),
            ("unusable", serde_json::json!(7.9)),
        ] {
            let mut v = if issue == "below_threshold" {
                unconfirmed_reply()
            } else {
                let mut v = short.clone();
                v["confirmation_issue"] = issue.into();
                v
            };
            v["confirmation_checks"]["speech_s"] = speech;
            assert_eq!(
                voice_outcome(&v),
                VoiceOutcome::Failed(MALFORMED_UNCONFIRMED.into()),
                "{v}"
            );
        }
        let mut missing = unconfirmed_reply();
        missing["confirmation_checks"]
            .as_object_mut()
            .unwrap()
            .remove("speech_s");
        assert!(matches!(voice_outcome(&missing), VoiceOutcome::Failed(_)));
        // An unusable confirmation never carries a score.
        let mut scored = exact.clone();
        scored["confirmation"] = serde_json::json!({"score": 0.5, "passed": false});
        assert!(matches!(voice_outcome(&scored), VoiceOutcome::Failed(_)));
        let mut boxed = short.clone();
        boxed["confirmation"] = serde_json::json!({"score": 0.5});
        assert!(matches!(voice_outcome(&boxed), VoiceOutcome::Failed(_)));
        // A superseded session stays a cancellation.
        assert_eq!(
            voice_outcome(&serde_json::json!({
                "committed": false, "reason": "session_superseded",
                "checks": {"warnings": []}, "confirmation": null
            })),
            VoiceOutcome::Cancelled
        );
        // Saved without the phrase: skipped, never shown as matched.
        let VoiceOutcome::Enrolled { confirmation, .. } = voice_outcome(&serde_json::json!({
            "committed": true, "voiceprints": 4, "enrollment_file": null,
            "checks": {"warnings": []}, "confirmation": null
        })) else {
            panic!("committed");
        };
        assert_eq!(confirmation, None);
        assert_eq!(enrolled_confirmation_line(None), PHRASE_SKIPPED);
        assert!(PHRASE_SKIPPED.contains("skipped") && !PHRASE_SKIPPED.contains("matched"));
    }

    /// A synthetic `broscribe` for `setup voice --from-file`: it checks the
    /// private session path, logs flags, captures the submitted WAV bytes
    /// and answers with `answer.json`. Cancel commands answer `{}`.
    fn voice_cli(dir: &Path) -> PathBuf {
        let script = r#"cat >/dev/null
if [ "$4 $5" != "setup voice" ]; then echo '{}'; exit 0; fi
passage=''
session=''
prev=''
flags=''
for a in "$@"; do
  case "$prev" in
    --from-file) passage="$a" ;;
    --session) session="$a" ;;
  esac
  case "$a" in
    --library|--json) ;;
    --*) flags="$flags $a" ;;
  esac
  prev="$a"
done
case "$passage" in */setup-capture/"$session"/passage.wav) ;; *) exit 9 ;; esac
case "$(ls -l "$passage")" in -rw-------*) ;; *) exit 9 ;; esac
echo "$flags" >> 'LOG'
copy_index=0
while [ -f "CAPTURE-$copy_index.wav" ]; do copy_index=$((copy_index + 1)); done
cat "$passage" > "CAPTURE-$copy_index.wav"
cat 'ANSWER'
"#
        .replace("LOG", &dir.join("voice-args.log").display().to_string())
        .replace("CAPTURE", &dir.join("passage-copy").display().to_string())
        .replace("ANSWER", &dir.join("answer.json").display().to_string());
        write_cli(
            &dir.join("synthetic-cli"),
            &script,
            &format!(
                "fx_drain_stdin;if((fx_words 4,5) -cne 'setup voice'){{fx_echo '{{}}';exit 0}};$passage='';$session='';$prev='';$flags='';foreach($a in $argv){{if($prev -ceq '--from-file'){{$passage=$a}};if($prev -ceq '--session'){{$session=$a}};if($a.StartsWith('--') -and $a -cne '--library' -and $a -cne '--json'){{$flags+=' '+$a}};$prev=$a}};$parent=[IO.Path]::GetDirectoryName($passage);if([IO.Path]::GetFileName($passage) -cne 'passage.wav' -or [IO.Path]::GetFileName($parent) -cne $session -or [IO.Path]::GetFileName([IO.Path]::GetDirectoryName($parent)) -cne 'setup-capture'){{exit 9}};fx_append {log} ($flags+[char]10);$prefix={prefix};$copyIndex=0;$copy=$prefix+'-'+$copyIndex+'.wav';while((fx_exists $copy)){{$copyIndex++;$copy=$prefix+'-'+$copyIndex+'.wav'}};[IO.File]::Copy($passage,$copy);fx_cat {answer}",
                log = ps_path(&dir.join("voice-args.log")),
                prefix = ps_path(&dir.join("passage-copy")),
                answer = ps_path(&dir.join("answer.json")),
            ),
        )
    }

    fn synthetic_reading() -> Clip {
        Clip {
            samples: (0..16_000 * 20)
                .map(|i| ((i % 160) as f32 / 160.0 - 0.5) * 0.2)
                .collect(),
            sample_rate: 16_000,
            clipped: 0,
        }
    }

    /// Holds the synthetic reading as recording 1 and submits it with a
    /// synthetic phrase, as recording the phrase does.
    fn submit_with_phrase(
        this: &mut SetupAssistant,
        window: &mut Window,
        cx: &mut Context<SetupAssistant>,
    ) {
        this.step = Step::Voice;
        this.helper = Some(Ok(HelperRoute::Native));
        this.passage = Some(Ok(synthetic_passage()));
        let name = this.name.clone();
        name.update(cx, |i, cx| i.set_value("Synthetic Me", window, cx));
        accept_reading(this, synthetic_reading(), cx);
        let phrase = Clip {
            samples: vec![0.05; 16_000 * 3],
            sample_rate: 16_000,
            clipped: 0,
        };
        this.captured(CapturePurpose::Confirmation, phrase, cx);
        assert_eq!(this.voice_stage(), VoiceStage::Processing);
        assert!(this.passage_clip.is_none());
    }

    fn assert_held_after_unconfirmed(this: &SetupAssistant) {
        assert!(
            matches!(this.voice_outcome, Some(VoiceOutcome::Unconfirmed(_))),
            "{:?}",
            this.voice_outcome
        );
        // The original recording 1, ready with the reply's reading warnings
        // only (not the phrase's).
        assert_eq!(this.passage_clip, Some(synthetic_reading()));
        assert_eq!(
            this.reading_feedback,
            Some(ReadingFeedback::Ready {
                warnings: vec!["low_level".into()]
            })
        );
        assert_eq!(this.voice_stage(), VoiceStage::Phrase);
        assert!(this.voice_job.is_none() && this.confirmation_clip.is_none());
    }

    #[gpui::test]
    fn inconclusive_phrase_keeps_the_first_reading_until_an_explicit_choice(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_cli(cx, dir.path(), voice_cli(dir.path()));
        let cache = state.read_with(cx, |s, _| s.paths().cache_dir.join(CAPTURE_DIR));
        let data = dir.path().join("data");
        let answer = |v: serde_json::Value| {
            std::fs::write(dir.path().join("answer.json"), v.to_string()).unwrap();
        };
        let log = || std::fs::read_to_string(dir.path().join("voice-args.log")).unwrap();
        let mic = Arc::new(FakeMicCapture::new(16_000));
        let deps = SetupDeps {
            mic: mic.clone(),
            system: Arc::new(FakeSystemInfo::default()),
        };
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state.clone(), deps, window, cx));
        cx.run_until_parked();
        answer(unconfirmed_reply());
        window.update(cx, submit_with_phrase).unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| assert_held_after_unconfirmed(this))
            .unwrap();
        // Nothing was saved: no Me, no completed step, no token, no files.
        let nothing_saved = |cx: &mut TestAppContext| {
            let config = Config::load(&data).unwrap();
            assert_eq!(config.setup.voice_session, None);
            assert!(!config.setup.completed_steps.iter().any(|s| s == "voice"));
            state.read_with(cx, |s, _| {
                assert!(s.library().list_people().unwrap().is_empty());
                assert!(s.cli().children().is_empty());
            });
            assert_eq!(std::fs::read_dir(&cache).map(|d| d.count()).unwrap_or(0), 0);
        };
        nothing_saved(cx);
        assert!(mic.started.lock().unwrap().is_empty());
        // Retry confirmation only, after each kind of report: the microphone
        // records the confirmation reading, never the reading again. Too
        // short and unusable keep the reply's own seconds and offer the same
        // choices without a score.
        let mut too_short = unconfirmed_reply();
        too_short["confirmation_issue"] = "too_short".into();
        too_short["confirmation"] = serde_json::Value::Null;
        too_short["confirmation_checks"]["speech_s"] = serde_json::json!(3.0);
        let mut unusable = too_short.clone();
        unusable["confirmation_issue"] = "unusable".into();
        unusable["confirmation_checks"]["speech_s"] = serde_json::json!(9.5);
        let below = PhraseComparison {
            score: Some(0.8),
            threshold: Some(0.9),
            method: Some("calibrated".into()),
        };
        for (attempt, (reply, issue)) in [
            (
                unconfirmed_reply(),
                ConfirmationIssue::BelowThreshold(below),
            ),
            (
                too_short,
                ConfirmationIssue::TooShort {
                    speech_s: 3.0,
                    min_speech_s: CONFIRM_MIN_SPEECH_S,
                },
            ),
            (
                unusable,
                ConfirmationIssue::Unusable {
                    speech_s: 9.5,
                    min_speech_s: CONFIRM_MIN_SPEECH_S,
                },
            ),
        ]
        .into_iter()
        .enumerate()
        {
            answer(reply);
            window
                .update(cx, |this, _, cx| {
                    installed_voice_models(this, cx);
                    this.record_phrase(cx);
                    assert_eq!(
                        this.capture.as_ref().map(|c| c.purpose),
                        Some(CapturePurpose::Confirmation)
                    );
                    assert_eq!(this.voice_outcome, None);
                    assert_eq!(this.passage_clip, Some(synthetic_reading()));
                })
                .unwrap();
            cx.run_until_parked();
            window
                .update(cx, |this, _, cx| this.stop_capture(cx))
                .unwrap();
            cx.run_until_parked();
            window
                .update(cx, |this, _, _| {
                    assert_held_after_unconfirmed(this);
                    let Some(VoiceOutcome::Unconfirmed(u)) = &this.voice_outcome else {
                        unreachable!("held after an unconfirmed report");
                    };
                    assert_eq!(u.issue, issue);
                })
                .unwrap();
            nothing_saved(cx);
            assert_eq!(mic.started.lock().unwrap().len(), attempt + 1);
        }
        // Save first reading without confirmation: the normal skip
        // submission of the same recording 1, with no phrase.
        answer(serde_json::json!({
            "committed": true, "person_id": "synthetic", "model": "synthetic-embedder",
            "voiceprints": 4, "enrollment_file": null,
            "checks": {"warnings": ["low_level"]}, "confirmation": null
        }));
        window
            .update(cx, |this, _, cx| {
                this.submit_recordings(None, cx);
                assert_eq!(this.voice_stage(), VoiceStage::Processing);
                assert!(this.passage_clip.is_none());
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert!(
                    matches!(
                        &this.voice_outcome,
                        Some(VoiceOutcome::Enrolled {
                            confirmation: None,
                            ..
                        })
                    ),
                    "{:?}",
                    this.voice_outcome
                );
                assert!(this.passage_clip.is_none());
            })
            .unwrap();
        let log = log();
        let runs: Vec<&str> = log.lines().collect();
        assert_eq!(runs.len(), 5, "{log}");
        for run in &runs[..4] {
            assert!(run.contains(" --confirm-file") && run.contains(" --report-unconfirmed"));
        }
        assert!(!runs[4].contains(" --confirm-file") && !runs[4].contains(" --report-unconfirmed"));
        // Every run got recording 1 itself.
        let expected = wav_bytes(&synthetic_reading().samples, 16_000);
        for run in 0..5 {
            let submitted =
                std::fs::read(dir.path().join(format!("passage-copy-{run}.wav"))).unwrap();
            assert_eq!(submitted, expected, "run {run} must receive recording 1");
        }
        assert_eq!(mic.started.lock().unwrap().len(), 3);
        assert_eq!(std::fs::read_dir(&cache).map(|d| d.count()).unwrap_or(0), 0);
        assert_eq!(Config::load(&data).unwrap().setup.voice_session, None);
    }

    #[gpui::test]
    fn inconclusive_reading_is_dropped_on_leave_close_cancel_or_supersede(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_cli(cx, dir.path(), voice_cli(dir.path()));
        let cache = state.read_with(cx, |s, _| s.paths().cache_dir.join(CAPTURE_DIR));
        let answer = |v: serde_json::Value| {
            std::fs::write(dir.path().join("answer.json"), v.to_string()).unwrap();
        };
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state.clone(), fake_deps(), window, cx));
        cx.run_until_parked();
        answer(unconfirmed_reply());
        let dropped = |this: &SetupAssistant| {
            assert!(this.passage_clip.is_none() && this.reading_feedback.is_none());
            assert!(!matches!(
                this.voice_outcome,
                Some(VoiceOutcome::Unconfirmed(_))
            ));
        };
        // Leaving the step.
        window.update(cx, submit_with_phrase).unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                assert_held_after_unconfirmed(this);
                this.go_to(Step::Words, cx);
                dropped(this);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| assert_eq!(this.step, Step::Words))
            .unwrap();
        // Closing: dropped at once, even before `Closed` is delivered.
        window.update(cx, submit_with_phrase).unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                assert_held_after_unconfirmed(this);
                this.close(cx);
                dropped(this);
            })
            .unwrap();
        cx.run_until_parked();
        // Explicit discard must not keep audio when edited fields are invalid.
        window.update(cx, submit_with_phrase).unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, window, cx| {
                assert_held_after_unconfirmed(this);
                this.emails.update(cx, |input, cx| {
                    input.set_value("invalid-email", window, cx);
                });
                this.discard_and_read_again(cx);
                dropped(this);
                assert!(this.capture.is_none());
                assert!(matches!(this.identity_error, Some(IdentityError::Email(_))));
                this.emails.update(cx, |input, cx| {
                    input.set_value("", window, cx);
                });
            })
            .unwrap();
        cx.run_until_parked();
        // Cancelled: a late reply of the cancelled job restores nothing.
        window
            .update(cx, |this, window, cx| {
                submit_with_phrase(this, window, cx);
                let generation = this.voice_job.as_ref().unwrap().generation;
                this.cancel_voice(cx);
                this.voice_done(generation, Ok(unconfirmed_reply()), cx);
                dropped(this);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert_eq!(this.voice_outcome, Some(VoiceOutcome::Cancelled));
                dropped(this);
                assert_eq!(this.voice_stage(), VoiceStage::Start);
            })
            .unwrap();
        // Superseded session: a cancellation, never a recovery.
        answer(serde_json::json!({
            "committed": false, "reason": "session_superseded", "model": "synthetic-embedder",
            "voiceprints": 0, "enrollment_file": null, "checks": {"warnings": []},
            "confirmation": {"score": 0.8, "threshold": 0.9, "method": "calibrated", "passed": false}
        }));
        window.update(cx, submit_with_phrase).unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                assert_eq!(this.voice_outcome, Some(VoiceOutcome::Cancelled));
                dropped(this);
                // A report whose recording is no longer held offers nothing.
                this.capture_generation += 1;
                let generation = this.capture_generation;
                this.voice_job = Some(VoiceJob {
                    generation,
                    session: "synthetic-missing".into(),
                    dir: cache.join("synthetic-missing"),
                    before: None,
                    cancelled: Arc::default(),
                    stop: Arc::default(),
                    ended: None,
                    reading: None,
                });
                this.voice_done(generation, Ok(unconfirmed_reply()), cx);
                assert_eq!(
                    this.voice_outcome,
                    Some(VoiceOutcome::Failed(MALFORMED_UNCONFIRMED.into()))
                );
                dropped(this);
                assert_eq!(this.voice_stage(), VoiceStage::Start);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(std::fs::read_dir(&cache).map(|d| d.count()).unwrap_or(0), 0);
        assert_eq!(
            Config::load(&dir.path().join("data"))
                .unwrap()
                .setup
                .voice_session,
            None
        );
        state.read_with(cx, |s, _| {
            assert!(s.cli().children().is_empty());
            assert!(s.library().list_people().unwrap().is_empty());
        });
    }

    #[gpui::test]
    fn languages_continue_moves_on_only_once_saved_and_only_from_its_step(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let data = state.read_with(cx, |s, _| s.paths().data_dir.clone());
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state.clone(), fake_deps(), window, cx));
        // An unwritable config: the progress write fails.
        std::fs::create_dir_all(data.join("config.toml")).unwrap();
        window
            .update(cx, |this, window, cx| {
                this.go_to(Step::Languages, cx);
                this.language
                    .update(cx, |input, cx| input.set_value("pt-BR", window, cx));
                this.continue_step(cx);
                assert!(this.advancing);
                // A second Continue while saving does nothing.
                this.continue_step(cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, _| {
                assert_eq!(this.step, Step::Languages);
                assert!(!this.advancing);
                assert!(this.progress_error.is_some());
            })
            .unwrap();
        // Writable again; the user navigates before the save lands.
        std::fs::remove_dir(data.join("config.toml")).unwrap();
        window
            .update(cx, |this, _, cx| {
                this.continue_step(cx);
                this.go_to(Step::Storage, cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                // Saved, but the late answer never moves Storage on.
                assert_eq!(this.step, Step::Storage);
                let config = this.state.read(cx).config();
                assert_eq!(config.default_language.as_deref(), Some("pt-BR"));
                assert!(
                    config
                        .setup
                        .completed_steps
                        .iter()
                        .any(|s| s == "languages")
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn finish_closes_only_after_progress_is_saved(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let data = state.read_with(cx, |s, _| s.paths().data_dir.clone());
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state.clone(), fake_deps(), window, cx));
        let view = window.root(cx).unwrap();
        let closed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = closed.clone();
        let _closed = cx.update(|cx| {
            cx.subscribe(&view, move |_, _: &SetupEvent, _| {
                seen.fetch_add(1, Ordering::SeqCst);
            })
        });
        std::fs::create_dir_all(data.join("config.toml")).unwrap();
        window
            .update(cx, |this, _, cx| {
                this.step = Step::Ready;
                this.finish(cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(closed.load(Ordering::SeqCst), 0);
        window
            .update(cx, |this, _, _| assert!(this.progress_error.is_some()))
            .unwrap();
        std::fs::remove_dir(data.join("config.toml")).unwrap();
        window
            .update(cx, |this, _, cx| {
                this.step = Step::Ready;
                this.finish(cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(closed.load(Ordering::SeqCst), 1);
        let done = Config::load(&data).unwrap().setup.completed_steps;
        assert_eq!(done, [ASSISTANT_DONE]);
    }

    #[test]
    fn orphan_cleanup_keeps_this_process_capture_folders() {
        let cache = tempfile::tempdir().unwrap();
        let mine = private_dir(cache.path(), &unique_name("check", 7)).unwrap();
        write_private_file(&mine.join("passage.wav"), b"synthetic").unwrap();
        let parent = cache.path().join(CAPTURE_DIR);
        // PID 1 never runs this app; a folder name from an older layout.
        let earlier = parent.join("check-1_00000000-7-1f");
        let unnamed = parent.join("broscribe-voice-123-abc");
        std::fs::create_dir_all(&earlier).unwrap();
        std::fs::create_dir_all(&unnamed).unwrap();
        remove_orphan_capture_dirs(cache.path());
        assert!(mine.join("passage.wav").is_file());
        assert!(!earlier.exists());
        assert!(!unnamed.exists());
    }

    #[cfg(unix)]
    #[test]
    fn orphan_cleanup_logs_the_io_kind_without_the_cache_path() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("synthetic-private-cache");
        std::fs::write(&file, b"synthetic").unwrap();
        let (log, _guard) = crate::test_support::LogCapture::start();
        remove_orphan_capture_dirs(&file);
        let log = log.text();
        assert!(log.contains("listing setup capture folders") && log.contains("kind="));
        assert!(!log.contains("synthetic-private-cache"));
    }

    #[test]
    fn meters_redraw_only_on_change_and_helper_time_runs_in_silence() {
        let mut helper =
            ActiveCapture::new(1, CapturePurpose::Passage, None, Some("sample-1".into()));
        helper.running = true;
        assert!(helper.redraw_needed());
        assert!(!helper.redraw_needed());
        // No level event arrives (silence): the elapsed second still moves.
        helper.started -= Duration::from_secs(2);
        assert!(helper.redraw_needed());
        assert!(!helper.redraw_needed());

        let mut native = ActiveCapture::new(2, CapturePurpose::TryIt, None, None);
        native.running = true;
        native.shared.push(&MicChunk::new(vec![0.25; 1000], 1000));
        assert!(native.redraw_needed());
        // A tenth of a second more at the same level shows nothing new.
        native.shared.push(&MicChunk::new(vec![0.25; 100], 1000));
        assert!(!native.redraw_needed());
        native.shared.push(&MicChunk::new(vec![0.9; 1000], 1000));
        assert!(native.redraw_needed());
        // Taken audio is the whole recording; later chunks are dropped.
        let clip = native.shared.take();
        assert_eq!(clip.samples.len(), 2100);
        native.shared.push(&MicChunk::new(vec![0.25; 1000], 1000));
        assert!(native.shared.rec().samples.is_empty());
    }

    #[gpui::test]
    fn hiding_the_assistant_stops_its_capture_and_polling(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let mic = Arc::new(FakeMicCapture::new(16_000));
        let deps = SetupDeps {
            mic: mic.clone(),
            system: Arc::new(FakeSystemInfo::default()),
        };
        cx.update(gpui::init);
        let window = cx.add_window(|window, cx| SetupAssistant::new(state, deps, window, cx));
        window
            .update(cx, |this, _, cx| {
                this.helper = Some(Ok(HelperRoute::Native));
                this.start_capture(CapturePurpose::TryIt, cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |this, _, cx| {
                assert!(this.capture.as_ref().is_some_and(|c| c.running));
                this.accessibility_poll = Some(Task::ready(()));
                this.set_visible(false, cx);
                assert!(this.capture.is_none());
                assert!(this.accessibility_poll.is_none());
                assert!(!this.recheck_accessibility(cx));
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(mic.stopped.load(Ordering::SeqCst), 1);
    }

    /// A device session that counts its explicit stops and its drops apart.
    struct CountedSession {
        stops: Arc<std::sync::atomic::AtomicUsize>,
        drops: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CaptureSession for CountedSession {
        fn sample_rate(&self) -> u32 {
            16_000
        }
        fn failure(&self) -> Option<String> {
            None
        }
        fn stop(self: Box<Self>) -> broscribe_platform::Result<()> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    impl Drop for CountedSession {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A microphone whose every start opens a [`CountedSession`].
    #[derive(Default)]
    struct CountedMic {
        starts: std::sync::atomic::AtomicUsize,
        stops: Arc<std::sync::atomic::AtomicUsize>,
        drops: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl MicCapture for CountedMic {
        fn devices(&self) -> broscribe_platform::Result<Vec<AudioDevice>> {
            Ok(Vec::new())
        }
        fn start(
            &self,
            _: Option<&str>,
            _: broscribe_platform::MicSink,
        ) -> broscribe_platform::Result<Box<dyn CaptureSession>> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(CountedSession {
                stops: self.stops.clone(),
                drops: self.drops.clone(),
            }))
        }
    }

    /// The assistant is closed while its microphone is still opening: the
    /// session that opens afterwards is never adopted, and is stopped
    /// through the tracked off-UI-thread stop rather than dropped (a drop
    /// joins the native capture worker on whatever thread drops it).
    #[gpui::test]
    fn a_microphone_opening_after_the_assistant_closed_is_stopped_not_dropped(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let mic = Arc::new(CountedMic::default());
        let deps = SetupDeps {
            mic: mic.clone(),
            system: Arc::new(FakeSystemInfo::default()),
        };
        cx.update(gpui::init);
        let window = cx.add_window(|window, cx| SetupAssistant::new(state, deps, window, cx));
        // The device opens on the background executor, which the test
        // scheduler runs only when parked: the window closes first.
        let assistant = window
            .update(cx, |this, window, cx| {
                this.helper = Some(Ok(HelperRoute::Native));
                this.start_capture(CapturePurpose::TryIt, cx);
                window.remove_window();
                cx.entity().downgrade()
            })
            .unwrap();
        assert!(assistant.upgrade().is_none(), "the assistant is released");
        assert_eq!(mic.starts.load(Ordering::SeqCst), 0, "still opening");

        cx.run_until_parked();
        assert_eq!(mic.starts.load(Ordering::SeqCst), 1);
        assert_eq!(
            mic.stops.load(Ordering::SeqCst),
            1,
            "stopped, not just dropped"
        );
        assert_eq!(mic.drops.load(Ordering::SeqCst), 1, "nothing keeps it");
    }

    /// Clicks the element `selector` names without running background
    /// work, so a child the click starts has not run yet.
    fn click_without_running(cx: &mut gpui::VisualTestContext, selector: &'static str) {
        let at = cx.debug_bounds(selector).expect(selector).center();
        cx.update(|window, cx| {
            for input in [
                gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                    position: at,
                    modifiers: gpui::Modifiers::none(),
                    button: gpui::MouseButton::Left,
                    click_count: 1,
                    first_mouse: false,
                }),
                gpui::PlatformInput::MouseUp(gpui::MouseUpEvent {
                    position: at,
                    modifiers: gpui::Modifiers::none(),
                    button: gpui::MouseButton::Left,
                    click_count: 1,
                }),
            ] {
                window.dispatch_event(input, cx);
            }
        });
    }

    /// A browser sign-in started on the Calendars step is cancelled, under
    /// its own attempt, when the assistant leaves the step or is hidden.
    #[gpui::test]
    fn leaving_or_hiding_calendars_cancels_its_running_sign_in(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        for hide in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("args.log");
            let cli = write_cli(
                &dir.path().join("synthetic-cli"),
                &format!(
                    "printf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n\
                     *'integrations cancel-login google'*) echo '{{\"cancelled\":true}}';;\n\
                     *'integrations login google'*) echo '{{\"error\":{{\"code\":\"login_cancelled\",\"message\":\"Nothing from it was saved\"}}}}'; exit 1;;\n\
                     *'integrations status'*) echo '{{\"integrations\":[{{\"integration\":\"google\",\"configured\":true,\"state\":{{\"status\":\"signed_out\"}}}}]}}';;\n\
                     *) echo '{{}}';;\nesac\n",
                    log.display()
                ),
                &format!(
                    "fx_log_args {log};if($all -clike '*integrations cancel-login google*'){{fx_echo {cancelled}}}elseif($all -clike '*integrations login google*'){{fx_echo {error};exit 1}}elseif($all -clike '*integrations status*'){{fx_echo {status}}}else{{fx_echo '{{}}'}}",
                    log = ps_path(&log),
                    cancelled = ps_str(r#"{"cancelled":true}"#),
                    error = ps_str(
                        r#"{"error":{"code":"login_cancelled","message":"Nothing from it was saved"}}"#
                    ),
                    status = ps_str(
                        r#"{"integrations":[{"integration":"google","configured":true,"state":{"status":"signed_out"}}]}"#
                    ),
                ),
            );
            let state = state_with_cli(cx, dir.path(), cli);
            let (view, cx) = cx
                .add_window_view(|window, cx| SetupAssistant::new(state, fake_deps(), window, cx));
            cx.simulate_resize(gpui::size(px(1200.), px(2400.)));
            view.update(cx, |this, cx| this.go_to(Step::Calendars, cx));
            redraw(cx);
            cx.run_until_parked();
            redraw(cx);
            // Linux and Windows show direct sign-in without a disclosure.
            if let Some(advanced) = cx.debug_bounds("integrations-advanced") {
                cx.simulate_click(advanced.center(), gpui::Modifiers::none());
                redraw(cx);
            }
            click_without_running(cx, "integration-login-google");
            if hide {
                view.update(cx, |this, cx| this.set_visible(false, cx));
            } else {
                view.update(cx, |this, cx| this.go_to(Step::Agents, cx));
            }
            cx.run_until_parked();

            let log = std::fs::read_to_string(&log).unwrap();
            let login = log
                .lines()
                .find(|line| line.contains("integrations login google --attempt "))
                .expect("the sign-in started");
            let attempt = login.rsplit(' ').next().unwrap();
            let cancel = format!("integrations cancel-login google --attempt {attempt}");
            assert_eq!(
                log.lines().filter(|line| line.ends_with(&cancel)).count(),
                1,
                "hide: {hide}\n{log}"
            );
        }
    }

    #[gpui::test]
    fn quit_waits_for_tracked_setup_cancellations_within_its_bound(cx: &mut TestAppContext) {
        let (ended, end) = oneshot::channel::<()>();
        let finished = Arc::new(AtomicBool::new(false));
        let observed = finished.clone();
        let quit = cx.update(|cx| {
            // A helper-capture discard still waiting for its request to end.
            let task = cx.background_executor().spawn(async move {
                let _ = end.await;
            });
            SetupRuns::track(cx, task.shared());
            let quit = SetupRuns::quit(cx);
            cx.spawn(async move |_| {
                quit.await;
                observed.store(true, Ordering::SeqCst);
            })
        });
        cx.run_until_parked();
        assert!(!finished.load(Ordering::SeqCst));
        drop(ended);
        cx.run_until_parked();
        assert!(finished.load(Ordering::SeqCst));
        quit.detach();

        // A cancellation that never ends is waited for no longer than the bound.
        let finished = Arc::new(AtomicBool::new(false));
        let observed = finished.clone();
        let quit = cx.update(|cx| {
            let task = cx
                .background_executor()
                .spawn(futures::future::pending::<()>());
            SetupRuns::track(cx, task.shared());
            let quit = SetupRuns::quit(cx);
            cx.spawn(async move |_| {
                quit.await;
                observed.store(true, Ordering::SeqCst);
            })
        });
        cx.run_until_parked();
        assert!(!finished.load(Ordering::SeqCst));
        cx.executor().advance_clock(QUIT_CANCEL_TIMEOUT);
        cx.run_until_parked();
        assert!(finished.load(Ordering::SeqCst));
        quit.detach();
    }

    #[gpui::test]
    fn enrollment_records_the_device_recording_one_was_made_on(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("voice-args.log");
        let cli = write_cli(
            &dir.path().join("synthetic-cli"),
            &format!(
                "if [ \"$4 $5\" = \"setup voice\" ]; then printf '%s\\n' \"$*\" >> '{}'; \
                 echo '{{\"committed\":false,\"reason\":\"session_superseded\"}}'; \
                 else echo '{{}}'; fi\n",
                log.display()
            ),
            &format!(
                "if((fx_words 4,5) -ceq 'setup voice'){{fx_log_args {log};fx_echo {reply}}}else{{fx_echo '{{}}'}}",
                log = ps_path(&log),
                reply = ps_str(r#"{"committed":false,"reason":"session_superseded"}"#),
            ),
        );
        let state = state_with_cli(cx, dir.path(), cli);
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| SetupAssistant::new(state.clone(), fake_deps(), window, cx));
        window
            .update(cx, |this, window, cx| {
                this.step = Step::Voice;
                this.helper = Some(Ok(HelperRoute::Native));
                this.passage = Some(Ok(synthetic_passage()));
                this.name
                    .update(cx, |i, cx| i.set_value("Synthetic Me", window, cx));
                // Recording 1 came from the pinned USB microphone the helper
                // reported; the picker shows another device by now.
                this.captured_device = Some("synthetic-usb".into());
                this.captured(CapturePurpose::Passage, synthetic_reading(), cx);
                let generation = this.reading_check.as_ref().unwrap().generation;
                this.reading_checked(
                    generation,
                    synthetic_reading(),
                    Ok(serde_json::json!({
                        "ready": true, "usable_parts": 4, "required_parts": 4,
                        "checks": {"warnings": []}
                    })),
                    cx,
                );
                assert!(this.first_recording_ready());
                this.device = Some("synthetic-builtin".into());
                this.submit_recordings(None, cx);
            })
            .unwrap();
        cx.run_until_parked();
        let args = std::fs::read_to_string(&log).unwrap();
        assert!(args.contains("--device synthetic-usb"), "{args}");
        assert!(!args.contains("synthetic-builtin"), "{args}");
        // The typed name travels on stdin, never in argv.
        assert!(args.contains("--spec-stdin"), "{args}");
        assert!(!args.contains("Synthetic Me"), "{args}");
    }

    #[gpui::test]
    fn storage_step_inherits_retention_confirmation_safeguard(cx: &mut TestAppContext) {
        use gpui::component::WindowExt as _;

        let dir = tempfile::tempdir().unwrap();
        // The step only asks about `processed`.
        let reply = r#"{"dry_run":true,"policy":"processed","checked":2,"recordings":2,"bytes":2048000,"kept_for_live_jobs":0,"kept_for_live_jobs_bytes":0,"due_dates_updated":0,"leftovers_removed":0,"enrollment_files_removed":0,"unreadable":0,"unreadable_people":0,"unreadable_jobs":0}"#;
        let cli = crate::test_support::write_cli(
            &dir.path().join("synthetic-setup-retention-cli"),
            &format!(
                "case \"$*\" in\n\
                 *'retention apply --dry-run'*)\n\
                 echo '{reply}';;\n\
                 *) echo '{{}}';;\n\
                 esac\n"
            ),
            &format!(
                "if($all -clike '*retention apply --dry-run*'){{fx_echo {}}}\
                 else{{fx_echo '{{}}'}}",
                crate::test_support::ps_str(reply)
            ),
        );
        let mut paths = resolve_paths(&FakePaths::new(dir.path())).unwrap();
        paths.library_from_env = None;
        Config::default().save(&paths.data_dir).unwrap();
        let data_dir = paths.data_dir.clone();
        // The library the settings name (the default one): a stricter choice
        // is checked there.
        let library = Library::open(&paths.default_library_dir).unwrap();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(paths, Config::default(), library, cli),
                cx,
            )
        });
        cx.update(crate::ui::theme::init);
        let view_slot: std::rc::Rc<std::cell::RefCell<Option<Entity<SetupAssistant>>>> =
            Default::default();
        let slot = view_slot.clone();
        let state_for_win = state.clone();
        let (_, vcx) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|cx| SetupAssistant::new(state_for_win, fake_deps(), window, cx));
            *slot.borrow_mut() = Some(view.clone());
            gpui::component::Root::new(view, window, cx)
        });
        vcx.simulate_resize(gpui::size(px(960.), px(720.)));
        vcx.run_until_parked();
        let view = view_slot.borrow().clone().unwrap();

        vcx.update(|window, cx| {
            view.update(cx, |this, cx| {
                this.go_to(Step::Storage, cx);
                this.request_retention(
                    Retention::DeleteImmediately {
                        wait_until_named: false,
                    },
                    RetentionTrigger::Immediate,
                    None,
                    window,
                    cx,
                );
            });
        });
        vcx.run_until_parked();
        vcx.update(|window, cx| window.draw(cx).clear(cx));
        vcx.update(|window, cx| window.draw(cx).clear(cx));

        // Dialog is open and nothing was saved yet.
        assert!(vcx.update(|window, cx| window.has_active_dialog(cx)));
        assert_eq!(
            Config::load(&data_dir).unwrap().retention,
            Retention::KeepForever
        );

        // Escape cancels without saving.
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        vcx.update(|window, cx| window.draw(cx).clear(cx));
        assert!(vcx.update(|window, cx| !window.has_active_dialog(cx)));
        assert_eq!(
            Config::load(&data_dir).unwrap().retention,
            Retention::KeepForever
        );

        // Re-open and confirm via the danger button: saves the policy.
        vcx.update(|window, cx| {
            view.update(cx, |this, cx| {
                this.request_retention(
                    Retention::DeleteImmediately {
                        wait_until_named: false,
                    },
                    RetentionTrigger::Immediate,
                    None,
                    window,
                    cx,
                );
            });
        });
        vcx.run_until_parked();
        vcx.update(|window, cx| window.draw(cx).clear(cx));
        vcx.update(|window, cx| window.draw(cx).clear(cx));
        let confirm_center = vcx.update(|window, _| {
            gpui::base::test_support::snapshots(window)
                .into_iter()
                .find(|node| {
                    node.role() == Some(Role::Button)
                        && node
                            .path()
                            .last()
                            .is_some_and(|last| last.to_string() == "retention-confirm-delete")
                })
                .expect("retention-confirm-delete button not painted")
                .bounds()
                .center()
        });
        vcx.simulate_click(confirm_center, gpui::Modifiers::none());
        vcx.run_until_parked();
        assert!(vcx.update(|window, cx| !window.has_active_dialog(cx)));
        assert_eq!(
            Config::load(&data_dir).unwrap().retention,
            Retention::DeleteImmediately {
                wait_until_named: false,
            }
        );
    }
    mod u54 {
        use super::*;
        use crate::ui::a11y_tree::{Tree, draw_and_read};
        use gpui::{VisualContext as _, VisualTestContext};

        fn click(visual: &mut VisualTestContext, label: &str) {
            let point = visual.update(|window, cx| {
                draw_and_read(window, cx);
                gpui::base::test_support::snapshots(window)
                    .iter()
                    .find(|node| node.label() == Some(label))
                    .unwrap_or_else(|| panic!("No control named {label}"))
                    .bounds()
                    .center()
            });
            visual.simulate_click(point, Default::default());
        }

        fn before(tree: &Tree, first: &str, second: &str) {
            fn visit(tree: &Tree, node: usize, order: &mut Vec<usize>) {
                order.push(node);
                for &child in &tree.nodes[node].children {
                    visit(tree, child, order);
                }
            }
            let mut order = Vec::new();
            for root in (0..tree.nodes.len()).filter(|&index| tree.parent(index).is_none()) {
                visit(tree, root, &mut order);
            }
            let first_index = tree.find(first);
            let second_index = tree.find(second);
            assert!(
                order
                    .iter()
                    .position(|&index| index == first_index)
                    .unwrap()
                    < order
                        .iter()
                        .position(|&index| index == second_index)
                        .unwrap(),
                "{first} should precede {second}"
            );
        }

        #[gpui::test]
        fn voice_fields_and_reading_controls_follow_the_document_order(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let state = new_state(cx, dir.path());
            cx.update(gpui::init);
            let (setup, visual) = cx.add_window_view(|window, cx| {
                let mut view = SetupAssistant::new(state, fake_deps(), window, cx);
                view.step = Step::Voice;
                view.passage = Some(Ok(synthetic_passage()));
                view.helper = Some(Ok(HelperRoute::Native));
                installed_voice_models(&mut view, cx);
                view.start_reading(cx);
                view
            });
            visual.simulate_resize(gpui::size(px(640.), px(1050.)));
            visual.update(|window, cx| {
                let tree = draw_and_read(window, cx);
                assert_eq!(
                    tree.read(tree.find("voice-name-label")),
                    ("Label", "Your name")
                );
                assert_eq!(
                    tree.read(tree.find("voice-emails-label")),
                    ("Label", "Email addresses")
                );
                assert!(
                    tree.nodes
                        .iter()
                        .any(|n| n.role == "TextInput" && n.name == "Your name")
                );
                assert!(
                    tree.nodes
                        .iter()
                        .any(|n| n.role == "TextInput" && n.name == "Email addresses")
                );
                assert_eq!(
                    tree.read(tree.find("voice-name-error")),
                    ("Alert", "Enter your name to save your voice.")
                );
                assert!(tree.all("save-me").is_empty());
                assert!(tree.all("setup-message").is_empty());
                before(&tree, "setup-intro", "voice-alternative");
                before(&tree, "voice-alternative", "voice-name-label");
                before(&tree, "voice-name-help", "voice-name-error");
                before(&tree, "voice-name-error", "voice-emails-label");
                before(&tree, "passage-instruction", "voice-record");
                before(&tree, "voice-record", "passage-text");
                assert_eq!(
                    tree.read(tree.find("setup-continue")),
                    ("Button", "Skip this step")
                );
            });
            click(visual, "Skip this step");
            visual.update(|window, cx| {
                assert_eq!(setup.read(cx).step, Step::TryIt);
                assert!(setup.read(cx).continue_focus.is_focused(window));
            });
        }

        #[gpui::test]
        fn voice_continue_validates_the_email_and_sends_details_only_on_stdin(
            cx: &mut TestAppContext,
        ) {
            use crate::test_support::ps_path;
            let dir = tempfile::tempdir().unwrap();
            let spec = dir.path().join("stdin.json");
            let args_path = dir.path().join("argv.txt");
            let cli = write_cli(
                &dir.path().join("synthetic-cli"),
                &format!(
                    "if [ \"$6\" = --spec-stdin ]; then printf '%s\\n' \"$@\" > '{}'; cat > '{}'; fi\necho '{{}}'\n",
                    args_path.display(),
                    spec.display()
                ),
                &format!(
                    "if ((fx_arg 6) -eq '--spec-stdin') {{ fx_log_args {}; fx_save_stdin {} }}; fx_echo '{{}}'",
                    ps_path(&args_path),
                    ps_path(&spec)
                ),
            );
            let state = state_with_cli(cx, dir.path(), cli);
            cx.update(gpui::init);
            let (setup, visual) = cx.add_window_view(|window, cx| {
                let mut view = SetupAssistant::new(state.clone(), fake_deps(), window, cx);
                view.step = Step::Voice;
                view.name
                    .update(cx, |input, cx| input.set_value("Ari Example", window, cx));
                view.emails
                    .update(cx, |input, cx| input.set_value("bad-address", window, cx));
                view
            });
            click(visual, "Continue");
            visual.update(|window, cx| {
                let tree = draw_and_read(window, cx);
                assert_eq!(
                    tree.read(tree.find("voice-emails-error")),
                    ("Alert", "Check this email address: bad-address.")
                );
                before(&tree, "voice-emails-help", "voice-emails-error");
                assert!(tree.all("setup-message").is_empty());
                setup.update(cx, |view, cx| {
                    view.emails.update(cx, |input, cx| {
                        input.set_value("ari@example.org", window, cx)
                    });
                });
            });
            assert!(!spec.exists());
            click(visual, "Continue");
            visual.run_until_parked();
            assert_eq!(setup.read_with(visual, |view, _| view.step), Step::TryIt);
            let sent: Value = serde_json::from_slice(&std::fs::read(spec).unwrap()).unwrap();
            assert_eq!(
                sent,
                serde_json::json!({"name":"Ari Example","emails":["ari@example.org"]})
            );
            let argv = std::fs::read_to_string(args_path).unwrap();
            assert!(argv.contains("--spec-stdin"));
            assert!(!argv.contains("Ari") && !argv.contains("ari@example.org"));
            assert!(!state.read_with(visual, |s, _| Step::Voice.completed(s.config())));
        }

        #[gpui::test]
        fn reading_language_radios_stay_on_one_row_at_both_text_sizes(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let state = new_state(cx, dir.path());
            cx.update(gpui::init);
            let (_, visual) = cx.add_window_view(|window, cx| {
                let mut view = SetupAssistant::new(state, fake_deps(), window, cx);
                view.step = Step::Voice;
                view.passage = Some(Ok(synthetic_passage()));
                view
            });
            visual.simulate_resize(gpui::size(px(640.), px(1150.)));
            for rem in [16., 32.] {
                visual.update(|window, cx| {
                    window.set_rem_size(px(rem));
                    let tree = draw_and_read(window, cx);
                    assert_eq!(
                        tree.read(tree.find("voice-language")),
                        ("RadioGroup", "Reading language")
                    );
                    before(&tree, "setup-voice-language-label", "voice-language");
                    let snapshots = gpui::base::test_support::snapshots(window);
                    let bounds =
                        ["English", "Português (Brasil)", "Português (Portugal)"].map(|label| {
                            snapshots
                                .iter()
                                .find(|node| node.label() == Some(label))
                                .unwrap()
                                .bounds()
                        });
                    assert!(
                        bounds.iter().all(|bound| bound.top() == bounds[0].top()),
                        "{bounds:?}"
                    );
                    assert!(bounds[2].right() <= px(640.), "{bounds:?}");
                });
            }
        }

        #[gpui::test]
        fn confirmation_retry_is_primary_and_saved_voice_links_to_transcription(
            cx: &mut TestAppContext,
        ) {
            let dir = tempfile::tempdir().unwrap();
            let state = new_state(cx, dir.path());
            cx.update(gpui::init);
            let (setup, visual) = cx.add_window_view(|window, cx| {
                let mut view = SetupAssistant::new(state, fake_deps(), window, cx);
                view.step = Step::Voice;
                view.passage = Some(Ok(synthetic_passage()));
                installed_voice_models(&mut view, cx);
                accept_reading(&mut view, synthetic_reading(), cx);
                view.voice_outcome = Some(voice_outcome(&unconfirmed_reply()));
                view
            });
            visual.simulate_resize(gpui::size(px(860.), px(1050.)));
            visual.update(|window, cx| {
                let tree = draw_and_read(window, cx);
                assert_eq!(
                    tree.read(tree.find("voice-retry-confirm")),
                    ("Button", "Read confirmation again")
                );
                assert!(
                    tree.read(tree.find("voice-unconfirmed-0"))
                        .1
                        .starts_with("The confirmation reading didn't match")
                );
                assert_eq!(
                    tree.read(tree.find("voice-comparison")).1,
                    "voice similarity 0.80, needed 0.90"
                );
                before(&tree, "voice-retry-confirm", "voice-save-unconfirmed");
                before(&tree, "voice-save-unconfirmed", "voice-discard-unconfirmed");
                let actions = kit::painted_primary_actions(window, cx);
                assert!(
                    actions.iter().any(|action| action == "voice-retry-confirm"),
                    "{actions:?}"
                );
                assert!(
                    !actions.iter().any(|action| action.contains("unconfirmed")),
                    "{actions:?}"
                );
                setup.update(cx, |view, cx| {
                    view.passage_clip = None;
                    view.voice_outcome = Some(VoiceOutcome::Enrolled {
                        kept: true,
                        warnings: vec![],
                        confirmation: Some(PhraseComparison {
                            score: Some(0.91),
                            threshold: Some(0.70),
                            method: None,
                        }),
                    });
                    cx.notify();
                });
            });
            visual.update(|window, cx| {
                let tree = draw_and_read(window, cx);
                assert_eq!(
                    tree.read(tree.find("voice-outcome")),
                    ("Status", "Your voice is saved.")
                );
                assert_eq!(
                    tree.read(tree.find("voice-confirmation")).1,
                    "Your confirmation reading matched your first reading."
                );
                assert!(tree.all("voice-comparison").is_empty());
                before(&tree, "voice-kept", "Open Transcription settings");
            });
            click(visual, "Details");
            visual.update(|window, cx| {
                let tree = draw_and_read(window, cx);
                assert_eq!(
                    tree.read(tree.find("voice-comparison")).1,
                    "voice similarity 0.91, needed 0.70"
                );
            });
            click(visual, "Open Transcription settings");
            visual.run_until_parked();
            let id = visual.window_handle().window_id();
            let settings = visual.update(|_, cx| {
                cx.windows()
                    .into_iter()
                    .find(|window| window.window_id() != id)
                    .unwrap()
            });
            assert_eq!(
                VisualTestContext::from_window(settings, cx)
                    .window_title()
                    .as_deref(),
                Some(crate::ui::windows::title("Transcription").as_str())
            );
        }

        #[gpui::test]
        fn try_feedback_precedes_the_transcript_and_saved_tests_open_the_meeting(
            cx: &mut TestAppContext,
        ) {
            use std::{cell::Cell, rc::Rc};
            let dir = tempfile::tempdir().unwrap();
            let state = new_state(cx, dir.path());
            let opened = Rc::new(Cell::new(false));
            let _subscription = cx.update(|cx| {
                cx.subscribe(&state, {
                    let opened = opened.clone();
                    move |_, event, _| {
                        if matches!(event, AppEvent::OpenMainWindow) {
                            opened.set(true);
                        }
                    }
                })
            });
            cx.update(gpui::init);
            let (setup, visual) = cx.add_window_view(|window, cx| {
                let mut view = SetupAssistant::new(state.clone(), fake_deps(), window, cx);
                view.step = Step::TryIt;
                view.voice_saved = Some(false);
                view
            });
            visual.simulate_resize(gpui::size(px(800.), px(900.)));
            visual.update(|window, cx| {
                let tree = draw_and_read(window, cx);
                assert_eq!(
                    tree.read(tree.find("try-no-voice")).1,
                    "Your voice isn't set up, so this test can't label you as Me."
                );
                assert_eq!(
                    tree.read(tree.find("setup-open-step-voice")).1,
                    "Go to Your voice"
                );
            });
            for (me, expected) in [
                ("named", "Recognised you as Me."),
                (
                    "suggested",
                    "This test wasn't labelled as Me. Recording your voice again can help.",
                ),
                (
                    "none",
                    "This test wasn't labelled as Me. Recording your voice again can help.",
                ),
            ] {
                setup.update(visual, |view, cx| {
                    view.try_state = TryState::Done(TryResult::from_reply(serde_json::json!({
                        "transcript":"Synthetic test words", "me":me, "meeting_id":"m-a1b2c3",
                    })));
                    cx.notify();
                });
                visual.update(|window, cx| {
                    let tree = draw_and_read(window, cx);
                    assert_eq!(tree.read(tree.find("try-result")).1, expected);
                    before(&tree, "try-result", "try-transcript");
                    assert_eq!(tree.read(tree.find("try-saved")).1, "Saved as a meeting.");
                    assert!(
                        tree.nodes
                            .iter()
                            .all(|node| !node.name.contains("m-a1b2c3"))
                    );
                });
            }
            click(visual, "Open meeting");
            assert!(opened.get());
            assert_eq!(
                state.read_with(visual, |s, _| s.route().clone()),
                Route::Library {
                    selected: Some(MeetingId::parse("m-a1b2c3").unwrap())
                }
            );
            setup.update(visual, |view, cx| {
                view.try_state = TryState::Done(TryResult::from_reply(
                    serde_json::json!({"transcript":"", "me":"none"}),
                ));
                cx.notify();
            });
            visual.update(|window, cx| {
                let tree = draw_and_read(window, cx);
                assert!(
                    tree.read(tree.find("try-result"))
                        .1
                        .starts_with("No speech was recognised.")
                );
                assert!(tree.all("try-transcript").is_empty());
            });
            setup.update(visual, |view, cx| {
                view.try_state = TryState::Done(Err("Synthetic failure.".into()));
                cx.notify();
            });
            visual.update(|window, cx| {
                let tree = draw_and_read(window, cx);
                assert_eq!(
                    tree.read(tree.find("try-result")),
                    ("Alert", "Couldn't transcribe the test. Synthetic failure.")
                );
                assert!(
                    tree.nodes
                        .iter()
                        .any(|node| node.role == "Button" && node.name.starts_with("Try again"))
                );
            });
        }

        #[gpui::test]
        fn audio_check_discloses_measurements_below_the_choices(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let mut report = acoustic_report(Some(12.), Some(-20.));
            report.setup = Some("speakers_mic".into());
            let (_, window) = acoustic_window(cx, dir.path(), report);
            let view = window.root(cx).unwrap();
            let mut visual = VisualTestContext::from_window(window.into(), cx);
            visual.simulate_resize(gpui::size(px(800.), px(900.)));
            visual.update(|window, cx| {
                let tree = draw_and_read(window, cx);
                assert!(
                    tree.read(tree.find("audio-check-result"))
                        .1
                        .starts_with("Check complete · Detected setup:")
                );
                assert!(tree.all("audio-check-delay").is_empty());
                before(&tree, "audio-check-result", "echo-handling");
                before(&tree, "echo-handling", "audio-check-measurements");
            });
            click(&mut visual, "Measurements");
            visual.update(|window, cx| {
                let tree = draw_and_read(window, cx);
                assert_eq!(
                    tree.read(tree.find("audio-check-delay")).1,
                    "Echo arrives after 12 ms · echo level −20 dB"
                );
                assert!(
                    tree.read(tree.find("audio-check-devices"))
                        .1
                        .contains("Speakers")
                );
            });
            view.update(&mut visual, |view, cx| {
                view.audio_check = Some(Ok(acoustic_report(None, None)));
                cx.notify();
            });
            visual.update(|window, cx| {
                let tree = draw_and_read(window, cx);
                assert!(
                    tree.read(tree.find("audio-check-delay"))
                        .1
                        .contains("not measurable")
                );
                assert!(tree.all("echo-handling").is_empty());
            });
        }

        #[gpui::test]
        fn setup_segment_meters_separate_the_tracks_and_latch_clipping(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let state = new_state(cx, dir.path());
            cx.update(gpui::init);
            let (setup, visual) = cx.add_window_view(|window, cx| {
                let mut view = SetupAssistant::new(state, fake_deps(), window, cx);
                view.step = Step::TryIt;
                view.capture = Some(ActiveCapture::new(
                    1,
                    CapturePurpose::TryIt,
                    None,
                    Some("synthetic-session".into()),
                ));
                view
            });
            let event = |session: Option<&str>, peak, rms, others: bool| {
                DaemonEvent::Received(broscribe_ipc::Event::SetupLevels {
                    session: session.map(str::to_owned),
                    me: broscribe_ipc::SetupLevel {
                        peak_dbfs: peak,
                        rms_dbfs: rms,
                    },
                    others: others.then_some(broscribe_ipc::SetupLevel {
                        peak_dbfs: -8.,
                        rms_dbfs: -15.,
                    }),
                })
            };
            setup.update(visual, |view, cx| {
                view.helper_event(&event(Some("other-session"), 0., -12., false), cx);
                assert!(!view.capture.as_ref().unwrap().clipped);
                view.helper_event(&event(Some("synthetic-session"), 0., -12., false), cx);
                view.helper_event(&event(Some("synthetic-session"), -90., -90., false), cx);
            });
            visual.update(|window, cx| {
                let tree = draw_and_read(window, cx);
                assert_eq!(
                    tree.read(tree.find("setup-level")),
                    ("Meter", "Me: recording, silent")
                );
                assert_eq!(
                    tree.read(tree.find("setup-too-loud")),
                    (
                        "Status",
                        "Too loud at times: move a little away from the microphone."
                    )
                );
                before(&tree, "try-stop", "setup-level");
            });
            setup.update(visual, |view, cx| {
                view.capture = None;
                view.step = Step::AudioCheck;
                view.audio_checking = true;
                view.helper_event(&event(None, -70., -75., true), cx);
                cx.notify();
            });
            visual.update(|window, cx| {
                let tree = draw_and_read(window, cx);
                assert_eq!(
                    tree.read(tree.find("audio-check-me")),
                    ("Meter", "Me: recording, silent")
                );
                assert_eq!(
                    tree.read(tree.find("audio-check-others")),
                    ("Meter", "Others: recording, sound")
                );
                assert!(tree.all("setup-too-loud").is_empty());
            });
        }

        #[gpui::test]
        fn setup_lists_available_providers_before_the_names_only_disclosure(
            cx: &mut TestAppContext,
        ) {
            use crate::views::providers::tests::providers_state;
            let dir = tempfile::tempdir().unwrap();
            let state = providers_state(cx, dir.path());
            std::fs::write(dir.path().join("providers.json"), serde_json::json!({"providers": [
                {"id":"missing","name":"Missing CLI","purpose":"llm","detection":{"status":"not_installed"}},
                {"id":"ready","name":"Available CLI","purpose":"llm","detection":{"status":"ready"},"consent_text":"Synthetic terms."},
                {"id":"http","name":"HTTP provider","purpose":"llm","runtime":"openai-compatible-http"}
            ]}).to_string()).unwrap();
            cx.update(gpui::init);
            let (setup, visual) = cx
                .add_window_view(|window, cx| SetupAssistant::new(state, fake_deps(), window, cx));
            setup.update(visual, |view, cx| view.go_to(Step::Summaries, cx));
            visual.run_until_parked();
            visual.simulate_resize(gpui::size(px(860.), px(900.)));
            visual.update(|window, cx| {
                let tree = draw_and_read(window, cx);
                before(&tree, "provider-name-ready", "provider-not-installed");
                before(&tree, "provider-name-http", "provider-not-installed");
                assert!(tree.all("provider-unavailable-missing").is_empty());
                assert_eq!(tree.read(tree.find("provider-state-ready")).1, "Off");
            });
            click(visual, "Not found on this computer (1)");
            visual.update(|window, cx| {
                let tree = draw_and_read(window, cx);
                assert_eq!(
                    tree.read(tree.find("provider-unavailable-missing")).1,
                    "Missing CLI"
                );
                assert!(tree.all("provider-enable-missing").is_empty());
                tree.find("provider-enable-ready");
                tree.find("provider-key-http");
            });
        }

        #[gpui::test]
        fn meeting_apps_use_plain_labels_and_open_recording_when_detection_is_off(
            cx: &mut TestAppContext,
        ) {
            let dir = tempfile::tempdir().unwrap();
            let state = new_state(cx, dir.path());
            cx.update(gpui::init);
            let (_, visual) = cx.add_window_view(|window, cx| {
                let mut view = SetupAssistant::new(state, fake_deps(), window, cx);
                view.step = Step::MeetingApps;
                view.detection_off = true;
                view.meeting_apps = Some(Ok(vec![MeetingApp {
                    id: "mock".into(),
                    name: "Synthetic Meet".into(),
                    policy: Some("ask".into()),
                    installed: Some(true),
                }]));
                view.policy_errors
                    .insert("mock".into(), "Synthetic policy failure.".into());
                view.progress_error = Some(progress_failure("Synthetic write failure."));
                view
            });
            visual.simulate_resize(gpui::size(px(800.), px(900.)));
            visual.update(|window, cx| {
                let tree = draw_and_read(window, cx);
                assert_eq!(
                    tree.read(tree.find("detect-mock")),
                    ("RadioGroup", "When Synthetic Meet starts a call")
                );
                for (id, label) in [
                    ("auto", "Record automatically"),
                    ("ask", "Ask first"),
                    ("off", "Don't record"),
                ] {
                    assert_eq!(
                        tree.read(tree.find(&format!("detect-mock-{id}"))),
                        ("RadioButton", label)
                    );
                }
                assert!(
                    tree.read(tree.find("meeting-apps-notice"))
                        .1
                        .contains("detection is off")
                );
                before(&tree, "meeting-apps-notice", "Recording settings");
                before(&tree, "detect-mock", "detect-error-mock");
                assert_eq!(
                    tree.read(tree.find("detect-error-mock")),
                    ("Alert", "Synthetic policy failure.")
                );
                let footer = tree.subtree(tree.find("setup-footer"));
                assert!(footer.contains(&tree.find("setup-message")));
                assert!(!footer.contains(&tree.find("detect-error-mock")));
            });
            click(visual, "Recording settings");
            visual.run_until_parked();
            let id = visual.window_handle().window_id();
            let settings = visual.update(|_, cx| {
                cx.windows()
                    .into_iter()
                    .find(|window| window.window_id() != id)
                    .unwrap()
            });
            assert_eq!(
                VisualTestContext::from_window(settings, cx)
                    .window_title()
                    .as_deref(),
                Some(crate::ui::windows::title("Recording").as_str())
            );
        }
        #[gpui::test]
        fn cancel_actions_stop_each_transcription_without_saving(cx: &mut TestAppContext) {
            let dir = tempfile::tempdir().unwrap();
            let reply = r#"{"transcript":"late reply"}"#;
            let cli = write_cli(
                &dir.path().join("synthetic-cli"),
                &format!("echo '{reply}'\n"),
                &format!("fx_echo {}", crate::test_support::ps_str(reply)),
            );
            let state = state_with_cli(cx, dir.path(), cli);
            let (data, cache, asr) = state.read_with(cx, |state, _| {
                (
                    state.paths().data_dir.clone(),
                    state.paths().cache_dir.join(CAPTURE_DIR),
                    state.config().profile.asr.clone(),
                )
            });
            let sample = data.join("models").join(&asr).join("test_wavs/en.wav");
            std::fs::create_dir_all(sample.parent().unwrap()).unwrap();
            std::fs::write(&sample, wav_bytes(&[0.1; 160], 16_000)).unwrap();
            cx.update(gpui::init);
            let (setup, visual) = cx.add_window_view(|window, cx| {
                SetupAssistant::new(state.clone(), fake_deps(), window, cx)
            });
            visual.simulate_resize(gpui::size(px(920.), px(1000.)));
            for (step, name, id) in [
                (Step::TryIt, "Transcribing the test…", "try-result"),
                (Step::Models, "Transcribing the example…", "sample-result"),
            ] {
                // Hold the previous cleanup to keep this run waiting while the
                // user cancels through the rendered action.
                let (release, wait) = oneshot::channel();
                let cancelled = setup.update(visual, |view, cx| {
                    view.step = step;
                    let previous = cx
                        .spawn(async move |_, _| {
                            let _ = wait.await;
                        })
                        .shared();
                    view.pending_cancel = Some(previous.clone());
                    cx.spawn(async move |_, _| previous.await).detach();
                    if step == Step::TryIt {
                        view.save_try_as_meeting = true;
                        view.run_try(synthetic_reading(), cx);
                        view.try_job.as_ref().unwrap().cancelled.clone()
                    } else {
                        view.models = Some(Ok(parse_models_list(&serde_json::json!({"models": [
                            {"id":asr, "state":"installed", "roles":["asr"]},
                        ], "profile_models": [asr]}))));
                        view.test_transcription(cx);
                        view.sample_cancelled.as_ref().unwrap().clone()
                    }
                });
                visual.update(|window, cx| {
                    let tree = draw_and_read(window, cx);
                    assert_eq!(tree.read(tree.find(id)).1, name);
                });
                click(visual, "Cancel");
                assert!(cancelled.load(Ordering::SeqCst));
                release.send(()).unwrap();
                visual.run_until_parked();
                visual.update(|window, cx| {
                    let tree = draw_and_read(window, cx);
                    assert_eq!(tree.read(tree.find(id)).1, TEST_CANCELLED);
                    assert!(tree.all("try-transcript").is_empty());
                    assert!(tree.all("sample-transcript").is_empty());
                });
                assert_eq!(
                    std::fs::read_dir(&cache)
                        .map(|dirs| dirs.count())
                        .unwrap_or(0),
                    0
                );
                assert!(state.read_with(visual, |state, _| state.cli().children().is_empty()));
                assert!(state.read_with(visual, |state, _| {
                    state.library().list_meeting_dirs().unwrap().is_empty()
                }));
                assert!(Config::load(&data).unwrap().setup.voice_session.is_none());
                assert!(
                    !Config::load(&data)
                        .unwrap()
                        .setup
                        .completed_steps
                        .iter()
                        .any(|step| step == "try")
                );
            }
        }
    }
}
