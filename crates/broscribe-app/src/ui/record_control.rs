//! The toolbar's recording requests and the small snapshot it paints.
use broscribe_ipc::{Call, RecordingStatus, StoppedRecordingState};
use gpui::component::button::{Button, ButtonGroup, ButtonVariants as _};
use gpui::component::{ActiveTheme as _, Disableable as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    Context, Entity, IntoElement, ParentElement as _, Render, Role, Styled as _, Subscription,
    Window, div, px,
};

use crate::daemon::{DaemonClient, agent_error_text};
use crate::jobs::failure_text;
use crate::launch::{AgentStartStatus, AgentStarter};
use crate::menu::{BeginRecording, PauseOrResumeRecording, ShowRecordingControls};
use crate::state::{AppEvent, AppState};

use super::{assets, brand, format, icons, kit, text_line};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Snapshot {
    connected: bool,
    connecting: bool,
    incompatible_agent: Option<Option<u32>>,
    active: Option<(String, RecordingStatus)>,
    saving: Option<(String, String)>,
    automatic_save: Option<u64>,
    access: broscribe_ipc::CaptureAccess,
}

impl Snapshot {
    fn read(daemon: &DaemonClient) -> Self {
        let panel = &daemon.panel;
        let saving = panel
            .stopped
            .iter()
            .find(|recording| recording.state.is_pending_save())
            .map(|recording| {
                let text = match recording.state {
                    StoppedRecordingState::Queued { .. } => "Queued".to_owned(),
                    StoppedRecordingState::Running { fraction } => {
                        fraction.and_then(format::progress_percent).map_or_else(
                            || "Transcribing".into(),
                            |percent| format!("Transcribing · {percent}"),
                        )
                    }
                    StoppedRecordingState::Retrying { .. } => "Waiting to finish saving".into(),
                    StoppedRecordingState::Failed { .. } => "Couldn't save recording".into(),
                    StoppedRecordingState::Discarding => {
                        unreachable!("excluded by is_pending_save")
                    }
                };
                (recording.session_id.clone(), text)
            });
        Self {
            connected: daemon.connected,
            connecting: daemon.connecting,
            incompatible_agent: daemon.incompatible_agent,
            access: panel.access,
            active: panel
                .recording
                .as_ref()
                .map(|r| (r.session_id.clone(), r.status)),
            saving,
            automatic_save: panel
                .recording
                .as_ref()
                .and_then(|r| Some(r.auto_stop.as_ref()?.pending.as_ref()?.remaining_s)),
        }
    }

    fn session(&self) -> Option<&str> {
        self.active.as_ref().map(|(id, _)| id.as_str())
    }
}

pub(super) struct RecordControl {
    state: Entity<AppState>,
    daemon: Option<Entity<DaemonClient>>,
    agent: Option<Entity<AgentStarter>>,
    agent_status: AgentStartStatus,
    snapshot: Snapshot,
    pending: bool,
    failure: Option<String>,
    start_refusal: Option<crate::daemon::StartRefusal>,
    _update_status: gpui::Subscription,
    _subscriptions: Vec<Subscription>,
}

