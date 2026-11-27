//! Notifications are submitted from GPUI's loop; callbacks only enqueue messages.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use broscribe_ipc::{Call, Event, IpcError, NotificationRequest};
use broscribe_platform::Notifier;
use broscribe_platform::system::{
    Notification, NotificationAction, NotificationActionSpec, NotificationDelivery,
    NotificationPreview,
};
use futures::{StreamExt as _, channel::mpsc};
use gpui::{App, AppContext as _, Context, Entity, EventEmitter, Subscription, Task};

use crate::daemon::{DaemonClient, DaemonEvent};
use crate::state::AppState;

const MAIN_WINDOW_CLOSED: &str = "window-closed";
const PANEL_CLOSED_WHILE_RECORDING: &str = "still-recording";

#[derive(Clone, Debug)]
pub enum NotificationEvent {
    FallbackChanged,
    OpenMainWindow,
    OpenMeeting(String),
    OpenPanel,
    OpenRecording(String),
    OpenLibrary,
    OpenIntegrations(String),
}

#[derive(Default)]
pub struct FallbackState {
    pub pending: BTreeMap<String, NotificationRequest>,
    pub fallbacks: BTreeMap<String, NotificationRequest>,
    dismissed: BTreeSet<String>,
    local: BTreeSet<String>,
    seen_untracked: BTreeSet<String>,
    prompt_order: Vec<String>,
    opened_prompt: Option<String>,
}
impl FallbackState {
    /// The daemon keeps only actionable notifications (asks, meeting opens,
    /// integration re-auth) in `DaemonState.pending_notifications`. Action-less
    /// notices with no meeting ("Recording stopped", errors) are never in a
    /// snapshot, so they behave like the app's own notices: only the user or
    /// an explicit `NotificationDismissed` removes them.
    pub fn daemon_tracked(request: &NotificationRequest) -> bool {
        !request.actions.is_empty()
            || request.meeting_id.is_some()
            || request.id.starts_with("failed-")
    }
    pub fn reconcile(&mut self, requests: &[NotificationRequest]) -> Vec<String> {
        let ids: BTreeSet<_> = requests.iter().map(|request| request.id.as_str()).collect();
        let stale: Vec<_> = self
            .pending
            .keys()
            .filter(|id| !ids.contains(id.as_str()) && !self.local.contains(id.as_str()))
            .cloned()
            .collect();
        for id in &stale {
            self.dismiss(id);
        }
        self.dismissed.retain(|id| ids.contains(id.as_str()));
        stale
    }
    pub fn dismiss_locally(&mut self, id: &str) -> bool {
        self.dismissed.insert(id.to_owned());
        self.dismiss(id)
    }
    /// Whether `request` waits for the user's answer about a meeting: the
    /// agent's "Meeting detected" ask, which offers the `record` action the
    /// agent handles for it together with `ignore`. The agent builds that
    /// ask in `ask_notification` (crates/broscribed/src/daemon.rs); keep its
    /// two button ids and these in step. Notices without buttons
    /// ("Recording stopped", the caption tip), notices whose only button
    /// opens something ("Final transcript ready", "Summary ready", "Sign-in
    /// required") and notices with any other button (a retry, a snooze) ask
    /// nothing about a meeting.
    pub fn is_meeting_prompt(request: &NotificationRequest) -> bool {
        request.is_recording_prompt()
    }
    pub fn accepts(request: &NotificationRequest, action: &str) -> bool {
        request.actions.iter().any(|button| button.id == action)
            || (action == "open"
                && (request.meeting_id.is_some() || request.id.starts_with("failed-")))
    }
    pub fn delivered(
        &mut self,
        request: NotificationRequest,
        delivery: NotificationDelivery,
    ) -> bool {
        if self.dismissed.contains(&request.id) {
            return false;
        }
        let id = request.id.clone();
        if !Self::daemon_tracked(&request) {
            self.local.insert(id.clone());
        }
        if request.is_recording_prompt() && !self.pending.contains_key(&id) {
            self.prompt_order.push(id.clone());
        }
        self.pending.insert(id.clone(), request);
        match delivery {
            NotificationDelivery::Denied => self.denied(&id),
            NotificationDelivery::Delivered => self.fallbacks.remove(&id).is_some(),
            NotificationDelivery::Pending => false,
        }
    }
    pub fn denied(&mut self, id: &str) -> bool {
        if let Some(request) = self.pending.get(id)
            && self.fallbacks.get(id) != Some(request)
        {
            self.fallbacks.insert(id.to_owned(), request.clone());
            true
        } else {
            false
        }
    }
    pub fn dismiss(&mut self, id: &str) -> bool {
        self.pending.remove(id);
        self.local.remove(id);
        self.prompt_order.retain(|waiting| waiting != id);
        let opened = self.opened_prompt.as_deref() == Some(id);
        if opened {
            self.opened_prompt = None;
        }
        self.fallbacks.remove(id).is_some() || opened
    }
}

