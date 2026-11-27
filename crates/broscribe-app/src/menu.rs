//! The menu-bar status item and the app's native menus.
//!
//! Both are owned by GPUI's foreground executor. The status item's clicks
//! arrive through a channel; the native menu bar and its shortcuts are GPUI
//! actions dispatched on the same single event loop ([`install_app_menu`]).
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use broscribe_core::config::{Config, ReadingSize};
use broscribe_ipc::recording_words::{JOURNAL_LIBRARY_MISMATCH, JOURNAL_LIBRARY_UNKNOWN};
use broscribe_ipc::{JournalIssue, RecordingStatus};
use broscribe_platform::{MenuItemSpec, StatusIcon, StatusItem, TaskbarState};
use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::component::WindowExt as _;
use gpui::{
    App, AsKeystroke as _, Entity, InteractiveElement as _, Menu, MenuItem, OsAction,
    ParentElement as _, Role, StatefulInteractiveElement as _, Styled as _, SystemMenuType, Window,
    actions, div, rems,
};

use crate::daemon::DaemonClient;
use crate::launch::{AgentStartStatus, AgentStarter};
use crate::notify::{NotificationEvent, NotificationHost};
use crate::state::{AppEvent, AppState};

use broscribe_platform::shell::{DockCommand, DockItem, JumpList};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DockRecording {
    Unavailable,
    Idle,
    Active,
}

impl DockRecording {
    fn from_agent(daemon: &DaemonClient) -> Self {
        if !daemon.connected {
            Self::Unavailable
        } else if daemon.panel.recording.is_some() {
            Self::Active
        } else {
            Self::Idle
        }
    }

    fn items(self) -> impl Iterator<Item = DockItem> {
        let recording = match self {
            Self::Unavailable => None,
            Self::Idle => Some(DockItem {
                command: DockCommand::StartRecording,
                label: "Start recording",
            }),
            Self::Active => Some(DockItem {
                command: DockCommand::StopRecording,
                label: "Stop and save recording",
            }),
        };
        recording.into_iter().chain([
            DockItem {
                command: DockCommand::ShowRecordingControls,
                label: "Show recording controls",
            },
            DockItem {
                command: DockCommand::ReviewQueue,
                label: "Review queue",
            },
        ])
    }
}

#[derive(Default)]
struct DockMenu(Option<DockRecording>);

impl DockMenu {
    fn update(&mut self, state: DockRecording) -> Option<Vec<DockItem>> {
        if self.0 == Some(state) {
            return None;
        }
        self.0 = Some(state);
        Some(state.items().collect())
    }
}

pub fn install_dock_menu(
    mac: bool,
    jump_list: Option<Box<dyn JumpList>>,
    daemon: &Entity<DaemonClient>,
    cx: &mut App,
) {
    if !mac && jump_list.is_none() {
        return;
    }
    // Serialize writes to Explorer. Otherwise a slow idle update could
    // finish after one for an active recording and overwrite its tasks.
    let jump_updates = jump_list.map(|jump_list| {
        let (tx, mut updates) = futures::channel::mpsc::unbounded::<Vec<DockItem>>();
        cx.background_executor()
            .spawn(async move {
                while let Some(items) = updates.next().await {
                    if let Err(error) = jump_list.replace(&items) {
                        tracing::warn!(code = error.code(), "updating the taskbar jump list");
                    }
                }
            })
            .detach();
        tx
    });
    let mut menu = DockMenu::default();
    let mut update = move |daemon: &DaemonClient, cx: &App| {
        let Some(items) = menu.update(DockRecording::from_agent(daemon)) else {
            return;
        };
        if let Some(tx) = &jump_updates {
            let _ = tx.unbounded_send(items);
        } else {
            cx.set_dock_menu(
                items
                    .into_iter()
                    .map(|item| match item.command {
                        DockCommand::StartRecording => MenuItem::action(item.label, BeginRecording),
                        DockCommand::StopRecording => {
                            MenuItem::action(item.label, StopAndSaveRecording)
                        }
                        DockCommand::ShowRecordingControls => {
                            MenuItem::action(item.label, ShowRecordingControlsFromStatus)
                        }
                        DockCommand::ReviewQueue => MenuItem::action(item.label, ShowReviewQueue),
                    })
                    .collect(),
            );
        }
    };
    update(daemon.read(cx), cx);
    cx.observe(daemon, move |daemon, cx| update(daemon.read(cx), cx))
        .detach();
}

pub const TITLE: &str = "BroScribe";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuCommand {
    OpenApp,
    ReviewQueue,
    Settings,
    FinishSetup,
    About,
    OpenPanel,
    StartRecording,
    PauseResumeRecording,
    StopRecording,
    StartAgent,
    CopyAgentDetails,
    Updates,
    Prompt { id: String, action: &'static str },
    Quit,
}

const TRAY_GROUPS: &[&[MenuCommand]] = &[
    &[
        MenuCommand::StartRecording,
        MenuCommand::PauseResumeRecording,
        MenuCommand::StopRecording,
        MenuCommand::StartAgent,
        MenuCommand::CopyAgentDetails,
        MenuCommand::OpenPanel,
    ],
    &[MenuCommand::OpenApp, MenuCommand::ReviewQueue],
    &[
        MenuCommand::FinishSetup,
        MenuCommand::Settings,
        MenuCommand::About,
        MenuCommand::Updates,
    ],
    &[MenuCommand::Quit],
];

impl MenuCommand {
    pub fn id(&self) -> String {
        match self {
            Self::OpenApp => "open-app",
            Self::ReviewQueue => "review-queue",
            Self::Settings => "settings",
            Self::FinishSetup => "finish-setup",
            Self::About => "about",
            Self::OpenPanel => "open-panel",
            Self::StartRecording => "record-start",
            Self::PauseResumeRecording => "record-pause-resume",
            Self::StopRecording => "record-stop",
            Self::StartAgent => "agent-start",
            Self::CopyAgentDetails => "agent-copy-details",
            Self::Updates => "update",
            Self::Prompt { id, action } => return format!("prompt:{id}:{action}"),
            Self::Quit => "quit",
        }
        .into()
    }

    pub fn from_id(id: &str) -> Option<Self> {
        if let Some(prompt) = id.strip_prefix("prompt:") {
            let (id, action) = prompt.rsplit_once(':')?;
            if id.is_empty() {
                return None;
            }
            let action = match action {
                "record" => "record",
                "ignore" => "ignore",
                _ => return None,
            };
            return Some(Self::Prompt {
                id: id.into(),
                action,
            });
        }
        TRAY_GROUPS
            .iter()
            .flat_map(|group| group.iter())
            .find(|command| command.id() == id)
            .cloned()
    }

    fn dispatch(self, cx: &mut App) {
        let action: Box<dyn gpui::Action> = match self {
            Self::Quit => Box::new(QuitApp),
            Self::OpenPanel => Box::new(ShowRecordingControlsFromStatus),
            Self::StartAgent => Box::new(StartRecordingAgent),
            Self::CopyAgentDetails => Box::new(CopyAgentDetails),
            Self::StartRecording => Box::new(BeginRecording),
            Self::PauseResumeRecording => Box::new(PauseOrResumeRecording),
            Self::StopRecording => Box::new(StopAndSaveRecording),
            Self::OpenApp => Box::new(OpenBroScribe),
            Self::FinishSetup => Box::new(ShowSetupAssistant),
            Self::About => Box::new(ShowAbout),
            Self::ReviewQueue => Box::new(ShowReviewQueue),
            Self::Settings => Box::new(OpenSettings),
            Self::Updates => Box::new(CheckForUpdatesFromStatus),
            Self::Prompt { .. } => return,
        };
        cx.dispatch_action(action.as_ref());
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum UpdateMenu {
    #[default]
    Check,
    Available(String),
    Finish(String),
    Recover,
}

impl UpdateMenu {
    fn holding(&self) -> bool {
        matches!(self, Self::Finish(_) | Self::Recover)
    }

    pub(crate) fn label(&self, mac: bool, tray: bool) -> Option<String> {
        Some(match self {
            Self::Check if tray => return None,
            Self::Check => menu_copy(mac, "Check for Updates", "Check for updates").into(),
            Self::Available(version) => {
                let label = menu_copy(mac, "Update Available", "Update available");
                if mac && !tray {
                    label.to_string()
                } else {
                    format!("{label} · {version}")
                }
            }
            Self::Finish(version) if tray => format!("Keep BroScribe {version}"),
            Self::Finish(_) | Self::Recover => {
                menu_copy(mac, "Finish Updating", "Finish updating").into()
            }
        })
    }
}

fn menu_copy(mac: bool, title_case: &'static str, sentence_case: &'static str) -> &'static str {
    if mac { title_case } else { sentence_case }
}

fn controls_label(mac: bool) -> &'static str {
    menu_copy(mac, "Show Recording Controls", "Show recording controls")
}

fn pause_label(recording: Option<RecordingStatus>, mac: bool) -> &'static str {
    if recording == Some(RecordingStatus::Paused) {
        menu_copy(mac, "Resume Recording", "Resume recording")
    } else {
        menu_copy(mac, "Pause Recording", "Pause recording")
    }
}

fn agent_label(status: Option<&AgentStartStatus>, mac: bool) -> String {
    let status = status.unwrap_or(&AgentStartStatus::Idle);
    status
        .recovery()
        .map(|action| action.label(mac).to_owned())
        .or_else(|| status.message())
        .unwrap_or_default()
}

#[derive(Default)]
struct TrayMenu<'a> {
    mac: bool,
    reviews: Option<usize>,
    recording: Option<RecordingStatus>,
    recording_name: Option<&'a str>,
    agent: Option<&'a AgentStartStatus>,
    unfinished_setup: bool,
    start_pending: bool,
    prompts: Vec<&'a broscribe_ipc::NotificationRequest>,
    journal_issues: &'a [JournalIssue],
    update: UpdateMenu,
}