impl RecordControl {
    pub(super) fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let update_status = crate::views::update::UpdateNotices::observe_hold(cx);
        let daemon = state.read(cx).daemon().cloned();
        let agent = state.read(cx).agent_starter();
        let mut subscriptions = Vec::new();
        let mut snapshot = Snapshot::default();
        if let Some(daemon) = &daemon {
            snapshot = Snapshot::read(daemon.read(cx));
            subscriptions.push(cx.observe(daemon, |this, daemon, cx| {
                let snapshot = Snapshot::read(daemon.read(cx));
                if snapshot != this.snapshot {
                    if snapshot.session() != this.snapshot.session() {
                        this.failure = None;
                        this.start_refusal = None;
                    }
                    this.snapshot = snapshot;
                    cx.notify();
                }
            }));
        }
        let mut agent_status = AgentStartStatus::Idle;
        if let Some(agent) = &agent {
            agent_status = agent.read(cx).status().clone();
            subscriptions.push(cx.observe(agent, |this, agent, cx| {
                let status = agent.read(cx).status();
                if !this.snapshot.connected && this.agent_status != *status {
                    this.agent_status = status.clone();
                    cx.notify();
                }
            }));
        }
        Self {
            state,
            daemon,
            agent,
            agent_status,
            snapshot,
            pending: false,
            failure: None,
            start_refusal: None,
            _update_status: update_status,
            _subscriptions: subscriptions,
        }
    }

    pub(super) fn in_capture(&self) -> bool {
        self.snapshot.active.is_some()
    }

    #[cfg(test)]
    pub(super) fn mark_connected(&mut self, cx: &mut Context<Self>) {
        self.snapshot.connected = true;
        cx.notify();
    }

    fn request(&mut self, call: Call, session: Option<&str>, cx: &mut Context<Self>) {
        if matches!(call, Call::RecordStart(_)) && crate::menu::update_waiting(cx) {
            return;
        }
        let Some(daemon) = &self.daemon else { return };
        let current = Snapshot::read(daemon.read(cx));
        if self.pending || !current.connected || current.session() != session {
            return;
        }
        let start = session.is_none();
        if start && daemon.read(cx).record_start_pending() {
            return;
        }
        let prefix = match &call {
            Call::RecordStart(_) => "Couldn't start recording.",
            Call::RecordPause { .. } => "Couldn't pause recording.",
            Call::RecordResume { .. } => "Couldn't resume recording.",
            _ => "Couldn't stop and save recording.",
        };
        let task = daemon.read(cx).request(call, cx);
        let session = session.map(str::to_owned);
        self.pending = true;
        self.failure = None;
        self.start_refusal = None;
        cx.notify();
        if start {
            self.state
                .update(cx, |_, cx| cx.emit(AppEvent::OpenRecordingControls));
        }
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.pending = false;
                if this.snapshot.session() == session.as_deref()
                    && let Err(error) = result
                {
                    if start {
                        let refusal = crate::daemon::start_refusal(
                            &error,
                            crate::ui::setup_window::capture_active(cx),
                        );
                        this.failure = Some(refusal.words.clone());
                        this.start_refusal = Some(refusal);
                    } else {
                        this.failure = Some(failure_text(prefix, &agent_error_text(&error), ""));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

fn open_controls(button: Button) -> Button {
    button.on_click(|_, window, cx| {
        window.dispatch_action(Box::new(ShowRecordingControls), cx);
    })
}

impl Render for RecordControl {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut layout = div().flex().flex_wrap().items_center().gap_1().flex_none();
        let feedback = if let Some(text) = &self.failure {
            Some((kit::StatusKind::Failed, text.clone()))
        } else if self.pending {
            let message = if self.snapshot.active.is_none() {
                "Starting recording…"
            } else {
                "Sending recording command…"
            };
            Some((kit::StatusKind::Pending, message.into()))
        } else {
            None
        };
        if !self.snapshot.connected {
            let status = self
                .agent
                .as_ref()
                .map_or(&self.agent_status, |agent| agent.read(cx).status());
            let status = self
                .daemon
                .as_ref()
                .and_then(|daemon| AgentStartStatus::unavailable(daemon.read(cx), status))
                .unwrap_or_else(|| status.clone());
            let text = status.message();
            let is_failure = status.is_failure();
            if let Some(action) = status.recovery() {
                layout = layout.child(
                    Button::new("toolbar-start-recorder")
                        .outline()
                        .label(action.label(false))
                        .disabled(self.agent.is_none())
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(agent) = &this.agent {
                                agent.update(cx, |agent, cx| {
                                    agent.recover(cx);
                                });
                            }
                        })),
                );
            }
            if status.copy_details().is_some() {
                layout = layout.child(
                    Button::new("toolbar-agent-details")
                        .ghost()
                        .label("Copy details")
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(agent) = &this.agent {
                                agent.update(cx, |agent, cx| agent.copy_recovery_details(cx));
                            }
                        })),
                );
            }
            if let Some(text) = text {
                layout = layout.child(kit::status_without_wait_clock(
                    "toolbar-agent-status",
                    if is_failure {
                        kit::StatusKind::Failed
                    } else if status.is_pending() {
                        kit::StatusKind::Pending
                    } else {
                        kit::StatusKind::Attention
                    },
                    text,
                    cx,
                ));
            }
        } else if let Some((session, status)) = &self.snapshot.active {
            let session = session.clone();
            let paused = *status == RecordingStatus::Paused;
            let label = if paused { "Paused" } else { "Recording" };
            let mark = if paused {
                kit::glyph(assets::STATUS_PAUSED, cx.theme().muted_foreground).into_any_element()
            } else {
                div()
                    .size(px(8.))
                    .rounded_full()
                    .bg(brand::brand(cx).recording)
                    .into_any_element()
            };
            let toggle_session = session.clone();
            let toggle = if paused { "Resume" } else { "Pause" };
            // Notification and tray commands use the same daemon methods.
            layout = layout.child(
                ButtonGroup::new("toolbar-active-recording")
                    .child(open_controls(
                        Button::new("toolbar-recording")
                            .ghost()
                            .accessibility_label(format!("{label} · show recording controls"))
                            .child(mark)
                            .child(text_line("toolbar-capture-status", Role::Status, label)),
                    ))
                    .child(
                        Button::new(if paused {
                            "toolbar-resume"
                        } else {
                            "toolbar-pause"
                        })
                        .icon(if paused { icons::PLAY } else { icons::PAUSE })
                        .label(toggle)
                        .tooltip_with_action(
                            format!("{toggle} recording"),
                            &PauseOrResumeRecording,
                            None,
                        )
                        .disabled(self.pending)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let session_id = toggle_session.clone();
                            let call = if paused {
                                Call::RecordResume { session_id }
                            } else {
                                Call::RecordPause { session_id }
                            };
                            this.request(call, Some(&toggle_session), cx);
                        })),
                    )
                    .child(
                        Button::new("toolbar-stop")
                            .icon(icons::STOP)
                            .label("Stop")
                            .accessibility_label("Stop and save recording")
                            .tooltip("Stop and save recording")
                            .disabled(self.pending)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.request(
                                    Call::RecordStop {
                                        session_id: session.clone(),
                                    },
                                    Some(&session),
                                    cx,
                                );
                            })),
                    ),
            );
        } else {
            let held = self.pending || crate::menu::update_waiting(cx);
            let words = if self.pending {
                "Starting…"
            } else {
                "Start recording"
            };
            // The record dot sits before the words, so they are children
            // rather than the label (which the button draws first).
            layout = layout.child(
                Button::new("toolbar-start")
                    .map(|button| kit::primary(button, held, cx))
                    .accessibility_label(words)
                    .child(kit::record_dot(true, held, cx))
                    .child(words)
                    .tooltip_with_action("Start recording", &BeginRecording, None)
                    .disabled(held)
                    .when(crate::menu::update_waiting(cx), |button| {
                        button.tooltip(crate::views::update::RECORDING_HELD)
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.request(Call::RecordStart(Default::default()), None, cx);
                    })),
            );
        }
        let saving = self
            .snapshot
            .automatic_save
            .map(|seconds| format!("Saving in {seconds} s"))
            .or_else(|| self.snapshot.saving.as_ref().map(|(_, text)| text.clone()));
        if self.snapshot.connected
            && let Some(saving) = saving
        {
            layout = layout.child(open_controls(
                Button::new("toolbar-saving")
                    .ghost()
                    .accessibility_label(format!("{saving} · show recording controls"))
                    .child(text_line("toolbar-saving-status", Role::Status, saving)),
            ));
        }
        if let Some((kind, text)) = feedback {
            layout = layout.child(kit::status_without_wait_clock(
                "toolbar-record-status",
                kind,
                text,
                cx,
            ));
        }
        layout = layout.children(self.start_refusal.as_ref().and_then(|refusal| {
            kit::start_recovery_button("toolbar-start-recovery", refusal, &self.state)
        }));
        if self.snapshot.connected && self.snapshot.active.is_none() {
            layout = layout.children(kit::capture_access_warnings(
                self.snapshot.access,
                None,
                &self.state,
                cx,
            ));
        }
        layout
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use broscribe_core::{Library, config::Config};
    use broscribe_ipc::{IpcError, RecordingState, StoppedRecording};
    use broscribe_platform::fakes::FakePaths;
    use gpui::{AppContext as _, TestAppContext, VisualTestContext};
    use serde_json::{Value, json};

    use crate::daemon::{EventStream, Transport};

    #[derive(Default)]
    struct Recorder(Mutex<Vec<Call>>);

    impl Transport for Recorder {
        fn subscribe(&self) -> Result<Box<dyn EventStream>, IpcError> {
            unreachable!("detached fake")
        }

        fn call(&self, call: Call) -> Result<Value, IpcError> {
            let is_start = matches!(call, Call::RecordStart(_));
            self.0.lock().unwrap().push(call);
            if is_start {
                Err(IpcError::new("busy", "Synthetic recording is busy."))
            } else {
                Ok(json!({}))
            }
        }
    }

    fn recording(id: &str, status: &str) -> RecordingState {
        serde_json::from_value(json!({
            "session_id": id, "status": status, "elapsed_s": 0,
            "started_at": "2026-11-23T00:00:00Z", "health": broscribe_ipc::CaptureHealth::starting(),
        }))
        .unwrap()
    }

    #[gpui::test]
    fn toolbar_recording_is_single_flight_session_bound_and_event_driven(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let status = r#"{"installed":false}"#;
        let cli = crate::test_support::write_cli(
            &dir.path().join("synthetic-cli"),
            &format!("echo '{status}'"),
            &format!("fx_echo {}", crate::test_support::ps_str(status)),
        );
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    broscribe_platform::resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Config::default(),
                    Library::open(dir.path().join("library")).unwrap(),
                    cli,
                ),
                cx,
            )
        });
        let transport = Arc::new(Recorder::default());
        let daemon = cx.update(|cx| DaemonClient::detached(transport.clone(), cx));
        let agent = cx.new(|cx| AgentStarter::new(state.clone(), daemon.clone(), cx));
        state.update(cx, |state, cx| {
            state.attach_daemon(daemon.clone(), cx);
            state.attach_agent_starter(&agent);
        });
        cx.update(gpui::init);
        let window = cx.add_window(|_, cx| RecordControl::new(state.clone(), cx));
        let control = window.root(cx).unwrap();
        let inspect = |cx: &mut TestAppContext, id: &str, expected: &str| {
            VisualTestContext::from_window(window.into(), cx).update(|window, cx| {
                let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
                assert_eq!(tree.nodes[tree.find(id)].name, expected, "{id}");
            });
        };
        inspect(cx, "toolbar-agent-status", "Connecting to BroScribe Agent…");
        daemon.update(cx, |daemon, cx| daemon.lost(None, cx));
        inspect(cx, "toolbar-start-recorder", "Start BroScribe Agent");
        agent.update(cx, |agent, cx| assert!(agent.start(cx)));
        cx.run_until_parked();
        assert_eq!(
            agent.read_with(cx, |agent, _| agent.status().clone()),
            AgentStartStatus::NotInstalled
        );
        inspect(
            cx,
            "toolbar-agent-status",
            &AgentStartStatus::NotInstalled.message().unwrap(),
        );
        inspect(cx, "toolbar-start-recorder", "Set up BroScribe Agent");

        daemon.update(cx, |daemon, cx| {
            daemon.connected = true;
            cx.notify();
        });
        inspect(cx, "toolbar-start", "Start recording");
        // A held update suppresses the capture request before it reaches the daemon.
        use crate::views::update::{UpdateNotices, parse_status};
        let set_update = |cx: &mut TestAppContext, held: bool| {
            cx.update(|cx| {
                let install = if held {
                    json!({"fence": 4, "durable": true})
                } else {
                    serde_json::Value::Null
                };
                let snapshot = parse_status(
                    &json!({"update": {"state": "idle"}, "agent": {"install": install}}),
                );
                UpdateNotices::global(cx)
                    .update(cx, |notices, cx| notices.set_update_state(&snapshot, cx));
            })
        };
        set_update(cx, true);
        control.update(cx, |control, cx| {
            control.request(Call::RecordStart(Default::default()), None, cx);
            assert!(!control.pending);
        });
        assert!(transport.0.lock().unwrap().is_empty());
        set_update(cx, false);
        control.update(cx, |control, cx| {
            control.request(Call::RecordStart(Default::default()), None, cx);
            control.request(Call::RecordStart(Default::default()), None, cx);
            assert!(control.pending);
        });
        inspect(cx, "toolbar-start", "Starting…");
        cx.run_until_parked();
        assert_eq!(transport.0.lock().unwrap().len(), 1);
        assert!(control.read_with(cx, |control, _| {
            control
                .failure
                .as_ref()
                .unwrap()
                .starts_with("Couldn't start recording.")
        }));

        let observed = std::rc::Rc::new(std::cell::Cell::new(0));
        let _watch = cx.update(|cx| {
            cx.observe(&control, {
                let observed = observed.clone();
                move |_, _| observed.set(observed.get() + 1)
            })
        });
        daemon.update(cx, |daemon, cx| {
            daemon.panel.recording = Some(recording("synthetic-a", "recording"));
            cx.notify();
        });
        inspect(cx, "toolbar-capture-status", "Recording");
        inspect(cx, "toolbar-pause", "Pause");
        assert_eq!(observed.get(), 1);
        daemon.update(cx, |daemon, cx| {
            let state = daemon.panel.recording.as_mut().unwrap();
            state.elapsed_s = 1.;
            state.title = Some("Synthetic meeting".into());
            daemon.panel.meters = [-30, -50];
            cx.notify();
        });
        assert_eq!(observed.get(), 1);
        control.update(cx, |control, cx| {
            control.request(
                Call::RecordPause {
                    session_id: "synthetic-a".into(),
                },
                Some("synthetic-a"),
                cx,
            );
        });
        cx.run_until_parked();
        daemon.update(cx, |daemon, cx| {
            daemon.panel.recording = Some(recording("synthetic-b", "paused"));
            cx.notify();
        });
        inspect(cx, "toolbar-capture-status", "Paused");
        inspect(cx, "toolbar-resume", "Resume");
        control.update(cx, |control, cx| {
            control.request(
                Call::RecordStop {
                    session_id: "synthetic-a".into(),
                },
                Some("synthetic-a"),
                cx,
            );
            control.request(
                Call::RecordResume {
                    session_id: "synthetic-b".into(),
                },
                Some("synthetic-b"),
                cx,
            );
        });
        cx.run_until_parked();
        control.update(cx, |control, cx| {
            control.request(
                Call::RecordStop {
                    session_id: "synthetic-b".into(),
                },
                Some("synthetic-b"),
                cx,
            );
        });
        cx.run_until_parked();
        assert!(matches!(&transport.0.lock().unwrap()[..], [
            Call::RecordStart(_),
            Call::RecordPause { session_id: a },
            Call::RecordResume { session_id: b },
            Call::RecordStop { session_id: c },
        ] if a == "synthetic-a" && b == "synthetic-b" && c == "synthetic-b"));

        daemon.update(cx, |daemon, cx| {
            daemon.panel.recording = None;
            daemon.panel.stopped.push(StoppedRecording {
                session_id: "synthetic-b".into(),
                title: None,
                tags: vec![],
                stopped_at: chrono::Utc::now(),
                stop_reason: None,
                state: StoppedRecordingState::Running {
                    fraction: Some(0.42),
                },
            });
            cx.notify();
        });
        inspect(cx, "toolbar-saving-status", "Transcribing · 42%");
    }
}