pub struct NotificationHost {
    state: FallbackState,
    pub fallbacks: BTreeMap<String, NotificationRequest>,
    daemon: Entity<DaemonClient>,
    notifier: Box<dyn Notifier>,
    callbacks: mpsc::UnboundedSender<(u64, NotificationAction)>,
    delivery_serial: u64,
    deliveries: BTreeMap<String, u64>,
    app_state: Option<gpui::WeakEntity<AppState>>,
    data_dir: PathBuf,
    caption_session: Option<String>,
    _subscription: Subscription,
    _pump: Task<()>,
}
impl EventEmitter<NotificationEvent> for NotificationHost {}
impl NotificationHost {
    pub fn has_fallback_ask(&self) -> bool {
        self.fallbacks
            .values()
            .any(NotificationRequest::is_recording_prompt)
    }

    pub(crate) fn pending_asks(&self) -> impl Iterator<Item = &NotificationRequest> {
        self.state
            .prompt_order
            .iter()
            .filter_map(|id| self.state.pending.get(id))
    }

    pub(crate) fn meeting_prompts(&self) -> impl Iterator<Item = &NotificationRequest> {
        self.state
            .prompt_order
            .iter()
            .filter_map(|id| self.fallbacks.get(id))
    }

    pub(crate) fn panel_notices(&self) -> impl Iterator<Item = &NotificationRequest> {
        let opened = self
            .state
            .opened_prompt
            .as_deref()
            .filter(|id| !self.fallbacks.contains_key(*id))
            .and_then(|id| self.state.pending.get(id));
        self.fallbacks.values().chain(opened)
    }

    pub(crate) fn act_on_prompt(&mut self, id: &str, action: &str, cx: &mut Context<Self>) {
        if self
            .state
            .pending
            .get(id)
            .is_some_and(FallbackState::is_meeting_prompt)
        {
            self.act(id, action, cx);
        }
    }