fn recording_menu(state: TrayMenu<'_>) -> Vec<MenuItemSpec> {
    let TrayMenu {
        mac,
        reviews,
        recording,
        recording_name,
        agent,
        unfinished_setup,
        start_pending,
        prompts,
        journal_issues,
        update,
    } = state;
    let mut caption = match (agent, recording, &update) {
        (_, None, UpdateMenu::Finish(_)) => "Recording off until the update is kept".into(),
        (_, None, UpdateMenu::Recover) => "Recording off until update recovery finishes".into(),
        (Some(status @ AgentStartStatus::Connecting), _, _) => status.message().unwrap_or_default(),
        (Some(_), _, _) => AgentStartStatus::Idle.message().unwrap_or_default(),
        (None, Some(RecordingStatus::Recording), _) => "Recording".into(),
        (None, Some(RecordingStatus::Paused), _) => "Paused".into(),
        (None, None, _) => "Not recording".into(),
    };
    if agent.is_none()
        && recording.is_some()
        && let Some(name) = recording_name
    {
        caption.push_str(" · ");
        caption.push_str(name);
    }
    let mut status = MenuItemSpec::new("recording-status", caption);
    status.enabled = false;
    let mut items = vec![status];
    if let Some(message) = agent.and_then(AgentStartStatus::message)
        && !matches!(
            agent,
            Some(
                AgentStartStatus::Idle | AgentStartStatus::Connecting | AgentStartStatus::Starting
            )
        )
    {
        let mut line = MenuItemSpec::new("agent-status", message);
        line.enabled = false;
        items.push(line);
    }
    for (code, label) in [
        (
            JOURNAL_LIBRARY_MISMATCH,
            "Open the original library to recover recordings",
        ),
        (
            JOURNAL_LIBRARY_UNKNOWN,
            "Older recordings need a choice in recording controls",
        ),
    ] {
        if journal_issues.iter().any(|issue| issue.code == code) {
            let mut line = MenuItemSpec::new(code, label);
            line.enabled = false;
            items.push(line);
        }
    }
    for (index, group) in TRAY_GROUPS.iter().enumerate() {
        if index == 1 && !prompts.is_empty() {
            items.push(MenuItemSpec::separator());
            for prompt in &prompts {
                let mut caption =
                    MenuItemSpec::new(format!("prompt-caption:{}", prompt.id), &prompt.body);
                caption.enabled = false;
                items.push(caption);
                for (action, title) in [("record", "Record"), ("ignore", "Ignore")] {
                    let command = MenuCommand::Prompt {
                        id: prompt.id.clone(),
                        action,
                    };
                    items.push(MenuItemSpec::new(command.id(), title));
                }
            }
        }
        items.push(MenuItemSpec::separator());
        for command in *group {
            let title = match command {
                MenuCommand::OpenApp => "Open BroScribe".into(),
                MenuCommand::Settings => "Settings".into(),
                MenuCommand::FinishSetup if !unfinished_setup => continue,
                MenuCommand::FinishSetup => menu_copy(mac, "Finish Setup", "Finish setup").into(),
                MenuCommand::About => "About BroScribe".into(),
                MenuCommand::OpenPanel => controls_label(mac).into(),
                MenuCommand::StartRecording => {
                    menu_copy(mac, "Start Recording", "Start recording").into()
                }
                MenuCommand::PauseResumeRecording => pause_label(recording, mac).into(),
                MenuCommand::StopRecording => {
                    menu_copy(mac, "Stop and Save Recording", "Stop and save recording").into()
                }
                MenuCommand::StartAgent
                    if matches!(agent, None | Some(AgentStartStatus::Connecting)) =>
                {
                    continue;
                }
                MenuCommand::StartAgent => agent_label(agent, mac),
                MenuCommand::CopyAgentDetails
                    if agent.and_then(AgentStartStatus::copy_details).is_none() =>
                {
                    continue;
                }
                MenuCommand::CopyAgentDetails => {
                    menu_copy(mac, "Copy Details", "Copy details").into()
                }
                MenuCommand::Updates => match update.label(mac, true) {
                    Some(label) => label,
                    None => continue,
                },
                MenuCommand::Prompt { .. } => {
                    unreachable!("prompt actions are added with their caption")
                }
                MenuCommand::Quit => "Quit BroScribe".into(),
                MenuCommand::ReviewQueue => {
                    let label = menu_copy(mac, "Review Queue", "Review queue");
                    reviews
                        .filter(|count| *count > 0)
                        .map_or_else(|| label.into(), |count| format!("{label} ({count})"))
                }
            };
            let mut item = MenuItemSpec::new(command.id(), title);
            item.default = matches!(command, MenuCommand::OpenApp);
            item.enabled = match command {
                MenuCommand::StartRecording => {
                    agent.is_none() && recording.is_none() && !start_pending && !update.holding()
                }
                MenuCommand::PauseResumeRecording | MenuCommand::StopRecording => {
                    agent.is_none() && recording.is_some()
                }
                MenuCommand::StartAgent => agent.and_then(AgentStartStatus::recovery).is_some(),
                _ => true,
            };
            items.push(item);
        }
    }
    items
}

/// The status item's words and glyph for the recording state: the waveform
/// with a dot while audio is captured, two bars while paused; the words say
/// the same, so the state never rests on the glyph alone.
fn status_look(recording: Option<RecordingStatus>) -> (&'static str, StatusIcon) {
    match recording {
        Some(RecordingStatus::Recording) => ("BroScribe · Recording", StatusIcon::Recording),
        Some(RecordingStatus::Paused) => ("BroScribe · Paused", StatusIcon::Paused),
        None => (TITLE, StatusIcon::Idle),
    }
}

/// The spoken tooltip and accessibility label built from the recording
/// state, the meeting prompts waiting for an answer and the speakers to
/// review.
fn status_title(
    recording: Option<RecordingStatus>,
    review_count: usize,
    meeting_prompts: usize,
) -> String {
    let (base, _) = status_look(recording);
    let mut parts = vec![base.to_owned()];
    if meeting_prompts > 0 {
        parts.push(crate::ui::format::count(
            meeting_prompts as u64,
            "meeting to answer",
            "meetings to answer",
        ));
    }
    if review_count > 0 {
        parts.push(crate::ui::format::count(
            review_count as u64,
            "speaker to review",
            "speakers to review",
        ));
    }
    parts.join(" · ")
}

#[derive(PartialEq, Eq)]
struct StatusPresentation {
    title: String,
    icon: StatusIcon,
    badge: usize,
    menu: Vec<MenuItemSpec>,
}

impl StatusPresentation {
    fn new(state: TrayMenu<'_>) -> Self {
        let recording = state.recording;
        let (_, icon) = status_look(recording);
        let badge = state.prompts.len();
        let title = status_title(recording, state.reviews.unwrap_or(0), badge);
        let title = match (&state.update, state.agent) {
            (update, _) if update.holding() => {
                title.replacen(TITLE, "BroScribe · update waiting", 1)
            }
            (_, Some(_)) => title.replacen(TITLE, "BroScribe · can't reach BroScribe Agent", 1),
            _ => title,
        };
        Self {
            title,
            icon,
            badge,
            menu: recording_menu(state),
        }
    }

    fn apply(
        &self,
        item: &mut dyn StatusItem,
        previous: Option<&Self>,
    ) -> broscribe_platform::Result<()> {
        if previous.is_none_or(|old| old.icon != self.icon) {
            item.set_icon(self.icon)?;
        }
        if previous.is_none_or(|old| old.title != self.title) {
            item.set_title(&self.title)?;
        }
        if previous.is_none_or(|old| old.menu != self.menu) {
            item.set_menu(self.menu.clone())?;
        }
        if previous.is_none_or(|old| old.badge != self.badge) {
            item.set_badge_count(self.badge)?;
        }
        Ok(())
    }
}

/// The entities the status item reflects.
#[derive(Clone)]
pub struct StatusSources {
    pub state: Entity<AppState>,
    pub daemon: Entity<DaemonClient>,
    pub notifications: Entity<NotificationHost>,
    pub agent: Entity<AgentStarter>,
}

struct InstalledStatus {
    item: Option<Box<dyn StatusItem>>,
    mac_factory: Option<StatusFactory>,
    wanted: bool,
    shown: Option<StatusPresentation>,
    taskbars: HashMap<gpui::WindowId, TaskbarState>,
}

struct TaskbarStatus(Rc<RefCell<InstalledStatus>>);
impl gpui::Global for TaskbarStatus {}

pub fn sync_taskbar(
    handle: gpui::WindowHandle<gpui::component::Root>,
    recording: Option<RecordingStatus>,
    cx: &mut App,
) {
    let Some(status) = cx
        .try_global::<TaskbarStatus>()
        .map(|status| status.0.clone())
    else {
        return;
    };
    let state = match recording {
        Some(RecordingStatus::Recording) => TaskbarState::Recording,
        Some(RecordingStatus::Paused) => TaskbarState::Paused,
        None => TaskbarState::None,
    };
    let id = handle.window_id();
    if status.borrow().taskbars.get(&id) == Some(&state) {
        return;
    }
    let _ = handle.update(cx, |_, window, _| {
        let mut status = status.borrow_mut();
        let Some(item) = status.item.as_mut() else {
            return;
        };
        match item.set_taskbar_state(&*window, state) {
            Ok(()) => {
                status.taskbars.insert(id, state);
            }
            Err(error) => tracing::warn!(code = error.code(), "setting taskbar recording overlay"),
        }
    });
}

impl StatusSources {
    fn presentation(&self, cx: &App) -> StatusPresentation {
        let state = self.state.read(cx);
        let daemon = self.daemon.read(cx);
        let recording = daemon.panel.recording.as_ref();
        let recording_name = recording.and_then(|r| {
            let app_name = r
                .auto_stop
                .as_ref()
                .and_then(|state| state.app.as_ref())
                .map(|app| app.app_name.as_str());
            [r.title.as_deref(), app_name.or(r.app.as_deref())]
                .into_iter()
                .flatten()
                .find(|name| !name.trim().is_empty())
        });
        let status = recording.map(|r| r.status);
        let agent = AgentStartStatus::unavailable(daemon, self.agent.read(cx).status());
        let update = crate::views::update::UpdateNotices::current(cx)
            .map_or_else(UpdateMenu::default, |notices| {
                notices.menu_state(status.is_some())
            });
        StatusPresentation::new(TrayMenu {
            mac: has_global_menu_bar(broscribe_platform_native::NativePlatform::host_platform()),
            reviews: state.review_count(),
            recording: status,
            recording_name,
            agent: agent.as_ref(),
            unfinished_setup: crate::views::setup::unfinished_essentials(state.config()).is_some(),
            start_pending: daemon.record_start_pending(),
            prompts: if !state.config().show_status_item()
                && has_global_menu_bar(broscribe_platform_native::NativePlatform::host_platform())
            {
                self.notifications.read(cx).pending_asks().collect()
            } else {
                self.notifications.read(cx).meeting_prompts().collect()
            },
            journal_issues: &daemon.panel.journal_issues,
            update,
        })
    }

    fn refresh(&self, item: &RefCell<InstalledStatus>, cx: &mut App) {
        let mut status = item.borrow_mut();
        if let Some(factory) = status.mac_factory.as_ref() {
            let preference = self.state.read(cx).config().show_status_item();
            crate::ui::windows::keep_dock_icon(!preference, cx);
            let wanted = preference || self.notifications.read(cx).pending_asks().next().is_some();
            if wanted != status.wanted {
                status.item = wanted.then(factory).and_then(|result| {
                    result
                        .inspect_err(|error| tracing::warn!(%error, "menu-bar item unavailable"))
                        .ok()
                });
                status.wanted = wanted;
                status.shown = None;
            }
        }
        let InstalledStatus { item, shown, .. } = &mut *status;
        let Some(item) = item.as_mut() else { return };
        let next = self.presentation(cx);
        if let Err(error) = next.apply(item.as_mut(), shown.as_ref()) {
            tracing::warn!(%error, "status item");
        } else {
            *shown = Some(next);
        }
    }

    fn config_presentation(&self, cx: &App) -> (bool, bool) {
        let config = self.state.read(cx).config();
        (
            config.show_status_item(),
            crate::views::setup::unfinished_essentials(config).is_some(),
        )
    }
}

type StatusFactory = Box<dyn Fn() -> broscribe_platform::Result<Box<dyn StatusItem>>>;

pub(crate) fn status_item_hidden(cx: &App) -> bool {
    cx.try_global::<TaskbarStatus>()
        .is_some_and(|status| status.0.borrow().item.is_none())
}

