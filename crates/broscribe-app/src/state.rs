//! `AppState`: the one GPUI entity holding what the views show (plan §3,
//! §8 items 10, 14).
//!
//! Three execution paths: heavy processing runs in the CLI worker child
//! ([`crate::jobs`]); interactive reads (search, review count) open the index
//! read-only per request on GPUI's background executor and apply their result
//! through a foreground task, discarding results older than the latest
//! request (generation counters); UI and platform updates stay on the main
//! thread. Views observe the entity and repaint only when it notifies.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use broscribe_core::config::Config;
use broscribe_core::index::{Index, SearchQuery};
use broscribe_core::learning::{LearningRequest, REIDENTIFY_KIND};
use broscribe_core::ops::{self, PeopleSamples};
use broscribe_core::queue::JobId;
use broscribe_core::{Error, Library, MeetingId, PersonId, Result};
use broscribe_platform::{AudioPlayer, BrowserLauncher, ResolvedPaths};
use broscribe_platform_native::NativePlatform;
use chrono::{DateTime, FixedOffset};
use futures::FutureExt as _;
use futures::channel::oneshot;
use futures::future::Shared;
use gpui::{AnyWindowHandle, App, AppContext as _, Context, Entity, EventEmitter, Task};

use crate::generation::{
    GenerationSubmission, GenerationWatcher, JobRead, NotificationSink, Outcome, WatchedJob,
    finished_outcome, generation_cause, notification, read_job,
};
use crate::jobs::{
    self, CRASH_RESTART_DELAY, ChildKind, CliError, CliRunner, DEFERRED_WAKEUP_MIN, JobsStatus,
    MAX_CONSECUTIVE_CRASHES, WorkerBatch, WorkerControl, WorkerMsg, WorkerState,
};
use crate::learning::{Recheck, read_recheck};
use crate::views::generate::{
    GenerationSelection, ModelCacheKey, ModelList, ModelLists, ProviderLookup, list_providers,
    providers_fingerprint,
};
use crate::views::library::pages::{MeetingSearch, PageJob, PageOutcome};
use crate::watch::{Changes, LibraryWatch};

/// A load of every person's voice samples ([`AppState::people_samples`]);
/// every clone awaits the same load. The error is its message.
pub type PeopleSamplesLoad = Shared<Task<std::result::Result<Arc<PeopleSamples>, String>>>;

/// The voice samples load of one people revision and speaker-embedding model.
struct SamplesCache {
    key: (u64, String),
    load: PeopleSamplesLoad,
}

/// A mounted meeting view and its window ([`AppState::meeting_view_shown`]).
struct ShownMeeting {
    token: u64,
    id: MeetingId,
    window: AnyWindowHandle,
}

#[derive(Clone)]
pub(crate) struct MeetingPlace {
    pub tab: crate::views::meeting::MeetingTab,
    pub playback_s: f64,
    pub transcript: gpui::ListOffset,
    pub overview: gpui::Point<gpui::Pixels>,
}

/// What one watched generation job read: the job, and the meeting's title
/// once it finished.
type GenerationRead = (JobId, u64, Result<JobRead>, Option<String>);

/// The wall clock the app's queue checks use (replaced in tests).
type Clock = Rc<dyn Fn() -> DateTime<FixedOffset>>;

/// The app's one timer that starts its worker again when a job waiting for
/// its `not_before` is due ([`jobs::QueueCheck::wake_at`]); dropping it
/// cancels it.
struct DeferredWakeup {
    at: DateTime<FixedOffset>,
    _timer: Task<()>,
}

/// The main area's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Library { selected: Option<MeetingId> },
    LibraryPerson { person: PersonId },
    ReviewQueue,
    People { person: Option<PersonId> },
}

impl Route {
    pub fn people() -> Self {
        Self::People { person: None }
    }

    pub fn library() -> Self {
        Self::Library { selected: None }
    }

    pub fn meeting(id: MeetingId) -> Self {
        Self::Library { selected: Some(id) }
    }

    pub fn selected_meeting(&self) -> Option<&MeetingId> {
        match self {
            Self::Library { selected } => selected.as_ref(),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsPage {
    General,
    Recording,
    Transcription,
    Storage,
    AiProviders,
    Summaries,
    Templates,
    Calendars,
    AiTools,
    Privacy,
    Updates,
}

impl SettingsPage {
    pub const ALL: [Self; 11] = [
        Self::General,
        Self::Recording,
        Self::Transcription,
        Self::Storage,
        Self::AiProviders,
        Self::Summaries,
        Self::Templates,
        Self::Calendars,
        Self::AiTools,
        Self::Privacy,
        Self::Updates,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Self::General => "General",
            Self::Recording => "Recording",
            Self::Transcription => "Transcription",
            Self::Storage => "Storage",
            Self::AiProviders => "AI providers",
            Self::Summaries => "Summaries",
            Self::Templates => "Templates",
            Self::Calendars => "Calendars",
            Self::AiTools => "AI tools",
            Self::Privacy => "Privacy",
            Self::Updates => "Updates",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::General => "How BroScribe starts and looks.",
            Self::Recording => "What BroScribe records and when.",
            Self::Transcription => "How speech becomes text, names and searchable meetings.",
            Self::Storage => "Where meetings live and how long recordings are kept.",
            Self::AiProviders => {
                "Off until you turn one on. Turning one on sends transcript text to it."
            }
            Self::Summaries => "What your AI provider writes from a transcript.",
            Self::Templates => {
                "Documents your AI provider can write from a meeting, such as a follow-up email."
            }
            Self::Calendars => "Name meetings and their participants from your calendar.",
            Self::AiTools => "Let AI tools such as Claude Code and Codex search your meetings.",
            Self::Privacy => "What stays on this computer and what can leave it.",
            Self::Updates => "Which version you have and how it updates.",
        }
    }
}

/// Why the library the saved settings name is not the open one
/// ([`AppState::settings_library_mismatch`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OtherLibrary {
    /// Another library was saved since launch: the app opens it when it
    /// restarts.
    SwitchPending,
    /// The app was started on another library than its settings name with
    /// `BROSCRIBE_LIBRARY`; a normal launch opens that one.
    LaunchEnv,
    /// The app was started on another library than its settings name with
    /// `--library`; a normal launch opens that one.
    LaunchFlag,
    /// The connected recording agent runs on another library than the open
    /// one (it started before the setting changed, or with its own
    /// `--library` or `BROSCRIBE_LIBRARY`): its retention passes apply the
    /// saved policy there.
    AgentElsewhere,
}

/// `dir` made absolute against the working directory, as launch opens a
/// library; no file-system access, so symbolic links stay as written.
fn absolute_dir(dir: &Path) -> PathBuf {
    std::path::absolute(dir).unwrap_or_else(|_| dir.to_owned())
}

/// Whether an agent reporting `agent` as its library runs on another one
/// than `open` (absolute). The agent makes its library absolute when it
/// opens it (`Daemon::new`), so a relative report is malformed and counts as
/// another. The daemon client compares here when a state arrives
/// (`DaemonClient::agent_library`).
pub(crate) fn agent_runs_elsewhere(agent: &Path, open: &Path) -> bool {
    // `display()` text turns non-UTF-8 bytes into U+FFFD: never a match.
    let lossy = agent
        .to_string_lossy()
        .contains(char::REPLACEMENT_CHARACTER);
    lossy || !agent.is_absolute() || absolute_dir(agent) != open
}

/// Emitted by [`AppState`] for listeners outside the view tree (the status item).
#[derive(Clone, Debug, PartialEq)]
pub enum AppEvent {
    ReviewCountChanged(usize),
    LibraryChanged(Changes),
    MeetingDeleted(MeetingId),
    OpenMainWindow,
    SetupFinished,
    OpenSettings(Option<SettingsPage>),
    OpenWordList,
    OpenRecordingControls,
    OpenRecordingSetup,
    OpenLoginItems,
    OpenMicrophonePrivacy,
    OpenSystemAudioPrivacy,
    ShowSetup,
}

#[derive(Clone, Debug, Default)]
struct ReviewCount {
    generation: u64,
    count: Option<usize>,
    error: Option<String>,
}

/// The background speaker recheck as last read ([`AppState::recheck`]).
#[derive(Clone, Debug, Default)]
struct RecheckRead {
    generation: u64,
    value: Recheck,
    error: Option<String>,
    /// The last agent progress event of the recheck job applied.
    seen_event: Option<(String, String, Option<String>, Option<u64>)>,
}

/// What [`AppState::new`] needs: the resolved folders, the config, the open
/// library and the CLI binary used for processing.
pub struct AppStateInit {
    pub(crate) paths: ResolvedPaths,
    pub(crate) config: Config,
    library: Library,
    pub(crate) cli: CliRunner,
    pub admission: Arc<broscribe_core::maintenance::ModeLease>,
}

impl AppStateInit {
    pub fn new(
        paths: ResolvedPaths,
        config: Config,
        library: Library,
        cli: PathBuf,
        admission: Arc<broscribe_core::maintenance::ModeLease>,
    ) -> Result<Self> {
        let cli = CliRunner::new(cli, library.root().into(), &paths, admission.clone())?;
        let library = library.with_mode_lease(admission.clone())?;
        Ok(Self {
            paths,
            config,
            library,
            cli,
            admission,
        })
    }
}

pub struct AppState {
    paths: ResolvedPaths,
    config: Config,
    library: Library,
    /// `config.library` when this app opened [`Self::library`]: tells a
    /// switch saved since launch from a launch on another library.
    launch_library_setting: Option<PathBuf>,
    /// The number of the newest Keep recordings choice made in any view
    /// (SET-01).
    retention_choices: Arc<AtomicU64>,
    /// Where the attached recording agent runs, compared with
    /// [`Self::library`] by the daemon client as its states arrive;
    /// [`crate::daemon::AgentLibrary::Unknown`] with no daemon client.
    agent_library: crate::daemon::AgentLibraryFlag,
    route: Route,
    /// "Identify voices" was asked for (`Some(meeting)` scopes it to one
    /// meeting); the review queue takes it when shown.
    identify_request: Option<Option<MeetingId>>,
    search: MeetingSearch,
    review: ReviewCount,
    meetings_revision: u64,
    people_revision: u64,
    jobs: JobsStatus,
    job_meeting_titles: HashMap<MeetingId, Option<String>>,
    dismissed_job_failures: std::collections::HashSet<String>,
    worker: WorkerControl,
    /// Set while a job waits for its `not_before` and this app's worker
    /// runs the queue (no agent).
    deferred_wakeup: Option<DeferredWakeup>,
    clock: Clock,
    cli: CliRunner,
    daemon: Option<Entity<crate::daemon::DaemonClient>>,
    agent_starter: Option<gpui::WeakEntity<crate::launch::AgentStarter>>,
    local_jobs_allowed: bool,
    player: Option<Box<dyn AudioPlayer>>,
    pub(crate) playback_rate: f32,
    player_owner: u64,
    /// Opens web links (http and https only), such as a link in a template
    /// result; `None` until the app sets the platform's.
    browser: Option<Arc<dyn BrowserLauncher>>,
    /// Model lists fetched by `providers models`, for the app session.
    model_lists: ModelLists,
    generation_providers: Option<(String, ProviderLookup)>,
    pub(crate) last_generation: Option<GenerationSelection>,
    model_downloads: Entity<crate::views::setup::ModelDownloads>,
    watch: Option<LibraryWatch>,
    /// Voice samples for the pickers' closest voices.
    samples: Option<SamplesCache>,
    /// Live setup assistants; while any is alive its quit hook kills the
    /// setup children (after clearing the voice-session token).
    setup_assistants: usize,
    /// Generation jobs the user started; notifies when they finish.
    generation: GenerationWatcher,
    /// The speaker recheck of the active embedding model, read from its
    /// job (so it survives navigation, the agent's restarts and reconnects).
    recheck: RecheckRead,
    /// The newest recheck a naming or forget in this app requested.
    recheck_request: Option<LearningRequest>,
    /// Meeting views alive now (one per window at most).
    shown_meetings: Vec<ShownMeeting>,
    meeting_places: std::collections::VecDeque<(MeetingId, MeetingPlace)>,
    meeting_deletion: Option<(String, bool)>,
    shown_token: u64,
    _worker_pump: Task<()>,
}

impl EventEmitter<AppEvent> for AppState {}

impl AppState {
    pub fn new(init: AppStateInit, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = jobs::worker_channel();
        Self {
            paths: init.paths,
            launch_library_setting: init.config.library.clone(),
            retention_choices: Arc::default(),
            agent_library: crate::daemon::AgentLibraryFlag::default(),
            config: init.config,
            route: Route::library(),
            identify_request: None,
            search: MeetingSearch::default(),
            review: ReviewCount::default(),
            meetings_revision: 0,
            people_revision: 0,
            jobs: JobsStatus::default(),
            job_meeting_titles: HashMap::new(),
            dismissed_job_failures: Default::default(),
            worker: WorkerControl::new(init.cli.clone(), tx),
            deferred_wakeup: None,
            clock: Rc::new(broscribe_core::schema::now),
            cli: init.cli,
            daemon: None,
            agent_starter: None,
            local_jobs_allowed: true,
            library: init.library,
            player: None,
            playback_rate: 1.0,
            player_owner: 0,
            browser: None,
            model_lists: ModelLists::default(),
            generation_providers: None,
            last_generation: None,
            model_downloads: cx.new(|_| Default::default()),
            watch: None,
            samples: None,
            setup_assistants: 0,
            generation: GenerationWatcher::default(),
            recheck: RecheckRead::default(),
            recheck_request: None,
            shown_meetings: Vec::new(),
            meeting_places: std::collections::VecDeque::new(),
            meeting_deletion: None,
            shown_token: 0,
            _worker_pump: jobs::spawn_worker_pump(rx, cx),
        }
    }