    pub fn attach(
        daemon: Entity<DaemonClient>,
        notifier: Box<dyn Notifier>,
        data_dir: PathBuf,
        cx: &mut App,
    ) -> Entity<Self> {
        let (callbacks, mut rx) = mpsc::unbounded::<(u64, NotificationAction)>();
        cx.new(|cx: &mut Context<Self>| {
            let subscription =
                cx.subscribe(&daemon, |this, _, event: &DaemonEvent, cx| match event {
                    DaemonEvent::Received(Event::Notification(request)) => {
                        this.post(request.clone(), cx)
                    }
                    DaemonEvent::Received(Event::NotificationDismissed { id }) => {
                        this.dismiss(id, cx);
                    }
                    DaemonEvent::Received(Event::StartReader {
                        session_id, app_id, ..
                    }) => {
                        if caption_capable(app_id) {
                            this.caption_session = Some(session_id.clone());
                        }
                    }
                    DaemonEvent::Received(Event::State(state)) => {
                        let stale = this.state.reconcile(&state.pending_notifications);
                        let changed = !stale.is_empty();
                        for id in stale {
                            this.deliveries.remove(&id);
                            if let Err(error) = this.notifier.withdraw(&id) {
                                tracing::warn!(%error, "withdrawing stale notification");
                            }
                        }
                        if changed {
                            this.changed(cx);
                        }
                        for request in &state.pending_notifications {
                            this.post(request.clone(), cx);
                        }
                        let active = state
                            .recording
                            .as_ref()
                            .map(|recording| recording.session_id.as_str());
                        if this
                            .caption_session
                            .as_deref()
                            .is_some_and(|id| Some(id) != active)
                        {
                            this.caption_session = None;
                            this.caption_tip(cx);
                        }
                    }
                    _ => {}
                });
            let pump = cx.spawn(async move |this, cx| {
                while let Some((serial, action)) = rx.next().await {
                    if this
                        .update(cx, |this, cx| {
                            if this.deliveries.get(&action.id) != Some(&serial) {
                                return;
                            }
                            match action.action.as_str() {
                                "__denied" => {
                                    if this.state.denied(&action.id) {
                                        this.changed(cx);
                                    }
                                }
                                "__delivered" => {
                                    if this.state.fallbacks.remove(&action.id).is_some() {
                                        this.changed(cx);
                                    }
                                }
                                _ => this.act(&action.id, &action.action, cx),
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
            Self {
                state: FallbackState::default(),
                fallbacks: BTreeMap::new(),
                daemon,
                notifier,
                callbacks,
                delivery_serial: 0,
                deliveries: BTreeMap::new(),
                app_state: None,
                data_dir,
                caption_session: None,
                _subscription: subscription,
                _pump: pump,
            }
        })
    }
    fn caption_tip(&mut self, cx: &mut Context<Self>) {
        let data_dir = self.data_dir.clone();
        let admission = self.daemon.read(cx).admission();
        let claim = cx.background_executor().spawn(async move {
            let _admission = admission;
            claim_caption_tip(&data_dir)
        });
        cx.spawn(async move |this, cx| match claim.await {
            Ok(true) => {
                this.update(cx, |this, cx| {
                    let request = caption_tip_notice();
                    this.state.local.insert(request.id.clone());
                    this.post(request, cx);
                })
                .ok();
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(%error, "saving caption tip progress"),
        })
        .detach();
    }
    pub fn follow_meeting_focus(&mut self, state: &Entity<AppState>) {
        self.app_state = Some(state.downgrade());
    }

    fn post(&mut self, request: NotificationRequest, cx: &mut Context<Self>) {
        if self.state.pending.contains_key(&request.id)
            || self.state.dismissed.contains(&request.id)
        {
            return;
        }
        if !FallbackState::daemon_tracked(&request)
            && !self.state.seen_untracked.insert(request.id.clone())
        {
            return;
        }
        if request.id.starts_with("final-")
            && let Some(id) = request
                .meeting_id
                .as_deref()
                .and_then(|id| broscribe_core::MeetingId::parse(id).ok())
            && self
                .app_state
                .as_ref()
                .and_then(gpui::WeakEntity::upgrade)
                .is_some_and(|state| state.read(cx).meeting_in_view(&id, cx))
        {
            // Final jobs replay pending notices in the stream and in snapshots.
            // Record suppression before telling the agent to clear its entry.
            self.state.dismiss_locally(&request.id);
            self.daemon
                .read(cx)
                .request_reporting(
                    Call::NotificationAction {
                        notification_id: request.id,
                        action: "dismiss".into(),
                    },
                    |error| !notification_not_found(error),
                    cx,
                )
                .detach();
            return;
        }
        self.post_new(request, cx);
    }

    fn post_new(&mut self, request: NotificationRequest, cx: &mut Context<Self>) {
        self.delivery_serial += 1;
        let serial = self.delivery_serial;
        self.deliveries.insert(request.id.clone(), serial);
        self.submit(request, serial, cx);
    }

    /// A new generation for the same meeting and target replaces the previous
    /// result under its stable native ID.
    pub fn post_local(&mut self, request: NotificationRequest, cx: &mut Context<Self>) {
        self.state.local.insert(request.id.clone());
        if request.id.starts_with("generation-") {
            self.state.dismissed.remove(&request.id);
            self.post_new(request, cx);
        } else {
            self.post(request, cx);
        }
    }

    /// The main window closed while the app and its background work stay up.
    pub fn main_window_closed(
        &mut self,
        host: broscribe_platform::HostPlatform,
        cx: &mut Context<Self>,
    ) {
        use broscribe_platform::HostPlatform;
        let body = match host {
            HostPlatform::Windows => {
                "Recording and processing continue. Open BroScribe from the Start menu or from Show hidden icons (^) on the taskbar."
            }
            HostPlatform::Linux => {
                "Recording and processing continue. Open BroScribe from your app launcher."
            }
            HostPlatform::MacOs => return,
        };
        let seen = crate::ui_state::current(cx)
            .is_some_and(|state| state.seen_notices.iter().any(|id| id == MAIN_WINDOW_CLOSED));
        if !seen && crate::ui::once::claim(&self.data_dir, MAIN_WINDOW_CLOSED) {
            self.post_local(
                NotificationRequest {
                    id: MAIN_WINDOW_CLOSED.into(),
                    title: "BroScribe is still running".into(),
                    body: body.into(),
                    actions: Vec::new(),
                    meeting_id: None,
                },
                cx,
            );
        }
    }

    pub fn recording_controls_closed(
        &mut self,
        host: broscribe_platform::HostPlatform,
        cx: &mut Context<Self>,
    ) {
        use broscribe_platform::{HostPlatform, StatusIconPlace};
        if !crate::ui::once::claim(&self.data_dir, PANEL_CLOSED_WHILE_RECORDING) {
            return;
        }
        let mac = host == HostPlatform::MacOs;
        let reopen = match (host, crate::ui::status_icon_place(cx)) {
            (HostPlatform::MacOs, StatusIconPlace::MenuBar)
                if !crate::menu::status_item_hidden(cx) =>
            {
                "The BroScribe icon in the menu bar opens them again."
            }
            (HostPlatform::MacOs, _) => "Window › Show Recording Controls opens them again.",
            (HostPlatform::Windows, _) => {
                "The BroScribe icon in the notification area opens them again; Windows may keep it under Show hidden icons (^)."
            }
            (HostPlatform::Linux, StatusIconPlace::SystemTray) => {
                "The BroScribe icon in the system tray opens them again."
            }
            (HostPlatform::Linux, _) => {
                "The Recording button in BroScribe's toolbar opens them again."
            }
        };
        self.post_local(
            NotificationRequest {
                id: PANEL_CLOSED_WHILE_RECORDING.into(),
                title: if mac {
                    "Still Recording"
                } else {
                    "Still recording"
                }
                .into(),
                body: format!("Closing the controls doesn’t stop the recording. {reopen}"),
                actions: Vec::new(),
                meeting_id: None,
            },
            cx,
        );
    }
    fn submit(&mut self, request: NotificationRequest, serial: u64, cx: &mut Context<Self>) {
        let notification = Notification {
            id: request.id.clone(),
            title: request.title.clone(),
            body: request.body.clone(),
            preview: notification_preview(&request),
            opens_on_click: request.meeting_id.is_some()
                || request.id == MAIN_WINDOW_CLOSED
                || request.id == PANEL_CLOSED_WHILE_RECORDING
                || request.id == crate::views::panel::OVERLAY_NOTICE_ID
                || request.id.starts_with("failed-")
                || request.is_recording_prompt(),
            actions: request
                .actions
                .iter()
                // Older agents can still send a redundant Open meeting button.
                .filter(|button| button.id != "open" || request.meeting_id.is_none())
                .map(|button| NotificationActionSpec {
                    id: button.id.clone(),
                    label: button.label.clone(),
                })
                .collect(),
        };
        let callbacks = self.callbacks.clone();
        let delivery = self
            .notifier
            .notify_actionable(
                notification,
                Arc::new(move |action| {
                    let _ = callbacks.unbounded_send((serial, action));
                }),
            )
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "notification unavailable");
                NotificationDelivery::Denied
            });
        let ask = request.is_recording_prompt();
        if self.state.delivered(request, delivery) || ask {
            self.changed(cx);
        }
    }
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.fallbacks = self.state.fallbacks.clone();
        cx.emit(NotificationEvent::FallbackChanged);
        cx.notify();
    }
    fn dismiss(&mut self, id: &str, cx: &mut Context<Self>) {
        self.deliveries.remove(id);
        let ask = self
            .state
            .pending
            .get(id)
            .is_some_and(NotificationRequest::is_recording_prompt);
        if let Err(error) = self.notifier.withdraw(id) {
            tracing::warn!(%error, "withdrawing notification");
        }
        if self.state.dismiss(id) || ask {
            self.changed(cx);
        }
    }
    pub fn dismiss_local(&mut self, id: &str, cx: &mut Context<Self>) {
        self.state.dismissed.insert(id.to_owned());
        self.dismiss(id, cx);
    }
    pub fn act(&mut self, id: &str, action: &str, cx: &mut Context<Self>) {
        let Some(request) = self.state.pending.get(id) else {
            if action == "open" && id.starts_with("failed-") {
                cx.emit(NotificationEvent::OpenLibrary);
            }
            return;
        };
        if action == "open" && self.state.local.contains(id) {
            let meeting = request.meeting_id.clone();
            self.dismiss_local(id, cx);
            if let Some(meeting) = meeting {
                cx.emit(NotificationEvent::OpenMeeting(meeting));
            } else if id == MAIN_WINDOW_CLOSED {
                cx.emit(NotificationEvent::OpenMainWindow);
            } else if matches!(
                id,
                crate::views::panel::OVERLAY_NOTICE_ID | PANEL_CLOSED_WHILE_RECORDING
            ) {
                cx.emit(NotificationEvent::OpenPanel);
            }
            return;
        }
        if action == "open"
            && request.meeting_id.is_none()
            && request.actions.is_empty()
            && !id.starts_with("failed-")
        {
            self.dismiss_local(id, cx);
            return;
        }
        if action == "open" && request.is_recording_prompt() {
            self.state.opened_prompt = Some(id.to_owned());
            cx.notify();
            cx.emit(NotificationEvent::OpenPanel);
            return;
        }
        if !FallbackState::accepts(request, action) {
            return;
        }
        // The app's own notices are unknown to the agent.
        if self.state.local.contains(id) {
            return;
        }
        let meeting = request.meeting_id.clone();
        let id = id.to_owned();
        let serial = self.deliveries.get(&id).copied();
        let action = action.to_owned();
        let task = self.daemon.read(cx).request_reporting(
            Call::NotificationAction {
                notification_id: id.clone(),
                action: action.clone(),
            },
            |error| !notification_not_found(error),
            cx,
        );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                let current = this.deliveries.get(&id).copied();
                if current.is_some() && current != serial {
                    return;
                }
                match result {
                    Ok(value) => {
                        this.dismiss(&id, cx);
                        if action == "open" {
                            if let Some(integration) =
                                value.get("integration").and_then(serde_json::Value::as_str)
                            {
                                let (provider, account) = integration
                                    .split_once('/')
                                    .map_or((integration, None), |(p, a)| (p, Some(a)));
                                let valid = broscribe_integrations::IntegrationId::parse(provider)
                                    .is_ok()
                                    && account.is_none_or(|a| {
                                        broscribe_core::calendar::AccountId::parse(a).is_ok()
                                    });
                                if valid {
                                    cx.emit(NotificationEvent::OpenIntegrations(
                                        integration.to_owned(),
                                    ));
                                }
                            } else if let Some(session) = value
                                .get("recording_session")
                                .and_then(serde_json::Value::as_str)
                            {
                                cx.emit(NotificationEvent::OpenRecording(session.to_owned()));
                            } else if let Some(meeting) = value
                                .get("meeting_id")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned)
                                .or(meeting)
                            {
                                cx.emit(NotificationEvent::OpenMeeting(meeting));
                            }
                        }
                    }
                    Err(error) if notification_not_found(&error) => {
                        this.dismiss(&id, cx);
                        if action == "open" && id.starts_with("failed-") {
                            cx.emit(NotificationEvent::OpenLibrary);
                        }
                    }
                    Err(_) => {
                        if this.state.denied(&id) {
                            this.changed(cx);
                        }
                    }
                }
            })
            .ok();
        })
        .detach();
    }
}