pub fn install(
    item: Option<Box<dyn StatusItem>>,
    mut clicks: UnboundedReceiver<String>,
    sources: StatusSources,
    mac_factory: Option<StatusFactory>,
    cx: &mut App,
) {
    let item = Rc::new(RefCell::new(InstalledStatus {
        item,
        mac_factory,
        wanted: sources.state.read(cx).config().show_status_item(),
        shown: None,
        taskbars: HashMap::new(),
    }));
    cx.set_global(TaskbarStatus(item.clone()));
    cx.on_window_closed(|cx, id| {
        if let Some(status) = cx.try_global::<TaskbarStatus>() {
            status.0.borrow_mut().taskbars.remove(&id);
        }
    })
    .detach();
    let updates = crate::views::update::UpdateNotices::global(cx);
    sources.refresh(&item, cx);
    cx.observe(&updates, {
        let item = item.clone();
        let sources = sources.clone();
        move |_, cx| sources.refresh(&item, cx)
    })
    .detach();
    cx.subscribe(&sources.state, {
        let item = item.clone();
        let sources = sources.clone();
        move |_, event: &AppEvent, cx| {
            if matches!(event, AppEvent::ReviewCountChanged(_)) {
                sources.refresh(&item, cx);
            }
        }
    })
    .detach();
    cx.observe(&sources.state, {
        let item = item.clone();
        let sources = sources.clone();
        let mut config = sources.config_presentation(cx);
        move |_, cx| {
            let next = sources.config_presentation(cx);
            if config != next {
                config = next;
                sources.refresh(&item, cx);
            }
        }
    })
    .detach();
    cx.observe(&sources.daemon, {
        let item = item.clone();
        let sources = sources.clone();
        move |_, cx| sources.refresh(&item, cx)
    })
    .detach();
    cx.subscribe(&sources.notifications, {
        let item = item.clone();
        let sources = sources.clone();
        move |_, _: &NotificationEvent, cx| sources.refresh(&item, cx)
    })
    .detach();
    cx.observe(&sources.agent, {
        let sources = sources.clone();
        move |_, cx| sources.refresh(&item, cx)
    })
    .detach();
    cx.spawn(async move |cx| {
        while let Some(id) = clicks.next().await {
            if let Some(command) = MenuCommand::from_id(&id) {
                cx.update(|cx| match command {
                    MenuCommand::Prompt { id, action } => {
                        sources
                            .notifications
                            .update(cx, |host, cx| host.act_on_prompt(&id, action, cx));
                    }
                    _ => command.dispatch(cx),
                });
            }
        }
    })
    .detach();
}

actions!(
    broscribe,
    [
        /// Opens the About window (version, build, signing, licences).
        ShowAbout,
        OpenSettings,
        /// Quits the app; recordings continue in the recording agent.
        QuitApp,
        HideApp,
        HideOtherApps,
        ShowAllApps,
        /// Closes the focused window (closing the panel never stops capture).
        CloseWindow,
        MinimizeWindow,
        ZoomWindow,
        ShowLibrary,
        ShowReviewQueue,
        ShowPeople,
        ShowRecordingControls,
        ShowRecordingControlsFromStatus,
        ShowSetupAssistant,
        CheckForUpdates,
        CheckForUpdatesFromStatus,
        BeginRecording,
        PauseOrResumeRecording,
        StopAndSaveRecording,
        NavigateBack,
        NavigateForward,
        FocusNextArea,
        FocusPreviousArea,
        ToggleSidebar,
        ToggleFullScreen,
        KeyboardShortcuts,
        SearchMeetings,
        FindInView,
        FindNext,
        FindPrevious,
        AddBookmark,
        OpenBroScribe,
        BringAllToFront,
        StartRecordingAgent,
        CopyAgentDetails,
        ExportMeeting,
        ShowDiagnostics,
        ShowLicences,
        RenameMeeting,
        CopyTranscript,
        RevealRecording,
        ToggleMeetingPlayback,
        BackFifteenSeconds,
        ForwardFifteenSeconds,
        ToggleMeetingPlaybackShortcut,
        BackFifteenSecondsShortcut,
        ForwardFifteenSecondsShortcut,
        GenerateMeetingSummary,
        CopyMeetingSummary,
        DeleteMeeting,
        ShowMainMenu,
        LargerText,
        SmallerText,
        StandardTextSize,
    ]
);

/// A native menu command the shell carries out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppCommand {
    About,
    Diagnostics,
    Licences,
    OpenMain,
    Search,
    Import { folder: bool },
    Shortcuts,
    Settings,
    Library,
    ReviewQueue,
    People,
    RecordingControls,
    RecordingControlsFromStatus,
    Setup,
    Updates,
    UpdatesFromStatus,
    Record(RecordingMenuAction),
    StartAgent,
    CopyAgentRecovery,
    Quit,
    TextSize(TextSizeChange),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordingMenuAction {
    Start,
    PauseOrResume,
    Stop,
    Bookmark,
}

impl RecordingMenuAction {
    pub fn call(
        self,
        recording: Option<&broscribe_ipc::RecordingState>,
    ) -> Option<broscribe_ipc::Call> {
        use broscribe_ipc::Call;
        match self {
            Self::Start => recording
                .is_none()
                .then(|| Call::RecordStart(Default::default())),
            Self::PauseOrResume => recording.map(|recording| {
                let session_id = recording.session_id.clone();
                if recording.status == RecordingStatus::Paused {
                    Call::RecordResume { session_id }
                } else {
                    Call::RecordPause { session_id }
                }
            }),
            Self::Stop => recording.map(|recording| Call::RecordStop {
                session_id: recording.session_id.clone(),
            }),
            Self::Bookmark => recording.map(|recording| Call::RecordBookmark {
                session_id: recording.session_id.clone(),
                label: None,
            }),
        }
    }
}

/// A View-menu step of the saved reading size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextSizeChange {
    Larger,
    Smaller,
    Standard,
}

impl TextSizeChange {
    /// The size after this step; the ends stay where they are.
    pub fn apply(self, size: ReadingSize) -> ReadingSize {
        match self {
            Self::Larger => size.larger().unwrap_or(size),
            Self::Smaller => size.smaller().unwrap_or(size),
            Self::Standard => ReadingSize::Standard,
        }
    }
}

pub use crate::ui::keymap::key_bindings;

pub(crate) fn has_global_menu_bar(host: broscribe_platform::HostPlatform) -> bool {
    matches!(host, broscribe_platform::HostPlatform::MacOs)
}

#[derive(Clone, Copy)]
enum SharedCommand {
    ImportRecordings,
    ImportFolder,
    Search,
    ShowControls,
    Settings,
    Setup,
    Updates,
    Larger,
    Smaller,
    Standard,
    Shortcuts,
    About,
    Quit,
}

impl SharedCommand {
    fn item(self, mac: bool, state: &NativeMenuState) -> MenuItem {
        use crate::views::library::{ImportFolder, ImportRecordings};
        let variant = state.reading.is_none();
        let size = state.reading.unwrap_or_default();
        match self {
            Self::ImportRecordings => MenuItem::action(
                menu_copy(mac, "Import Recordings…", "Import recordings…"),
                ImportRecordings,
            )
            .disabled(variant || state.main.dialog),
            Self::ImportFolder => MenuItem::action(
                menu_copy(mac, "Import Folder…", "Import folder…"),
                ImportFolder,
            )
            .disabled(variant || state.main.dialog),
            Self::Search => MenuItem::action(
                menu_copy(mac, "Search Meetings", "Search meetings"),
                SearchMeetings,
            )
            .disabled(variant),
            Self::ShowControls => {
                MenuItem::action(controls_label(mac), ShowRecordingControls).disabled(variant)
            }
            // Every Mac app's menu says "Settings…" (⌘,); elsewhere no ellipsis.
            Self::Settings => {
                MenuItem::action(menu_copy(mac, "Settings…", "Settings"), OpenSettings)
                    .disabled(variant)
            }
            Self::Setup => MenuItem::action(
                menu_copy(mac, "Setup Assistant", "Setup assistant"),
                ShowSetupAssistant,
            )
            .disabled(variant),
            Self::Updates => MenuItem::action(update_label(&state.update, mac), CheckForUpdates)
                .disabled(variant),
            Self::Larger => {
                MenuItem::action(menu_copy(mac, "Larger Text", "Larger text"), LargerText)
                    .disabled(variant || size.larger().is_none())
            }
            Self::Smaller => {
                MenuItem::action(menu_copy(mac, "Smaller Text", "Smaller text"), SmallerText)
                    .disabled(variant || size.smaller().is_none())
            }
            Self::Standard => MenuItem::action(
                menu_copy(mac, "Standard Text Size", "Standard text size"),
                StandardTextSize,
            )
            .disabled(variant || size == ReadingSize::Standard),
            Self::Shortcuts => MenuItem::action(
                menu_copy(mac, "Keyboard Shortcuts", "Keyboard shortcuts"),
                KeyboardShortcuts,
            )
            .disabled(variant),
            Self::About => MenuItem::action("About BroScribe", ShowAbout),
            Self::Quit => MenuItem::action("Quit BroScribe", QuitApp),
        }
    }
}

fn toolbar_items(state: &NativeMenuState) -> Vec<MenuItem> {
    use SharedCommand::*;
    let groups: &[&[SharedCommand]] = &[
        &[ImportRecordings, ImportFolder, Search],
        &[ShowControls],
        &[Settings, Setup, Updates],
        &[Larger, Smaller, Standard],
        &[Shortcuts, About],
        &[Quit],
    ];
    let mut items = Vec::new();
    for (index, group) in groups.iter().enumerate() {
        if index > 0 {
            items.push(MenuItem::separator());
        }
        items.extend(group.iter().map(|command| command.item(false, state)));
    }
    items
}

pub(crate) fn toolbar_menu(
    mut menu: gpui::component::menu::PopupMenu,
    reading: ReadingSize,
    can_import: bool,
    update: UpdateMenu,
) -> gpui::component::menu::PopupMenu {
    let state = NativeMenuState {
        reading: Some(reading),
        main: MainMenuState {
            dialog: !can_import,
            ..Default::default()
        },
        update,
        ..Default::default()
    };
    for item in toolbar_items(&state) {
        menu = match item {
            MenuItem::Action {
                name,
                action,
                disabled,
                ..
            } => menu.menu_with_disabled(name, action, disabled),
            MenuItem::Separator => menu.separator(),
            _ => unreachable!("the main menu is flat"),
        };
    }
    menu
}

pub fn show_shortcuts(active: gpui::AnyWindowHandle, cx: &mut App) {
    cx.defer(move |cx| {
        let _ = active.update(cx, |_, window, cx| {
            if window.has_active_dialog(cx) {
                return;
            }
            window.open_dialog(cx, |dialog, _, cx| {
                // BroScribe's dialogs stand on the popover ground, not the desk.
                let dialog = dialog.bg(gpui::component::ActiveTheme::theme(cx).popover);
                use gpui::component::kbd::Kbd;
                let mut last_group = "";
                let mut list = div()
                    .id("keyboard-shortcuts")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .max_h(rems(24.))
                    .overflow_y_scroll();
                let host = broscribe_platform_native::NativePlatform::host_platform();
                for (index, shortcut) in crate::ui::keymap::shortcuts(host).into_iter().enumerate()
                {
                    let (group, label) = (shortcut.group, shortcut.label);
                    let Some(key) = shortcut.binding(cx).ok().and_then(|binding| {
                        binding
                            .keystrokes()
                            .first()
                            .map(|key| key.as_keystroke().clone())
                    }) else {
                        continue;
                    };
                    if group != last_group {
                        list = list.child(crate::ui::heading(("shortcut-group", index), group));
                        last_group = group;
                    }
                    list = list.child(
                        div()
                            .flex()
                            .justify_between()
                            .gap_3()
                            .child(crate::ui::text_line(
                                ("shortcut", index),
                                Role::Label,
                                label,
                            ))
                            .child(Kbd::new(key)),
                    );
                }
                dialog
                    .title(crate::ui::heading("shortcuts-title", "Keyboard shortcuts"))
                    .child(list)
            });
        });
    });
}