    // ---- read access ----

    pub(crate) fn model_downloads(&self) -> &Entity<crate::views::setup::ModelDownloads> {
        &self.model_downloads
    }

    pub fn paths(&self) -> &ResolvedPaths {
        &self.paths
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Applies a persisted config without letting a late save/reload undo a newer one.
    pub fn set_config(&mut self, config: Config, cx: &mut Context<Self>) {
        if config.revision() < self.config.revision() || config == self.config {
            return;
        }
        self.config = config;
        cx.notify();
    }

    pub fn library(&self) -> &Library {
        &self.library
    }

    /// Whether the saved settings, or the connected recording agent, name
    /// another folder than the open library, and why: a switch saved since
    /// launch (here or from a terminal), a launch on another library, or an
    /// agent running on another one. What is checked in [`Self::library`]
    /// then says nothing about the library the saved policy applies to,
    /// at the next normal launch or at the agent's next retention pass.
    /// The settings are resolved as a normal launch resolves
    /// `config.library` (unset is the platform default, made absolute), so
    /// the default folder named explicitly is the same library. The agent's
    /// folder is compared as it reported it ([`agent_runs_elsewhere`]), by
    /// the daemon client when its state arrives. With no agent connected,
    /// the next agent normally opens the folder the settings name (a launch
    /// override can make it open another one).
    pub fn settings_library_mismatch(&self) -> Option<OtherLibrary> {
        let open = absolute_dir(self.library.root());
        let named = |setting: Option<&Path>| {
            absolute_dir(setting.unwrap_or(&self.paths.default_library_dir))
        };
        if named(self.config.library.as_deref()) != open {
            return Some(if named(self.launch_library_setting.as_deref()) == open {
                OtherLibrary::SwitchPending
            } else if self
                .paths
                .library_from_env
                .as_deref()
                .is_some_and(|env| absolute_dir(env) == open)
            {
                OtherLibrary::LaunchEnv
            } else {
                OtherLibrary::LaunchFlag
            });
        }
        self.agent_library
            .elsewhere()
            .then_some(OtherLibrary::AgentElsewhere)
    }

    /// The agent half of [`Self::settings_library_mismatch`], for a check
    /// that runs later, off the UI thread: an atomic value the daemon
    /// client publishes, so reading it takes no lock (a retention save
    /// reads it under the config lock, which must stay the last lock
    /// taken).
    pub(crate) fn agent_library(&self) -> &crate::daemon::AgentLibraryFlag {
        &self.agent_library
    }

    /// Numbers Keep recordings choices, in Settings and in Setup alike: a
    /// choice takes the next number, and a save writes only while its
    /// number is still the newest (SET-01).
    pub(crate) fn retention_choices(&self) -> &Arc<AtomicU64> {
        &self.retention_choices
    }

    pub fn route(&self) -> &Route {
        &self.route
    }

    pub fn search(&self) -> &MeetingSearch {
        &self.search
    }

    /// Speakers waiting for review; `None` until first counted.
    pub fn review_count(&self) -> Option<usize> {
        self.review.count
    }

    pub fn review_count_error(&self) -> Option<&str> {
        self.review.error.as_deref()
    }

    /// Bumped whenever meetings may have changed on disk; views showing
    /// meetings reload when it moves.
    pub fn meetings_revision(&self) -> u64 {
        self.meetings_revision
    }

    /// Bumped whenever people may have changed on disk.
    pub fn people_revision(&self) -> u64 {
        self.people_revision
    }

    pub fn jobs(&self) -> &JobsStatus {
        &self.jobs
    }

    pub(crate) fn job_meeting_title(&self, id: &MeetingId) -> Option<&str> {
        self.job_meeting_titles
            .get(id)
            .and_then(|title| title.as_deref())
    }

    pub(crate) fn shown_job_failure(&self) -> Option<&jobs::FailedJob> {
        self.jobs
            .recent_failures
            .iter()
            .rev()
            .find(|job| !self.dismissed_job_failures.contains(&job.job_id))
    }

    pub(crate) fn dismiss_job_failure(&mut self, id: String, cx: &mut Context<Self>) {
        if self.dismissed_job_failures.insert(id) {
            cx.notify();
        }
    }

    pub(crate) fn clear_playback_job_failure(&mut self, job: &JobId) {
        self.jobs.recent_failures.retain(|failure| {
            failure.job_id != job.as_str()
                || failure.kind.as_deref() != Some(broscribe_core::queue::PLAYBACK_KIND)
        });
    }

    fn resolve_job_meetings(&mut self, cx: &mut Context<Self>) {
        let ids: std::collections::HashSet<_> = self
            .jobs
            .active
            .iter()
            .filter_map(|job| job.meeting_id.clone())
            .chain(
                self.jobs
                    .recent_failures
                    .iter()
                    .filter_map(|job| job.meeting_id.clone()),
            )
            .collect();
        self.job_meeting_titles.retain(|id, _| ids.contains(id));
        for id in ids {
            if self.job_meeting_titles.contains_key(&id) {
                continue;
            }
            self.job_meeting_titles.insert(id.clone(), None);
            let library = self.library.clone();
            let task = self.cli.spawn(cx.background_executor(), {
                let id = id.clone();
                async move {
                    library.load_meeting(&id).ok().map(|meeting| {
                        meeting
                            .title
                            .filter(|title| !title.trim().is_empty())
                            .unwrap_or_else(|| "Untitled meeting".into())
                    })
                }
            });
            cx.spawn(async move |this, cx| {
                let title = task.await;
                let _ = this.update(cx, |state, cx| {
                    if let Some(entry) = state.job_meeting_titles.get_mut(&id)
                        && *entry != title
                    {
                        *entry = title;
                        cx.notify();
                    }
                });
            })
            .detach();
        }
    }

    pub fn cli(&self) -> &CliRunner {
        &self.cli
    }

    /// The background speaker recheck as last read; refreshed when its job
    /// or the queue changes.
    pub fn recheck(&self) -> &Recheck {
        &self.recheck.value
    }

    pub fn recheck_error(&self) -> Option<&str> {
        self.recheck.error.as_deref()
    }

    /// The newest recheck requested from this app (its generation and the
    /// meeting named in).
    pub fn recheck_request(&self) -> Option<&LearningRequest> {
        self.recheck_request.as_ref()
    }

    /// The running recheck's own progress text (counts), when its worker
    /// reported one.
    pub fn recheck_progress(&self) -> Option<&str> {
        self.jobs
            .active
            .iter()
            .find(|j| j.kind.as_deref() == Some(REIDENTIFY_KIND))
            .and_then(|j| j.message.as_deref())
    }

    /// A naming or forget written here requested the speaker recheck: keep
    /// the newest request, start the worker and read the job now.
    pub fn recheck_requested(&mut self, request: LearningRequest, cx: &mut Context<Self>) {
        let newer = self
            .recheck_request
            .as_ref()
            .is_none_or(|old| old.job_id != request.job_id || old.generation <= request.generation);
        if newer {
            self.recheck_request = Some(request);
        }
        self.kick_jobs(cx);
        self.refresh_recheck(cx);
    }

    /// Reads the active embedding model's recheck, or the job explicitly
    /// requested by a forget of evidence from an earlier model. Only the
    /// newest background read applies.
    pub fn refresh_recheck(&mut self, cx: &mut Context<Self>) {
        self.recheck.generation += 1;
        let generation = self.recheck.generation;
        let lib = self.library.clone();
        let model = self.config.profile.speaker_embedding.clone();
        let other_model_job = self
            .recheck_request
            .as_ref()
            .filter(|request| {
                request.source_meeting.is_none()
                    && request.job_id != broscribe_core::learning::job_id(&model)
            })
            .map(|request| request.job_id.clone());
        let task = cx.background_executor().spawn(async move {
            let model = if let Some(id) = other_model_job
                && let Some(queue) = broscribe_core::queue::Queue::open_existing(lib.root())?
            {
                queue.get(&id)?.key
            } else {
                model
            };
            read_recheck(&lib, &model)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |state, cx| {
                if generation != state.recheck.generation {
                    return;
                }
                match result {
                    Ok(value) => {
                        state.recheck.value = value;
                        state.recheck.error = None;
                    }
                    Err(e) => {
                        tracing::warn!("reading the speaker recheck: {}", e.code());
                        state.recheck.error = Some(crate::jobs::core_user_text(&e));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The recheck committed meeting writes (a progress or final event of
    /// its job): views showing meetings reload, the review count and the
    /// recheck's counts are read again.
    fn recheck_committed(&mut self, cx: &mut Context<Self>) {
        self.meetings_revision += 1;
        self.rerun_search(cx);
        self.refresh_review_count(cx);
        self.refresh_recheck(cx);
    }

    /// The complete provider catalog, including providers that are now off.
    pub(crate) fn generation_providers(&mut self, cx: &App) -> ProviderLookup {
        let fingerprint = providers_fingerprint(&self.config);
        if let Some((cached, list)) = &self.generation_providers
            && cached == &fingerprint
        {
            return list.clone();
        }
        let list = list_providers(self, cx);
        self.generation_providers = Some((fingerprint, list.clone()));
        list
    }

    pub(crate) fn retry_generation_providers(&mut self) {
        if self
            .generation_providers
            .as_ref()
            .is_some_and(|(_, lookup)| lookup.peek().is_some_and(std::result::Result::is_err))
        {
            self.generation_providers = None;
        }
    }

    pub(crate) fn provider_name(&self, id: &str) -> Option<&str> {
        self.generation_providers
            .as_ref()?
            .1
            .peek()?
            .as_ref()
            .ok()?
            .iter()
            .find(|row| row.id == id)
            .filter(|row| row.name != row.id)
            .map(|row| row.name.as_str())
    }

    /// The cached model list of a provider under one config.
    pub fn model_list(&self, key: &ModelCacheKey) -> Option<&ModelList> {
        self.model_lists.get(key)
    }

    pub fn cache_model_list(&mut self, key: ModelCacheKey, list: ModelList) {
        self.model_lists.insert(key, list);
    }

    pub fn forget_model_list(&mut self, key: &ModelCacheKey) {
        self.model_lists.remove(key);
    }

    /// Every person's voice samples for the active speaker-embedding model,
    /// read on the background executor once per (people revision, model):
    /// pickers open at the same time share one load. A failed load is
    /// started again by the next caller.
    pub fn people_samples(&mut self, cx: &mut Context<Self>) -> PeopleSamplesLoad {
        let key = (
            self.people_revision,
            self.config.profile.speaker_embedding.clone(),
        );
        if let Some(cache) = &self.samples
            && cache.key == key
            && !matches!(cache.load.peek(), Some(Err(_)))
        {
            return cache.load.clone();
        }
        let lib = self.library.clone();
        let model = key.1.clone();
        let load = cx
            .background_executor()
            .spawn(async move {
                // The error's code only: its text can name a person's
                // folder, and nothing shows it.
                ops::load_person_samples(&lib, &model)
                    .map(Arc::new)
                    .map_err(|e| e.code().to_owned())
            })
            .shared();
        self.samples = Some(SamplesCache {
            key,
            load: load.clone(),
        });
        load
    }

    pub fn attach_daemon(&mut self, daemon: Entity<crate::daemon::DaemonClient>, cx: &mut App) {
        // The open library is fixed for the app's life: the client compares
        // each library the agent reports with it as the state arrives.
        let open = absolute_dir(self.library.root());
        self.agent_library = daemon.update(cx, |daemon, _| {
            daemon.compare_agent_library_with(open);
            daemon.agent_library().clone()
        });
        self.daemon = Some(daemon);
        // Installation is checked asynchronously. Fail closed until known.
        self.local_jobs_allowed = false;
        self.deferred_wakeup = None;
    }

    pub fn attach_agent_starter(&mut self, agent: &Entity<crate::launch::AgentStarter>) {
        self.agent_starter = Some(agent.downgrade());
    }

    pub(crate) fn agent_starter(&self) -> Option<Entity<crate::launch::AgentStarter>> {
        self.agent_starter.as_ref()?.upgrade()
    }

    pub fn daemon(&self) -> Option<&Entity<crate::daemon::DaemonClient>> {
        self.daemon.as_ref()
    }

    pub fn set_local_jobs_allowed(&mut self, allowed: bool, cx: &mut Context<Self>) {
        self.local_jobs_allowed = allowed;
        if allowed {
            self.process_jobs(cx);
        } else {
            // The agent runs (and wakes) the queue now.
            self.deferred_wakeup = None;
        }
    }

    /// Applies one job event of the agent (a `JobProgress`, or the running
    /// job a state snapshot repeats). Observers are notified, and watched
    /// generations read again, only when the jobs status it shows changed.
    pub fn daemon_progress(
        &mut self,
        progress: &broscribe_ipc::JobProgress,
        cx: &mut Context<Self>,
    ) {
        use crate::jobs::{ProgressEvent, ProgressEventKind};
        let changed = if progress.phase == "waiting" {
            self.jobs.left_waiting(&progress.job_id)
        } else {
            let event = match progress.phase.as_str() {
                "started" => ProgressEventKind::JobStarted,
                "done" => ProgressEventKind::JobDone,
                "failed" | "crashed" | "gave_up" => ProgressEventKind::JobFailed,
                _ => ProgressEventKind::Progress,
            };
            self.jobs.apply(ProgressEvent {
                event,
                job_id: Some(progress.job_id.clone()),
                kind: Some(progress.kind.clone()),
                meeting_id: progress
                    .meeting_id
                    .clone()
                    .or_else(|| self.generation.meeting_for(&progress.job_id).cloned()),
                fraction: progress.fraction,
                message: progress.message.clone(),
            })
        };
        // State snapshots repeat the running job's last event: only a new
        // one means another committed write.
        let seen = (
            progress.job_id.clone(),
            progress.phase.clone(),
            progress.message.clone(),
            progress.fraction.map(f64::to_bits),
        );
        if progress.kind == REIDENTIFY_KIND
            && progress.phase != "started"
            && self.recheck.seen_event.as_ref() != Some(&seen)
        {
            self.recheck.seen_event = Some(seen);
            self.recheck_committed(cx);
        }
        if changed {
            self.resolve_job_meetings(cx);
            self.check_generations(cx);
            cx.notify();
        }
    }

    // ---- generations the user started ----

    /// Where finished generations are announced (the app wires
    /// [`crate::notify::NotificationHost::post_local`]).
    pub fn set_generation_sink(&mut self, sink: NotificationSink) {
        self.generation.set_sink(sink);
    }

    pub fn generations(&self) -> &GenerationWatcher {
        &self.generation
    }

    /// Remembers a generation job the user started, so its end is announced
    /// even after its meeting view is gone.
    pub fn watch_generation(&mut self, job: JobId, watched: WatchedJob, cx: &mut Context<Self>) {
        self.generation.watch(job, watched);
        self.check_generations(cx);
    }

    /// Queues a generation the user asked for (`enrich --queue` or
    /// `templates run --queue`, `stdin` holding per-run instructions) in a
    /// task the app owns: the job is watched as soon as the CLI answers,
    /// whether or not the meeting view that asked still exists. The answer
    /// (the job, or why it was refused) arrives on the returned channel; a
    /// dropped receiver changes nothing.
    pub fn submit_generation(
        &mut self,
        args: Vec<OsString>,
        stdin: Option<Vec<u8>>,
        mut watched: WatchedJob,
        cx: &mut Context<Self>,
    ) -> oneshot::Receiver<std::result::Result<GenerationSubmission, CliError>> {
        let expected_job = watched.target.job_id(&watched.meeting_id);
        let run_id = self.generation.reserve(expected_job.clone());
        watched.failures_before = self
            .jobs
            .recent_failures
            .iter()
            .filter(|failure| failure.job_id == expected_job.as_str())
            .count();
        let run = match stdin {
            Some(stdin) => {
                self.cli
                    .run_with_stdin(args, ChildKind::Command, stdin, cx.background_executor())
            }
            None => self.run_cli(args, ChildKind::Command, cx),
        };
        let (tx, rx) = oneshot::channel();
        cx.spawn(async move |this, cx| {
            let result = run.await.and_then(|value| {
                let bad_output = || {
                    CliError::new(
                        "bad_output",
                        "the CLI did not return the generation job and its baseline",
                    )
                };
                let job = value["job_id"]
                    .as_str()
                    .and_then(|id| JobId::parse(id).ok())
                    .filter(|job| job == &expected_job)
                    .ok_or_else(bad_output)?;
                // Read by the CLI under the same write lock as the enqueue:
                // another generation cannot publish between these two steps.
                let baseline =
                    serde_json::from_value(value.get("baseline").cloned().ok_or_else(bad_output)?)
                        .map_err(|_| bad_output())?;
                Ok(GenerationSubmission {
                    generation_run: value["run"].as_str().map(str::to_owned),
                    job_id: job,
                    run_id,
                    baseline,
                    failures_before: watched.failures_before,
                })
            });
            this.update(cx, |state, cx| match &result {
                Ok(receipt) => {
                    watched.baseline = receipt.baseline;
                    watched.generation_run.clone_from(&receipt.generation_run);
                    if state
                        .generation
                        .accept(receipt.job_id.clone(), run_id, watched)
                    {
                        state.check_generations(cx);
                    }
                    state.kick_jobs(cx);
                }
                Err(_) => state.generation.cancel_submission(run_id),
            })
            .ok();
            tx.send(result).ok();
        })
        .detach();
        rx
    }

    /// A meeting view was mounted in `window`; returns the token
    /// [`Self::meeting_view_hidden`] takes when it is dropped.
    pub fn meeting_view_shown(&mut self, id: MeetingId, window: AnyWindowHandle) -> u64 {
        self.shown_token += 1;
        let token = self.shown_token;
        self.shown_meetings.push(ShownMeeting { token, id, window });
        token
    }

    pub fn meeting_view_hidden(&mut self, token: u64) {
        self.shown_meetings.retain(|shown| shown.token != token);
    }

    pub(crate) fn meeting_deleted(
        &mut self,
        id: MeetingId,
        title: &str,
        cleanup: u64,
        cx: &mut Context<Self>,
    ) {
        self.meeting_places.retain(|(meeting, _)| meeting != &id);
        let mut text = format!("Deleted “{title}”.");
        if cleanup > 0 {
            let files = crate::ui::format::count(cleanup, "leftover file", "leftover files");
            text.push_str(&format!(
                " {files} couldn't be removed yet. BroScribe retries the cleanup."
            ));
        }
        self.meeting_deletion = Some((text, cleanup > 0));
        cx.emit(AppEvent::MeetingDeleted(id));
        cx.notify();
    }

    pub(crate) fn meeting_deletion(&self) -> Option<(&str, bool)> {
        self.meeting_deletion
            .as_ref()
            .map(|(text, attention)| (text.as_str(), *attention))
    }

    pub(crate) fn meeting_place(&self, id: &MeetingId) -> Option<MeetingPlace> {
        self.meeting_places
            .iter()
            .find(|(meeting, _)| meeting == id)
            .map(|(_, place)| place.clone())
    }

    pub(crate) fn remember_meeting_place(&mut self, id: MeetingId, place: MeetingPlace) {
        const MAX_RECENT_MEETINGS: usize = 20;
        self.meeting_places.retain(|(meeting, _)| meeting != &id);
        self.meeting_places.push_front((id, place));
        self.meeting_places.truncate(MAX_RECENT_MEETINGS);
    }

    /// Whether the user sees meeting `id` now: it is the route, its view is
    /// mounted, and that view's window is the active window.
    pub fn meeting_in_view(&self, id: &MeetingId, cx: &App) -> bool {
        matches!(&self.route, Route::Library { selected: Some(shown) } if shown == id)
            && self
                .shown_meetings
                .iter()
                .any(|shown| &shown.id == id && cx.active_window() == Some(shown.window))
    }

    /// Reads the watched jobs on the background executor; finished ones are
    /// announced (see [`Self::apply_generation_reads`]).
    fn check_generations(&mut self, cx: &mut Context<Self>) {
        if self.generation.is_empty() {
            return;
        }
        if self.generation.checking {
            self.generation.recheck = true;
            return;
        }
        self.generation.checking = true;
        let lib = self.library.clone();
        let watched = self.generation.snapshot();
        let task = cx.background_executor().spawn(async move {
            watched
                .into_iter()
                .map(|(job, run, w)| {
                    let read = read_job(
                        &lib,
                        &job,
                        &w.meeting_id,
                        &w.target,
                        w.generation_run.as_deref(),
                    );
                    let title = match &read {
                        Ok(JobRead::Pending(_)) => None,
                        _ => lib.load_meeting(&w.meeting_id).ok().and_then(|m| m.title),
                    };
                    (job, run, read, title)
                })
                .collect::<Vec<GenerationRead>>()
        });
        cx.spawn(async move |this, cx| {
            let reads = task.await;
            this.update(cx, |state, cx| state.apply_generation_reads(reads, cx))
                .ok();
        })
        .detach();
    }

    /// Ends the finished jobs among `reads` and posts their notification,
    /// unless the user is looking at that meeting (checked now, at delivery).
    fn apply_generation_reads(&mut self, reads: Vec<GenerationRead>, cx: &mut Context<Self>) {
        self.generation.checking = false;
        for (job, run, read, title) in reads {
            let Some(watched) = self.generation.get(&job, run) else {
                continue;
            };
            let outcome = match read {
                // An attempt failed but the scheduler runs it again: the
                // job is not over.
                Ok(JobRead::Pending(_) | JobRead::Retrying(_)) => continue,
                Ok(JobRead::Cancelled | JobRead::Superseded) => {
                    self.generation.finish(&job, run);
                    continue;
                }
                // The stored error in words, never as stored (WRT-04).
                Ok(JobRead::Failed(error)) => {
                    Outcome::Failed(generation_cause(error.as_deref().unwrap_or_default()))
                }
                Ok(JobRead::Finished(created)) => {
                    let failures: Vec<_> = self
                        .jobs
                        .recent_failures
                        .iter()
                        .filter(|f| f.job_id == job.as_str())
                        .collect();
                    let failure = failures
                        .last()
                        .filter(|_| failures.len() > watched.failures_before)
                        .map(|f| generation_cause(f.message.as_deref().unwrap_or_default()));
                    finished_outcome(&watched.target, watched.baseline, created, failure)
                }
                Err(e) => {
                    tracing::warn!(code = e.code(), "reading a generation job");
                    continue;
                }
            };
            let Some(watched) = self.generation.finish(&job, run) else {
                continue;
            };
            if self.meeting_in_view(&watched.meeting_id, cx) {
                continue;
            }
            if let Some(sink) = self.generation.sink() {
                sink(notification(&watched, title.as_deref(), &outcome), cx);
            }
        }
        if std::mem::take(&mut self.generation.recheck) {
            self.check_generations(cx);
        }
    }

    // ---- navigation ----

    /// Opens the review queue in its focused "Identify voices" mode, for
    /// every waiting voice or (`Some`) the voices heard in one meeting.
    pub fn open_identify(&mut self, meeting: Option<MeetingId>, cx: &mut Context<Self>) {
        self.identify_request = Some(meeting);
        self.route = Route::ReviewQueue;
        cx.notify();
    }

    /// The pending [`Self::open_identify`] request, taken once.
    pub fn take_identify_request(&mut self) -> Option<Option<MeetingId>> {
        self.identify_request.take()
    }

    pub fn navigate(&mut self, route: Route, cx: &mut Context<Self>) {
        if self.route != route {
            self.route = route;
            cx.notify();
        }
    }

    // ---- search ----

    /// Starts a bounded, cancellable read of the first search page.
    pub fn search_meetings(&mut self, query: SearchQuery, cx: &mut Context<Self>) {
        let job = self.search.start(query);
        self.run_search(job, cx);
    }

    #[cfg(test)]
    pub(crate) fn hold_search_for_test(&mut self, cx: &mut Context<Self>) -> PageJob {
        let job = self.search.start(SearchQuery::default());
        cx.notify();
        job
    }

    /// Refreshes at least as many meetings as are already listed.
    pub fn rerun_search(&mut self, cx: &mut Context<Self>) {
        let job = self.search.refresh();
        self.run_search(job, cx);
    }

    pub fn load_more_meetings(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(job) = self.search.more() else {
            return false;
        };
        self.run_search(job, cx);
        true
    }

    pub fn apply_search(
        &mut self,
        generation: u64,
        outcome: PageOutcome,
        cx: &mut Context<Self>,
    ) -> bool {
        let applied = self.search.apply(generation, outcome);
        if applied {
            cx.notify();
        }
        applied
    }

    fn run_search(&mut self, job: PageJob, cx: &mut Context<Self>) {
        cx.notify();
        let generation = job.generation();
        let root = self.library.root().to_path_buf();
        let cli = self.cli.clone();
        let task = cx
            .background_executor()
            .spawn(async move { job.run(&root, &cli) });
        cx.spawn(async move |this, cx| {
            let outcome = task.await;
            this.update(cx, |state, cx| state.apply_search(generation, outcome, cx))
                .ok();
        })
        .detach();
    }

    // ---- review count ----

    /// Recounts speakers waiting for review on the background executor.
    pub fn refresh_review_count(&mut self, cx: &mut Context<Self>) {
        self.review.generation += 1;
        let generation = self.review.generation;
        let root = self.library.root().to_path_buf();
        let task = self.cli.spawn(
            cx.background_executor(),
            async move { read_review_count(&root) },
        );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |state, cx| {
                state.apply_review_count(generation, result, cx)
            })
            .ok();
        })
        .detach();
    }

    /// Applies a review count; `false` when stale. Emits
    /// [`AppEvent::ReviewCountChanged`] when the number changes.
    pub fn apply_review_count(
        &mut self,
        generation: u64,
        result: Result<usize>,
        cx: &mut Context<Self>,
    ) -> bool {
        if generation != self.review.generation {
            return false;
        }
        match result {
            Ok(n) => {
                self.review.error = None;
                if self.review.count != Some(n) {
                    self.review.count = Some(n);
                    cx.emit(AppEvent::ReviewCountChanged(n));
                }
            }
            Err(e) => {
                tracing::warn!(code = e.code(), "reading the review count");
                self.review.error = Some(jobs::core_user_text(&e));
            }
        }
        cx.notify();
        true
    }

    // ---- refresh ----

    /// Applies external changes reported by the library watcher.
    pub fn library_changed(&mut self, changes: &Changes, cx: &mut Context<Self>) {
        if changes.is_empty() {
            return;
        }
        if changes.affects_meetings() {
            self.meetings_revision += 1;
            self.rerun_search(cx);
        }
        if changes.affects_people() {
            self.people_revision += 1;
        }
        if changes.affects_meetings() || changes.affects_people() {
            self.refresh_review_count(cx);
        }
        if changes.queue || changes.all {
            self.process_jobs(cx);
            self.check_generations(cx);
            self.refresh_recheck(cx);
        }
        cx.emit(AppEvent::LibraryChanged(changes.clone()));
        cx.notify();
    }

    /// Reloads everything shown (window activation, launch) and starts the
    /// worker when jobs are pending. Never resets a crash circuit.
    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        self.meetings_revision += 1;
        self.people_revision += 1;
        self.rerun_search(cx);
        self.refresh_review_count(cx);
        self.process_jobs(cx);
        self.check_generations(cx);
        self.refresh_recheck(cx);
        cx.notify();
    }

    /// The watcher keeps Library and People current while the window is in
    /// the background. Fall back to a read on activation if it failed to start.
    pub(crate) fn window_activated(&mut self, cx: &mut Context<Self>) {
        if self.watch.is_none() {
            self.refresh_all(cx);
        } else {
            self.process_jobs(cx);
            self.check_generations(cx);
            self.refresh_recheck(cx);
        }
    }

    pub(crate) fn set_watch(&mut self, watch: LibraryWatch) {
        self.watch = Some(watch);
    }

    // ---- background processing ----

    /// Makes sure pending queue jobs get processed: call after a user action
    /// enqueues work (import, naming, retry, playback). Being a user action,
    /// it also resets the crash circuit so the worker gets another try.
    pub fn kick_jobs(&mut self, cx: &mut Context<Self>) {
        self.worker.crashes = 0;
        self.process_jobs(cx);
    }

    /// Starts the worker when the queue has work, or asks the running one
    /// for another pass. Leaves the crash circuit as it is.
    fn process_jobs(&mut self, cx: &mut Context<Self>) {
        if let Some(daemon) = &self.daemon {
            if daemon.read(cx).connected {
                // The agent runs the queue and wakes its waiting jobs.
                self.deferred_wakeup = None;
                daemon.update(cx, |daemon, cx| {
                    daemon.send(broscribe_ipc::Call::KickQueue, cx)
                });
                return;
            }
            if !self.local_jobs_allowed {
                self.deferred_wakeup = None;
                return;
            }
        }
        if self.worker.is_running() {
            self.worker.request_rerun();
            return;
        }
        self.start_worker_if_pending(cx);
    }

    fn start_worker_if_pending(&mut self, cx: &mut Context<Self>) {
        self.check_queue(true, cx);
    }

    /// Reads the queue off the main thread: reports crashed jobs, keeps the
    /// wake-up for jobs waiting for their time and, when `start`, starts
    /// the worker if there is work. A `start` request made while a check is
    /// in flight is honoured by that check.
    fn check_queue(&mut self, start: bool, cx: &mut Context<Self>) {
        if !self.local_jobs_allowed
            || self.worker.is_running()
            || (start && self.worker.crashes >= MAX_CONSECUTIVE_CRASHES)
        {
            return;
        }
        self.worker.start_after_check |= start;
        if self.worker.checking {
            return;
        }
        self.worker.checking = true;
        let root = self.library.root().to_path_buf();
        let now = (self.clock)();
        let task = self.cli.spawn(cx.background_executor(), async move {
            jobs::has_pending_jobs(&root, &*NativePlatform::process_probe(), now)
        });
        cx.spawn(async move |this, cx| {
            let pending = task.await;
            this.update(cx, |state, cx| {
                state.worker.checking = false;
                let start = std::mem::take(&mut state.worker.start_after_check);
                match pending {
                    Ok(check) => {
                        if !check.crashed.is_empty() {
                            // No worker reports these: show them as failed.
                            for failed in check.crashed {
                                state.jobs.record_failure(failed);
                            }
                            state.resolve_job_meetings(cx);
                            cx.notify();
                        }
                        if check.pending && start {
                            state.start_worker(cx);
                        } else if matches!(state.jobs.worker, WorkerState::Restarting { .. }) {
                            state.jobs.worker = WorkerState::Idle;
                            cx.notify();
                        }
                        state.schedule_wakeup(check.wake_at, cx);
                    }
                    Err(e) => tracing::warn!(code = e.code(), "checking the queue"),
                }
            })
            .ok();
        })
        .detach();
    }

    /// Keeps one timer for the earliest waiting job, `at`, while this app's
    /// worker runs the queue and is idle; cancels it otherwise (no waiting
    /// job, the worker running (it is checked again when it exits) or the
    /// agent owning the queue). A due job gets [`DEFERRED_WAKEUP_MIN`], so a
    /// job no worker takes is never retried in a tight loop.
    fn schedule_wakeup(&mut self, at: Option<DateTime<FixedOffset>>, cx: &mut Context<Self>) {
        let agent = self
            .daemon
            .as_ref()
            .is_some_and(|daemon| daemon.read(cx).connected);
        let at = at.filter(|_| self.local_jobs_allowed && !agent && !self.worker.is_running());
        let Some(at) = at else {
            self.deferred_wakeup = None;
            return;
        };
        if self.deferred_wakeup.as_ref().is_some_and(|w| w.at == at) {
            return;
        }
        let delay = (at - (self.clock)())
            .to_std()
            .unwrap_or_default()
            .max(DEFERRED_WAKEUP_MIN);
        let timer = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            this.update(cx, |state, cx| {
                state.deferred_wakeup = None;
                state.process_jobs(cx);
            })
            .ok();
        });
        self.deferred_wakeup = Some(DeferredWakeup { at, _timer: timer });
    }

    fn start_worker(&mut self, cx: &mut Context<Self>) {
        if !self.local_jobs_allowed || self.worker.is_running() {
            return;
        }
        self.worker.spawn();
        if self.worker.is_running() {
            self.jobs.worker = WorkerState::Running;
            cx.notify();
        }
    }

    /// When the app starts its worker again for a waiting job, if it will.
    #[cfg(test)]
    pub(crate) fn deferred_wakeup_at(&self) -> Option<DateTime<FixedOffset>> {
        self.deferred_wakeup.as_ref().map(|w| w.at)
    }

    #[cfg(test)]
    pub(crate) fn set_clock(&mut self, clock: impl Fn() -> DateTime<FixedOffset> + 'static) {
        self.clock = Rc::new(clock);
    }

    /// The worker handle, to start it with a test launcher and report its
    /// exit on the test thread.
    #[cfg(test)]
    pub(crate) fn worker_mut(&mut self) -> &mut WorkerControl {
        &mut self.worker
    }

    /// Shows the worker as `worker` (busy jobs hold a library switch back).
    #[cfg(test)]
    pub(crate) fn set_worker_state(&mut self, worker: WorkerState) {
        self.jobs.worker = worker;
    }

    /// Applies one coalesced batch of worker messages (one UI update).
    pub(crate) fn apply_worker_batch(&mut self, batch: WorkerBatch, cx: &mut Context<Self>) {
        let mut restart_after_crash = false;
        let mut check_again = false;
        let mut finished = false;
        let mut recheck_progress = false;
        let mut changed = false;
        for msg in batch.into_msgs() {
            match msg {
                WorkerMsg::Progress(mut ev) => {
                    if ev.meeting_id.is_none() {
                        ev.meeting_id = ev
                            .job_id
                            .as_deref()
                            .and_then(|id| self.generation.meeting_for(id))
                            .cloned();
                    }
                    recheck_progress |= ev.kind.as_deref() == Some(REIDENTIFY_KIND)
                        && ev.event != crate::jobs::ProgressEventKind::JobStarted;
                    changed |= self.jobs.apply(ev);
                }
                WorkerMsg::Finished => {
                    changed |=
                        !self.jobs.active.is_empty() || self.jobs.worker != WorkerState::Idle;
                    self.worker.exited();
                    self.worker.crashes = 0;
                    self.jobs.active.clear();
                    self.jobs.worker = WorkerState::Idle;
                    check_again = self.worker.take_rerun();
                    finished = true;
                }
                WorkerMsg::Crashed(detail) => {
                    changed = true;
                    self.worker.exited();
                    self.worker.take_rerun();
                    self.jobs.active.clear();
                    self.worker.crashes += 1;
                    let crashes = self.worker.crashes;
                    tracing::warn!(crashes, "queue worker crashed");
                    if crashes < MAX_CONSECUTIVE_CRASHES {
                        self.jobs.worker = WorkerState::Restarting { crashes };
                        restart_after_crash = true;
                    } else {
                        self.jobs.worker = WorkerState::Unavailable {
                            detail: detail.clone(),
                            crashes: Some(crashes),
                        };
                    }
                    self.jobs.last_crash = Some(detail);
                }
                WorkerMsg::SpawnFailed(failure) => {
                    self.worker.exited();
                    self.worker.take_rerun();
                    tracing::error!(kind = ?failure.kind, "cannot start the queue worker");
                    let worker = WorkerState::Unavailable {
                        detail: failure.to_string(),
                        crashes: None,
                    };
                    changed |= self.jobs.worker != worker;
                    self.jobs.worker = worker;
                }
            }
        }
        self.jobs.ui_updates += 1;
        if restart_after_crash {
            // The dead worker's lease is reclaimed by the next claim; its job
            // is retried with one more attempt.
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(CRASH_RESTART_DELAY).await;
                this.update(cx, |state, cx| {
                    if matches!(state.jobs.worker, WorkerState::Restarting { .. }) {
                        state.start_worker_if_pending(cx);
                    }
                })
                .ok();
            })
            .detach();
        } else if check_again {
            self.start_worker_if_pending(cx);
        } else if finished {
            // Jobs it left waiting for their time: wake it up then.
            self.check_queue(false, cx);
        }
        if recheck_progress {
            self.recheck_committed(cx);
        }
        self.check_generations(cx);
        if changed {
            self.resolve_job_meetings(cx);
            cx.notify();
        }
    }