fn notification_preview(request: &NotificationRequest) -> NotificationPreview {
    if FallbackState::is_meeting_prompt(request) {
        NotificationPreview::Detection
    } else if request.meeting_id.is_some() {
        NotificationPreview::Meeting
    } else if [
        "stopped-",
        "auto-stopped-",
        "failed-",
        "disk-low-",
        "autorecord-",
        "recording-",
        "still-recording",
    ]
    .iter()
    .any(|prefix| request.id.starts_with(prefix))
    {
        NotificationPreview::Recording
    } else {
        NotificationPreview::App
    }
}

/// The agent no longer has the notification: its answer's code, never its
/// text (WRT-04).
fn notification_not_found(error: &IpcError) -> bool {
    error.code == "not_found"
}

fn caption_capable(app: &str) -> bool {
    broscribe_readers::builtin_rules().is_ok_and(|rules| {
        rules
            .iter()
            .any(|rule| rule.id == app && rule.captions.is_some())
    })
}

fn caption_tip_notice() -> NotificationRequest {
    NotificationRequest {
        id: "caption-tip".into(),
        title: broscribe_platform_native::NativePlatform::notification_title(
            "Captions Help with Names",
            "Captions help with names",
        ),
        body: "For your next meeting, turn on captions in the meeting app. \
               BroScribe can use named captions to help identify speakers."
            .into(),
        actions: Vec::new(),
        meeting_id: None,
    }
}