pub fn update_waiting(cx: &App) -> bool {
    crate::views::update::UpdateNotices::current(cx)
        .is_some_and(crate::views::update::UpdateNotices::holding)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct MainMenuState {
    pub(crate) back: bool,
    pub(crate) forward: bool,
    pub(crate) sidebar: bool,
    pub(crate) fullscreen: bool,
    pub(crate) meeting: Option<crate::views::meeting::MeetingMenuState>,
    pub(crate) people_undo: bool,
    pub(crate) review_undo: bool,
    pub(crate) find: bool,
    pub(crate) matches: bool,
    pub(crate) dialog: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct NativeMenuState {
    reading: Option<ReadingSize>,
    main: MainMenuState,
    main_active: bool,
    recording: Option<RecordingStatus>,
    start_pending: bool,
    agent: Option<AgentStartStatus>,
    update: UpdateMenu,
    window: bool,
    minimize: bool,
    zoom: bool,
}

fn app_menus(state: &NativeMenuState) -> Vec<Menu> {
    use SharedCommand::*;
    use gpui::component::input::{Copy, Cut, Paste, Redo, SelectAll, Undo};

    let variant = state.reading.is_none();
    let main = state.main;
    let main_active = !variant && state.main_active;
    let can_capture = !variant && state.agent.is_none();
    let active_capture = can_capture && state.recording.is_some();
    let separator = MenuItem::separator;
    let shared = |command: SharedCommand| command.item(true, state);
    vec![
        Menu::new("BroScribe").items([
            shared(About),
            separator(),
            shared(Settings),
            shared(Updates),
            shared(Setup),
            separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            separator(),
            MenuItem::action("Hide BroScribe", HideApp),
            MenuItem::action("Hide Others", HideOtherApps),
            MenuItem::action("Show All", ShowAllApps),
            separator(),
            shared(Quit),
        ]),
        Menu::new("File").items([
            shared(ImportRecordings),
            shared(ImportFolder),
            separator(),
            shared(Search),
            separator(),
            MenuItem::action("Export Meeting…", ExportMeeting)
                .disabled(!main_active || main.meeting.is_none() || main.dialog),
            separator(),
            MenuItem::action("Close", CloseWindow).disabled(!state.window),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action(
                main.meeting
                    .filter(|_| main_active && !main.dialog)
                    .and_then(|meeting| meeting.undo)
                    .or_else(|| {
                        (main_active && !main.dialog && main.people_undo).then_some("Edit Person")
                    })
                    .or_else(|| {
                        (main_active && !main.dialog && main.review_undo).then_some("Can’t Tell")
                    })
                    .map_or_else(|| "Undo".to_owned(), |step| format!("Undo {step}")),
                Undo,
                OsAction::Undo,
            )
            .disabled(variant),
            MenuItem::os_action(
                main.meeting
                    .filter(|_| main_active && !main.dialog)
                    .and_then(|meeting| meeting.redo)
                    .map_or_else(|| "Redo".to_owned(), |step| format!("Redo {step}")),
                Redo,
                OsAction::Redo,
            )
            .disabled(variant),
            separator(),
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
            separator(),
            MenuItem::submenu(
                Menu::new("Find").items([
                    MenuItem::action("Find…", FindInView)
                        .disabled(!main_active || !main.find || main.dialog),
                    MenuItem::action("Find Next", FindNext)
                        .disabled(!main_active || !main.matches || main.dialog),
                    MenuItem::action("Find Previous", FindPrevious)
                        .disabled(!main_active || !main.matches || main.dialog),
                ]),
            ),
        ]),
        Menu::new("View").items([
            MenuItem::action(
                if main.sidebar {
                    "Hide Sidebar"
                } else {
                    "Show Sidebar"
                },
                ToggleSidebar,
            )
            .disabled(!main_active),
            separator(),
            shared(Larger),
            shared(Smaller),
            shared(Standard),
            separator(),
            MenuItem::action(
                if main.fullscreen {
                    "Exit Full Screen"
                } else {
                    "Enter Full Screen"
                },
                ToggleFullScreen,
            )
            .disabled(!main_active),
        ]),
        Menu::new("Go").items([
            MenuItem::action("Back", NavigateBack)
                .disabled(!main_active || !main.back || main.dialog),
            MenuItem::action("Forward", NavigateForward)
                .disabled(!main_active || !main.forward || main.dialog),
            separator(),
            MenuItem::action("Library", ShowLibrary).disabled(variant),
            MenuItem::action("Review Queue", ShowReviewQueue).disabled(variant),
            MenuItem::action("People", ShowPeople).disabled(variant),
        ]),
        Menu::new("Meeting").items(crate::views::meeting::native_menu(
            main.meeting.filter(|_| main_active && !main.dialog),
        )),
        Menu::new("Recording").items([
            MenuItem::action(
                menu_copy(true, "Start Recording", "Start recording"),
                BeginRecording,
            )
            .disabled(
                !can_capture || active_capture || state.start_pending || state.update.holding(),
            ),
            MenuItem::action(pause_label(state.recording, true), PauseOrResumeRecording)
                .disabled(!active_capture),
            MenuItem::action(
                menu_copy(true, "Stop and Save Recording", "Stop and save recording"),
                StopAndSaveRecording,
            )
            .disabled(!active_capture),
            separator(),
            MenuItem::action("Add Bookmark", AddBookmark).disabled(!active_capture),
            separator(),
            MenuItem::action(agent_label(state.agent.as_ref(), true), StartRecordingAgent)
                .disabled(
                    variant
                        || state
                            .agent
                            .as_ref()
                            .and_then(AgentStartStatus::recovery)
                            .is_none(),
                ),
            MenuItem::action("Copy Details", CopyAgentDetails).disabled(
                variant
                    || state
                        .agent
                        .as_ref()
                        .and_then(AgentStartStatus::copy_details)
                        .is_none(),
            ),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", MinimizeWindow).disabled(!state.minimize),
            MenuItem::action("Zoom", ZoomWindow).disabled(!state.zoom),
            separator(),
            shared(ShowControls),
            separator(),
            MenuItem::action("Bring All to Front", BringAllToFront),
            separator(),
            MenuItem::action("BroScribe", OpenBroScribe).disabled(variant),
        ]),
        Menu::new("Help").items([
            shared(Shortcuts),
            separator(),
            MenuItem::action("Diagnostics", ShowDiagnostics),
            MenuItem::action("Third-Party Licences", ShowLicences),
        ]),
    ]
}

fn update_label(update: &UpdateMenu, mac: bool) -> String {
    update
        .label(mac, false)
        .expect("app menus always show updates")
}

struct InstalledMenus {
    app: Option<Entity<AppState>>,
    main: Option<(gpui::WindowId, MainMenuState)>,
    shown: Option<NativeMenuState>,
}
impl gpui::Global for InstalledMenus {}

fn refresh_native_menus(cx: &mut App) {
    let Some(installed) = cx.try_global::<InstalledMenus>() else {
        return;
    };
    let app = installed.app.clone();
    let main = installed.main;
    let mut state = NativeMenuState::default();
    if let Some(app) = app {
        let app = app.read(cx);
        state.reading = Some(app.config().appearance.reading_size);
        let daemon = app.daemon().map(|daemon| daemon.read(cx));
        state.recording = daemon
            .and_then(|daemon| daemon.panel.recording.as_ref())
            .map(|recording| recording.status);
        state.start_pending = daemon.is_some_and(DaemonClient::record_start_pending);
        let started = app.agent_starter();
        let status = started
            .as_ref()
            .map_or(&AgentStartStatus::Idle, |agent| agent.read(cx).status());
        state.agent = daemon.map_or_else(
            || Some(status.clone()),
            |daemon| AgentStartStatus::unavailable(daemon, status),
        );
        if let Some(notices) = crate::views::update::UpdateNotices::current(cx) {
            state.update = notices.menu_state(state.recording.is_some());
        }
    }
    if let Some((_, main)) = main {
        state.main = main;
    }
    if let Some(active) = cx.active_window() {
        state.window = true;
        state.main_active = main.is_some_and(|(id, _)| id == active.window_id());
        let _ = active.update(cx, |_, window, _| {
            state.minimize = window.is_minimizable();
            state.zoom = window.is_resizable();
        });
    }
    if let Some((main, _)) = main
        && let Some(handle) = cx
            .windows()
            .into_iter()
            .find(|window| window.window_id() == main)
    {
        let _ = handle.update(cx, |_, window, cx| {
            state.main.dialog = window.root::<gpui::component::Root>().flatten().is_some()
                && window.has_active_dialog(cx);
        });
    }
    if cx.global::<InstalledMenus>().shown.as_ref() != Some(&state) {
        cx.set_menus(app_menus(&state));
        cx.global_mut::<InstalledMenus>().shown = Some(state);
    }
}

pub(crate) fn publish_main_menu(window: gpui::WindowId, state: MainMenuState, cx: &mut App) {
    if let Some(installed) = cx.try_global::<InstalledMenus>() {
        if installed.main != Some((window, state)) {
            cx.global_mut::<InstalledMenus>().main = Some((window, state));
        }
        cx.defer(refresh_native_menus);
    }
}

pub(crate) fn window_activation_changed(cx: &mut App) {
    if cx.try_global::<InstalledMenus>().is_some() {
        cx.defer(refresh_native_menus);
    }
}

/// Applies `act` to the focused BroScribe window, if any. Shortcuts and
/// menu commands are dispatched while GPUI holds that window, so the
/// window is updated once the dispatch has returned it.
fn active_window(cx: &mut App, act: impl FnOnce(&mut Window) + 'static) {
    if let Some(handle) = cx.active_window() {
        cx.defer(move |cx| {
            if let Err(error) = handle.update(cx, |_, window, _| act(window)) {
                tracing::debug!(%error, "window command");
            }
        });
    }
}

/// Installs the menu bar and shortcuts. With `state` (a running app) the
/// View menu follows the saved reading size; without it this is the startup
/// error's menu bar. Window, Hide and Edit commands are handled here; the
/// rest go to `on_command`.
pub fn install_app_menu(
    state: Option<&Entity<AppState>>,
    on_command: impl Fn(AppCommand, &mut App) + 'static,
    cx: &mut App,
) {
    let on_command = Rc::new(on_command);
    let bindings = key_bindings(cx);
    cx.bind_keys(bindings);
    let send = |command: AppCommand| {
        let on_command = on_command.clone();
        move |cx: &mut App| {
            // GPUI holds the dispatch window until this action returns.
            let on_command = on_command.clone();
            cx.defer(move |cx| on_command(command, cx));
        }
    };
    let about = send(AppCommand::About);
    cx.on_action(move |_: &ShowAbout, cx| about(cx));
    let diagnostics = send(AppCommand::Diagnostics);
    cx.on_action(move |_: &ShowDiagnostics, cx| diagnostics(cx));
    let licences = send(AppCommand::Licences);
    cx.on_action(move |_: &ShowLicences, cx| licences(cx));
    let open = send(AppCommand::OpenMain);
    cx.on_action(move |_: &OpenBroScribe, cx| open(cx));
    let search = send(AppCommand::Search);
    cx.on_action(move |_: &SearchMeetings, cx| search(cx));
    let import = send(AppCommand::Import { folder: false });
    cx.on_action(move |_: &crate::views::library::ImportRecordings, cx| import(cx));
    let folder = send(AppCommand::Import { folder: true });
    cx.on_action(move |_: &crate::views::library::ImportFolder, cx| folder(cx));
    let shortcuts = send(AppCommand::Shortcuts);
    cx.on_action(move |_: &KeyboardShortcuts, cx| shortcuts(cx));
    let settings = send(AppCommand::Settings);
    cx.on_action(move |_: &OpenSettings, cx| settings(cx));
    let quit = send(AppCommand::Quit);
    cx.on_action(move |_: &QuitApp, cx| quit(cx));
    let library = send(AppCommand::Library);
    cx.on_action(move |_: &ShowLibrary, cx| library(cx));
    let review = send(AppCommand::ReviewQueue);
    cx.on_action(move |_: &ShowReviewQueue, cx| review(cx));
    let people = send(AppCommand::People);
    cx.on_action(move |_: &ShowPeople, cx| people(cx));
    let setup = send(AppCommand::Setup);
    cx.on_action(move |_: &ShowSetupAssistant, cx| setup(cx));
    let updates = send(AppCommand::Updates);
    cx.on_action(move |_: &CheckForUpdates, cx| updates(cx));
    let updates_from_status = send(AppCommand::UpdatesFromStatus);
    cx.on_action(move |_: &CheckForUpdatesFromStatus, cx| updates_from_status(cx));
    let start = send(AppCommand::Record(RecordingMenuAction::Start));
    cx.on_action(move |_: &BeginRecording, cx| start(cx));
    let pause = send(AppCommand::Record(RecordingMenuAction::PauseOrResume));
    cx.on_action(move |_: &PauseOrResumeRecording, cx| pause(cx));
    let stop = send(AppCommand::Record(RecordingMenuAction::Stop));
    cx.on_action(move |_: &StopAndSaveRecording, cx| stop(cx));
    let bookmark = send(AppCommand::Record(RecordingMenuAction::Bookmark));
    cx.on_action(move |_: &AddBookmark, cx| bookmark(cx));
    let start_agent = send(AppCommand::StartAgent);
    cx.on_action(move |_: &StartRecordingAgent, cx| start_agent(cx));
    let copy_agent = send(AppCommand::CopyAgentRecovery);
    cx.on_action(move |_: &CopyAgentDetails, cx| copy_agent(cx));
    let controls = send(AppCommand::RecordingControls);
    cx.on_action(move |_: &ShowRecordingControls, cx| controls(cx));
    let controls_from_status = send(AppCommand::RecordingControlsFromStatus);
    cx.on_action(move |_: &ShowRecordingControlsFromStatus, cx| controls_from_status(cx));
    let larger = send(AppCommand::TextSize(TextSizeChange::Larger));
    cx.on_action(move |_: &LargerText, cx| larger(cx));
    let smaller = send(AppCommand::TextSize(TextSizeChange::Smaller));
    cx.on_action(move |_: &SmallerText, cx| smaller(cx));
    let standard = send(AppCommand::TextSize(TextSizeChange::Standard));
    cx.on_action(move |_: &StandardTextSize, cx| standard(cx));
    cx.on_action(|_: &HideApp, cx| cx.hide());
    cx.on_action(|_: &HideOtherApps, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAllApps, cx| cx.unhide_other_apps());
    cx.on_action(|_: &CloseWindow, cx| active_window(cx, |window| window.remove_window()));
    // About and the recording controls can't be minimized, also not with
    // the shortcut (SHL-53).
    cx.on_action(|_: &MinimizeWindow, cx| {
        active_window(cx, |window| {
            if window.is_minimizable() {
                window.minimize_window();
            }
        })
    });
    cx.on_action(|_: &ZoomWindow, cx| active_window(cx, |window| window.zoom_window()));
    cx.on_action(|_: &BringAllToFront, cx| crate::ui::windows::bring_all_to_front(cx));
    cx.set_global(InstalledMenus {
        app: state.cloned(),
        main: None,
        shown: None,
    });
    if let Some(state) = state {
        cx.observe(state, |_, cx| window_activation_changed(cx))
            .detach();
        if let Some(daemon) = state.read(cx).daemon().cloned() {
            cx.observe(&daemon, |_, cx| window_activation_changed(cx))
                .detach();
        }
        if let Some(agent) = state.read(cx).agent_starter() {
            cx.observe(&agent, |_, cx| window_activation_changed(cx))
                .detach();
        }
        let updates = crate::views::update::UpdateNotices::global(cx);
        cx.observe(&updates, |_, cx| window_activation_changed(cx))
            .detach();
    }
    cx.on_window_closed(|cx, id| {
        if let Some(installed) = cx.try_global::<InstalledMenus>()
            && installed.main.is_some_and(|(window, _)| window == id)
        {
            cx.global_mut::<InstalledMenus>().main = None;
        }
        refresh_native_menus(cx);
    })
    .detach();
    refresh_native_menus(cx);
}

/// Steps the saved reading size under the config lock, from the file as it
/// is now (two quick presses make two steps), and returns the saved config.
pub fn change_reading_size(
    data_dir: &Path,
    change: TextSizeChange,
) -> broscribe_core::Result<Config> {
    Config::update(data_dir, |config| {
        config.appearance.reading_size = change.apply(config.appearance.reading_size);
        Ok(())
    })
    .map(|(config, ())| config)
}

/// [`change_reading_size`] off the UI loop; the saved config then goes to
/// the app state, which applies configs in revision order.
pub fn apply_text_size(state: &Entity<AppState>, change: TextSizeChange, cx: &mut App) {
    let data_dir = state.read(cx).paths().data_dir.clone();
    let saved = state
        .read(cx)
        .cli()
        .spawn(cx.background_executor(), async move {
            change_reading_size(&data_dir, change)
        });
    let state = state.downgrade();
    cx.spawn(async move |cx| match saved.await {
        Ok(config) => {
            state
                .update(cx, |state, cx| state.set_config(config, cx))
                .ok();
        }
        Err(error) => tracing::warn!(%error, "changing the reading size"),
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use broscribe_core::Library;
    use broscribe_ipc::IpcError;
    use broscribe_platform::fakes::FakePaths;
    use broscribe_platform::resolve_paths;
    use gpui::{AppContext as _, Context, IntoElement, OwnedMenuItem, Render, TestAppContext, div};

    use super::*;

    #[gpui::test]
    fn taskbar_overlay_projects_recording_paused_and_clear_once_per_window(
        cx: &mut TestAppContext,
    ) {
        use broscribe_platform::fakes::FakeStatusItem;
        let (send, _receive) = futures::channel::mpsc::unbounded();
        let item = FakeStatusItem::new(send);
        let calls = item.taskbar_states.clone();
        cx.update(|cx| {
            cx.set_global(TaskbarStatus(Rc::new(RefCell::new(InstalledStatus {
                item: Some(Box::new(item)),
                mac_factory: None,
                wanted: true,
                shown: None,
                taskbars: HashMap::new(),
            }))));
            let windows = std::array::from_fn::<_, 2, _>(|_| {
                cx.open_window(gpui::WindowOptions::default(), |window, cx| {
                    let view = cx.new(|_| gpui::Empty);
                    cx.new(|cx| gpui::component::Root::new(view, window, cx))
                })
                .unwrap()
            });
            for recording in [
                Some(RecordingStatus::Recording),
                Some(RecordingStatus::Recording),
                Some(RecordingStatus::Paused),
                None,
                None,
            ] {
                for window in windows {
                    sync_taskbar(window, recording, cx);
                }
            }
        });
        assert_eq!(
            *calls.lock().unwrap(),
            [
                TaskbarState::Recording,
                TaskbarState::Recording,
                TaskbarState::Paused,
                TaskbarState::Paused,
                TaskbarState::None,
                TaskbarState::None
            ]
        );
    }

    #[test]
    fn the_status_item_says_its_state_in_words_and_glyph() {
        assert_eq!(status_look(None), ("BroScribe", StatusIcon::Idle));
        assert_eq!(
            status_look(Some(RecordingStatus::Recording)),
            ("BroScribe · Recording", StatusIcon::Recording)
        );
        assert_eq!(
            status_look(Some(RecordingStatus::Paused)),
            ("BroScribe · Paused", StatusIcon::Paused)
        );
        assert_eq!(status_title(None, 0, 0), "BroScribe");
        assert_eq!(
            status_title(Some(RecordingStatus::Recording), 3, 0),
            "BroScribe · Recording · 3 speakers to review"
        );
        assert_eq!(
            status_title(Some(RecordingStatus::Paused), 1, 0),
            "BroScribe · Paused · 1 speaker to review"
        );
        let mixed = status_title(Some(RecordingStatus::Recording), 2, 1);
        assert_eq!(
            mixed,
            "BroScribe · Recording · 1 meeting to answer · 2 speakers to review"
        );
        assert!(mixed.len() < 128);
        assert_eq!(status_title(None, 0, 2), "BroScribe · 2 meetings to answer");
    }

    #[test]
    fn menu_ids_and_recording_state() {
        for command in TRAY_GROUPS.iter().flat_map(|group| group.iter()) {
            assert_eq!(MenuCommand::from_id(&command.id()), Some(command.clone()));
        }
        assert_eq!(MenuCommand::from_id("nope"), None);
        let idle = recording_menu(TrayMenu {
            reviews: Some(3),
            ..Default::default()
        });
        assert!(
            idle.iter()
                .find(|item| item.id == "record-start")
                .unwrap()
                .enabled
        );
        assert!(
            !idle
                .iter()
                .find(|item| item.id == "record-stop")
                .unwrap()
                .enabled
        );
        assert!(
            idle.iter().all(|item| item.id != "agent-start"),
            "a connected agent is not offered a start"
        );
        assert_eq!(
            idle.iter()
                .find(|item| item.id == "review-queue")
                .unwrap()
                .title,
            "Review queue (3)"
        );
        let paused = recording_menu(TrayMenu {
            recording: Some(RecordingStatus::Paused),
            ..Default::default()
        });
        assert_eq!(paused[0].title, "Paused");
        assert!(
            paused
                .iter()
                .find(|item| item.id == "record-stop")
                .unwrap()
                .enabled
        );
        let stop = paused.iter().find(|item| item.id == "record-stop").unwrap();
        assert_eq!(stop.title, "Stop and save recording");
        assert!(
            !paused.iter().any(|item| item.title.contains("Discard")),
            "the menu never discards"
        );
    }

    #[test]
    fn tray_explains_both_kinds_of_blocked_journal() {
        let issues = [JOURNAL_LIBRARY_UNKNOWN, JOURNAL_LIBRARY_MISMATCH].map(|code| JournalIssue {
            session_id: "s-synthetic".into(),
            code: code.into(),
            corrupt: false,
        });
        let menu = recording_menu(TrayMenu {
            journal_issues: &issues,
            ..TrayMenu::default()
        });
        for (code, words) in [
            (JOURNAL_LIBRARY_MISMATCH, "Open the original library"),
            (JOURNAL_LIBRARY_UNKNOWN, "Older recordings"),
        ] {
            assert!(
                menu.iter()
                    .any(|item| item.id == code && item.title.starts_with(words) && !item.enabled)
            );
        }
    }

    #[test]
    fn tray_disables_start_and_offers_update_recovery_for_every_hold() {
        for (update, caption, command) in [
            (
                UpdateMenu::Finish("1.2.0".into()),
                "Recording off until the update is kept",
                "Keep BroScribe 1.2.0",
            ),
            (
                UpdateMenu::Recover,
                "Recording off until update recovery finishes",
                "Finish updating",
            ),
        ] {
            let menu = recording_menu(TrayMenu {
                update,
                ..Default::default()
            });
            let item = |id| menu.iter().find(|item| item.id == id).unwrap();
            assert!(!item("record-start").enabled);
            assert_eq!(item("recording-status").title, caption);
            assert!(item("update").enabled);
            assert_eq!(item("update").title, command);
        }
    }

    #[test]
    fn tray_groups_keep_recording_commands_and_use_platform_copy() {
        let expected = [
            "recording-status",
            "",
            "record-start",
            "record-pause-resume",
            "record-stop",
            "open-panel",
            "",
            "open-app",
            "review-queue",
            "",
            "settings",
            "about",
            "",
            "quit",
        ];
        for mac in [false, true] {
            for recording in [
                None,
                Some(RecordingStatus::Recording),
                Some(RecordingStatus::Paused),
            ] {
                let menu = recording_menu(TrayMenu {
                    mac,
                    reviews: Some(3),
                    recording,
                    recording_name: Some("Synthetic orbit review"),
                    ..Default::default()
                });
                assert_eq!(
                    menu.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
                    expected
                );
                let item = |id| menu.iter().find(|item| item.id == id).unwrap();
                assert_eq!(item("record-start").enabled, recording.is_none());
                assert_eq!(item("record-stop").enabled, recording.is_some());
                assert_eq!(item("record-pause-resume").enabled, recording.is_some());
                assert_eq!(
                    item("open-panel").title,
                    if mac {
                        "Show Recording Controls"
                    } else {
                        "Show recording controls"
                    }
                );
                assert_eq!(
                    item("review-queue").title,
                    if mac {
                        "Review Queue (3)"
                    } else {
                        "Review queue (3)"
                    }
                );
                if recording == Some(RecordingStatus::Paused) {
                    assert_eq!(
                        item("recording-status").title,
                        "Paused · Synthetic orbit review"
                    );
                    assert_eq!(
                        item("record-pause-resume").title,
                        if mac {
                            "Resume Recording"
                        } else {
                            "Resume recording"
                        }
                    );
                }
            }
        }
        let no_reviews = recording_menu(TrayMenu {
            reviews: Some(0),
            ..Default::default()
        });
        assert!(no_reviews.iter().any(|item| item.title == "Review queue"));
    }

    #[test]
    fn recording_menu_has_stable_commands_and_session_fenced_actions() {
        use broscribe_ipc::Call;
        let mut menu = NativeMenuState {
            reading: Some(ReadingSize::Standard),
            ..Default::default()
        };
        let commands = |state: &NativeMenuState| {
            app_menus(state)
                .remove(6)
                .items
                .into_iter()
                .filter_map(|item| match item {
                    MenuItem::Action { name, disabled, .. } => Some((name.to_string(), !disabled)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let enabled = |state: &NativeMenuState| {
            commands(state)
                .into_iter()
                .map(|(_, enabled)| enabled)
                .collect::<Vec<_>>()
        };
        assert_eq!(enabled(&menu), [true, false, false, false, false, false]);
        menu.start_pending = true;
        assert_eq!(enabled(&menu), [false; 6]);
        menu.start_pending = false;
        menu.agent = Some(AgentStartStatus::NotInstalled);
        assert_eq!(enabled(&menu), [false, false, false, false, true, false]);
        assert_eq!(commands(&menu)[4].0, "Set Up BroScribe Agent");
        menu.agent = None;
        menu.recording = Some(RecordingStatus::Paused);
        assert_eq!(enabled(&menu), [false, true, true, true, false, false]);
        assert_eq!(commands(&menu)[1].0, "Resume Recording");
        menu.recording = Some(RecordingStatus::Recording);
        assert_eq!(commands(&menu)[1].0, "Pause Recording");
        assert_eq!(commands(&menu)[2].0, "Stop and Save Recording");

        let mut active: broscribe_ipc::RecordingState = serde_json::from_value(serde_json::json!({
            "session_id": "s-synthetic", "status": "recording", "started_at": "2026-11-23T09:00:00Z",
            "elapsed_s": 14., "health": { "tracks": [] }
        })).unwrap();
        assert!(matches!(
            RecordingMenuAction::Start.call(None),
            Some(Call::RecordStart(_))
        ));
        assert!(RecordingMenuAction::Start.call(Some(&active)).is_none());
        assert_eq!(RecordingMenuAction::Stop.call(None), None);
        assert_eq!(
            RecordingMenuAction::PauseOrResume.call(Some(&active)),
            Some(Call::RecordPause {
                session_id: active.session_id.clone()
            })
        );
        active.status = RecordingStatus::Paused;
        assert_eq!(
            RecordingMenuAction::PauseOrResume.call(Some(&active)),
            Some(Call::RecordResume {
                session_id: active.session_id.clone()
            })
        );
        assert_eq!(
            RecordingMenuAction::Bookmark.call(Some(&active)),
            Some(Call::RecordBookmark {
                session_id: active.session_id.clone(),
                label: None
            })
        );
        assert_eq!(
            RecordingMenuAction::Stop.call(Some(&active)),
            Some(Call::RecordStop {
                session_id: active.session_id
            })
        );
    }

    #[test]
    fn file_go_view_window_and_help_follow_the_main_window() {
        let mut state = NativeMenuState {
            reading: Some(ReadingSize::Standard),
            window: true,
            minimize: true,
            zoom: true,
            ..Default::default()
        };
        let rows = |state: &NativeMenuState, menu: &str| {
            let menus: Vec<_> = app_menus(state).into_iter().map(Menu::owned).collect();
            menu_titles(&menus)
                .into_iter()
                .find(|(name, _)| name == menu)
                .unwrap()
                .1
        };
        assert_eq!(
            rows(&state, "File"),
            [
                "Import Recordings…",
                "Import Folder…",
                "Search Meetings",
                "Export Meeting…",
                "Close"
            ]
            .map(|label| (label.to_owned(), label == "Export Meeting…"))
        );
        assert_eq!(
            rows(&state, "Window"),
            [
                "Minimize",
                "Zoom",
                "Show Recording Controls",
                "Bring All to Front",
                "BroScribe"
            ]
            .map(|label| (label.to_owned(), false))
        );
        assert_eq!(
            rows(&state, "Help"),
            ["Keyboard Shortcuts", "Diagnostics", "Third-Party Licences"]
                .map(|label| (label.to_owned(), false))
        );
        assert_eq!(
            rows(&state, "Go"),
            ["Back", "Forward", "Library", "Review Queue", "People"]
                .map(|label| (label.to_owned(), ["Back", "Forward"].contains(&label)))
        );
        assert_eq!(rows(&state, "View")[0], ("Show Sidebar".into(), true));

        state.main_active = true;
        state.main.back = true;
        state.main.sidebar = true;
        assert_eq!(rows(&state, "Go")[0], ("Back".into(), false));
        assert_eq!(rows(&state, "Go")[1], ("Forward".into(), true));
        assert_eq!(rows(&state, "View")[0], ("Hide Sidebar".into(), false));
        assert_eq!(rows(&state, "View")[4], ("Enter Full Screen".into(), false));
        state.main.forward = true;
        state.main.fullscreen = true;
        state.main.back = false;
        assert_eq!(rows(&state, "Go")[0], ("Back".into(), true));
        assert_eq!(rows(&state, "Go")[1], ("Forward".into(), false));
        assert_eq!(rows(&state, "View")[4], ("Exit Full Screen".into(), false));
        state.main.dialog = true;
        assert!(rows(&state, "File")[0].1);
        assert!(rows(&state, "File")[1].1);
        state.main_active = false;
        assert!(rows(&state, "Go")[1].1);
        assert!(rows(&state, "View")[0].1);
    }

    #[gpui::test]
    fn popup_menu_actions_resolve_the_bound_accelerators(cx: &mut TestAppContext) {
        use broscribe_platform::HostPlatform;

        for (host, os) in [
            (HostPlatform::Windows, gpui::OperatingSystem::Windows),
            (HostPlatform::Linux, gpui::OperatingSystem::Linux),
        ] {
            cx.set_platform_info(gpui::PlatformInfo {
                os,
                target: gpui::PlatformTarget::Native,
            });
            cx.update(|cx| {
                cx.clear_key_bindings();
                cx.bind_keys(crate::ui::keymap::key_bindings_for(host, cx));
            });
            let window = cx.add_window(|_, _| Blank);
            let state = NativeMenuState {
                reading: Some(ReadingSize::Standard),
                ..Default::default()
            };
            window
                .update(cx, |_, window, _| {
                    let rows = crate::ui::keymap::shortcuts(host);
                    for item in toolbar_items(&state) {
                        let MenuItem::Action { action, name, .. } = item else {
                            continue;
                        };
                        let expected: Vec<_> = rows
                            .iter()
                            .filter(|row| row.action.partial_eq(action.as_ref()))
                            .map(|row| gpui::Keystroke::parse_with_os(row.keys, os).unwrap())
                            .collect();
                        let actual = window.highest_precedence_binding_for_action_in_context(
                            action.as_ref(),
                            gpui::KeyContext::new_with_defaults(os),
                        );
                        if expected.is_empty() {
                            assert!(actual.is_none(), "{host:?} {name}");
                        } else {
                            let binding = actual
                                .unwrap_or_else(|| panic!("{host:?} {name} has no accelerator"));
                            assert!(
                                expected.contains(binding.keystrokes()[0].as_keystroke()),
                                "{host:?} {name}"
                            );
                        }
                    }
                })
                .unwrap();
        }
    }

    #[test]
    fn setup_resume_is_available_from_the_app_menu_and_from_an_unfinished_tray() {
        let menus = app_menus(&NativeMenuState {
            reading: Some(ReadingSize::Standard),
            ..Default::default()
        });
        let names: Vec<_> = menus[0]
            .items
            .iter()
            .filter_map(|item| {
                if let MenuItem::Action { name, .. } = item {
                    Some(name.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert!(
            names
                .windows(3)
                .any(|group| group == ["Settings…", "Check for Updates", "Setup Assistant"])
        );
        let unfinished = recording_menu(TrayMenu {
            unfinished_setup: true,
            ..Default::default()
        });
        assert!(
            unfinished
                .iter()
                .any(|item| item.id == MenuCommand::FinishSetup.id()
                    && item.title == "Finish setup"
                    && item.enabled)
        );
        assert!(
            !recording_menu(TrayMenu::default())
                .iter()
                .any(|item| item.id == MenuCommand::FinishSetup.id())
        );
    }

    #[test]
    fn a_disconnected_agent_is_offered_one_start_with_its_feedback() {
        let find = |items: &[MenuItemSpec], id: &str| {
            items.iter().find(|item| item.id == id).cloned().unwrap()
        };
        let offered = recording_menu(TrayMenu {
            agent: Some(&AgentStartStatus::Idle),
            ..Default::default()
        });
        assert_eq!(offered[0].title, "Can't reach BroScribe Agent.");
        assert!(find(&offered, "agent-start").enabled);
        assert!(!find(&offered, "record-start").enabled);
        assert!(!find(&offered, "record-stop").enabled);

        let starting = recording_menu(TrayMenu {
            agent: Some(&AgentStartStatus::Starting),
            ..Default::default()
        });
        let start = find(&starting, "agent-start");
        assert!(!start.enabled, "a start in flight takes no second click");
        assert_eq!(start.title, "Starting BroScribe Agent…");

        let failed = recording_menu(TrayMenu {
            agent: Some(&AgentStartStatus::Failed("synthetic refusal".into())),
            ..Default::default()
        });
        let line = find(&failed, "agent-status");
        assert!(!line.enabled);
        assert!(line.title.contains("synthetic refusal"));
        assert!(
            find(&failed, "agent-start").enabled,
            "a failure can be retried"
        );
    }

    #[test]
    fn tray_uses_the_shared_agent_recovery_actions() {
        for (state, label, mac_label) in [
            (
                AgentStartStatus::NotInstalled,
                "Set up BroScribe Agent",
                "Set Up BroScribe Agent",
            ),
            (
                AgentStartStatus::Stopped,
                "Start BroScribe Agent",
                "Start BroScribe Agent",
            ),
            (
                AgentStartStatus::NeedsApproval,
                "Open Login Items",
                "Open Login Items",
            ),
            (
                AgentStartStatus::Outdated {
                    agent_protocol: Some(6),
                },
                "Reinstall",
                "Reinstall",
            ),
            (AgentStartStatus::DidNotConnect, "Try again", "Try Again"),
        ] {
            for (mac, label) in [(false, label), (true, mac_label)] {
                let items = recording_menu(TrayMenu {
                    mac,
                    agent: Some(&state),
                    ..Default::default()
                });
                let action = items.iter().find(|item| item.id == "agent-start").unwrap();
                assert!(action.enabled);
                assert_eq!(action.title, label);
                assert_eq!(
                    items
                        .iter()
                        .find(|item| item.id == "agent-status")
                        .unwrap()
                        .title,
                    state.message().unwrap()
                );
                assert_eq!(
                    items.iter().any(|item| item.id == "agent-copy-details"),
                    state.copy_details().is_some()
                );
            }
        }
        let items = recording_menu(TrayMenu {
            agent: Some(&AgentStartStatus::Connecting),
            ..Default::default()
        });
        assert!(items.iter().all(|item| item.id != "agent-start"));
        assert_eq!(items[0].title, "Connecting to BroScribe Agent…");
    }

    #[test]
    fn tray_activation_opens_the_app_in_every_state_and_never_acts_on_capture() {
        use broscribe_platform::fakes::FakeStatusItem;

        let states: [(&str, Option<RecordingStatus>, Option<&AgentStartStatus>); 6] = [
            ("idle", None, None),
            ("recording", Some(RecordingStatus::Recording), None),
            ("paused", Some(RecordingStatus::Paused), None),
            ("agent down", None, Some(&AgentStartStatus::Idle)),
            ("agent starting", None, Some(&AgentStartStatus::Starting)),
            (
                "agent failed",
                None,
                Some(&AgentStartStatus::Failed("synthetic".into())),
            ),
        ];

        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let mut status = FakeStatusItem::new(tx);
        for (name, recording, agent) in states {
            let menu = recording_menu(TrayMenu {
                reviews: Some(2),
                recording,
                agent,
                ..Default::default()
            });
            let defaults: Vec<&str> = menu
                .iter()
                .filter(|item| item.default)
                .map(|item| item.id.as_str())
                .collect();
            assert_eq!(defaults, ["open-app"], "{name}: only open-app is default");
            let open = menu.iter().find(|item| item.id == "open-app").unwrap();
            assert_eq!(open.title, "Open BroScribe");
            assert!(open.enabled, "{name}: open-app must stay enabled");

            status.set_menu(menu).unwrap();
            assert!(status.activate(), "{name}");
            let first = rx.try_recv().unwrap();
            assert_eq!(first, "open-app", "{name}");
            assert!(status.secondary_activate(), "{name}");
            let second = rx.try_recv().unwrap();
            assert_eq!(second, "open-app", "{name}");
            assert!(rx.try_recv().is_err(), "{name}");
        }

        // Removing the default flag makes activation do nothing instead of
        // falling back to record-start / record-stop / agent-start.
        let mut without_default = recording_menu(TrayMenu::default());
        for item in &mut without_default {
            item.default = false;
        }
        status.set_menu(without_default).unwrap();
        assert!(!status.activate());
        assert!(!status.secondary_activate());
        assert!(rx.try_recv().is_err());
    }

    /// An agent that is never reached: the status item reads only the
    /// client's state and the notification host's fallbacks.
    struct Unreachable;
    impl crate::daemon::Transport for Unreachable {
        fn subscribe(&self) -> Result<Box<dyn crate::daemon::EventStream>, IpcError> {
            Err(IpcError::new("agent_unreachable", "synthetic: no agent"))
        }
        fn call(&self, _: broscribe_ipc::Call) -> Result<serde_json::Value, IpcError> {
            Err(IpcError::new("agent_unreachable", "synthetic: no agent"))
        }
    }

    /// "N meetings to answer" counts only asks among the notifications that
    /// fell back to the status item, in both words and the badge number.
    #[gpui::test]
    fn only_asks_count_in_the_status_badge_and_title(cx: &mut TestAppContext) {
        use broscribe_ipc::{Event, NotificationButton, NotificationRequest};
        use broscribe_platform::fakes::{FakeNotifier, FakeStatusItem};

        let dir = tempfile::tempdir().unwrap();
        let state = state(cx, dir.path());
        let daemon = cx.update(|cx| DaemonClient::detached(std::sync::Arc::new(Unreachable), cx));
        // The fake notifier denies every banner, so each one falls back.
        let notifications = cx.update(|cx| {
            NotificationHost::attach(
                daemon.clone(),
                Box::new(FakeNotifier::default()),
                dir.path().join("data"),
                cx,
            )
        });
        let agent = cx.new(|cx| AgentStarter::new(state.clone(), daemon.clone(), cx));
        daemon.update(cx, |daemon, _| daemon.connected = true);
        let button = |id: &str, label: &str| NotificationButton {
            id: id.into(),
            label: label.into(),
        };
        let notice = |id: &str, title: &str, actions: Vec<NotificationButton>, meeting: bool| {
            NotificationRequest {
                id: id.into(),
                title: title.into(),
                body: "Synthetic".into(),
                actions,
                meeting_id: meeting.then(|| "m-0000a1".into()),
            }
        };
        let ask = |id: &str| {
            notice(
                id,
                "Meeting detected",
                vec![button("record", "Record"), button("ignore", "Ignore")],
                false,
            )
        };
        let from_agent = |request: NotificationRequest, cx: &mut TestAppContext| {
            daemon.update(cx, |daemon, cx| {
                daemon.received(Event::Notification(request), cx)
            });
            cx.run_until_parked();
        };
        let (tx, _clicks) = futures::channel::mpsc::unbounded();
        let mut item = FakeStatusItem::new(tx);
        let sources = StatusSources {
            state,
            daemon: daemon.clone(),
            notifications: notifications.clone(),
            agent,
        };
        let mut refresh_item = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                sources.presentation(cx).apply(&mut item, None).unwrap();
            });
            (item.display_title(), item.badge_count)
        };

        // Notices that ask nothing: a stop, a tip, finished results, a sign-in.
        from_agent(
            notice("stopped-0000abcd", "Recording stopped", Vec::new(), false),
            cx,
        );
        from_agent(
            notice(
                "ready-synthetic",
                "Final transcript ready",
                vec![button("open", "Open")],
                true,
            ),
            cx,
        );
        from_agent(
            notice(
                "integration-reauth-synthetic",
                "Sign-in required",
                vec![button("open", "Sign in")],
                false,
            ),
            cx,
        );
        notifications.update(cx, |host, cx| {
            host.post_local(
                notice("caption-tip", "Captions help with names", Vec::new(), false),
                cx,
            );
            host.post_local(
                notice(
                    "generation-synthetic-1",
                    "Summary ready — Synthetic sync",
                    vec![button("open", "Open")],
                    true,
                ),
                cx,
            );
        });
        cx.run_until_parked();
        assert_eq!(
            notifications.read_with(cx, |host, _| host.fallbacks.len()),
            5
        );
        assert_eq!(refresh_item(cx), ("BroScribe".to_owned(), 0));

        // One ask among them is one meeting to answer, two are two.
        from_agent(ask("ask-synthetic-1"), cx);
        assert_eq!(
            refresh_item(cx),
            ("BroScribe · 1 meeting to answer".to_owned(), 1)
        );
        from_agent(ask("ask-synthetic-2"), cx);
        assert_eq!(
            refresh_item(cx),
            ("BroScribe · 2 meetings to answer".to_owned(), 2)
        );
    }

    #[gpui::test]
    fn tray_asks_route_record_and_ignore_to_their_exact_pending_requests(cx: &mut TestAppContext) {
        use broscribe_ipc::{Call, Event, NotificationButton, NotificationRequest};
        use broscribe_platform::fakes::{FakeNotifier, FakeStatusItem};
        use std::sync::{Arc, Mutex};

        #[derive(Default)]
        struct Agent(Mutex<Vec<Call>>);
        impl crate::daemon::Transport for Agent {
            fn subscribe(&self) -> Result<Box<dyn crate::daemon::EventStream>, IpcError> {
                unreachable!("detached client")
            }
            fn call(&self, call: Call) -> Result<serde_json::Value, IpcError> {
                self.0.lock().unwrap().push(call);
                Ok(serde_json::json!({ "answered": true }))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let state = state(cx, dir.path());
        let transport = Arc::new(Agent::default());
        let (tx, clicks) = futures::channel::mpsc::unbounded();
        let sources = cx.update(|cx| {
            let daemon = DaemonClient::detached(transport.clone(), cx);
            let notifications = NotificationHost::attach(
                daemon.clone(),
                Box::new(FakeNotifier::default()),
                dir.path().into(),
                cx,
            );
            let agent = cx.new(|cx| AgentStarter::new(state.clone(), daemon.clone(), cx));
            let sources = StatusSources {
                state,
                daemon,
                notifications,
                agent,
            };
            install(
                Some(Box::new(FakeStatusItem::new(tx.clone()))),
                clicks,
                sources.clone(),
                None,
                cx,
            );
            sources
        });
        let ids = ["ask-z-synthetic", "ask-a-synthetic"];
        for id in ids {
            sources.daemon.update(cx, |daemon, cx| {
                daemon.connected = true;
                daemon.received(
                    Event::Notification(NotificationRequest {
                        id: id.into(),
                        title: "Meeting detected".into(),
                        body: format!("Synthetic meeting {id} is using the microphone."),
                        actions: ["record", "ignore"]
                            .map(|action| NotificationButton {
                                id: action.into(),
                                label: action.into(),
                            })
                            .into(),
                        meeting_id: None,
                    }),
                    cx,
                );
            });
        }
        let mut status = FakeStatusItem::new(tx.clone());
        cx.update(|cx| sources.presentation(cx).apply(&mut status, None))
            .unwrap();
        assert_eq!(status.badge_count, 2);
        let menu: Vec<_> = status
            .menu
            .iter()
            .filter(|item| item.id.starts_with("prompt"))
            .collect();
        assert_eq!(
            menu.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
            [
                "prompt-caption:ask-z-synthetic",
                "prompt:ask-z-synthetic:record",
                "prompt:ask-z-synthetic:ignore",
                "prompt-caption:ask-a-synthetic",
                "prompt:ask-a-synthetic:record",
                "prompt:ask-a-synthetic:ignore",
            ]
        );
        assert!(menu[0].title.contains("Synthetic meeting ask-z-synthetic"));
        assert!(!menu[0].enabled);
        assert!(transport.0.lock().unwrap().is_empty());

        assert!(status.click("prompt:ask-z-synthetic:ignore"));
        cx.run_until_parked();
        assert!(status.click("prompt:ask-a-synthetic:record"));
        cx.run_until_parked();
        assert_eq!(
            *transport.0.lock().unwrap(),
            [
                Call::NotificationAction {
                    notification_id: ids[0].into(),
                    action: "ignore".into()
                },
                Call::NotificationAction {
                    notification_id: ids[1].into(),
                    action: "record".into()
                },
            ]
        );
        cx.update(|cx| sources.presentation(cx).apply(&mut status, None))
            .unwrap();
        assert_eq!(status.badge_count, 0);
        assert!(!status.click("prompt:ask-a-synthetic:record"));
        tx.unbounded_send("prompt:ask-z-synthetic:record".into())
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            transport.0.lock().unwrap().len(),
            2,
            "stale clicks cannot answer"
        );
    }

    #[test]
    fn reading_size_steps_stop_at_the_ends_and_keep_other_settings() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.app.open_at_login = Some(false);
        config.appearance.reading_size = ReadingSize::Larger;
        config.save(dir.path()).unwrap();

        let saved = change_reading_size(dir.path(), TextSizeChange::Larger).unwrap();
        assert_eq!(saved.appearance.reading_size, ReadingSize::Largest);
        let saved = change_reading_size(dir.path(), TextSizeChange::Larger).unwrap();
        assert_eq!(saved.appearance.reading_size, ReadingSize::Largest);
        assert_eq!(saved.app.open_at_login, Some(false));

        // Steps start from the saved file, not from a stale copy.
        change_reading_size(dir.path(), TextSizeChange::Standard).unwrap();
        change_reading_size(dir.path(), TextSizeChange::Smaller).unwrap();
        let saved = change_reading_size(dir.path(), TextSizeChange::Smaller).unwrap();
        assert_eq!(saved.appearance.reading_size, ReadingSize::Small);
        let loaded = Config::load(dir.path()).unwrap();
        assert_eq!(loaded.appearance.reading_size, ReadingSize::Small);
        assert_eq!(loaded.app.open_at_login, Some(false));
        assert!(loaded.revision() > config.revision());
    }

    struct Blank;
    impl Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    fn state(cx: &mut TestAppContext, dir: &Path) -> Entity<AppState> {
        cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir)).unwrap(),
                    Config::default(),
                    Library::open(dir.join("lib")).unwrap(),
                    dir.join("no-such-cli"),
                ),
                cx,
            )
        })
    }

    #[test]
    fn dock_menu_publishes_only_when_recording_actions_change() {
        use broscribe_platform::fakes::FakeJumpList;
        let mut menu = DockMenu::default();
        let jump = FakeJumpList::default();
        for state in [
            DockRecording::Unavailable,
            DockRecording::Unavailable,
            DockRecording::Idle,
            DockRecording::Idle,
            DockRecording::Active,
            DockRecording::Active,
            DockRecording::Idle,
        ] {
            if let Some(items) = menu.update(state) {
                jump.replace(&items).unwrap();
            }
        }
        let lists = jump.0.lock().unwrap();
        let commands = |at: usize| {
            lists[at]
                .iter()
                .map(|item| item.command)
                .collect::<Vec<_>>()
        };
        let routes = [DockCommand::ShowRecordingControls, DockCommand::ReviewQueue];
        assert_eq!(lists.len(), 4);
        assert_eq!(commands(0), routes);
        assert_eq!(
            commands(1),
            [vec![DockCommand::StartRecording], routes.to_vec()].concat()
        );
        assert_eq!(
            commands(2),
            [vec![DockCommand::StopRecording], routes.to_vec()].concat()
        );
        assert_eq!(lists[1], lists[3]);
    }

    #[gpui::test]
    fn hidden_menu_bar_item_leaves_dock_recovery_and_returns_for_an_ask(cx: &mut TestAppContext) {
        use broscribe_platform::ActivationPolicy;
        use broscribe_platform::fakes::{
            ActivationCall, FakeActivation, FakeNotifier, FakeStatusItem,
        };
        use broscribe_platform::system::NotificationDelivery;
        let dir = tempfile::tempdir().unwrap();
        let state = state(cx, dir.path());
        let activation = Rc::new(FakeActivation::new());
        let created = Rc::new(std::cell::Cell::new(0));
        struct NoCalls;
        impl crate::daemon::Transport for NoCalls {
            fn subscribe(
                &self,
            ) -> Result<Box<dyn crate::daemon::EventStream>, broscribe_ipc::IpcError> {
                unreachable!("detached")
            }
            fn call(
                &self,
                _: broscribe_ipc::Call,
            ) -> Result<serde_json::Value, broscribe_ipc::IpcError> {
                unreachable!("no recording actions")
            }
        }
        let transport = std::sync::Arc::new(NoCalls);
        let (daemon, _) = cx.update(|cx| {
            crate::ui::windows::install_presence(true, false, activation.clone(), cx);
            let daemon = DaemonClient::detached(transport, cx);
            let notifier = FakeNotifier::default();
            *notifier.delivery.lock().unwrap() = Some(NotificationDelivery::Delivered);
            let notifications =
                NotificationHost::attach(daemon.clone(), Box::new(notifier), dir.path().into(), cx);
            let agent = cx.new(|cx| AgentStarter::new(state.clone(), daemon.clone(), cx));
            let (tx, clicks) = futures::channel::mpsc::unbounded();
            install(
                Some(Box::new(FakeStatusItem::new(tx.clone()))),
                clicks,
                StatusSources {
                    state: state.clone(),
                    daemon: daemon.clone(),
                    notifications: notifications.clone(),
                    agent,
                },
                Some(Box::new({
                    let created = created.clone();
                    move || {
                        created.set(created.get() + 1);
                        Ok(Box::new(FakeStatusItem::new(tx.clone())))
                    }
                })),
                cx,
            );
            (daemon, notifications)
        });
        let set = |on: bool, cx: &mut TestAppContext| {
            state.update(cx, |state, cx| {
                let (config, ()) = Config::update(&state.paths().data_dir, |config| {
                    config.app.show_status_item = Some(on);
                    Ok(())
                })
                .unwrap();
                state.set_config(config, cx);
            });
            cx.run_until_parked();
        };
        assert!(!cx.update(|cx| status_item_hidden(cx)));
        set(false, cx);
        assert!(cx.update(|cx| status_item_hidden(cx)));
        cx.update(crate::ui::windows::settle_presence);
        assert_eq!(
            activation.calls(),
            [
                ActivationCall::Policy(ActivationPolicy::Accessory),
                ActivationCall::Policy(ActivationPolicy::Regular),
            ]
        );

        daemon.update(cx, |daemon, cx| {
            daemon.received(
                broscribe_ipc::Event::Notification(broscribe_ipc::NotificationRequest {
                    id: "ask-synthetic".into(),
                    title: "Meeting detected".into(),
                    body: "A synthetic meeting is using the microphone.".into(),
                    actions: ["record", "ignore"]
                        .into_iter()
                        .map(|id| broscribe_ipc::NotificationButton {
                            id: id.into(),
                            label: id.into(),
                        })
                        .collect(),
                    meeting_id: None,
                }),
                cx,
            )
        });
        cx.run_until_parked();
        assert!(!cx.update(|cx| status_item_hidden(cx)));
        daemon.update(cx, |daemon, cx| {
            daemon.received(
                broscribe_ipc::Event::NotificationDismissed {
                    id: "ask-synthetic".into(),
                },
                cx,
            )
        });
        cx.run_until_parked();
        assert!(cx.update(|cx| status_item_hidden(cx)));

        set(true, cx);
        assert!(!cx.update(|cx| status_item_hidden(cx)));
        assert_eq!(created.get(), 2);
        assert_eq!(
            activation.calls().last(),
            Some(&ActivationCall::Policy(ActivationPolicy::Accessory))
        );
    }

    fn menu_titles(menus: &[gpui::OwnedMenu]) -> Vec<(String, Vec<(String, bool)>)> {
        menus
            .iter()
            .map(|menu| {
                let items = menu
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        OwnedMenuItem::Action { name, disabled, .. } => {
                            Some((name.clone(), *disabled))
                        }
                        _ => None,
                    })
                    .collect();
                (menu.name.to_string(), items)
            })
            .collect()
    }

    #[gpui::test]
    fn shortcuts_and_menus_reach_the_shell_and_follow_the_reading_size(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let state = state(cx, dir.path());
        let commands = Rc::new(RefCell::new(Vec::new()));
        cx.update(|cx| {
            let commands = commands.clone();
            install_app_menu(
                Some(&state),
                move |command, _| commands.borrow_mut().push(command),
                cx,
            )
        });
        let menus = menu_titles(&cx.update(|cx| cx.get_menus()).unwrap());
        let names: Vec<&str> = menus.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "BroScribe",
                "File",
                "Edit",
                "View",
                "Go",
                "Meeting",
                "Recording",
                "Window",
                "Help"
            ]
        );
        let view = &menus.iter().find(|(name, _)| name == "View").unwrap().1[1..4];
        assert_eq!(
            view,
            &[
                ("Larger Text".to_owned(), false),
                ("Smaller Text".to_owned(), false),
                ("Standard Text Size".to_owned(), true),
            ]
        );

        let window = cx.add_window(|_, _| Blank);
        window
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.simulate_keystrokes(
            window.into(),
            "secondary-, secondary-= secondary-- secondary-0 secondary-q",
        );
        assert_eq!(
            *commands.borrow(),
            [
                AppCommand::Settings,
                AppCommand::TextSize(TextSizeChange::Larger),
                AppCommand::TextSize(TextSizeChange::Smaller),
                AppCommand::TextSize(TextSizeChange::Standard),
                AppCommand::Quit,
            ]
        );

        // A saved size at the largest step disables only Larger Text.
        cx.update(|cx| apply_text_size(&state, TextSizeChange::Standard, cx));
        cx.run_until_parked();
        for _ in 0..4 {
            cx.update(|cx| apply_text_size(&state, TextSizeChange::Larger, cx));
            cx.run_until_parked();
        }
        state.read_with(cx, |state, _| {
            assert_eq!(state.config().appearance.reading_size, ReadingSize::Largest)
        });
        let menus = menu_titles(&cx.update(|cx| cx.get_menus()).unwrap());
        assert_eq!(
            menus.iter().find(|(name, _)| name == "View").unwrap().1[1..4],
            [
                ("Larger Text".to_owned(), true),
                ("Smaller Text".to_owned(), false),
                ("Standard Text Size".to_owned(), false),
            ]
        );

        // Close Window closes the focused window; nothing else is asked.
        cx.simulate_keystrokes(window.into(), "secondary-w");
        assert!(cx.update(|cx| cx.windows().is_empty()));
        assert_eq!(commands.borrow().len(), 5);
    }

    /// SHL-53: Minimize (⌘M, Ctrl+M) minimizes the window in front unless
    /// it can't be minimized (About, the recording controls).
    #[gpui::test]
    fn minimize_skips_windows_that_cannot_be_minimized(cx: &mut TestAppContext) {
        cx.update(|cx| install_app_menu(None, |_, _| {}, cx));
        for minimizable in [true, false] {
            let window = cx
                .update(|cx| {
                    cx.open_window(
                        gpui::WindowOptions {
                            is_minimizable: minimizable,
                            ..Default::default()
                        },
                        |_, cx| cx.new(|_| Blank),
                    )
                })
                .unwrap();
            window
                .update(cx, |_, window, _| window.activate_window())
                .unwrap();
            cx.simulate_keystrokes(window.into(), "secondary-m");
            cx.run_until_parked();
            assert_eq!(
                cx.window_requests(window.into()).minimizes,
                usize::from(minimizable),
                "minimizable: {minimizable}"
            );
        }
    }

    #[gpui::test]
    fn startup_variant_preserves_menus_and_disables_unavailable_actions(cx: &mut TestAppContext) {
        cx.update(|cx| install_app_menu(None, |_, _| {}, cx));
        let menus = menu_titles(&cx.update(|cx| cx.get_menus()).unwrap());
        let names: Vec<&str> = menus.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "BroScribe",
                "File",
                "Edit",
                "View",
                "Go",
                "Meeting",
                "Recording",
                "Window",
                "Help"
            ]
        );
        assert!(menus[0].1.contains(&("Settings…".into(), true)));
        assert!(menus[0].1.contains(&("Quit BroScribe".into(), false)));
        for index in [3, 4, 5, 6] {
            assert!(menus[index].1.iter().all(|(_, disabled)| *disabled));
        }
        assert!(
            menus[7]
                .1
                .contains(&("Show Recording Controls".into(), true))
        );

        let dir = tempfile::tempdir().unwrap();
        let window = cx.update(|cx| {
            gpui::init(cx);
            crate::ui::open_maintenance_window(dir.path().into(), cx).unwrap()
        });
        window
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        cx.run_until_parked();
        let menus = menu_titles(&cx.update(|cx| cx.get_menus()).unwrap());
        assert!(menus[1].1.contains(&("Close".into(), false)));
        assert!(menus[7].1.contains(&("Minimize".into(), false)));
        assert!(menus[7].1.contains(&("Zoom".into(), false)));
    }
}