    /// Runs a one-shot `broscribe --json` command on the background executor
    /// (see [`CliRunner::run`]).
    pub fn run_cli(
        &self,
        args: Vec<OsString>,
        kind: ChildKind,
        cx: &App,
    ) -> Task<std::result::Result<serde_json::Value, CliError>> {
        self.cli.run(args, kind, cx.background_executor())
    }

    /// Launch recovery: removes setup Try-it imports (job and staged audio)
    /// a killed or cancelled setup step left behind, whether or not another
    /// import follows (`broscribe queue discard-setup`). Failures are logged.
    pub fn discard_abandoned_setup_imports(&self, cx: &App) {
        let task = self.run_cli(
            vec!["queue".into(), "discard-setup".into()],
            ChildKind::Command,
            cx,
        );
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = task.await {
                    tracing::warn!(code = %e.code, "discarding abandoned setup imports");
                }
            })
            .detach();
    }

    // ---- media owned by the app ----

    /// The player used for playback, created on first use by the views.
    pub fn player_mut(&mut self) -> Option<&mut (dyn AudioPlayer + 'static)> {
        self.player.as_deref_mut()
    }

    /// Starting playback transfers exclusive ownership from the previous view.
    pub fn claim_player(&mut self) -> u64 {
        self.player_owner = self.player_owner.wrapping_add(1);
        self.player_owner
    }

    pub fn player_owned(&mut self, owner: u64) -> Option<&mut (dyn AudioPlayer + 'static)> {
        if self.player_owner != owner {
            return None;
        }
        self.player.as_deref_mut()
    }

    /// Releases only this view's current playback, including pending preparation.
    pub fn release_player(&mut self, owner: u64) -> bool {
        if self.player_owner != owner {
            return false;
        }
        self.player_owner = self.player_owner.wrapping_add(1);
        if let Some(player) = self.player.as_mut() {
            player.stop();
        }
        true
    }

    /// The launcher web links open through, once set.
    pub fn browser(&self) -> Option<Arc<dyn BrowserLauncher>> {
        self.browser.clone()
    }

    pub fn set_browser(&mut self, browser: Arc<dyn BrowserLauncher>) {
        self.browser = Some(browser);
    }

    pub fn set_player(&mut self, player: Box<dyn AudioPlayer>) {
        self.player_owner = self.player_owner.wrapping_add(1);
        if let Some(mut old) = self.player.replace(player) {
            old.stop();
        }
    }

    /// Called by [`crate::views::setup::SetupAssistant`] on creation / release.
    pub fn setup_assistant_opened(&mut self) {
        self.setup_assistants += 1;
    }

    pub fn setup_assistant_closed(&mut self) {
        self.setup_assistants = self.setup_assistants.saturating_sub(1);
    }

    /// Called from `cx.on_app_quit`: stops playback and the setup children
    /// this app started. Setup owns its capture shutdown; the queue worker
    /// is left to finish its current job under its own lease.
    ///
    /// Quit order (plan item 22): while a setup assistant is alive, its own
    /// quit hook clears the voice-session token under the library lock and
    /// only then kills the setup children, so they are not killed here
    /// (killing first could leave a token whose child is gone, or let a
    /// child commit after the app stopped tracking it).
    pub fn on_quit(&mut self) -> Option<jobs::ChildrenStopping> {
        if let Some(player) = self.player.as_mut() {
            player.stop();
        }
        self.search.cancel();
        let stopping =
            (self.setup_assistants == 0).then(|| self.cli.children().cancel_setup_children());
        self.watch = None;
        stopping
    }
}