fn claim_caption_tip(data_dir: &Path) -> std::io::Result<bool> {
    let state_dir = data_dir.join("state");
    std::fs::create_dir_all(&state_dir)?;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(state_dir.join("caption-tip-shown"))
    {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn only_ask_fallbacks_present_controls_and_terminal_notices_keep_their_destination(
        cx: &mut gpui::TestAppContext,
    ) {
        struct Routing;
        impl crate::daemon::Transport for Routing {
            fn subscribe(&self) -> Result<Box<dyn crate::daemon::EventStream>, IpcError> {
                unreachable!("detached")
            }
            fn call(&self, call: Call) -> Result<serde_json::Value, IpcError> {
                match call {
                    Call::NotificationAction {
                        notification_id,
                        action,
                    } if notification_id == "failed-synthetic" && action == "open" => {
                        Ok(serde_json::json!({"recording_session":"synthetic"}))
                    }
                    _ => panic!("unexpected request"),
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let notifier = broscribe_platform::fakes::FakeNotifier::default();
        let (host, daemon) = cx.update(|cx| {
            let daemon = DaemonClient::detached(Arc::new(Routing), cx);
            let host = NotificationHost::attach(
                daemon.clone(),
                Box::new(notifier.clone()),
                dir.path().into(),
                cx,
            );
            (host, daemon)
        });
        let routes = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let _sub = cx.update(|cx| {
            let routes = routes.clone();
            cx.subscribe(&host, move |_, event: &NotificationEvent, _| {
                routes.borrow_mut().push(event.clone())
            })
        });
        let notice = |id: &str, actions: &[&str]| NotificationRequest {
            id: id.into(),
            title: "Synthetic notice".into(),
            body: "Synthetic body".into(),
            meeting_id: None,
            actions: actions
                .iter()
                .map(|id| broscribe_ipc::NotificationButton {
                    id: (*id).into(),
                    label: (*id).into(),
                })
                .collect(),
        };
        for (id, actions) in [
            ("failed-synthetic", &[][..]),
            ("info-synthetic", &["open"][..]),
            ("reminder-synthetic", &["snooze"][..]),
        ] {
            daemon.update(cx, |daemon, cx| {
                daemon.received(Event::Notification(notice(id, actions)), cx)
            });
            cx.run_until_parked();
            assert!(!host.read_with(cx, |host, _| host.has_fallback_ask()));
        }
        let failed = notifier
            .actions
            .lock()
            .unwrap()
            .iter()
            .find(|notice| notice.id == "failed-synthetic")
            .cloned()
            .unwrap();
        assert!(failed.opens_on_click && failed.actions.is_empty());
        host.update(cx, |host, cx| host.act("failed-synthetic", "open", cx));
        cx.run_until_parked();
        assert!(routes.borrow().iter().any(|event| matches!(event, NotificationEvent::OpenRecording(session) if session == "synthetic")));
        host.update(cx, |host, cx| host.act("failed-synthetic", "open", cx));
        assert!(
            routes
                .borrow()
                .iter()
                .any(|event| matches!(event, NotificationEvent::OpenLibrary))
        );
        daemon.update(cx, |daemon, cx| {
            daemon.received(
                Event::Notification(notice("ask-synthetic", &["record", "ignore"])),
                cx,
            )
        });
        cx.run_until_parked();
        assert!(host.read_with(cx, |host, _| host.has_fallback_ask()));
    }

    #[gpui::test]
    fn clicking_delivered_informational_notices_dismisses_without_fallback(
        cx: &mut gpui::TestAppContext,
    ) {
        struct NoCalls;
        impl crate::daemon::Transport for NoCalls {
            fn subscribe(&self) -> Result<Box<dyn crate::daemon::EventStream>, IpcError> {
                unreachable!("detached client")
            }
            fn call(&self, _: Call) -> Result<serde_json::Value, IpcError> {
                panic!("informational clicks never call the agent")
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let notifier = broscribe_platform::fakes::FakeNotifier::default();
        *notifier.delivery.lock().unwrap() = Some(NotificationDelivery::Delivered);
        let (host, daemon) = cx.update(|cx| {
            let daemon = DaemonClient::detached(Arc::new(NoCalls), cx);
            let host = NotificationHost::attach(
                daemon.clone(),
                Box::new(notifier.clone()),
                dir.path().into(),
                cx,
            );
            (host, daemon)
        });
        let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&host, move |_, event: &NotificationEvent, _| {
                events.borrow_mut().push(event.clone())
            })
            .detach();
        });
        for (id, local) in [
            ("stopped-synthetic", false),
            ("caption-tip", true),
            (MAIN_WINDOW_CLOSED, true),
        ] {
            let notice = NotificationRequest {
                id: id.into(),
                title: "Synthetic notice".into(),
                body: "A synthetic update.".into(),
                actions: Vec::new(),
                meeting_id: None,
            };
            if local {
                host.update(cx, |host, cx| host.post_local(notice, cx));
            } else {
                daemon.update(cx, |daemon, cx| {
                    daemon.received(Event::Notification(notice), cx)
                });
            }
            cx.run_until_parked();
            host.update(cx, |host, _| {
                assert!(host.state.pending.contains_key(id));
                notifier.act(id, "open");
                assert!(host.fallbacks.is_empty());
            });
            cx.run_until_parked();
            host.read_with(cx, |host, _| {
                assert!(host.state.dismissed.contains(id));
                assert!(!host.state.pending.contains_key(id));
                assert!(host.fallbacks.is_empty());
            });
        }
        assert!(matches!(
            events.borrow().as_slice(),
            [NotificationEvent::OpenMainWindow]
        ));
    }
    #[gpui::test]
    fn final_notices_deduplicate_open_on_click_and_clear_when_the_meeting_is_focused(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::state::Route;
        use crate::views::person_picker::fixtures;
        use broscribe_platform::fakes::FakeNotifier;
        use std::{cell::RefCell, rc::Rc, sync::Mutex};

        #[derive(Default)]
        struct Routing(Mutex<Vec<(String, String)>>);
        impl crate::daemon::Transport for Routing {
            fn subscribe(&self) -> Result<Box<dyn crate::daemon::EventStream>, IpcError> {
                unreachable!("detached")
            }
            fn call(&self, call: Call) -> Result<serde_json::Value, IpcError> {
                let Call::NotificationAction {
                    notification_id,
                    action,
                } = call
                else {
                    panic!("unexpected call {call:?}");
                };
                let result = match action.as_str() {
                    "dismiss" => serde_json::json!({"dismissed": true}),
                    "open" => {
                        serde_json::json!({"meeting_id": notification_id.trim_start_matches("final-")})
                    }
                    _ => panic!("unexpected action"),
                };
                self.0.lock().unwrap().push((notification_id, action));
                Ok(result)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let (app, _, synthetic) = fixtures::state(cx, dir.path());
        let id = synthetic.meeting;
        let open_id = "m-000123";
        assert_ne!(open_id, id.as_str());
        let route = Route::meeting(id.clone());
        let window = cx.add_window(|_, _| gpui::Empty);
        app.update(cx, |app, cx| {
            app.navigate(route, cx);
            app.meeting_view_shown(id.clone(), window.into());
        });
        window
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        let routing = Arc::new(Routing::default());
        let notifier = FakeNotifier::default();
        *notifier.delivery.lock().unwrap() = Some(NotificationDelivery::Delivered);
        let opened = Rc::new(RefCell::new(Vec::new()));
        let (host, daemon) = cx.update(|cx| {
            let daemon = DaemonClient::detached(routing.clone(), cx);
            let host = NotificationHost::attach(
                daemon.clone(),
                Box::new(notifier.clone()),
                dir.path().into(),
                cx,
            );
            host.update(cx, |host, _| host.follow_meeting_focus(&app));
            let opened = opened.clone();
            cx.subscribe(&host, move |_, event: &NotificationEvent, _| {
                if let NotificationEvent::OpenMeeting(id) = event {
                    opened.borrow_mut().push(id.clone());
                }
            })
            .detach();
            (host, daemon)
        });
        let request = |id: &str| NotificationRequest {
            id: format!("final-{id}"),
            title: "Final transcript ready".into(),
            body: "A synthetic meeting is ready.".into(),
            actions: vec![broscribe_ipc::NotificationButton {
                id: "open".into(),
                label: "Open meeting".into(),
            }],
            meeting_id: Some(id.into()),
        };
        let focus = request(id.as_str());
        let hidden = request(open_id);
        daemon.update(cx, |daemon, cx| {
            for request in [focus.clone(), focus.clone(), hidden.clone(), hidden.clone()] {
                daemon.received(Event::Notification(request), cx);
            }
        });
        cx.run_until_parked();
        {
            let posts = notifier.actions.lock().unwrap();
            assert_eq!(posts.len(), 1);
            assert_eq!(posts[0].id, hidden.id);
            assert_eq!(posts[0].preview, NotificationPreview::Meeting);
            assert!(posts[0].opens_on_click && posts[0].actions.is_empty());
        }
        assert!(notifier.act(&hidden.id, "open"));
        cx.run_until_parked();
        assert_eq!(&*opened.borrow(), &[open_id]);
        assert_eq!(
            &*routing.0.lock().unwrap(),
            &[(focus.id, "dismiss".into()), (hidden.id, "open".into()),]
        );

        let generation = NotificationRequest {
            id: format!("generation-{open_id}-summary"),
            title: "Summary ready".into(),
            body: "A synthetic summary is ready.".into(),
            actions: Vec::new(),
            meeting_id: Some(open_id.into()),
        };
        host.update(cx, |host, cx| {
            host.post_local(generation.clone(), cx);
            host.post_local(
                NotificationRequest {
                    title: "Summary failed".into(),
                    ..generation.clone()
                },
                cx,
            );
        });
        {
            let posts = notifier.actions.lock().unwrap();
            assert_eq!(posts[1].id, posts[2].id);
            assert_eq!(posts[2].title, "Summary failed");
        }
        assert!(notifier.act(&generation.id, "open"));
        cx.run_until_parked();
        assert_eq!(&*opened.borrow(), &[open_id, open_id]);
        assert_eq!(routing.0.lock().unwrap().len(), 2, "generation is local");
    }

    fn prompt() -> NotificationRequest {
        NotificationRequest {
            id: "synthetic-ask".into(),
            title: "Record?".into(),
            body: "Synthetic meeting".into(),
            actions: vec![
                broscribe_ipc::NotificationButton {
                    id: "record".into(),
                    label: "Record".into(),
                },
                broscribe_ipc::NotificationButton {
                    id: "ignore".into(),
                    label: "Ignore".into(),
                },
            ],
            meeting_id: None,
        }
    }
    #[test]
    fn denial_preserves_prompt_until_explicit_action_or_daemon_dismissal() {
        let mut state = FallbackState::default();
        assert!(!state.delivered(prompt(), NotificationDelivery::Pending));
        assert!(state.denied("synthetic-ask"));
        assert!(state.fallbacks.contains_key("synthetic-ask"));
        assert!(!state.denied("synthetic-ask"));
        assert!(state.dismiss("synthetic-ask"));
        assert!(state.fallbacks.is_empty());
        assert!(!state.denied("synthetic-ask"));
    }
    #[test]
    fn immediate_denial_badges_but_success_never_auto_records() {
        let mut state = FallbackState::default();
        assert!(state.delivered(prompt(), NotificationDelivery::Denied));
        assert_eq!(state.fallbacks.len(), 1);
        state.dismiss("synthetic-ask");
        assert!(!state.delivered(prompt(), NotificationDelivery::Delivered));
        assert!(state.fallbacks.is_empty());
        assert_eq!(state.pending.len(), 1);
    }
    #[test]
    fn snapshot_removes_stale_fallbacks_and_preserves_live_prompts() {
        let mut state = FallbackState::default();
        state.delivered(prompt(), NotificationDelivery::Denied);
        assert!(state.reconcile(&[prompt()]).is_empty());
        assert_eq!(state.fallbacks.len(), 1);
        assert_eq!(state.reconcile(&[]), vec!["synthetic-ask"]);
        assert!(state.pending.is_empty());
        assert!(state.fallbacks.is_empty());
    }
    #[test]
    fn banner_open_keeps_record_controls_and_meeting_open_is_accepted() {
        let mut state = FallbackState::default();
        state.delivered(prompt(), NotificationDelivery::Delivered);
        assert!(state.denied("synthetic-ask"));
        assert_eq!(state.fallbacks["synthetic-ask"].actions[0].id, "record");
        assert_eq!(state.fallbacks["synthetic-ask"].actions[1].id, "ignore");
        let mut ready = prompt();
        ready.actions.clear();
        ready.meeting_id = Some("m_synthetic".into());
        assert!(FallbackState::accepts(&ready, "open"));
        assert!(!FallbackState::accepts(&ready, "record"));
    }
    #[test]
    fn local_dismiss_does_not_resurrect_from_snapshot_or_redelivery() {
        let mut state = FallbackState::default();
        let mut notice = prompt();
        notice.actions.clear();
        state.delivered(notice.clone(), NotificationDelivery::Denied);
        assert!(state.dismiss_locally(&notice.id));
        state.reconcile(&[notice.clone()]);
        assert!(!state.delivered(notice, NotificationDelivery::Denied));
        assert!(state.fallbacks.is_empty());
        assert!(notification_not_found(&IpcError::new(
            "not_found",
            "notification expired"
        )));
        // The code decides, never words in the message.
        assert!(!notification_not_found(&IpcError::new(
            crate::daemon::AGENT_UNREACHABLE,
            "transport disconnected (not_found)"
        )));
    }
    #[test]
    fn daemon_notice_without_actions_survives_state_snapshots() {
        let mut state = FallbackState::default();
        let stopped = NotificationRequest {
            id: "stopped-0000abcd".into(),
            title: "Recording stopped".into(),
            body: "Synthetic: disk below 500 MB".into(),
            actions: Vec::new(),
            meeting_id: None,
        };
        assert!(!FallbackState::daemon_tracked(&stopped));
        assert!(FallbackState::daemon_tracked(&prompt()));
        assert!(state.delivered(stopped.clone(), NotificationDelivery::Denied));
        state.delivered(prompt(), NotificationDelivery::Denied);
        // A snapshot without either withdraws only the daemon-tracked ask.
        assert_eq!(state.reconcile(&[]), vec!["synthetic-ask"]);
        assert!(state.reconcile(&[]).is_empty());
        assert!(state.fallbacks.contains_key(&stopped.id));
        assert!(state.pending.contains_key(&stopped.id));
        // The user still dismisses it explicitly.
        assert!(state.dismiss_locally(&stopped.id));
        assert!(state.fallbacks.is_empty());
    }
    #[test]
    fn only_the_record_and_ignore_ask_is_a_meeting_prompt() {
        let notice = |id: &str, title: &str, buttons: &[(&str, &str)], meeting: Option<&str>| {
            NotificationRequest {
                id: id.into(),
                title: title.into(),
                body: "Synthetic".into(),
                actions: buttons
                    .iter()
                    .map(|&(id, label)| broscribe_ipc::NotificationButton {
                        id: id.into(),
                        label: label.into(),
                    })
                    .collect(),
                meeting_id: meeting.map(Into::into),
            }
        };
        assert!(FallbackState::is_meeting_prompt(&prompt()));
        for request in [
            notice("stopped-0000abcd", "Recording stopped", &[], None),
            notice("caption-tip", "Captions help with names", &[], None),
            notice(
                "ready-synthetic",
                "Final transcript ready",
                &[("open", "Open")],
                Some("m-0000a1"),
            ),
            notice(
                "generation-synthetic-1",
                "Summary ready — Synthetic sync",
                &[("open", "Open")],
                Some("m-0000a1"),
            ),
            notice(
                "integration-reauth-synthetic",
                "Sign-in required",
                &[("open", "Sign in")],
                None,
            ),
            // A button other than Open doesn't make a notice an ask.
            notice(
                "failed-synthetic",
                "Summary failed",
                &[("retry", "Retry"), ("open", "Open")],
                Some("m-0000a1"),
            ),
            notice(
                "reminder-synthetic",
                "Synthetic sync starts soon",
                &[("snooze", "Snooze")],
                None,
            ),
            // It takes both buttons: `record` or `ignore` alone isn't the ask.
            notice(
                "record-only-synthetic",
                "Meeting detected",
                &[("record", "Record")],
                None,
            ),
            notice(
                "ignore-only-synthetic",
                "Meeting detected",
                &[("ignore", "Ignore")],
                None,
            ),
        ] {
            assert!(
                !FallbackState::is_meeting_prompt(&request),
                "{}",
                request.title
            );
        }
    }
    #[test]
    fn caption_tip_is_relevant_and_claimed_only_once_across_restarts() {
        let notice = caption_tip_notice();
        let title = if broscribe_platform_native::NativePlatform::host_platform()
            == broscribe_platform::HostPlatform::MacOs
        {
            "Captions Help with Names"
        } else {
            "Captions help with names"
        };
        assert_eq!(notice.title, title);
        assert_eq!(
            notice.body,
            "For your next meeting, turn on captions in the meeting app. BroScribe can use named captions to help identify speakers."
        );
        assert!(caption_capable("teams"));
        assert!(caption_capable("meet-chrome"));
        assert!(!caption_capable("synthetic-unsupported-app"));
        let dir = tempfile::tempdir().unwrap();
        assert!(claim_caption_tip(dir.path()).unwrap());
        assert!(!claim_caption_tip(dir.path()).unwrap());
    }
    #[test]
    fn local_tip_survives_daemon_snapshot_and_can_be_dismissed() {
        let mut state = FallbackState::default();
        let mut tip = prompt();
        tip.actions.clear();
        state.local.insert(tip.id.clone());
        state.delivered(tip.clone(), NotificationDelivery::Denied);
        assert!(state.reconcile(&[]).is_empty());
        assert!(state.fallbacks.contains_key(&tip.id));
        assert!(state.dismiss_locally(&tip.id));
        assert!(state.pending.is_empty());
        assert!(state.fallbacks.is_empty());
    }
    #[test]
    fn local_generation_notice_survives_snapshots_and_opens_its_meeting() {
        let mut state = FallbackState::default();
        let ready = NotificationRequest {
            id: "generation-synthetic-1".into(),
            title: "Summary ready — Synthetic sync".into(),
            body: "Open the meeting to read it.".into(),
            actions: vec![broscribe_ipc::NotificationButton {
                id: "open".into(),
                label: "Open".into(),
            }],
            meeting_id: Some("m-0000a1".into()),
        };
        // What `post_local` does before submitting.
        state.local.insert(ready.id.clone());
        assert!(state.delivered(ready.clone(), NotificationDelivery::Denied));
        // Daemon-tracked by shape (meeting, actions), yet the agent never
        // lists it: a snapshot keeps it.
        assert!(FallbackState::daemon_tracked(&ready));
        assert!(state.reconcile(&[]).is_empty());
        assert!(state.fallbacks.contains_key(&ready.id));
        assert!(FallbackState::accepts(&ready, "open"));
        assert!(state.dismiss_locally(&ready.id));
        assert!(state.pending.is_empty());
    }
}