/// Speakers waiting for review; 0 when the index does not exist yet.
fn read_review_count(root: &Path) -> Result<usize> {
    match Library::open_existing(root)
        .and_then(|lib| broscribe_core::ops::review_naming_count(&lib))
    {
        Ok(count) => Ok(count),
        Err(Error::NotFound(_)) if !Index::path(root).exists() => Ok(0),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use broscribe_platform::fakes::FakePaths;
    use broscribe_platform::resolve_paths;
    use gpui::{Entity, TestAppContext};

    use super::*;
    use crate::jobs::{PROGRESS_INTERVAL, parse_progress_line};

    fn new_state(cx: &mut TestAppContext, dir: &Path) -> Entity<AppState> {
        let library = Library::open(dir.join("lib")).unwrap();
        let paths = resolve_paths(&FakePaths::new(dir)).unwrap();
        cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    paths,
                    Config::default(),
                    library,
                    dir.join("no-such-cli"),
                ),
                cx,
            )
        })
    }

    #[gpui::test]
    fn a_parked_library_writer_retains_admission_after_app_state_is_released(
        cx: &mut TestAppContext,
    ) {
        use broscribe_core::PersonId;
        use broscribe_core::maintenance::ModeChange;
        use broscribe_core::schema::Person;
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let (library, data) = state.read_with(cx, |state, _| {
            (state.library().clone(), state.paths().data_dir.clone())
        });
        let root = library.root().to_path_buf();
        let person = Person::new(PersonId::random(), "Synthetic maintenance writer");
        let id = person.id.clone();
        let (release, parked) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            parked.recv().unwrap();
            library.write(|tx| tx.save_person(&person)).unwrap();
        });
        drop(state);
        // Entity destruction is committed by the next real app update, not
        // by merely draining the deterministic task dispatcher.
        cx.update(|_| {});
        cx.run_until_parked();
        assert!(
            ModeChange::acquire(&data).is_err(),
            "the parked writer still owns admission"
        );
        release.send(()).unwrap();
        writer.join().unwrap();
        assert!(
            ModeChange::acquire(&data).is_ok(),
            "actual writer completion releases admission"
        );
        assert_eq!(
            Library::open_existing(root)
                .unwrap()
                .load_person(&id)
                .unwrap()
                .name,
            "Synthetic maintenance writer"
        );
    }

    #[gpui::test]
    fn older_config_replies_never_replace_the_latest_persisted_edit(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let data = state.read_with(cx, |s, _| s.paths().data_dir.clone());
        let (earlier, ()) = Config::update(&data, |config| {
            config.default_language = Some("en".into());
            Ok(())
        })
        .unwrap();
        let (latest, ()) = Config::update(&data, |config| {
            config.default_language = Some("pt-BR".into());
            Ok(())
        })
        .unwrap();
        let revision = latest.revision();
        state.update(cx, |s, cx| s.set_config(latest, cx));
        state.update(cx, |s, cx| s.set_config(earlier, cx));
        state.read_with(cx, |s, _| {
            assert_eq!(s.config().revision(), revision);
            assert_eq!(s.config().default_language.as_deref(), Some("pt-BR"));
        });
        assert_eq!(Config::load(&data).unwrap().revision(), revision);
    }

    fn line(json: &str) -> WorkerMsg {
        WorkerMsg::Progress(parse_progress_line(json).unwrap())
    }

    #[gpui::test]
    fn people_samples_load_once_per_people_revision_and_model(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let (first, second) =
            state.update(cx, |s, cx| (s.people_samples(cx), s.people_samples(cx)));
        assert!(first.ptr_eq(&second), "concurrent pickers share one load");
        // A picker awaits the load; the result is then kept.
        cx.executor().spawn(first.clone()).detach();
        cx.run_until_parked();
        assert!(matches!(first.peek(), Some(Ok(_))));
        let reused = state.update(cx, |s, cx| s.people_samples(cx));
        assert!(reused.ptr_eq(&first));
        let revised = state.update(cx, |s, cx| {
            s.refresh_all(cx);
            s.people_samples(cx)
        });
        assert!(!revised.ptr_eq(&first), "a new people revision reloads");
        let other_model = state.update(cx, |s, cx| {
            let mut config = s.config().clone();
            config.profile.speaker_embedding = "another-embedder".into();
            s.set_config(config, cx);
            s.people_samples(cx)
        });
        assert!(!other_model.ptr_eq(&revised), "a model change reloads");
    }

    #[gpui::test]
    fn playback_owner_prevents_stale_view_stop(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        state.update(cx, |state, _| {
            state.set_player(Box::new(broscribe_platform::fakes::FakeAudioPlayer::new(
                30.0,
            )));
            let first = state.claim_player();
            let second = state.claim_player();
            assert!(state.player_owned(first).is_none());
            assert!(state.player_owned(second).is_some());
        });
    }

    #[gpui::test]
    fn only_user_actions_reset_worker_crash_circuit(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        state.update(cx, |state, cx| {
            state.worker.crashes = MAX_CONSECUTIVE_CRASHES;
            // Window activation and queue-folder changes leave it open.
            state.refresh_all(cx);
            state.library_changed(
                &Changes {
                    queue: true,
                    ..Changes::default()
                },
                cx,
            );
            assert_eq!(state.worker.crashes, MAX_CONSECUTIVE_CRASHES);
            assert!(!state.worker.checking);
            // A user action that enqueued work closes it and checks the queue.
            state.kick_jobs(cx);
            assert_eq!(state.worker.crashes, 0);
            assert!(state.worker.checking);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn review_count_without_index_is_zero(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        state.update(cx, |s, cx| s.refresh_review_count(cx));
        cx.run_until_parked();
        state.read_with(cx, |s, _| assert_eq!(s.review_count(), Some(0)));
        state.update(cx, |s, cx| {
            assert!(!s.apply_review_count(0, Ok(7), cx));
            assert_eq!(s.review_count(), Some(0));
        });
    }

    #[gpui::test]
    fn review_count_error_uses_words_without_the_library_path(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        state.update(cx, |s, cx| {
            let generation = s.review.generation;
            assert!(s.apply_review_count(
                generation,
                Err(broscribe_core::Error::Schema(
                    "/synthetic/private/library".into()
                )),
                cx,
            ));
            assert_eq!(
                s.review_count_error(),
                Some("BroScribe couldn't read its library data.")
            );
        });
    }

    #[gpui::test]
    fn job_progress_is_coalesced(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let tx = state.read_with(cx, |s, _| s.worker.sender());

        // A burst arriving at once is one UI update.
        tx.send(line(
            r#"{"event":"job_started","job_id":"j-000001","kind":"import"}"#,
        ));
        for i in 1..=100 {
            let json = format!(
                r#"{{"event":"progress","job_id":"j-000001","fraction":{},"message":"step {i}"}}"#,
                f64::from(i) / 200.0
            );
            tx.send(line(&json));
        }
        cx.run_until_parked();
        state.read_with(cx, |s, _| {
            let jobs = s.jobs();
            assert_eq!(jobs.ui_updates, 1);
            assert_eq!(jobs.active.len(), 1);
            assert_eq!(jobs.active[0].fraction, Some(0.5));
            assert_eq!(jobs.active[0].message.as_deref(), Some("step 100"));
        });

        // More lines within the interval wait for it.
        for i in 101..=150 {
            let json = format!(
                r#"{{"event":"progress","job_id":"j-000001","fraction":{}}}"#,
                f64::from(i) / 200.0
            );
            tx.send(line(&json));
        }
        tx.send(line(r#"{"event":"job_done","job_id":"j-000001"}"#));
        cx.run_until_parked();
        state.read_with(cx, |s, _| assert_eq!(s.jobs().ui_updates, 1));

        cx.executor()
            .advance_clock(PROGRESS_INTERVAL - Duration::from_millis(1));
        cx.run_until_parked();
        state.read_with(cx, |s, _| assert_eq!(s.jobs().ui_updates, 1));

        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        state.read_with(cx, |s, _| {
            let jobs = s.jobs();
            assert_eq!(jobs.ui_updates, 2);
            assert!(jobs.active.is_empty());
            assert_eq!(jobs.completed, 1);
        });
    }

    #[gpui::test]
    fn crashed_worker_is_reported(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        let tx = state.read_with(cx, |s, _| s.worker.sender());
        tx.send(line(
            r#"{"event":"job_started","job_id":"j-000001","kind":"import"}"#,
        ));
        tx.send(WorkerMsg::Crashed("signal: 9 (SIGKILL)".into()));
        cx.run_until_parked();
        state.read_with(cx, |s, _| {
            let jobs = s.jobs();
            assert_eq!(jobs.worker, WorkerState::Restarting { crashes: 1 });
            assert!(jobs.active.is_empty());
            assert_eq!(jobs.last_crash.as_deref(), Some("signal: 9 (SIGKILL)"));
        });
        // The restart finds an empty queue and spawns nothing.
        cx.executor().advance_clock(CRASH_RESTART_DELAY);
        cx.run_until_parked();
        state.read_with(cx, |s, _| {
            assert!(!s.worker.is_running());
            assert_eq!(s.jobs().worker, WorkerState::Idle);
        });
    }

    /// Without an agent, a speaker recheck the worker put back to wait for
    /// a busy meeting (its `not_before`) starts the app's worker again by
    /// itself once due: one timer, no worker before it, none while the agent
    /// owns the queue, and none again once the job is done (no loop).
    #[gpui::test]
    fn waiting_recheck_wakes_the_local_worker_once_due(cx: &mut TestAppContext) {
        use std::cell::{Cell, RefCell};

        use broscribe_core::fsutil;
        use broscribe_core::learning;
        use broscribe_core::queue::{JOB_JSON, JobState, Queue};
        use broscribe_core::schema::{self, Meeting, ProcessingStatus, Source};
        use broscribe_pipeline::reidentify;
        use broscribe_platform::fakes::FakeProcessProbe;
        use chrono::TimeDelta;

        let dir = tempfile::tempdir().unwrap();
        // The app's worker, started on the test thread: its launches are
        // recorded, and the test reports its exit through the same channel.
        let launches: Rc<RefCell<Vec<Vec<OsString>>>> = Rc::default();
        let count = || launches.borrow().len();
        let library = Library::open(dir.path().join("lib")).unwrap();
        let lib = library.clone();
        let paths = resolve_paths(&FakePaths::new(dir.path())).unwrap();
        let data = paths.data_dir.clone();
        let config = Config::default();
        let model = config.profile.speaker_embedding.clone();

        // A `done` meeting another job works on, and a naming's recheck
        // that a worker runs and puts back to wait for it.
        let mut meeting =
            Meeting::new(lib.new_meeting_id().unwrap(), Source::Import, schema::now());
        meeting.processing.status = ProcessingStatus::Done;
        lib.write(|tx| {
            tx.create_meeting_dir(&meeting)?;
            tx.save_meeting(&meeting)
        })
        .unwrap();
        let queue = Queue::open(lib.root()).unwrap();
        let other = lib
            .write(|tx| {
                queue.enqueue(
                    tx,
                    "reprocess",
                    meeting.id.as_str(),
                    serde_json::json!({ "meeting_id": meeting.id }),
                )
            })
            .unwrap();
        let job = lib
            .write(|tx| learning::request(tx, &model, Some(&meeting.id)))
            .unwrap()
            .job_id;
        let probe = FakeProcessProbe::new(std::process::id());
        let run = || {
            for _ in 0..5 {
                if !queue.get(&job).unwrap().is_claimable() {
                    return;
                }
                let lease = queue.claim(&lib, &job, &probe).unwrap().unwrap();
                reidentify::run(
                    &lib,
                    &lease,
                    &data,
                    reidentify::MEETINGS_PER_RUN,
                    &mut |_| {},
                )
                .unwrap();
                lease.release(&lib).unwrap();
            }
        };
        run();
        let waiting = queue.get(&job).unwrap();
        assert_eq!(waiting.state, JobState::Queued);
        let due = waiting
            .not_before
            .expect("the recheck waits for the meeting");
        // The other job ends; nothing else is queued.
        lib.write(|tx| queue.set_state(tx, &other, JobState::Done, None))
            .unwrap();

        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(paths, config, library, dir.path().join("broscribe")),
                cx,
            )
        });
        state.update(cx, |s, _| {
            let launches = launches.clone();
            s.worker.set_launcher(move |_, args, _| {
                launches.borrow_mut().push(args.to_vec());
                Ok(())
            });
        });
        // Anchor fake time to the persisted deadline: filesystem/setup time
        // must not consume the interval this test advances explicitly.
        let start = due - TimeDelta::seconds(60);
        let elapsed = Rc::new(Cell::new(0_i64));
        state.update(cx, |s, _| {
            let elapsed = elapsed.clone();
            s.set_clock(move || start + TimeDelta::seconds(elapsed.get()));
        });
        let advance = |cx: &mut TestAppContext, secs: u64| {
            elapsed.set(elapsed.get() + secs as i64);
            cx.executor().advance_clock(Duration::from_secs(secs));
            cx.run_until_parked();
        };
        let wakeup = |cx: &mut TestAppContext| state.read_with(cx, |s, _| s.deferred_wakeup_at());
        let running = |cx: &mut TestAppContext| state.read_with(cx, |s, _| s.worker.is_running());

        state.update(cx, |s, cx| s.kick_jobs(cx));
        cx.run_until_parked();
        assert_eq!(wakeup(cx), Some(due));
        assert!(!running(cx));

        // The agent takes the queue over: its scheduler wakes the job, the
        // app's timer is gone and starts nothing.
        state.update(cx, |s, cx| s.set_local_jobs_allowed(false, cx));
        assert_eq!(wakeup(cx), None);
        advance(cx, 30);
        // The app runs the queue again: one timer for the same job.
        state.update(cx, |s, cx| s.set_local_jobs_allowed(true, cx));
        cx.run_until_parked();
        assert_eq!(wakeup(cx), Some(due));
        advance(cx, 29);
        assert!(!running(cx));
        assert_eq!(count(), 0, "no worker before the job is due");

        // Due: the worker starts without any user action.
        advance(cx, 2);
        assert!(running(cx));
        assert_eq!(wakeup(cx), None);
        assert_eq!(count(), 1);
        let library_arg = OsString::from(lib.root().as_os_str());
        assert_eq!(
            launches.borrow()[0],
            [
                OsString::from("--library"),
                library_arg,
                "queue".into(),
                "work".into(),
                "--json-progress".into(),
            ]
        );
        // The worker takes the job (its wall clock has reached the time
        // too), finishes it and exits.
        let mut stored = queue.get(&job).unwrap();
        stored.not_before = Some(schema::now() - TimeDelta::seconds(1));
        fsutil::write_json_atomic(&queue.job_dir(&job).join(JOB_JSON), &stored).unwrap();
        run();
        assert_eq!(queue.get(&job).unwrap().state, JobState::Done);
        let tx = state.read_with(cx, |s, _| s.worker.sender());
        tx.send(WorkerMsg::Finished);
        cx.run_until_parked();
        assert!(!running(cx));
        // Nothing waits: no timer, and no worker started again.
        advance(cx, 120);
        assert_eq!(wakeup(cx), None);
        assert!(!running(cx));
        assert_eq!(count(), 1);
    }

    /// A job the app's worker failed with attempts left, without an agent,
    /// runs again by itself once its retry delay passed: the worker exits,
    /// one timer waits for the delay, and the next worker starts without
    /// any user action.
    #[gpui::test]
    fn failed_job_with_attempts_left_restarts_the_local_worker_after_its_delay(
        cx: &mut TestAppContext,
    ) {
        use std::cell::Cell;

        use broscribe_core::queue::{JobState, Queue};
        use broscribe_core::schema;
        use broscribe_platform::fakes::FakeProcessProbe;
        use chrono::TimeDelta;

        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path().join("lib")).unwrap();
        let lib = library.clone();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Config::default(),
                    library,
                    dir.path().join("broscribe"),
                ),
                cx,
            )
        });
        let launches = Rc::new(Cell::new(0_usize));
        state.update(cx, |s, _| {
            let launches = launches.clone();
            s.worker.set_launcher(move |_, _, _| {
                launches.set(launches.get() + 1);
                Ok(())
            });
        });
        let running = |cx: &mut TestAppContext| state.read_with(cx, |s, _| s.worker.is_running());
        let wakeup = |cx: &mut TestAppContext| state.read_with(cx, |s, _| s.deferred_wakeup_at());

        let queue = Queue::open(lib.root()).unwrap();
        let job = lib
            .write(|tx| queue.enqueue(tx, "import", "transient", serde_json::json!({})))
            .unwrap();
        state.update(cx, |s, cx| s.kick_jobs(cx));
        cx.run_until_parked();
        assert!(running(cx));
        assert_eq!(launches.get(), 1);

        // The worker fails the job once, transiently, and exits.
        let lease = queue
            .claim(&lib, &job, &FakeProcessProbe::new(4242))
            .unwrap()
            .unwrap();
        lease.fail(&lib, "synthetic transient failure").unwrap();
        lease.release(&lib).unwrap();
        let failed = queue.get(&job).unwrap();
        assert_eq!((failed.state, failed.attempts), (JobState::Failed, 1));
        let retry_at = failed.updated_at + TimeDelta::from_std(jobs::retry_delay(1)).unwrap();
        // The app's wall clock follows the test executor's fake time.
        let start = schema::now();
        let elapsed = Rc::new(Cell::new(0_i64));
        state.update(cx, |s, _| {
            let elapsed = elapsed.clone();
            s.set_clock(move || start + TimeDelta::seconds(elapsed.get()));
        });
        let advance = |cx: &mut TestAppContext, secs: u64| {
            elapsed.set(elapsed.get() + secs as i64);
            cx.executor().advance_clock(Duration::from_secs(secs));
            cx.run_until_parked();
        };
        let tx = state.read_with(cx, |s, _| s.worker.sender());
        tx.send(WorkerMsg::Finished);
        cx.run_until_parked();
        assert!(!running(cx));
        assert_eq!(wakeup(cx), Some(retry_at), "one timer for the retry");

        advance(cx, 9);
        assert!(!running(cx));
        assert_eq!(launches.get(), 1, "no worker before the retry delay");
        advance(cx, 2);
        assert!(running(cx), "the retry starts the worker by itself");
        assert_eq!(launches.get(), 2);
        assert_eq!(wakeup(cx), None);
    }

    /// The agent repeats the running job's last event in every state
    /// snapshot: observers hear only of changes to the jobs shown.
    #[gpui::test]
    fn repeated_agent_progress_notifies_only_on_change(cx: &mut TestAppContext) {
        use std::cell::Cell;

        let dir = tempfile::tempdir().unwrap();
        let state = new_state(cx, dir.path());
        cx.run_until_parked();
        let notified = Rc::new(Cell::new(0_usize));
        let _observer = cx.update(|cx| {
            let notified = notified.clone();
            cx.observe(&state, move |_, _| notified.set(notified.get() + 1))
        });
        let event = |job_id: &str, phase: &str| broscribe_ipc::JobProgress {
            job_id: job_id.into(),
            kind: "import".into(),
            meeting_id: None,
            phase: phase.into(),
            fraction: Some(0.5),
            message: Some("Transcribing".into()),
        };
        let deliver = |cx: &mut TestAppContext, progress: &broscribe_ipc::JobProgress| {
            state.update(cx, |s, cx| s.daemon_progress(progress, cx));
            cx.run_until_parked();
        };

        let running = event("j-1", "progress");
        for _ in 0..3 {
            deliver(cx, &running);
        }
        assert_eq!(notified.get(), 1, "a repeated event changes nothing");
        // A job the app never showed is left waiting: nothing to update.
        deliver(cx, &event("j-2", "waiting"));
        assert_eq!(notified.get(), 1);
        deliver(cx, &event("j-1", "waiting"));
        assert_eq!(notified.get(), 2);
        deliver(cx, &event("j-1", "waiting"));
        assert_eq!(notified.get(), 2);
        state.read_with(cx, |s, _| assert!(s.jobs().active.is_empty()));
    }

    /// The agent reports its library as `display()` text, so U+FFFD may
    /// stand for bytes that aren't UTF-8 in a different folder: such a
    /// report never names the open library, even when the text is equal.
    #[test]
    fn an_agent_library_with_a_replacement_character_never_matches() {
        let open = std::env::temp_dir().join("lib-\u{FFFD}");
        assert!(open.is_absolute());
        assert!(agent_runs_elsewhere(&open, &open));
        let plain = std::env::temp_dir().join("lib-plain");
        assert!(!agent_runs_elsewhere(&plain, &plain));
        assert!(agent_runs_elsewhere(Path::new("lib-plain"), &plain));
        assert!(agent_runs_elsewhere(&open, &plain));
    }
}
