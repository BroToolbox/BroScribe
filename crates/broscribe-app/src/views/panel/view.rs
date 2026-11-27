//! The recording panel's view: its controls, the live transcript list and
//! rendering (the module docs of [`super`] describe the behaviour).

use super::*;
use crate::launch::AgentStartStatus;
use crate::state::{AppEvent, SettingsPage};
use crate::ui::Spoken as _;
use crate::ui::type_scale::{TextRole, TextRoleExt as _};
use broscribe_ipc::JournalResolution;
use broscribe_ipc::recording_words::{
    JOURNAL_LIBRARY_UNKNOWN, UNTITLED_MEETING, journal_library_issue, meeting_title,
    stop_reason_text,
};
use gpui::Focusable as _;
use gpui::base::ElementExt as _;
use gpui::component::Sizable as _;
use gpui::component::WindowExt as _;
use gpui::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui::component::scroll::ScrollableElement as _;
use gpui::component::switch::Switch;

struct LiveRowAction {
    action: LiveAction,
    id: &'static str,
    label: &'static str,
    shortcut: &'static str,
    spoken: String,
}

fn transcript_empty(connected: bool, preview: &PreviewState) -> String {
    if !connected {
        return "Live transcript paused while BroScribe Agent is disconnected. The recording is unaffected.".into();
    }
    match preview {
        PreviewState::Off => "Live transcript is off for this recording. The full transcript is made after the meeting.".into(),
        _ => preview_notice(preview).unwrap_or_else(|| "Listening. Lines appear a few seconds after someone speaks.".into()),
    }
}

fn discard_spec(active: bool) -> kit::ConfirmSpec {
    use kit::confirm_line as line;
    let mut spec = kit::ConfirmSpec::new(
        line("discard-confirmation", "Discard this recording?"),
        "discard-body",
        "discard-footer",
        "discard-cancel",
        line("discard-confirm", "Discard recording"),
    );
    spec.warning = Some(if active {
        line("discard-warning", "Capture continues until you confirm.")
    } else {
        line("discard-processing", "Its processing is cancelled.")
    });
    spec.goes = vec![
        line("discard-audio", "The audio"),
        line(
            "discard-disclosure",
            "The live transcript and any draft transcript",
        ),
        line("discard-voice-samples", "Voice samples from this recording"),
    ];
    spec.stays = vec![line(
        "discard-people",
        "People you confirmed as participants, including ones added by name",
    )];
    spec
}

const PROCESS_NOW_EXPLANATION: &str =
    "Make a draft transcript of what's recorded so far. Recording continues.";

fn visor(
    face: kit::Face,
    state: impl Into<SharedString>,
    trailing: Option<(&'static str, AnyElement)>,
    context: Option<String>,
    cx: &App,
) -> Stateful<gpui::Div> {
    kit::visor(face, cx)
        .w_full()
        .id("recording-visor")
        .role(Role::Group)
        .aria_label("Recording state")
        .child(
            div()
                .flex()
                .flex_col()
                .gap_0p5()
                .min_w_0()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_x_1()
                        .child(text_line("visor-state", Role::Status, state))
                        .when_some(trailing, |row, (separator, value)| {
                            row.when(!separator.is_empty(), |row| row.child(separator))
                                .child(value)
                        }),
                )
                .when_some(context, |el, context| {
                    el.child(
                        text_line("visor-context", Role::Label, context.clone())
                            .text_role(TextRole::Caption)
                            .text_color(crate::ui::brand::brand(cx).visor_accent)
                            .truncate()
                            .tooltip(kit::title_tooltip(context)),
                    )
                }),
        )
}

fn recording_context(recording: &RecordingState) -> String {
    let app = recording
        .auto_stop
        .as_ref()
        .and_then(|status| status.app.as_ref())
        .map(|app| app.app_name.as_str())
        .or(recording.app.as_deref())
        .filter(|app| !app.trim().is_empty());
    let title = recording
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty());
    match (app, title) {
        (Some(app), Some(title)) => format!("{app} · {title}"),
        (Some(app), None) => app.to_owned(),
        (None, title) => meeting_title(title).to_owned(),
    }
}

const DISCONNECTED_CONTROL: &str = "Can’t reach BroScribe Agent";

fn disconnected_button(button: Button, connected: bool, cx: &App) -> Button {
    button.when(!connected, |button| {
        kit::disable_with_reason(button, true, DISCONNECTED_CONTROL, cx)
    })
}

fn saved_words(count: usize) -> String {
    if count == 0 {
        "Saved".into()
    } else {
        format!(
            "Saved · {count} {} to review",
            if count == 1 { "name" } else { "names" }
        )
    }
}

impl RecordingPanel {
    fn grace_pending(&self, cx: &App) -> bool {
        self.daemon.read(cx).connected
            && self
                .model(cx)
                .recording
                .as_ref()
                .and_then(|recording| recording.auto_stop.as_ref())
                .is_some_and(|status| status.pending.is_some())
    }

    pub(super) fn has_urgent_action(&self, cx: &App) -> bool {
        self.grace_pending(cx)
            || self
                .notifications
                .read(cx)
                .panel_notices()
                .any(|notice| notice.is_recording_prompt())
    }

    pub(super) fn new(
        daemon: Entity<DaemonClient>,
        notifications: Entity<NotificationHost>,
        state: Entity<AppState>,
        agent: Entity<AgentStarter>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let note =
            cx.new(|cx| InputState::new(window, cx).placeholder("Add a note and press Enter"));
        let tag = cx.new(|cx| InputState::new(window, cx).placeholder("Tags, comma-separated"));
        let picker = cx
            .new(|cx| PersonPicker::new_unfocused(state.clone(), "Add a participant", window, cx));
        let on_enter = |submit: fn(&mut Self, &mut Window, &mut Context<Self>)| {
            move |this: &mut Self,
                  _: &Entity<InputState>,
                  ev: &InputEvent,
                  window: &mut Window,
                  cx: &mut Context<Self>| match ev {
                InputEvent::PressEnter { .. } => submit(this, window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            }
        };
        let subscriptions = vec![
            crate::views::update::UpdateNotices::observe_hold(cx),
            cx.observe_in(&daemon, window, |this, _, window, cx| {
                this.daemon_notified(window, cx)
            }),
            cx.observe(&notifications, |_, _, cx| cx.notify()),
            cx.observe(&agent, |_, _, cx| cx.notify()),
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.daemon.read(cx).refresh_capture_access(cx);
                    this.agent.update(cx, |agent, cx| agent.refresh_service(cx));
                }
            }),
            cx.observe_in(&state, window, |this, _, window, cx| {
                let config = this.state.read(cx).config();
                let compact = config.panel.compact;
                let reminder = config.consent_reminder;
                let excluded = config.app.hide_controls_from_screen_sharing;
                if this.compact != compact {
                    this.compact = compact;
                    this.temporarily_expanded = false;
                    cx.notify();
                }
                if this.consent_reminder != reminder {
                    this.consent_reminder = reminder;
                    cx.notify();
                }
                if this.exclusion_setting != excluded {
                    this.exclusion_setting = excluded;
                    this.apply_capture_exclusion(excluded, window, cx);
                }
            }),
            cx.observe_window_visibility(window, |this, visibility, _, cx| {
                let visible = visibility.is_visible();
                if this.visible != visible {
                    this.visible = visible;
                    this.sync_ticker(cx);
                    if visible {
                        cx.notify();
                    }
                }
            }),
            cx.subscribe_in(&note, window, on_enter(Self::submit_note)),
            cx.subscribe_in(&tag, window, on_enter(Self::submit_tags)),
            cx.subscribe_in(
                &picker,
                window,
                |this, picker, ev: &PersonPickerEvent, window, cx| match ev {
                    PersonPickerEvent::Picked(choice) => {
                        this.confirm_participant(choice.clone(), window, cx)
                    }
                    PersonPickerEvent::Cancelled => {
                        this.selected_person = None;
                        this.roster_open = false;
                        picker.update(cx, |p, cx| p.set_query("", window, cx));
                        this.roster_focus.focus(window, cx);
                        cx.notify();
                    }
                },
            ),
        ];
        let transcript_view = {
            let panel = cx.entity().downgrade();
            cx.new(|_| TranscriptView::new(panel))
        };
        let profiles = state.read(cx).run_cli(
            crate::views::settings::args(["models", "list"]),
            crate::jobs::ChildKind::Command,
            cx,
        );
        cx.spawn(async move |panel, cx| {
            if let Ok(value) = profiles.await {
                let _ = panel.update(cx, |panel, cx| {
                    panel.language_profiles =
                        crate::views::settings::parse_models_list(&value).profiles;
                    cx.notify();
                });
            }
        })
        .detach();
        let details_scroll = gpui::ScrollHandle::new();
        let exclusion_setting = state
            .read(cx)
            .config()
            .app
            .hide_controls_from_screen_sharing;
        let mut this = Self {
            tab: RecordingPanelTab::default(),
            compact: state.read(cx).config().panel.compact,
            compact_saving: false,
            compact_error: None,
            temporarily_expanded: false,
            compact_height: None,
            expanded_height: window.viewport_size().height,
            compact_focus: cx.focus_handle(),
            other_recordings_anchor: gpui::ScrollAnchor::for_handle(details_scroll.clone()),
            notice_anchor: gpui::ScrollAnchor::for_handle(details_scroll.clone()),
            details_scroll,
            header_scroll: gpui::ScrollHandle::new(),
            first_control: cx.focus_handle(),
            urgent_control: cx.focus_handle(),
            following_visible: true,
            start_refusal: None,
            stop_reason_dismissed: None,
            notice_target: None,
            notice_focus: cx.focus_handle(),
            consent_dismissed: None,
            agent,
            language_profiles: Vec::new(),
            titles: BTreeMap::new(),
            note,
            tag,
            picker,
            roster_open: false,
            roster_focus: cx.focus_handle(),
            _subscriptions: subscriptions,
            consent_reminder: state.read(cx).config().consent_reminder,
            daemon,
            notifications,
            state,
            confirmed: BTreeMap::new(),
            selected_person: None,
            roster_key: (None, Vec::new()),
            controls: BTreeMap::new(),
            generation: 0,
            session_key: None,
            tags: None,
            snapshot_tags: None,
            tag_snapshots: 0,
            discard_confirm: None,
            discard_modal: false,
            more_open: false,
            more_focus: cx.focus_handle(),
            delete: None,
            live_naming: None,
            live_status_turn: None,
            live_revision: 0,
            transcript: Self::following_list(cx),
            transcript_focus: cx.focus_handle(),
            transcript_page: Default::default(),
            transcript_selected: None,
            transcript_hovered: None,
            transcript_menu: None,
            transcript_marks: VecDeque::new(),
            transcript_mark_number: 0,
            transcript_view,
            transcript_rows: Vec::new(),
            transcript_key: None,
            naming_followed: None,
            capture_ended: None,
            live_reviews: BTreeMap::new(),
            seen_revision: None,
            visible: window.is_visible(),
            _overlay: None,
            platform_overlay: None,
            exclusion_setting,
            exclusion_applied: None,
            exclusion_pending: false,
            exclusion_error: None,
            overlay_warning: None,
            overlay_note: None,
            ticker: None,
            #[cfg(test)]
            renders: 0,
        };
        let focus = this.transcript_focus.clone();
        this._subscriptions.extend([
            cx.on_focus(&focus, window, |this, _, cx| {
                this.refresh_transcript_view(cx)
            }),
            cx.on_focus_out(&focus, window, |this, _, _, cx| {
                this.refresh_transcript_view(cx)
            }),
        ]);
        // Any change of the panel (an answer, a closed confirmation, a
        // cleared draft) may change the transcript's rows, and may end the
        // user's use of it after the capture.
        let panel = cx.entity();
        this._subscriptions
            .push(cx.observe_in(&panel, window, |this, _, window, cx| {
                this.sync_transcript(cx);
                this.close_if_capture_ended(window, cx)
            }));
        this.daemon_changed(window, cx);
        this.sync_transcript(cx);
        this
    }

    pub(super) fn model<'a>(&self, cx: &'a App) -> &'a PanelModel {
        &self.daemon.read(cx).panel
    }

    pub(super) fn set_platform_overlay(
        &mut self,
        overlay: Rc<dyn RecordingOverlay>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.exclusion_applied = overlay.capture_exclusion_supported().then_some(false);
        self.platform_overlay = Some(overlay);
        if self.exclusion_setting {
            self.apply_capture_exclusion(true, window, cx);
        }
    }

    fn apply_capture_exclusion(
        &mut self,
        excluded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(overlay) = &self.platform_overlay else {
            return false;
        };
        if self.exclusion_applied == Some(excluded) {
            return true;
        }
        if self.exclusion_applied.is_none() {
            return false;
        }
        match overlay.set_capture_excluded(&*window, excluded) {
            Ok(CaptureExclusion::Applied) => {
                self.exclusion_applied = Some(excluded);
                self.exclusion_error = None;
            }
            Ok(CaptureExclusion::Unsupported) => self.exclusion_applied = None,
            Err(error) => {
                tracing::warn!(code = error.code(), "setting screen-sharing exclusion");
                self.exclusion_error = Some(if excluded {
                    "Couldn’t hide Recording controls from screen sharing. The window may be visible in the share."
                } else {
                    "Couldn’t turn off screen-sharing exclusion. Recording controls are still hidden in supported shares."
                }.into());
            }
        }
        cx.notify();
        self.exclusion_applied == Some(excluded)
    }

    fn change_capture_exclusion(
        &mut self,
        excluded: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.exclusion_pending || !self.apply_capture_exclusion(excluded, window, cx) {
            return;
        }
        self.exclusion_pending = true;
        let save = crate::views::settings::update_config(
            &self.state,
            move |config| {
                config.app.hide_controls_from_screen_sharing = excluded;
            },
            cx,
        );
        cx.spawn(async move |panel, cx| {
            let saved = save.await;
            let _ = panel.update(cx, |panel, cx| {
                panel.exclusion_pending = false;
                panel.exclusion_error = saved.err();
                cx.notify();
            });
        })
        .detach();
    }

    fn capture_exclusion_section(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut section = div().flex().flex_col().gap_1();
        if let Some(excluded) = self.exclusion_applied {
            section = section
                .child(Switch::new("hide-from-sharing")
                    .label("Hide this window from screen sharing")
                    .checked(excluded)
                    .disabled(self.exclusion_pending)
                    .on_change(cx.listener(|this, checked, window, cx| this.change_capture_exclusion(*checked, window, cx))))
                .child(text_line("capture-exclusion-caption", Role::Label,
                    "When on, this window is hidden in supported screen shares and screenshots.")
                    .text_role(TextRole::Caption).text_color(cx.theme().muted_foreground));
        } else {
            section = section.child(text_line(
                "capture-exclusion",
                Role::Status,
                "This system can’t hide windows from screen sharing.",
            ));
        }
        section.children(self.exclusion_error.clone().map(|error| {
            kit::status_line(
                "capture-exclusion-error",
                kit::StatusKind::Attention,
                error,
                cx,
            )
        }))
    }

    pub(super) fn compact_layout(&self) -> bool {
        self.compact && !self.temporarily_expanded
    }

    fn compact_button(&self, label: &'static str, cx: &Context<Self>) -> Button {
        Button::new("panel-compact")
            .label(label)
            .small()
            .ghost()
            .track_focus(&self.compact_focus)
            .disabled(self.compact_saving)
            .on_click(cx.listener(|this, _, window, cx| {
                this.change_compact_mode(window, cx);
            }))
    }

    fn change_compact_mode(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let compact = !self.compact_layout();
        self.compact_focus.focus(window, cx);
        if compact == self.compact {
            self.temporarily_expanded = false;
            cx.notify();
            return;
        }
        self.compact_saving = true;
        self.compact_error = None;
        let save = crate::views::settings::update_config(
            &self.state,
            move |config| config.panel.compact = compact,
            cx,
        );
        cx.spawn(async move |panel, cx| {
            let result = save.await;
            let _ = panel.update(cx, |panel, cx| {
                panel.compact_saving = false;
                panel.compact_error = result.err();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn size_for_layout(&mut self, height: Pixels, window: &mut Window, cx: &App) {
        let viewport = window.viewport_size();
        let minimum = px(crate::ui_state::PANEL_MIN.1);
        let target = if self.compact_layout() {
            if self.compact_height.is_none() {
                self.expanded_height = viewport.height.max(minimum);
            }
            let available = window.display(cx).map_or(px(PANEL_SIZE.1), |display| {
                // Leave room for server-side decorations.
                display.visible_bounds().size.height - px(40.)
            });
            let target = height
                .min(available)
                .min(self.expanded_height)
                .max(px(160.))
                .ceil();
            if self.compact_height.replace(target) == Some(target) {
                return;
            }
            target
        } else if self.compact_height.take().is_some() {
            self.expanded_height
        } else if viewport.height < minimum {
            minimum
        } else {
            return;
        };
        // An earlier resize can still be in flight when the layout changes back.
        window.resize(size(viewport.width, target));
    }

    fn return_overlay_focus(&self, window: &Window) -> bool {
        self.platform_overlay.as_ref().is_some_and(|overlay| {
            overlay.return_user_focus(window).unwrap_or_else(|error| {
                tracing::warn!(
                    code = error.code(),
                    "returning focus from recording controls"
                );
                false
            })
        })
    }

    /// A transcript list that follows the newest line.
    fn following_list(cx: &Context<Self>) -> ListState {
        // Every row is measured once, not only those in view: rows that
        // arrive while the user reads earlier lines count in the scroll
        // extent. Measured rows are kept; only new or changed rows are
        // measured again (see `sync_rows`).
        let list = ListState::new(0, ListAlignment::Top, px(TRANSCRIPT_OVERDRAW)).measure_all();
        list.set_follow_mode(FollowMode::Tail);
        let panel = cx.weak_entity();
        list.set_scroll_handler(move |_, _, cx| {
            let panel = panel.clone();
            cx.defer(move |cx| {
                let _ = panel.update(cx, |panel, cx| {
                    let following = panel.transcript.is_following_tail();
                    if panel.following_visible != following {
                        panel.following_visible = following;
                        cx.notify();
                    }
                });
            });
        });
        list
    }

    /// The user asked for the panel: after the capture it showed ended, it
    /// stays open until they close it.
    pub fn keep_open(&mut self) {
        self.capture_ended = None;
    }

    /// Closes the panel once the capture it showed has ended (the agent
    /// lists it as stopped or saved, never a disconnect or an unknown state)
    /// and the user is not using the panel. Final processing, notifications
    /// and recovery stay with the agent and the app; the menu or a
    /// notification opens the panel again.
    fn close_if_capture_ended(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ended) = &self.capture_ended else {
            return;
        };
        let model = self.model(cx);
        let listed = model.active_session().is_none()
            && (model.stopped.iter().any(|s| s.session_id == ended.session)
                || model.saved.iter().any(|s| s.session_id == ended.session));
        if !listed || self.in_use(ended.generation, cx) {
            return;
        }
        self.capture_ended = None;
        window.remove_window();
    }

    /// Whether the user is still using the panel: a request in flight, a
    /// control used since generation `since`, a failed rename, discard or
    /// retry to read, an open confirmation, an unsaved title, or a
    /// notification shown here.
    fn in_use(&self, since: u64, cx: &App) -> bool {
        let controls = self.controls.iter().any(|(key, state)| {
            state.status == ControlStatus::Pending
                || state.generation > since
                || (matches!(
                    key.control,
                    Control::Title | Control::Discard | Control::Retry
                ) && matches!(state.status, ControlStatus::Failed(_)))
        });
        let titles = self.titles.values().any(|editor| {
            editor.input.read(cx).value().trim() != editor.seen.as_deref().unwrap_or("")
        });
        controls
            || titles
            || self.has_unsent_drafts(cx)
            || self.discard_confirm.is_some()
            || self.delete.is_some()
            || self.notifications.read(cx).panel_notices().next().is_some()
    }

    fn has_unsent_drafts(&self, cx: &App) -> bool {
        !self.note.read(cx).value().trim().is_empty()
            || !self.tag.read(cx).value().trim().is_empty()
            || !self.picker.read(cx).query().trim().is_empty()
    }

    pub(super) fn clear_unsent_drafts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.note
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.tag
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.picker
            .update(cx, |picker, cx| picker.set_query("", window, cx));
        self.close_if_capture_ended(window, cx);
        cx.notify();
    }

    // ---- the live transcript list ----

    /// The rows the transcript shows now: the live turns and the preview
    /// gaps, or the track-only lines while there are no turns.
    fn transcript_rows_now(&self, cx: &App) -> Vec<TranscriptRow> {
        let model = self.model(cx);
        let naming = self.live_naming.as_ref().map(|n| n.shown.turn_id.as_str());
        let mut rows: Vec<_> = model
            .turns
            .iter()
            .map(|turn| {
                (
                    turn.start_s,
                    TranscriptRow {
                        key: RowKey::Turn(turn.turn_id.clone()),
                        revision: turn.revision,
                        naming: naming == Some(turn.turn_id.as_str()),
                        selected: false,
                        hovered: false,
                        status: self
                            .controls
                            .get(&ControlKey::live(&turn.session_id, &turn.turn_id))
                            .map(|s| s.status.clone()),
                    },
                )
            })
            .chain(
                model
                    .numbered_lines()
                    .filter(|(_, line)| {
                        model.turns.is_empty() || matches!(line, PreviewLine::Gap { .. })
                    })
                    .map(|(number, line)| {
                        (line.at_s(), TranscriptRow::plain(RowKey::Line(number)))
                    }),
            )
            .chain(
                self.transcript_marks
                    .iter()
                    .map(|mark| (mark.at_s, TranscriptRow::plain(RowKey::Mark(mark.number)))),
            )
            .collect();
        rows.sort_by(|(a, _), (b, _)| a.total_cmp(b));
        rows.into_iter()
            .map(|(_, mut row)| {
                row.selected = self.transcript_selected.as_ref() == Some(&row.key);
                row.hovered = self.transcript_hovered.as_ref() == Some(&row.key);
                row
            })
            .collect()
    }

    /// Brings the transcript list up to date after the panel changed (never
    /// while drawing), only when its rows, the open picker or a live status
    /// changed; then the cached transcript view draws again. Anything else
    /// that redraws the panel (meters, the clock, typing elsewhere) reuses
    /// the transcript as it was drawn. Once the naming picker closed, it
    /// follows the newest line again if it did when the picker opened.
    fn sync_transcript(&mut self, cx: &mut Context<Self>) {
        let key = (
            self.model(cx).rows_revision(),
            self.live_naming.as_ref().map(|n| n.shown.turn_id.clone()),
            self.live_revision,
        );
        let mut changed = false;
        if self.transcript_key.as_ref() != Some(&key) {
            let rows = self.transcript_rows_now(cx);
            if rows != self.transcript_rows {
                sync_rows(&self.transcript, &self.transcript_rows, &rows);
                self.transcript_rows = rows;
            }
            self.transcript_key = Some(key);
            changed = true;
        }
        if self.live_naming.is_none()
            && let Some(followed) = self.naming_followed.take()
        {
            let top = self.transcript.logical_scroll_top();
            self.transcript.set_follow_mode(FollowMode::Tail);
            if !followed {
                // The user was reading earlier lines: the view stays, and
                // follows again once they scroll back to the bottom.
                self.transcript.scroll_to(top);
            }
            changed = true;
        }
        if changed {
            self.refresh_transcript_view(cx);
        }
    }

    fn refresh_transcript_view(&self, cx: &mut Context<Self>) {
        self.transcript_view.update(cx, |_, cx| cx.notify());
    }

    fn show_latest(&mut self, cx: &mut Context<Self>) {
        self.transcript.set_follow_mode(FollowMode::Tail);
        self.transcript.scroll_to_end();
        self.following_visible = true;
        self.refresh_transcript_view(cx);
        cx.notify();
    }

    fn transcript_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.transcript_focus.is_focused(window) || self.live_naming.is_some() {
            return;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let menu = matches!(key, "menu" | "contextmenu") && !modifiers.modified()
            || key == "f10"
                && modifiers.shift
                && !(modifiers.alt || modifiers.control || modifiers.platform);
        if (!menu && modifiers.modified()) || self.transcript_rows.is_empty() {
            return;
        }
        let last = self.transcript_rows.len() - 1;
        let selected = self.transcript_rows.iter().position(|row| row.selected);
        let current =
            selected.unwrap_or_else(|| self.transcript.logical_scroll_top().item_ix.min(last));
        if menu {
            if let RowKey::Turn(id) = self.transcript_rows[current].key.clone() {
                self.open_transcript_menu(id, window, cx);
            }
            cx.stop_propagation();
            return;
        }
        let destination = match key {
            "up" => selected.map_or(current, |ix| ix.saturating_sub(1)),
            "down" => selected.map_or(current, |ix| (ix + 1).min(last)),
            "home" => 0,
            "end" => last,
            "pageup" | "pagedown" => {
                let page = self.transcript_page.get();
                self.transcript
                    .scroll_by(if key == "pageup" { -page } else { page });
                self.refresh_transcript_view(cx);
                cx.stop_propagation();
                return;
            }
            "enter" | "n" | "backspace" | "delete" => {
                if let RowKey::Turn(id) = self.transcript_rows[current].key.clone() {
                    let action = match key {
                        "backspace" | "delete" => LiveAction::Clear,
                        "enter"
                            if self.model(cx).turn(&id).is_some_and(|t| {
                                t.status == LiveTurnStatus::Suggested && t.suggestion.is_some()
                            }) =>
                        {
                            LiveAction::Confirm
                        }
                        _ => LiveAction::Name,
                    };
                    self.run_live_row_action(&id, action, window, cx);
                }
                cx.stop_propagation();
                return;
            }
            _ => return,
        };
        self.transcript_selected = Some(self.transcript_rows[destination].key.clone());
        self.live_revision += 1;
        if key == "end" {
            self.show_latest(cx);
        } else {
            if destination < last && self.transcript.is_following_tail() {
                self.transcript.pause_following_tail();
            }
            self.transcript.scroll_to_reveal_item(destination);
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn run_live_row_action(
        &mut self,
        id: &str,
        action: LiveAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            LiveAction::Name => self.open_live_naming(id, window, cx),
            _ => self.live_action(id, action, window, cx),
        }
    }

    fn open_transcript_menu(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(turn) = self.model(cx).turn(&id) else {
            return;
        };
        if self.live_pending() || !self.daemon.read(cx).connected {
            return;
        }
        let shown = ShownTurn::of(turn);
        let actions = Self::live_row_actions(turn);
        let panel = cx.weak_entity();
        let focus = self.transcript_focus.clone();
        let menu = PopupMenu::build(window, cx, move |menu, _, _| {
            Self::live_row_menu(menu.action_context(focus), &actions, &shown, &panel, false)
        });
        let subscription = cx.subscribe(&menu, |this, _, _: &gpui::DismissEvent, cx| {
            this.transcript_menu = None;
            cx.notify();
        });
        menu.read(cx).focus_handle(cx).focus(window, cx);
        self.transcript_menu = Some((id, menu, subscription));
        cx.notify();
    }

    fn transcript_menu_layer(&self, window: &Window) -> Option<AnyElement> {
        let (id, menu, _) = self.transcript_menu.as_ref()?;
        let ix = self
            .transcript_rows
            .iter()
            .position(|row| row.key == RowKey::Turn(id.clone()));
        let anchor = ix
            .and_then(|ix| self.transcript.bounds_for_item(ix))
            .map_or_else(|| window.bounds().center(), |bounds| bounds.bottom_left());
        Some(
            gpui::deferred(
                gpui::anchored()
                    .position(anchor)
                    .snap_to_window_with_margin(px(8.))
                    .child(menu.clone()),
            )
            .with_priority(gpui::base::POPUP_PRIORITY)
            .into_any_element(),
        )
    }

    /// How often the panel and its transcript view were drawn.
    #[cfg(test)]
    pub(crate) fn render_counts(&self, cx: &App) -> (usize, usize) {
        (self.renders, self.transcript_view.read(cx).renders)
    }

    /// Stops the transcript where it is while a naming picker is open, so
    /// the row, the typed name and the focus stay put.
    fn hold_transcript(&mut self) {
        if self.naming_followed.is_none() {
            self.naming_followed = Some(self.transcript.is_following_tail());
        }
        let top = self.transcript.logical_scroll_top();
        self.transcript.set_follow_mode(FollowMode::Normal);
        self.transcript.scroll_to(top);
    }

    /// Row `ix` of the transcript list; a row gone since the list was
    /// synchronised renders empty, never as another row.
    fn transcript_row(&self, ix: usize, window: &Window, cx: &Context<Self>) -> AnyElement {
        let model = self.model(cx);
        self.transcript_rows
            .get(ix)
            .and_then(|row| match &row.key {
                RowKey::Turn(id) => model
                    .turn(id)
                    .map(|turn| self.live_row(turn, row, window, cx)),
                RowKey::Line(number) => model.line(*number).map(|line| {
                    let (meta, badge, text, spoken) = line.presentation();
                    self.transcript_row_frame(
                        ("live-preview-line", *number),
                        row,
                        spoken,
                        window,
                        cx,
                    )
                    .child(Self::transcript_meta(
                        meta,
                        badge.map(|label| (kit::BadgeTone::Outline, label)),
                        cx,
                    ))
                    .child(
                        div()
                            .when(badge.is_none(), |el| {
                                el.text_color(cx.theme().muted_foreground)
                            })
                            .child(text.to_owned()),
                    )
                    .into_any_element()
                }),
                RowKey::Mark(number) => self
                    .transcript_marks
                    .iter()
                    .find(|mark| mark.number == *number)
                    .map(|mark| {
                        self.transcript_row_frame(
                            ("live-mark", *number),
                            row,
                            format!(
                                "{} at {}: {}",
                                mark.kind,
                                spoken_position(mark.at_s),
                                mark.text
                            ),
                            window,
                            cx,
                        )
                        .child(Self::transcript_meta(
                            format!("{} · {}", format_clock(mark.at_s), mark.kind),
                            Some((kit::BadgeTone::Outline, mark.kind)),
                            cx,
                        ))
                        .child(div().child(mark.text.clone()))
                        .into_any_element()
                    }),
            })
            .unwrap_or_else(|| div().into_any_element())
    }

    pub(super) fn transcript_row_frame(
        &self,
        id: impl Into<gpui::ElementId>,
        row: &TranscriptRow,
        spoken: String,
        window: &Window,
        cx: &Context<Self>,
    ) -> Stateful<gpui::Div> {
        let focused = self.transcript_focus.is_focused(window);
        kit::list_row(id, row.selected || row.naming, focused, cx)
            .role(Role::ListItem)
            .aria_label(spoken)
            .when(row.selected && focused, |el| el.aria_active_descendant())
            .flex_col()
            .gap_1()
            .min_w_0()
            .pb_2()
            .text_role(TextRole::Reading)
    }

    fn transcript_meta<T: Into<SharedString>>(
        meta: String,
        badge: Option<(kit::BadgeTone, T)>,
        cx: &App,
    ) -> gpui::Div {
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                crate::ui::type_scale::tabular(div())
                    .text_role(TextRole::Caption)
                    .text_color(cx.theme().muted_foreground)
                    .child(meta),
            )
            .when_some(badge, |el, (tone, text)| {
                el.child(
                    kit::badge(tone, text, cx)
                        .id("live-row-badge")
                        .debug_selector(|| "live-row-badge".into()),
                )
            })
    }

    // ---- statuses ----

    /// The status of `control` for the recording the panel focuses on: the
    /// active one, else the first stopped one still waiting to be saved
    /// (for the per-recording controls); a live decision's status is that
    /// of the latest row acted on.
    #[cfg(test)]
    pub(crate) fn shown_status(&self, control: Control, cx: &App) -> Option<&ControlStatus> {
        let model = self.model(cx);
        let key = match control {
            Control::Live => {
                ControlKey::live(model.active_session()?, self.live_status_turn.as_deref()?)
            }
            control if control.per_recording() => ControlKey::new(
                control,
                model
                    .active_session()
                    .or_else(|| model.title_targets().next().map(|(s, _)| s)),
            ),
            control => ControlKey::new(control, model.active_session()),
        };
        self.controls.get(&key).map(|s| &s.status)
    }

    /// The status of `control` for recording `session`.
    #[cfg(test)]
    pub(crate) fn session_status(&self, control: Control, session: &str) -> Option<&ControlStatus> {
        self.controls
            .get(&ControlKey::new(control, Some(session)))
            .map(|s| &s.status)
    }

    /// The status of the live decision on `turn` of `session`.
    #[cfg(test)]
    pub(crate) fn live_status(&self, session: &str, turn: &str) -> Option<&ControlStatus> {
        self.controls
            .get(&ControlKey::live(session, turn))
            .map(|s| &s.status)
    }

    fn pending(&self, control: Control, session: Option<&str>) -> bool {
        self.controls
            .get(&ControlKey::new(control, session))
            .is_some_and(|s| s.status == ControlStatus::Pending)
    }

    /// A live decision is in flight (one at a time).
    fn live_pending(&self) -> bool {
        self.controls
            .iter()
            .any(|(key, s)| key.control == Control::Live && s.status == ControlStatus::Pending)
    }

    /// Whether the recording `key` acted on is still the one that control
    /// acts on, so its answer may touch drafts and lists.
    fn still_current(&self, key: &ControlKey, cx: &App) -> bool {
        let Some(session) = key.session.as_deref() else {
            return true;
        };
        let model = self.model(cx);
        match key.control {
            Control::Title => self.titles.contains_key(session),
            Control::Session | Control::Discard | Control::Retry => model.knows_session(session),
            _ => model.active_session() == Some(session),
        }
    }

    /// Sends `call` for `key` (one request per control and recording at a
    /// time). `done` maps the answer to a status; its `current` argument
    /// says whether the recording is still the one the control acts on, so
    /// it may touch drafts and lists. A result replaces only its own
    /// status: other controls, recordings and rows keep theirs.
    fn submit(
        &mut self,
        key: ControlKey,
        call: Call,
        window: &mut Window,
        cx: &mut Context<Self>,
        done: impl FnOnce(&mut Self, Value, bool, &mut Window, &mut Context<Self>) -> ControlStatus
        + 'static,
    ) {
        if !self.daemon.read(cx).connected
            || self
                .controls
                .get(&key)
                .is_some_and(|s| s.status == ControlStatus::Pending)
        {
            return;
        }
        self.generation += 1;
        let generation = self.generation;
        if key.control == Control::Live {
            self.live_revision += 1;
        }
        self.controls.insert(
            key.clone(),
            ControlState {
                status: ControlStatus::Pending,
                generation,
            },
        );
        cx.notify();
        let request = self.daemon.read(cx).request(call, cx);
        cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            this.update_in(cx, |this, window, cx| {
                if this
                    .controls
                    .get(&key)
                    .is_none_or(|s| s.generation != generation)
                {
                    return;
                }
                let current = this.still_current(&key, cx);
                let status = match result {
                    Ok(value) => done(this, value, current, window, cx),
                    Err(error) => {
                        // A changed live row: refetch the authoritative rows.
                        if key.control == Control::Live && error.code.starts_with("stale_") {
                            this.daemon.update(cx, |daemon, cx| {
                                daemon.panel.want_snapshot();
                                daemon.refresh_live(cx);
                            });
                        }
                        if key.control == Control::Start {
                            let refusal = crate::daemon::start_refusal(
                                &error,
                                crate::ui::setup_window::capture_active(cx),
                            );
                            let words = refusal.words.clone();
                            this.start_refusal = Some(refusal);
                            ControlStatus::Failed(words)
                        } else {
                            ControlStatus::Failed(control_error(&error))
                        }
                    }
                };
                if key.control == Control::Live {
                    this.live_revision += 1;
                }
                this.controls
                    .insert(key, ControlState { status, generation });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---- recording controls ----

    pub(super) fn start_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let daemon = self.daemon.read(cx);
        if !daemon.connected
            || daemon.panel.active_session().is_some()
            || daemon.record_start_pending()
            || crate::menu::update_waiting(cx)
        {
            return;
        }
        self.start_refusal = None;
        self.submit(
            ControlKey::new(Control::Start, None),
            Call::RecordStart(Default::default()),
            window,
            cx,
            |_, _, _, _, _| ControlStatus::done("Recording started."),
        );
    }

    /// Pause / Resume / Stop and save / Process now on the recording shown:
    /// the request names that session, so it never acts on a newer one.
    pub fn session_action(
        &mut self,
        command: SessionCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.model(cx).active_session().map(str::to_owned) else {
            return;
        };
        let text = match command {
            SessionCommand::Stop => "Stopped. Transcribing.",
            SessionCommand::Pause => "Paused. Nothing is recorded until you resume.",
            SessionCommand::Resume => "Recording again.",
            SessionCommand::ProcessNow => {
                "Making a draft transcript of the recording so far. Recording continues."
            }
        };
        self.submit(
            ControlKey::new(Control::Session, Some(&session)),
            command.call(session),
            window,
            cx,
            move |_, _, _, _, _| ControlStatus::done(text),
        );
    }

    pub(super) fn shown_language<'a>(&'a self, cx: &'a App) -> &'a str {
        self.model(cx)
            .recording
            .as_ref()
            .and_then(|recording| recording.language.as_deref())
            .or(self.state.read(cx).config().default_language.as_deref())
            .filter(|code| *code != "auto")
            .unwrap_or_default()
    }

    pub fn set_language(&mut self, language: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.model(cx).active_session().map(str::to_owned) else {
            return;
        };
        let language = if language.is_empty() {
            "auto".to_owned()
        } else {
            language
        };
        let call = PanelModel::language_action(&session, &language);
        let at = self
            .model(cx)
            .recording
            .as_ref()
            .map_or(0., |recording| recording.elapsed_s);
        let name = crate::ui::format::language_name(&language);
        self.submit(
            ControlKey::new(Control::Language, Some(&session)),
            call,
            window,
            cx,
            move |_, result, _, _, _| {
                ControlStatus::done(format!(
                    "Language set to {name} from {}.",
                    spoken_position(result["at_s"].as_f64().unwrap_or(at)),
                ))
            },
        );
    }

    /// The title editor of `session` (the active recording or a stopped one
    /// not saved yet).
    #[cfg(test)]
    pub(crate) fn title_input(&self, session: &str) -> Option<Entity<InputState>> {
        self.titles.get(session).map(|editor| editor.input.clone())
    }

    /// The title typed for `session`, when it differs from the agent's.
    fn title_draft(&self, session: &str, cx: &App) -> Option<String> {
        let editor = self.titles.get(session)?;
        let title = editor.input.read(cx).value().trim().to_owned();
        (!title.is_empty() && Some(title.as_str()) != editor.seen.as_deref()).then_some(title)
    }

    /// Renames recording `session` (active, or stopped and not saved yet)
    /// with its own editor's title.
    pub fn submit_title(&mut self, session: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(title) = self.title_draft(session, cx) else {
            return;
        };
        let session = session.to_owned();
        let call = Call::RecordTitle {
            session_id: session.clone(),
            request_id: RequestId::random(),
            title: title.clone(),
        };
        let submitted_from = window.focused(cx);
        self.submit(
            ControlKey::new(Control::Title, Some(&session)),
            call,
            window,
            cx,
            move |this, value, current, window, cx| {
                if current && value["status"] == "saved" {
                    let active = this.model(cx).active_session() == Some(session.as_str());
                    if let Some(editor) = this.titles.get_mut(&session) {
                        // Keep a newer draft open, and never steal focus if
                        // the user moved away while the save was pending.
                        if editor.revealed && editor.input.read(cx).value().trim() == title {
                            editor.revealed = false;
                            if submitted_from.is_some_and(|focus| focus.is_focused(window)) {
                                let opener = if active {
                                    &this.more_focus
                                } else {
                                    &editor.rename_focus
                                };
                                opener.focus(window, cx);
                            }
                        }
                        editor.seen = Some(title);
                    }
                }
                title_outcome(&value)
            },
        );
    }

    /// A choice from the people picker: confirm that person's attendance.
    /// Only the search the choice came from is cleared on success.
    pub fn confirm_participant(
        &mut self,
        choice: PersonChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.model(cx).active_session().map(str::to_owned) else {
            return;
        };
        let picker = self.picker.read(cx);
        let label = match &choice {
            PersonChoice::Existing(id) => picker
                .candidates()
                .find(|p| p.id == *id)
                .map_or_else(|| id.to_string(), |p| p.name.clone()),
            PersonChoice::Typed(text) => text.clone(),
        };
        let query = picker.query().to_owned();
        let replaced = self.selected_person.clone();
        let call = Call::RecordParticipant {
            session_id: session.clone(),
            request_id: RequestId::random(),
            person_ref: choice.person_ref().to_owned(),
            action: ParticipantAction::Confirm,
        };
        self.submit(
            ControlKey::new(Control::Participant, Some(&session)),
            call,
            window,
            cx,
            move |this, value, current, window, cx| {
                let status = participant_outcome(&value, &label);
                if current
                    && value["status"] != "partial"
                    && let Some(id) = value["person_id"].as_str().map(str::to_owned)
                {
                    // A person the roster already loaded keeps its catalog
                    // details (emails match observed rows).
                    this.confirmed
                        .entry(id.clone())
                        .or_insert_with(|| RosterPerson {
                            name: label.clone(),
                            emails: Vec::new(),
                        });
                    if this.picker.read(cx).query() == query && this.selected_person == replaced {
                        this.selected_person = None;
                        this.picker.update(cx, |p, cx| p.set_query("", window, cx));
                    }
                    if let Some(previous) = replaced.filter(|p| *p != id) {
                        this.remove_participant(previous, window, cx);
                    }
                }
                status
            },
        );
    }

    pub fn remove_participant(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.model(cx).active_session().map(str::to_owned) else {
            return;
        };
        let label = self
            .confirmed
            .get(&id)
            .map_or_else(|| id.clone(), |p| p.name.clone());
        let call = Call::RecordParticipant {
            session_id: session.clone(),
            request_id: RequestId::random(),
            person_ref: id.clone(),
            action: ParticipantAction::Remove,
        };
        self.submit(
            ControlKey::new(Control::Roster, Some(&session)),
            call,
            window,
            cx,
            move |this, value, current, _, _| {
                if current {
                    this.confirmed.remove(&id);
                }
                participant_outcome(&value, &label)
            },
        );
    }

    pub fn submit_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.note.read(cx).value().to_string();
        let text = draft.trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.note_call(
            move |session_id| Call::RecordNote { session_id, text },
            Some(draft),
            window,
            cx,
        );
    }

    pub fn bookmark(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.note.read(cx).value().to_string();
        let label = (!draft.trim().is_empty()).then(|| draft.trim().to_owned());
        self.note_call(
            |session_id| Call::RecordBookmark { session_id, label },
            Some(draft),
            window,
            cx,
        );
    }

    /// Sends a note or bookmark made from `draft`; once accepted, the field
    /// is cleared only if it still holds exactly that draft (text typed
    /// meanwhile stays).
    fn note_call(
        &mut self,
        call: impl FnOnce(String) -> Call,
        draft: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.model(cx).active_session().map(str::to_owned) else {
            return;
        };
        let fallback_at = self.model(cx).elapsed_s(Instant::now());
        let call = call(session.clone());
        let (kind, mark_text) = match &call {
            Call::RecordNote { text, .. } => ("Note", text.clone()),
            Call::RecordBookmark { label, .. } => ("Bookmark", label.clone().unwrap_or_default()),
            _ => unreachable!("note controls only send notes and bookmarks"),
        };
        self.submit(
            ControlKey::new(Control::Note, Some(&session)),
            call,
            window,
            cx,
            move |this, value, current, window, cx| {
                let at_s = value
                    .get("at_s")
                    .and_then(Value::as_f64)
                    .unwrap_or(fallback_at);
                if current {
                    if draft.as_deref() == Some(this.note.read(cx).value().as_ref()) {
                        this.note
                            .update(cx, |input, cx| input.set_value("", window, cx));
                    }
                    if this.transcript_marks.len() == live_turns_max() {
                        this.transcript_marks.pop_front();
                    }
                    this.transcript_mark_number += 1;
                    this.transcript_marks.push_back(TranscriptMark {
                        number: this.transcript_mark_number,
                        at_s,
                        kind,
                        text: mark_text,
                    });
                    this.live_revision += 1;
                }
                ControlStatus::done(format!("{kind} added at {}.", format_clock(at_s)))
            },
        );
    }

    pub fn submit_tags(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.model(cx).active_session().map(str::to_owned) else {
            return;
        };
        let draft = self.tag.read(cx).value().to_string();
        let Some(call) = tag_call(&session, &draft) else {
            return;
        };
        self.tag_request(session, call, Some(draft), window, cx);
    }

    pub fn remove_tag(&mut self, tag: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.model(cx).active_session().map(str::to_owned) else {
            return;
        };
        let call = remove_tag_call(&session, &tag);
        self.tag_request(session, call, None, window, cx);
    }

    /// Sends a tag change; `draft` is the typed input it was made from,
    /// cleared on success only while the field still holds exactly it.
    fn tag_request(
        &mut self,
        session: String,
        call: Call,
        draft: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let snapshots = self.tag_snapshots;
        let added = draft.is_some();
        self.submit(
            ControlKey::new(Control::Tag, Some(&session)),
            call,
            window,
            cx,
            move |this, value, current, window, cx| {
                if current {
                    // A snapshot that changed the tags after this request was
                    // sent is newer than the answer: it stays shown.
                    if let Some(tags) = value["tags"].as_array()
                        && this.tag_snapshots == snapshots
                        && value["session_id"].as_str().is_none_or(|s| s == session)
                    {
                        let tags = tags
                            .iter()
                            .filter_map(|t| t.as_str().map(str::to_owned))
                            .collect();
                        this.tags = Some((session, tags));
                    }
                    if let Some(draft) = draft
                        && *this.tag.read(cx).value() == *draft
                    {
                        this.tag
                            .update(cx, |input, cx| input.set_value("", window, cx));
                    }
                }
                ControlStatus::done(if added { "Tags saved." } else { "Tag removed." })
            },
        );
    }

    /// The tags shown for the active session.
    pub fn shown_tags(&self, cx: &App) -> Vec<String> {
        let model = self.model(cx);
        let Some(recording) = &model.recording else {
            return Vec::new();
        };
        match &self.tags {
            Some((session, tags)) if *session == recording.session_id => tags.clone(),
            _ => recording.tags.clone(),
        }
    }

    /// "Discard": opens the confirmation for `session`; nothing is sent yet.
    /// A discard already in flight for another recording never blocks it.
    pub fn open_discard(&mut self, session: String, cx: &mut Context<Self>) {
        if self.daemon.read(cx).connected
            && self.discard_confirm.is_none()
            && self.model(cx).discardable(&session)
            && !self.pending(Control::Discard, Some(&session))
        {
            self.discard_confirm = Some(session);
            cx.notify();
        }
    }

    pub fn cancel_discard(&mut self, cx: &mut Context<Self>) {
        self.discard_confirm = None;
        cx.notify();
    }

    fn present_discard(
        &mut self,
        session: String,
        opener: gpui::FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_discard(session.clone(), cx);
        if self.discard_confirm.as_deref() != Some(session.as_str()) {
            return;
        }
        self.discard_modal = true;
        let panel = cx.weak_entity();
        let target = session.clone();
        let model = panel.clone();
        let safe = panel.clone();
        kit::confirm_dialog(
            opener,
            window,
            cx,
            move |cx| {
                let panel = model.upgrade()?;
                let active = panel.read(cx).model(cx).active_session() == Some(target.as_str());
                Some(discard_spec(active))
            },
            move |_, cx| {
                let _ = safe.update(cx, |this, cx| {
                    this.discard_modal = false;
                    this.cancel_discard(cx);
                });
                true
            },
            move |window, cx| {
                let _ = panel.update(cx, |this, cx| {
                    this.discard_modal = false;
                    this.confirm_discard(window, cx);
                });
                true
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn discard_target(&self) -> Option<&str> {
        self.discard_confirm.as_deref()
    }

    /// "Discard recording" in the confirmation: discards the session the
    /// confirmation was opened for, never a newer one; its pending state and
    /// result belong to that session alone.
    pub fn confirm_discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.discard_confirm.take() else {
            return;
        };
        if !self.model(cx).discardable(&session) {
            cx.notify();
            return;
        }
        let call = Call::RecordDiscard {
            session_id: session.clone(),
            request_id: RequestId::random(),
        };
        self.submit(
            ControlKey::new(Control::Discard, Some(&session)),
            call,
            window,
            cx,
            |_, value, _, _, _| discard_outcome(&value),
        );
    }

    /// "Retry" on a stopped recording whose processing failed for good:
    /// runs its final job again with fresh attempts (never automatic).
    pub fn retry(&mut self, session: String, window: &mut Window, cx: &mut Context<Self>) {
        // Only a failure the agent retries on request ([`SaveFailure::Manual`]).
        let model = self.model(cx);
        let retryable = model.stopped_session(&session).is_some_and(|s| {
            let issue = model
                .journal_issues
                .iter()
                .find(|i| i.session_id == session);
            save_failure(&s.state, issue) == Some(SaveFailure::Manual)
        });
        if !retryable {
            return;
        }
        let call = Call::RecordRetry {
            session_id: session.clone(),
            request_id: RequestId::random(),
        };
        self.submit(
            ControlKey::new(Control::Retry, Some(&session)),
            call,
            window,
            cx,
            |_, _, _, _, _| {
                ControlStatus::done(
                    "Saving again: its final transcript was queued with fresh attempts.",
                )
            },
        );
    }

    fn can_resolve_journal(&self, session: &str, cx: &App) -> bool {
        let model = self.model(cx);
        let daemon = self.daemon.read(cx);
        daemon.connected
            && daemon.reported_library().is_some()
            && model.active_session().is_none()
            && model
                .journal_issues
                .iter()
                .any(|issue| issue.session_id == session && issue.code == JOURNAL_LIBRARY_UNKNOWN)
            && !self.pending(Control::ResolveJournal, Some(session))
    }

    fn journal_library<'a>(&self, cx: &'a App) -> Option<&'a std::path::Path> {
        let reported = self.daemon.read(cx).reported_library()?;
        let open = self.state.read(cx).library().root();
        (open == reported || open.canonicalize().as_deref().ok() == Some(reported))
            .then_some(reported)
    }

    fn present_journal_resolution(
        &mut self,
        session: String,
        resolution: JournalResolution,
        opener: gpui::FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_resolve_journal(&session, cx) {
            return;
        }
        let Some(library_root) = self.journal_library(cx).map(std::path::Path::to_path_buf) else {
            self.controls.insert(
                ControlKey::new(Control::ResolveJournal, Some(&session)),
                ControlState {
                    status: ControlStatus::Failed("BroScribe Agent is using a different library. Open that library to resolve this recording.".into()),
                    generation: self.generation,
                },
            );
            cx.notify();
            return;
        };
        let panel = cx.weak_entity();
        let target = session.clone();
        let discard = resolution == JournalResolution::Discard;
        let title = if discard {
            "Discard this recording?"
        } else {
            "Save this recording here?"
        };
        let action = if discard {
            "Discard recording"
        } else {
            "Save in this library"
        };
        kit::confirm_dialog(
            opener,
            window,
            cx,
            move |_| {
                let mut spec = kit::ConfirmSpec::new(
                    kit::confirm_line("legacy-journal-confirmation", title),
                    "legacy-journal-body",
                    "legacy-journal-footer",
                    "legacy-journal-cancel",
                    kit::confirm_line("legacy-journal-confirm", action),
                );
                spec.warning = Some(kit::confirm_line(
                    "legacy-journal-effect",
                    if discard {
                        "The audio and other files in this recording's journal will be deleted."
                    } else {
                        "BroScribe will transcribe and save the recording in the open library."
                    },
                ));
                spec.caveat = Some(kit::confirm_line(
                    "legacy-journal-session",
                    format!("Recording {target}"),
                ));
                spec.primary_action = !discard;
                spec.irreversible = discard;
                Some(spec)
            },
            |_, _| true,
            move |window, cx| {
                let _ = panel.update(cx, |this, cx| {
                    if !this.can_resolve_journal(&session, cx)
                        || this.journal_library(cx) != Some(library_root.as_path())
                    {
                        return;
                    }
                    this.submit(
                        ControlKey::new(Control::ResolveJournal, Some(&session)),
                        Call::RecordResolveJournal {
                            session_id: session.clone(),
                            request_id: RequestId::random(),
                            resolution,
                            library_root: library_root.clone(),
                            confirmed: true,
                        },
                        window,
                        cx,
                        move |_, _, _, _, _| {
                            ControlStatus::done(if discard {
                                "Recording journal discard requested."
                            } else {
                                "Recording added to this library for processing."
                            })
                        },
                    );
                });
                true
            },
        );
    }

    fn library_journal_rows(&self, window: &mut Window, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let issues: Vec<_> = self
            .model(cx)
            .journal_issues
            .iter()
            .filter_map(|issue| {
                let words = journal_library_issue(&issue.code)?;
                Some((
                    issue.session_id.clone(),
                    issue.code == JOURNAL_LIBRARY_UNKNOWN,
                    words,
                ))
            })
            .collect();
        issues
            .into_iter()
            .map(|(session, unknown, words)| {
                let mut row = kit::stack().child(kit::status_line(
                    SharedString::from(format!("journal-issue-{session}")),
                    kit::StatusKind::Attention,
                    format!("Recording {session}. {words}"),
                    cx,
                ));
                if unknown {
                    let enabled = self.can_resolve_journal(&session, cx);
                    let actions = [
                        (JournalResolution::Adopt, "save", "Save in this library"),
                        (JournalResolution::Discard, "discard", "Discard recording"),
                    ]
                    .map(|(resolution, id, label)| {
                        let session = session.clone();
                        let id = SharedString::from(format!("legacy-journal-{id}-{session}"));
                        let opener = window
                            .use_keyed_state(id.clone(), cx, |_, cx| cx.focus_handle())
                            .read(cx)
                            .clone();
                        let focus = opener.clone();
                        Button::new(id)
                            .label(label)
                            .small()
                            .when(resolution == JournalResolution::Discard, |button| {
                                button.danger().outline()
                            })
                            .track_focus(&opener)
                            .map(|button| kit::disable_action(button, !enabled, cx))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.present_journal_resolution(
                                    session.clone(),
                                    resolution,
                                    focus.clone(),
                                    window,
                                    cx,
                                );
                            }))
                    });
                    row = row
                        .child(div().flex().flex_wrap().gap_2().children(actions))
                        .children(self.status_for(Control::ResolveJournal, &session, cx));
                }
                row.into_any_element()
            })
            .collect()
    }

    fn open_meeting(&mut self, meeting_id: &str, cx: &mut Context<Self>) {
        let Ok(id) = MeetingId::parse(meeting_id) else {
            return;
        };
        self.state.update(cx, |state, cx| {
            state.navigate(Route::meeting(id), cx);
            cx.emit(crate::state::AppEvent::OpenMainWindow);
        });
    }

    fn open_library(&self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.navigate(Route::library(), cx);
            cx.emit(AppEvent::OpenMainWindow);
        });
    }

    pub(super) fn show_failed_recording(
        &mut self,
        session: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self
            .model(cx)
            .stopped_session(session)
            .is_some_and(|entry| matches!(entry.state, StoppedRecordingState::Failed { .. }))
        {
            return false;
        }
        self.keep_open();
        self.notice_target = Some(session.to_owned());
        self.temporarily_expanded = true;
        self.tab = RecordingPanelTab::Details;
        if self.model(cx).recording.is_some() {
            self.notice_anchor.scroll_to(window, cx);
        }
        let focus = self.notice_focus.clone();
        window.on_next_frame(move |window, cx| focus.focus(window, cx));
        cx.notify();
        true
    }

    /// "Delete meeting" for a saved recording: the shared confirmation.
    pub fn open_delete(
        &mut self,
        meeting_id: &str,
        title: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(id) = MeetingId::parse(meeting_id) else {
            return;
        };
        if self.delete.is_some() {
            return;
        }
        let state = self.state.clone();
        let title = title.unwrap_or_else(|| "Untitled meeting".to_owned());
        let prompt = cx.new(|cx| DeleteMeetingPrompt::new(state, id, title, cx));
        let subscription = cx.subscribe(&prompt, |this, _, ev: &DeleteEvent, cx| {
            this.delete = None;
            if let DeleteEvent::Deleted { text, .. } = ev {
                this.generation += 1;
                this.controls.insert(
                    ControlKey::new(Control::Discard, None),
                    ControlState {
                        status: ControlStatus::done(text.clone()),
                        generation: this.generation,
                    },
                );
            }
            cx.notify();
        });
        let opener = window.focused(cx).unwrap_or_else(|| cx.focus_handle());
        DeleteMeetingPrompt::present(&prompt, opener, window, cx);
        self.delete = Some((prompt, subscription));
        cx.notify();
    }

    // ---- live decisions ----

    /// "Name…" on a live row: a people picker bound to the row exactly as
    /// shown now (revision, epoch, evidence). A change to the row, or the
    /// row leaving the preview, closes it with nothing saved.
    /// While it is open the transcript does not scroll by itself.
    pub fn open_live_naming(&mut self, turn_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.live_pending() || self.live_naming.is_some() || !self.daemon.read(cx).connected {
            return;
        }
        let Some(turn) = self.model(cx).turn(turn_id) else {
            return;
        };
        let shown = ShownTurn::of(turn);
        let label = format!(
            "Name the speaker at {} ({})",
            format_clock(turn.start_s),
            track_label(turn.track),
        );
        let state = self.state.clone();
        let picker = cx.new(|cx| PersonPicker::new(state, label, window, cx));
        let subscription = cx.subscribe_in(
            &picker,
            window,
            |this, _, ev: &PersonPickerEvent, window, cx| match ev {
                PersonPickerEvent::Picked(choice) => {
                    let Some(naming) = this.live_naming.take() else {
                        return;
                    };
                    let person_ref = choice.person_ref().to_owned();
                    this.live_decision(
                        naming.shown,
                        LiveAction::Name,
                        Some(person_ref),
                        window,
                        cx,
                    );
                    this.transcript_focus.focus(window, cx);
                }
                PersonPickerEvent::Cancelled => this.cancel_live_naming(window, cx),
            },
        );
        self.live_naming = Some(LiveNaming {
            shown,
            picker,
            reveal_after_layout: std::cell::Cell::new(true),
            _subscription: subscription,
        });
        self.hold_transcript();
        // Once the row is laid out with its picker, show all of it (this
        // row, never one found by position).
        let key = RowKey::Turn(turn_id.to_owned());
        cx.on_next_frame(window, move |this, _, cx| {
            if let Some(ix) = this
                .transcript_rows
                .iter()
                .position(|row| row.key == key && row.naming)
            {
                this.transcript.scroll_to_reveal_item(ix);
                this.transcript_view.update(cx, |_, cx| cx.notify());
                cx.notify();
            }
        });
        cx.notify();
    }

    pub fn cancel_live_naming(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.live_naming.take().is_some() {
            self.transcript_focus.focus(window, cx);
        }
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn live_picker(&self) -> Option<&Entity<PersonPicker>> {
        self.live_naming.as_ref().map(|n| &n.picker)
    }

    /// "Confirm" (the row's suggestion) or "Clear" (the user's name) on a
    /// live row, as it is shown now.
    pub fn live_action(
        &mut self,
        turn_id: &str,
        action: LiveAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(turn) = self.model(cx).turn(turn_id) else {
            return;
        };
        let person_ref = match action {
            LiveAction::Confirm => match &turn.suggestion {
                Some(s) if turn.status == LiveTurnStatus::Suggested => {
                    Some(s.person.person_id.clone())
                }
                _ => return,
            },
            LiveAction::Clear if turn.status == LiveTurnStatus::Named => None,
            LiveAction::Clear | LiveAction::Name => return,
        };
        let shown = ShownTurn::of(turn);
        self.live_decision(shown, action, person_ref, window, cx);
    }

    /// Sends one decision on `shown`; the answer's row replaces the shown
    /// one (only for the session it was made in, and only while the row is
    /// still held). Its status stays on that row.
    fn live_decision(
        &mut self,
        shown: ShownTurn,
        action: LiveAction,
        person_ref: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.live_pending() {
            return;
        }
        self.live_naming = None;
        self.live_status_turn = Some(shown.turn_id.clone());
        let call = shown.call(action, person_ref);
        self.submit(
            ControlKey::live(&shown.session_id, &shown.turn_id),
            call,
            window,
            cx,
            move |this, value, current, _, cx| {
                let Ok(outcome) = serde_json::from_value::<LiveAttributeOutcome>(value) else {
                    return ControlStatus::Failed(
                        "BroScribe Agent answered in an unexpected form.".into(),
                    );
                };
                if current {
                    let turn = outcome.turn.clone();
                    this.daemon.update(cx, |daemon, cx| {
                        if daemon.panel.replace_turn(turn) {
                            cx.notify();
                        }
                    });
                }
                live_outcome(&outcome, action)
            },
        );
    }

    /// Records the outcome of a live row that was not sent (its picker
    /// closed because the row changed or left the preview).
    fn set_live_status(&mut self, session: &str, turn: &str, status: ControlStatus) {
        self.generation += 1;
        self.live_status_turn = Some(turn.to_owned());
        self.live_revision += 1;
        self.controls.insert(
            ControlKey::live(session, turn),
            ControlState {
                status,
                generation: self.generation,
            },
        );
    }

    fn live_row_actions(turn: &LiveTurn) -> Vec<LiveRowAction> {
        let position = spoken_position(turn.start_s);
        let mut actions = Vec::new();
        let suggested = turn
            .suggestion
            .as_ref()
            .filter(|_| turn.status == LiveTurnStatus::Suggested);
        if let Some(suggestion) = suggested {
            actions.push(LiveRowAction {
                action: LiveAction::Confirm,
                id: "confirm",
                label: "Accept",
                shortcut: "enter",
                spoken: format!(
                    "Accept {} for the line at {position}",
                    suggestion.person.name
                ),
            });
        }
        if turn.status == LiveTurnStatus::Named {
            actions.push(LiveRowAction {
                action: LiveAction::Clear,
                id: "clear",
                label: "Clear",
                shortcut: "backspace",
                spoken: format!("Remove your name from the line at {position}"),
            });
        }
        actions.push(LiveRowAction {
            action: LiveAction::Name,
            id: "name",
            label: "Name…",
            shortcut: if suggested.is_some() { "n" } else { "enter" },
            spoken: format!("Name the speaker of the line at {position}"),
        });
        actions
    }

    fn live_row_menu(
        menu: PopupMenu,
        actions: &[LiveRowAction],
        shown: &ShownTurn,
        panel: &WeakEntity<Self>,
        busy: bool,
    ) -> PopupMenu {
        actions.iter().fold(menu, |menu, item| {
            let panel = panel.clone();
            let shown = shown.clone();
            let action = item.action;
            menu.item(PopupMenuItem::new(item.label).disabled(busy).on_click(
                move |_, window, cx| {
                    let _ = panel.update(cx, |this, cx| {
                        this.run_shown_live_action(&shown, action, window, cx)
                    });
                },
            ))
        })
    }

    /// One live speaker turn of the transcript list.
    fn live_row(
        &self,
        turn: &LiveTurn,
        state: &TranscriptRow,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let id = &turn.turn_id;
        let selector = format!("live-turn-{id}");
        let shown = ShownTurn::of(turn);
        let panel = cx.weak_entity();
        let busy =
            self.live_pending() || self.live_naming.is_some() || !self.daemon.read(cx).connected;
        let actions = Self::live_row_actions(turn);
        let row_key = state.key.clone();
        let muted = cx.theme().muted_foreground;
        let badge = if state.naming {
            (kit::BadgeTone::Neutral, "Naming…".into())
        } else {
            let tone = match turn.status {
                LiveTurnStatus::Named => kit::BadgeTone::Strong,
                LiveTurnStatus::Suggested => kit::BadgeTone::Suggested,
                LiveTurnStatus::Voice => kit::BadgeTone::Neutral,
                // Undecided lines are warning-soft with the attention mark.
                LiveTurnStatus::Unresolved => kit::BadgeTone::Attention,
                LiveTurnStatus::Preview => kit::BadgeTone::Outline,
            };
            (tone, turn_badge(turn))
        };
        let row = self
            .transcript_row_frame(
                SharedString::from(selector.clone()),
                state,
                spoken_turn_line(turn),
                window,
                cx,
            )
            .debug_selector(|| selector)
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                let previous = this.transcript_hovered.clone();
                if *hovered {
                    this.transcript_hovered = Some(row_key.clone());
                } else if this.transcript_hovered.as_ref() == Some(&row_key) {
                    this.transcript_hovered = None;
                }
                if previous != this.transcript_hovered {
                    this.live_revision += 1;
                    cx.notify();
                }
            }))
            .child(Self::transcript_meta(
                format!(
                    "{} · {}",
                    format_clock(turn.start_s),
                    track_label(turn.track)
                ),
                Some(badge),
                cx,
            ))
            .children(turn_qualifier(turn).map(|text| {
                div()
                    .text_role(TextRole::Caption)
                    .text_color(muted)
                    .child(text)
            }))
            .child(div().child(turn.text.clone()))
            .when(
                state.selected || state.hovered || window.is_a11y_active(),
                |row| {
                    row.child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_1()
                            .when(state.naming, |el| el.invisible())
                            .children(actions.iter().map(|item| {
                                let shown = shown.clone();
                                let action = item.action;
                                let button = Button::new(SharedString::from(format!(
                                    "live-{}-{id}",
                                    item.id
                                )))
                                .label(item.label)
                                .accessibility_label(item.spoken.clone())
                                .small()
                                .ghost()
                                .tab_stop(false)
                                .disabled(busy)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.run_shown_live_action(&shown, action, window, cx);
                                }));
                                div().flex().items_center().gap_1().child(button).when(
                                    state.selected && !busy,
                                    |el| {
                                        let stroke = gpui::Keystroke::parse(item.shortcut)
                                            .expect("valid live row shortcut");
                                        el.child(gpui::component::kbd::Kbd::new(stroke))
                                    },
                                )
                            })),
                    )
                },
            )
            .children(self.status_line(&ControlKey::live(&turn.session_id, id), cx));

        row.context_menu(move |menu, _, _| {
            Self::live_row_menu(menu, &actions, &shown, &panel, busy)
        })
        .into_any_element()
    }

    fn run_shown_live_action(
        &mut self,
        shown: &ShownTurn,
        action: LiveAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .model(cx)
            .turn(&shown.turn_id)
            .is_some_and(|turn| ShownTurn::of(turn) == *shown)
        {
            self.run_live_row_action(&shown.turn_id, action, window, cx);
        } else if self.model(cx).active_session() == Some(shown.session_id.as_str()) {
            self.set_live_status(
                &shown.session_id,
                &shown.turn_id,
                ControlStatus::Failed(LIVE_ROW_GONE.into()),
            );
            cx.notify();
        }
    }
    fn docked_line_picker(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let naming = self.live_naming.as_ref()?;
        let title = format!(
            "Name the speaker at {} ({})",
            format_clock(naming.shown.start_s),
            track_label(naming.shown.track)
        );
        let spoken = format!(
            "Names this line now, from {} to {}. After the meeting, agreeing names can name the speaker and teach BroScribe their voice.",
            spoken_position(naming.shown.start_s),
            spoken_position(naming.shown.end_s),
        );
        Some(
            div()
                .id("live-naming-dock")
                .debug_selector(|| "live-naming-dock".into())
                .role(Role::Group)
                .aria_label("Name this transcript line")
                .flex()
                .flex_col()
                .gap_2()
                .flex_none()
                .max_h(gpui::relative(0.45))
                .overflow_y_scroll()
                .when(
                    naming.reveal_after_layout.replace(false) && self.naming_followed == Some(true),
                    |el| {
                        let target = RowKey::Turn(naming.shown.turn_id.clone());
                        let panel = cx.weak_entity();
                        el.on_prepaint(move |_, window, cx| {
                            window.defer(cx, move |_, cx| {
                                let _ = panel.update(cx, |this, cx| {
                                    if let Some(index) = this
                                        .transcript_rows
                                        .iter()
                                        .position(|row| row.key == target)
                                    {
                                        this.transcript.scroll_to_reveal_item(index);
                                        cx.notify();
                                    }
                                });
                            });
                        })
                    },
                )
                .child(crate::ui::subheading("live-naming-title", title))
                .child(
                    text_line("live-line-scope", Role::Label, spoken)
                        .text_role(TextRole::Caption)
                        .text_color(cx.theme().muted_foreground),
                )
                .child(naming.picker.clone())
                .into_any_element(),
        )
    }

    // ---- following the agent ----

    /// The agent's client changed. Meters alone (and the reconnection flag)
    /// only redraw; anything else is followed by [`Self::daemon_changed`].
    fn daemon_notified(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.seen_revision == Some(self.model(cx).revision()) {
            self.sync_ticker(cx);
            cx.notify();
        } else {
            self.daemon_changed(window, cx);
        }
        if self.discard_modal && self.discard_confirm.is_none() {
            self.discard_modal = false;
            window.close_dialog(cx);
        }
    }

    /// Follows the agent's state: a new session drops the previous one's
    /// drafts and statuses and resets the language; an ended discard target
    /// closes its confirmation; each title field follows its recording's
    /// title unless the user is editing it. The end of the capture shown
    /// (the session leaves the agent's active state; a pending automatic
    /// stop is not an end) lets the panel close.
    fn daemon_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.seen_revision = Some(self.model(cx).revision());
        self.sync_roster(cx);
        if self.discard_confirm.as_deref().is_some_and(|session| {
            !self.daemon.read(cx).connected || !self.model(cx).discardable(session)
        }) {
            self.discard_confirm = None;
            if self.discard_modal {
                self.discard_modal = false;
                window.close_dialog(cx);
            }
        }
        let model = self.model(cx);
        if self.notice_target.as_deref().is_some_and(|session| {
            !model
                .stopped_session(session)
                .is_some_and(|entry| matches!(entry.state, StoppedRecordingState::Failed { .. }))
        }) {
            self.notice_target = None;
        }
        let active = model.active_session().map(str::to_owned);
        let snapshot_tags = model
            .recording
            .as_ref()
            .map(|r| (r.session_id.clone(), r.tags.clone()));
        let session_changed = active != self.session_key;
        if session_changed {
            self.more_open = false;
            let started = active.is_some();
            // A recording that is still listed keeps its results (on its
            // stopped row); a new recording drops finished results of
            // recordings no longer listed. Statuses of the active
            // recording's other controls end with it.
            self.controls.retain(|key, state| {
                if key.control.per_recording() {
                    key.session
                        .as_deref()
                        .is_some_and(|s| model.knows_session(s))
                        || !started
                        || matches!(
                            state.status,
                            ControlStatus::Pending | ControlStatus::Failed(_)
                        )
                } else {
                    key.session.is_some() && key.session == active
                }
            });
            self.selected_person = None;
            self.tags = None;
            self.live_naming = None;
            self.live_status_turn = None;
            self.live_revision += 1;
            if started {
                self.start_refusal = None;
                self.tab = RecordingPanelTab::Transcript;
                self.details_scroll.set_offset(gpui::point(px(0.), px(0.)));
                self.clear_unsent_drafts(window, cx);
                self.roster_open = false;
            }
            self.capture_ended = match (&self.session_key, started) {
                (Some(shown), false) => Some(EndedCapture {
                    session: shown.clone(),
                    generation: self.generation,
                }),
                _ => None,
            };
            self.transcript = Self::following_list(cx);
            self.following_visible = true;
            self.transcript_selected = None;
            self.transcript_hovered = None;
            self.transcript_menu = None;
            self.transcript_marks.clear();
            self.transcript_mark_number = 0;
            self.transcript_rows.clear();
            self.transcript_key = None;
            self.naming_followed = None;
            self.session_key = active;
        }
        self.sync_titles(window, cx);
        self.sync_live_statuses(window, cx);
        self.sync_live_reviews(cx);
        if snapshot_tags != self.snapshot_tags {
            self.tags = snapshot_tags.clone();
            self.snapshot_tags = snapshot_tags;
            self.tag_snapshots += 1;
        }
        self.sync_ticker(cx);
        cx.notify();
    }

    /// One title editor per recording the agent lists as renamable; an
    /// editor follows its recording's title while the user hasn't changed
    /// it, and is dropped once its recording is saved or discarded.
    fn sync_titles(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let targets: Vec<(String, Option<String>)> = self
            .model(cx)
            .title_targets()
            .map(|(s, t)| (s.to_owned(), t.map(str::to_owned)))
            .collect();
        self.titles
            .retain(|session, _| targets.iter().any(|(s, _)| s == session));
        for (session, title) in targets {
            if let Some(editor) = self.titles.get_mut(&session) {
                if editor.seen == title {
                    continue;
                }
                let draft = editor.input.read(cx).value().trim().to_owned();
                if draft == editor.seen.as_deref().unwrap_or("") {
                    let text = title.clone().unwrap_or_default();
                    editor
                        .input
                        .update(cx, |input, cx| input.set_value(text, window, cx));
                }
                editor.seen = title;
                continue;
            }
            let text = title.clone().unwrap_or_default();
            let input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(UNTITLED_MEETING)
                    .default_value(text)
            });
            let target = session.clone();
            let subscription = cx.subscribe_in(
                &input,
                window,
                move |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter { .. } => this.submit_title(&target, window, cx),
                    InputEvent::Change => cx.notify(),
                    _ => {}
                },
            );
            self.titles.insert(
                session,
                TitleEditor {
                    input,
                    revealed: false,
                    rename_focus: cx.focus_handle(),
                    discard_focus: cx.focus_handle(),
                    seen: title,
                    _subscription: subscription,
                },
            );
        }
    }

    /// A picker opened on a row that changed, or that left the preview (the
    /// agent keeps only its latest turns), no longer matches what the user
    /// saw: it closes with nothing saved, and says so on that row (or above
    /// the list once the row is gone), never on another one. Statuses of
    /// rows gone from the preview are dropped, except the latest one.
    fn sync_live_statuses(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.model(cx);
        let closed =
            self.live_naming.as_ref().and_then(|naming| {
                let current = model.turn(&naming.shown.turn_id);
                current.is_none_or(|turn| ShownTurn::of(turn) != naming.shown).then(|| {
                let text = if current.is_none() {
                    LIVE_ROW_GONE
                } else {
                    "This line changed while you were choosing; nothing was saved. Check it \
                     and choose again."
                };
                (naming.shown.clone(), text)
            })
            });
        let latest = self.live_status_turn.clone();
        let before = self.controls.len();
        self.controls.retain(|key, state| {
            key.control != Control::Live
                || state.status == ControlStatus::Pending
                || key.turn == latest
                || key.turn.as_deref().is_some_and(|t| model.turn(t).is_some())
        });
        if self.controls.len() != before {
            self.live_revision += 1;
        }
        if let Some((shown, text)) = closed {
            if self
                .live_naming
                .as_ref()
                .is_some_and(|naming| naming.picker.read(cx).search_focused(window, cx))
            {
                self.transcript_focus.focus(window, cx);
            }
            self.live_naming = None;
            self.set_live_status(
                &shown.session_id,
                &shown.turn_id,
                ControlStatus::Failed(text.into()),
            );
        }
    }

    /// Whether anything shown counts by itself: the recording clock, an
    /// automatic-stop countdown or a retry countdown, in a presented window
    /// connected to the agent. Idle, paused and hidden panels never tick.
    fn needs_tick(&self, cx: &App) -> bool {
        let daemon = self.daemon.read(cx);
        if !self.visible || !daemon.connected {
            return false;
        }
        let model = &daemon.panel;
        model.recording.as_ref().is_some_and(|r| {
            r.status == RecordingStatus::Recording
                || r.auto_stop.as_ref().is_some_and(|a| a.pending.is_some())
        }) || model
            .stopped
            .iter()
            .any(|s| matches!(s.state, StoppedRecordingState::Retrying { .. }))
    }

    /// Starts the once-a-second redraw when something counts, and stops it
    /// when nothing does.
    fn sync_ticker(&mut self, cx: &mut Context<Self>) {
        if !self.needs_tick(cx) {
            self.ticker = None;
            return;
        }
        if self.ticker.is_some() {
            return;
        }
        self.ticker = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let ticking = this.update(cx, |this, cx| {
                    let ticking = this.needs_tick(cx);
                    if ticking {
                        cx.notify();
                    }
                    ticking
                });
                if !matches!(ticking, Ok(true)) {
                    break;
                }
            }
            this.update(cx, |this, _| this.ticker = None).ok();
        }));
    }

    /// Whether the once-a-second redraw runs.
    #[cfg(test)]
    pub(crate) fn ticking(&self) -> bool {
        self.ticker.is_some()
    }

    /// Reads, off the UI thread, how many live names of each saved
    /// recording's published meeting await the user's review; once per
    /// meeting, dropped when the agent no longer lists it.
    fn sync_live_reviews(&mut self, cx: &mut Context<Self>) {
        let saved: Vec<String> = self
            .model(cx)
            .saved
            .iter()
            .map(|s| s.meeting_id.clone())
            .collect();
        self.live_reviews.retain(|id, _| saved.contains(id));
        for meeting_id in saved {
            if self.live_reviews.contains_key(&meeting_id) {
                continue;
            }
            let Ok(id) = MeetingId::parse(&meeting_id) else {
                self.live_reviews.insert(meeting_id, LiveReviews::Unknown);
                continue;
            };
            self.live_reviews
                .insert(meeting_id.clone(), LiveReviews::Reading);
            let library = self.state.read(cx).library().clone();
            let read = cx.background_executor().spawn(async move {
                library
                    .load_meeting(&id)
                    .map(|meeting| broscribe_core::live_identity::review_count(&meeting))
            });
            cx.spawn(async move |this, cx| {
                let reviews = match read.await {
                    Ok(count) => LiveReviews::Count(count),
                    Err(_) => {
                        tracing::warn!("a saved recording's meeting could not be read for review");
                        LiveReviews::Unknown
                    }
                };
                this.update(cx, |this, cx| {
                    if let Some(held) = this.live_reviews.get_mut(&meeting_id)
                        && *held == LiveReviews::Reading
                    {
                        *held = reviews;
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
    }

    /// How many live names of the saved meeting `meeting_id` await review,
    /// once read.
    pub fn live_reviews(&self, meeting_id: &str) -> Option<usize> {
        match self.live_reviews.get(meeting_id)? {
            LiveReviews::Count(count) => Some(*count),
            LiveReviews::Reading | LiveReviews::Unknown => None,
        }
    }

    fn sync_roster(&mut self, cx: &mut Context<Self>) {
        let recording = self.daemon.read(cx).panel.recording.as_ref();
        let key = recording.map_or((None, Vec::new()), |recording| {
            (
                Some(recording.session_id.clone()),
                recording.participants.clone(),
            )
        });
        if self.roster_key == key {
            return;
        }
        if self.roster_key.0 != key.0 {
            self.selected_person = None;
        }
        self.roster_key = key.clone();
        self.confirmed.retain(|id, _| key.1.contains(id));
        for id in &key.1 {
            self.confirmed
                .entry(id.clone())
                .or_insert_with(|| RosterPerson {
                    name: id.clone(),
                    emails: Vec::new(),
                });
        }
        if key.1.is_empty() {
            return;
        }
        let library = self.state.read(cx).library().clone();
        let ids = key.1.clone();
        let load = cx.background_executor().spawn(async move {
            ids.into_iter()
                .map(|id| {
                    let person = broscribe_core::PersonId::parse(&id)
                        .ok()
                        .and_then(|id| library.load_person(&id).ok());
                    let person = person.map_or_else(
                        || RosterPerson {
                            name: id.clone(),
                            emails: Vec::new(),
                        },
                        |p| RosterPerson {
                            name: p.name,
                            emails: p.emails,
                        },
                    );
                    (id, person)
                })
                .collect::<BTreeMap<_, _>>()
        });
        cx.spawn(async move |this, cx| {
            let people = load.await;
            this.update(cx, |this, cx| {
                if this.roster_key == key {
                    this.confirmed = people;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    // ---- rendering parts ----

    fn status_line(&self, key: &ControlKey, cx: &Context<Self>) -> Option<AnyElement> {
        let status = &self.controls.get(key)?.status;
        let id = SharedString::from(
            format!(
                // raw-text-allowed: an element id, never shown.
                "status-{:?}-{}-{}",
                key.control,
                key.session.as_deref().unwrap_or(""),
                key.turn.as_deref().unwrap_or("")
            )
            .to_lowercase(),
        );
        Some(match status {
            ControlStatus::Pending => kit::status_line(
                id,
                kit::StatusKind::Pending,
                match key.control {
                    Control::Start => "Starting…",
                    Control::Discard => "Discarding…",
                    _ => "Waiting for BroScribe Agent…",
                },
                cx,
            )
            .into_any_element(),
            ControlStatus::Failed(text) => {
                kit::status_line(id, kit::StatusKind::Failed, text.clone(), cx).into_any_element()
            }
            ControlStatus::Done { text, meeting } => kit::stack()
                .child(
                    kit::status_line(id, kit::StatusKind::Done, text.clone(), cx)
                        .text_role(TextRole::Caption),
                )
                .when_some(meeting.clone(), |el, link| {
                    el.child(self.meeting_actions(&link.meeting_id, None, link.offer_delete, cx))
                })
                .into_any_element(),
        })
    }

    /// The status line of `control` for recording `session`.
    fn status_for(
        &self,
        control: Control,
        session: &str,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        self.status_line(&ControlKey::new(control, Some(session)), cx)
    }

    fn meeting_actions(
        &self,
        meeting_id: &str,
        title: Option<String>,
        offer_delete: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let open = meeting_id.to_owned();
        let delete = meeting_id.to_owned();
        div()
            .flex()
            .flex_wrap()
            .gap_2()
            .child(
                Button::new(SharedString::from(format!("open-meeting-{meeting_id}")))
                    .label("Open meeting")
                    .small()
                    .when(offer_delete, |button| kit::primary(button, false, cx))
                    .on_click(cx.listener(move |this, _, _, cx| this.open_meeting(&open, cx))),
            )
            .when(offer_delete, |el| {
                el.child(
                    Button::new(SharedString::from(format!("delete-meeting-{meeting_id}")))
                        .label("Delete meeting")
                        .small()
                        .danger()
                        .outline()
                        .map(|button| {
                            crate::ui::kit::disable_with_reason(
                                button,
                                self.delete.is_some(),
                                "Another meeting deletion is already open.",
                                cx,
                            )
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_delete(&delete, title.clone(), window, cx)
                        })),
                )
            })
    }

    fn more_actions(&self, session: &str, busy: bool, cx: &Context<Self>) -> impl IntoElement {
        use gpui::component::popover::Popover;
        let shown = session.to_owned();
        let panel = cx.weak_entity();
        let connected = self.daemon.read(cx).connected;
        let discard_unavailable =
            self.pending(Control::Discard, Some(session)) || self.discard_confirm.is_some();
        Popover::new("recording-more")
            .trigger(
                Button::new("recording-more-button")
                    .label("More")
                    .accessibility_label("More recording actions")
                    .dropdown_caret(true)
                    .ghost()
                    .disabled(busy)
                    .map(|button| disconnected_button(button, connected, cx))
                    .track_focus(&self.more_focus)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.more_open = true;
                        cx.stop_propagation();
                        cx.notify();
                    })),
            )
            .open(self.more_open)
            .on_open_change(cx.listener(|this, open, _, cx| {
                this.more_open = *open;
                cx.notify();
            }))
            .content(move |_, _, cx| {
                let rename_panel = panel.clone();
                let rename_session = shown.clone();
                let process_panel = panel.clone();
                let process_session = shown.clone();
                let discard_panel = panel.clone();
                let discard_session = shown.clone();
                div()
                    .id("recording-more-menu")
                    .role(Role::Menu)
                    .aria_label("More recording actions")
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap_1()
                    .child(
                        Button::new("panel-rename")
                            .label("Rename")
                            .ghost()
                            .role(Role::MenuItem)
                            .on_click(move |_, window, cx| {
                                let _ = rename_panel.update(cx, |this, cx| {
                                    this.more_open = false;
                                    this.reveal_title(&rename_session, window, cx);
                                });
                            }),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                Button::new("panel-process")
                                    .label("Process now")
                                    .tooltip(PROCESS_NOW_EXPLANATION)
                                    .ghost()
                                    .role(Role::MenuItem)
                                    .map(|button| {
                                        crate::ui::kit::disable_with_reason(
                                            button,
                                            busy,
                                            "Waiting for BroScribe Agent to answer this action.",
                                            cx,
                                        )
                                    })
                                    .on_click(move |_, window, cx| {
                                        let _ = process_panel.update(cx, |this, cx| {
                                            this.more_open = false;
                                            if this.model(cx).active_session()
                                                == Some(process_session.as_str())
                                            {
                                                this.session_action(
                                                    SessionCommand::ProcessNow,
                                                    window,
                                                    cx,
                                                );
                                            }
                                            cx.notify();
                                        });
                                    }),
                            )
                            .child(
                                text_line(
                                    "panel-process-caption",
                                    Role::Label,
                                    PROCESS_NOW_EXPLANATION,
                                )
                                .text_role(TextRole::Caption),
                            ),
                    )
                    .child(div().w_full().border_t_1().border_color(cx.theme().border))
                    .child(
                        Button::new("panel-discard")
                            .label("Discard")
                            .ghost()
                            .text_color(cx.theme().danger)
                            .disabled(discard_unavailable || busy)
                            .role(Role::MenuItem)
                            .map(|button| {
                                crate::ui::kit::disable_with_reason(
                                    button,
                                    busy,
                                    "Waiting for BroScribe Agent to answer this action.",
                                    cx,
                                )
                            })
                            .on_click(move |_, window, cx| {
                                let _ = discard_panel.update(cx, |this, cx| {
                                    this.more_open = false;
                                    this.present_discard(
                                        discard_session.clone(),
                                        this.more_focus.clone(),
                                        window,
                                        cx,
                                    );
                                    cx.notify();
                                });
                            }),
                    )
            })
    }

    /// Turns automatic stop on or off for the active recording, or links it
    /// to a fresh detected call, against the shown binding generation. An
    /// agent without automatic stop gets nothing to send.
    pub fn set_auto_stop(
        &mut self,
        enabled: bool,
        app_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(recording) = &self.model(cx).recording else {
            return;
        };
        let Some(shown) = recording.auto_stop.as_ref() else {
            return;
        };
        let session = recording.session_id.clone();
        let call = auto_stop_call(&session, shown, enabled, app_id);
        self.submit(
            ControlKey::new(Control::AutoStop, Some(&session)),
            call,
            window,
            cx,
            move |this, value, current, _, cx| {
                let Ok(status) = serde_json::from_value::<AutoStopStatus>(value) else {
                    return ControlStatus::Failed(
                        "BroScribe Agent answered in an unexpected form.".into(),
                    );
                };
                if current {
                    let answer = status.clone();
                    this.daemon.update(cx, |daemon, cx| {
                        if daemon.panel.apply_auto_stop(&session, answer) {
                            cx.notify();
                        }
                    });
                }
                auto_stop_outcome(&status)
            },
        );
    }

    fn open_roster(
        &mut self,
        person_id: Option<String>,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.daemon.read(cx).connected {
            return;
        }
        self.selected_person = person_id;
        self.roster_open = true;
        self.picker.update(cx, |picker, cx| {
            picker.set_query(query, window, cx);
            picker.focus_search(window, cx);
        });
        cx.notify();
    }

    fn participant_section(
        &self,
        recording: &RecordingState,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let model = self.model(cx);
        let connected = self.daemon.read(cx).connected;
        let session = recording.session_id.as_str();
        let muted = cx.theme().muted_foreground;
        let roster_busy = self.pending(Control::Roster, Some(session));
        let row = |id: SharedString, name: String, caption: Stateful<gpui::Div>, action: Button| {
            div()
                .id(id)
                .role(Role::ListItem)
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w(gpui::rems(7.))
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(text_line("participant-name", Role::Label, name))
                        .child(caption),
                )
                .child(action)
        };
        let unconfirmed = |id: SharedString, person_id: Option<String>, name: String, caption| {
            let query = name.clone();
            let confirm = Button::new(format!("confirm-{id}"))
                .label("Confirm…")
                .accessibility_label(format!("Confirm {name} as a participant"))
                .small()
                .outline()
                .map(|button| disconnected_button(button, connected, cx))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_roster(person_id.clone(), &query, window, cx);
                }));
            row(
                id,
                name,
                text_line("participant-state", Role::Label, caption)
                    .text_role(TextRole::Caption)
                    .text_color(muted),
                confirm,
            )
        };
        let people = div()
            .id("panel-people")
            .role(Role::List)
            .aria_label("Recording participants")
            .flex()
            .flex_col()
            .gap_2()
            .children(
                recording
                    .observed_roster
                    .iter()
                    .enumerate()
                    .filter(|(_, member)| !observed_confirmed(member, &self.confirmed))
                    .map(|(ix, member)| {
                        unconfirmed(
                            format!("observed-person-{ix}").into(),
                            None,
                            member.name.clone(),
                            "seen in the call · not confirmed",
                        )
                    }),
            )
            .children(
                model
                    .people
                    .iter()
                    .filter(|(id, _)| !self.confirmed.contains_key(*id))
                    .map(|(id, name)| {
                        unconfirmed(
                            format!("person-{id}").into(),
                            Some(id.clone()),
                            name.clone(),
                            "named live · not confirmed",
                        )
                    }),
            )
            .children(
                self.confirmed
                    .iter()
                    .enumerate()
                    .map(|(index, (id, person))| {
                        let target = id.clone();
                        let remove = Button::new(SharedString::from(format!("remove-{id}")))
                            .label("Remove")
                            .accessibility_label(format!(
                                "Remove {} from participants",
                                person.name
                            ))
                            .small()
                            .ghost()
                            .map(|button| {
                                kit::disable_with_reason(
                                    button,
                                    roster_busy,
                                    "Waiting for BroScribe Agent to confirm attendance.",
                                    cx,
                                )
                            })
                            .map(|button| disconnected_button(button, connected, cx))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.remove_participant(target.clone(), window, cx)
                            }));
                        row(
                            format!("panel-person-{index}").into(),
                            person.name.clone(),
                            kit::status_line(
                                "participant-attended",
                                kit::StatusKind::Done,
                                "attended",
                                cx,
                            ),
                            remove,
                        )
                    }),
            );

        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(crate::ui::subheading(
                "panel-people-heading",
                "Participants",
            ))
            .child(
                text_line(
                    "panel-attendance-note",
                    Role::Label,
                    "Who attended. Confirming someone doesn’t teach BroScribe their voice.",
                )
                .text_role(TextRole::Caption)
                .text_color(muted),
            )
            .child(people)
            .children(self.status_for(Control::Roster, session, cx))
            .child(
                Button::new("show-participant-picker")
                    .label("Add participant…")
                    .small()
                    .ghost()
                    .self_start()
                    .track_focus(&self.roster_focus)
                    .map(|button| disconnected_button(button, connected, cx))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.roster_open {
                            this.picker
                                .update(cx, |picker, cx| picker.focus_search(window, cx));
                        } else {
                            this.open_roster(None, "", window, cx);
                        }
                    })),
            )
            .when(self.roster_open && connected, |el| {
                el.child(crate::ui::subheading(
                    "add-participant-title",
                    "Add a participant",
                ))
                .child(self.picker.clone())
            })
            .children(self.status_for(Control::Participant, session, cx))
    }

    fn auto_stop_section(
        &self,
        recording: &RecordingState,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let connected = self.daemon.read(cx).connected;
        let session = recording.session_id.as_str();
        let status = recording.auto_stop.as_ref();
        let busy = self.pending(Control::AutoStop, Some(session));
        let mut section = div()
            .id("auto-stop")
            .role(Role::Group)
            .aria_label("Automatic stop")
            .flex()
            .flex_col()
            .gap_1()
            .text_role(TextRole::Caption)
            .children(
                reader_notice(
                    recording.reader.as_ref(),
                    recording.reader_health.as_ref(),
                    status
                        .and_then(|status| status.app.as_ref())
                        .map(|app| app.app_name.as_str()),
                )
                .map(|text| {
                    text_line("meeting-reader", Role::Status, text)
                        .text_color(theme.muted_foreground)
                }),
            );
        if let Some(app) = status.and_then(|status| status.app.as_ref()) {
            let enabled = status.is_some_and(|status| status.enabled);
            section = section.child(
                Switch::new("auto-stop-switch")
                    .label(format!("Save when the {} call ends", app.app_name))
                    .checked(enabled)
                    .disabled(busy || !connected)
                    .on_change(cx.listener(|this, checked, window, cx| {
                        this.set_auto_stop(*checked, None, window, cx);
                    })),
            );
        }
        section = section.child(text_line(
            "auto-stop-summary",
            if status.is_some_and(|status| status.app.is_some()) {
                Role::Label
            } else {
                Role::Status
            },
            auto_stop_summary(status),
        ));
        if let Some(status) = status {
            let mut actions = div().flex().flex_wrap().gap_2();
            // Linking is always explicit: one button per fresh detected call.
            if status.pending.is_none() {
                let bound = status.app.as_ref().map(|a| a.app_id.as_str());
                for candidate in status
                    .candidates
                    .iter()
                    .filter(|c| Some(c.app_id.as_str()) != bound)
                {
                    let app_id = candidate.app_id.clone();
                    let label = if bound.is_some() {
                        format!("Stop when the {} call ends instead", candidate.app_name)
                    } else {
                        format!("Stop when the {} call ends", candidate.app_name)
                    };
                    actions = actions.child(
                        Button::new(SharedString::from(format!("auto-stop-link-{app_id}")))
                            .label(label)
                            .map(|button| {
                                crate::ui::kit::disable_with_reason(
                                    button,
                                    busy,
                                    "Waiting for BroScribe Agent to answer this action.",
                                    cx,
                                )
                            })
                            .map(|button| disconnected_button(button, connected, cx))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.set_auto_stop(true, Some(app_id.clone()), window, cx)
                            })),
                    );
                }
            }
            section = section.child(actions);
        }
        section.children(self.status_for(Control::AutoStop, session, cx))
    }

    fn auto_stop_attention(
        &self,
        recording: &RecordingState,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let status = recording.auto_stop.as_ref()?;
        let pending = status.pending.as_ref()?;
        Some(
            kit::stack()
                .child(kit::status_line_role(
                    "auto-stop-pending",
                    kit::StatusKind::Attention,
                    Role::Alert,
                    format!(
                        "{}. Saving automatically unless you keep recording.",
                        auto_stop_reason(pending.reason, status.app.as_ref())
                    ),
                    cx,
                ))
                .child(
                    Button::new("auto-stop-keep")
                        .label("Keep recording")
                        .tooltip("Turn off automatic stop for this recording")
                        .map(|button| {
                            kit::primary(
                                button,
                                self.pending(Control::AutoStop, Some(&recording.session_id)),
                                cx,
                            )
                        })
                        .track_focus(&self.urgent_control)
                        .small()
                        .self_start()
                        .disabled(self.pending(Control::AutoStop, Some(&recording.session_id)))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.set_auto_stop(false, None, window, cx)
                        })),
                )
                .children(self.status_for(Control::AutoStop, &recording.session_id, cx))
                .into_any_element(),
        )
    }

    fn recording_visor(&self, recording: &RecordingState, cx: &App) -> Stateful<gpui::Div> {
        let model = self.model(cx);
        let now = Instant::now();
        let elapsed = model.elapsed_s(now);
        let paused = recording.status == RecordingStatus::Paused;
        let state = if paused { "Paused" } else { "Recording" };
        if !self.daemon.read(cx).connected {
            return visor(
                kit::Face::Attention,
                "Not connected",
                None,
                Some(format!(
                    "Last known: {} · {}",
                    state.to_lowercase(),
                    format_clock(elapsed)
                )),
                cx,
            );
        }
        if let Some(auto_stop) = &recording.auto_stop
            && let Some(pending) = &auto_stop.pending
        {
            let remaining = auto_stop_remaining(pending.remaining_s, model.since_state(now));
            let context = auto_stop.app.as_ref().map_or_else(
                || "Call ended".into(),
                |app| format!("{} call ended", app.app_name),
            );
            return visor(
                kit::Face::Recording,
                "Recording",
                Some((
                    "·",
                    timer_line(
                        "auto-stop-countdown",
                        "Saving in",
                        spoken_seconds(remaining),
                        format!("Saving in {}", crate::ui::format::seconds(remaining)),
                    )
                    .into_any_element(),
                )),
                Some(context),
                cx,
            );
        }
        visor(
            if paused {
                kit::Face::Paused
            } else {
                kit::Face::Recording
            },
            state,
            Some((
                "·",
                timer_line(
                    "visor-elapsed",
                    "Elapsed time",
                    spoken_position(elapsed),
                    format_clock(elapsed),
                )
                .into_any_element(),
            )),
            Some(recording_context(recording)),
            cx,
        )
    }

    fn new_idle_visor(&self, cx: &Context<Self>) -> (AnyElement, bool) {
        let model = self.model(cx);
        let target = self
            .notice_target
            .as_deref()
            .and_then(|id| model.stopped_session(id));
        let stopped = target.or_else(|| model.stopped.iter().max_by_key(|entry| entry.stopped_at));
        let saved = model.saved.iter().max_by_key(|entry| entry.saved_at);
        if let Some(saved) = saved
            && target.is_none()
            && stopped.is_none_or(|stopped| stopped.stopped_at <= saved.saved_at)
        {
            let count = self.live_reviews(&saved.meeting_id).unwrap_or(0);
            let state = saved_words(count);
            let target = saved.meeting_id.clone();
            return (
                kit::stack()
                    .child(visor(
                        kit::Face::Saved,
                        state,
                        None,
                        Some(meeting_title(saved.title.as_deref()).to_owned()),
                        cx,
                    ))
                    .child(
                        Button::new("visor-open")
                            .label("Open meeting")
                            .map(|button| kit::primary(button, false, cx))
                            .self_start()
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.open_meeting(&target, cx)),
                            ),
                    )
                    .into_any_element(),
                true,
            );
        }
        if let Some(stopped) = stopped {
            use StoppedRecordingState::*;
            let mut time = None;
            let (face, words) = match stopped.state {
                Queued { .. } => (kit::Face::Waiting, stopped_summary(&stopped.state)),
                Running { fraction } => {
                    if let Some(percent) = fraction.and_then(crate::ui::format::progress_percent) {
                        time = Some((
                            "·",
                            text_line("visor-progress", Role::Label, percent).into_any_element(),
                        ));
                        (kit::Face::Working, "Transcribing".into())
                    } else {
                        (kit::Face::Working, "Transcribing…".into())
                    }
                }
                Retrying { retry_in_s, .. } => {
                    let remaining =
                        retry_countdown_remaining(retry_in_s, model.since_state(Instant::now()));
                    time = Some((
                        "",
                        timer_line(
                            "visor-retry-countdown",
                            "Retrying in",
                            spoken_seconds(remaining),
                            crate::ui::format::seconds(remaining),
                        )
                        .into_any_element(),
                    ));
                    (kit::Face::Attention, "Retrying in".into())
                }
                Failed { .. } => (kit::Face::Failed, "Couldn't transcribe".into()),
                Discarding => (kit::Face::Working, "Discarding…".into()),
            };
            let failure = save_failure(
                &stopped.state,
                model
                    .journal_issues
                    .iter()
                    .find(|issue| issue.session_id == stopped.session_id),
            );
            let retry = failure == Some(SaveFailure::Manual);
            return (
                kit::stack()
                    .child(visor(
                        face,
                        words,
                        time,
                        Some(meeting_title(stopped.title.as_deref()).to_owned()),
                        cx,
                    ))
                    .when(retry, |stack| {
                        stack.child(self.retry_button(
                            "visor-retry".into(),
                            &stopped.session_id,
                            cx,
                        ))
                    })
                    .into_any_element(),
                retry,
            );
        }
        (
            visor(kit::Face::Waiting, "Not recording", None, None, cx).into_any_element(),
            false,
        )
    }

    fn retry_button(&self, id: SharedString, session: &str, cx: &Context<Self>) -> Button {
        let target = session.to_owned();
        let is_notice_target = self.notice_target.as_deref() == Some(session)
            && ((self.model(cx).recording.is_none() && id == "visor-retry")
                || (self.model(cx).recording.is_some()
                    && id.as_ref() == format!("retry-{session}")));
        let selector = id.to_string();
        Button::new(id)
            .debug_selector(move || selector)
            .label("Try again")
            .map(|button| {
                kit::primary(
                    button,
                    self.pending(Control::Retry, Some(session)) || !self.daemon.read(cx).connected,
                    cx,
                )
            })
            .small()
            .self_start()
            .when(is_notice_target, |button| {
                button.track_focus(&self.notice_focus)
            })
            .tooltip("Process this recording again")
            .disabled(self.pending(Control::Retry, Some(session)))
            .map(|button| disconnected_button(button, self.daemon.read(cx).connected, cx))
            .on_click(
                cx.listener(move |this, _, window, cx| this.retry(target.clone(), window, cx)),
            )
    }

    fn settings_link(
        &self,
        id: impl Into<gpui::ElementId>,
        page: SettingsPage,
        cx: &Context<Self>,
    ) -> Button {
        Button::new(id)
            .label(format!("Open {} settings", page.title()))
            .link()
            .small()
            .self_start()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.state
                    .update(cx, |_, cx| cx.emit(AppEvent::OpenSettings(Some(page))));
            }))
    }

    fn stop_reason_row(
        &self,
        stopped: &StoppedRecording,
        attention: bool,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let reason = stopped.stop_reason.as_deref()?;
        let session = stopped.session_id.clone();
        let page = match reason {
            "disk_low" | "disk_check_failed" => Some(SettingsPage::Storage),
            "capture_device_unavailable" => Some(SettingsPage::Recording),
            _ => None,
        };
        let id = |suffix| {
            SharedString::from(format!(
                "stop-{suffix}-{}-{}",
                if attention { "attention" } else { "row" },
                stopped.session_id
            ))
        };
        Some(
            kit::stack()
                .child(kit::status_line_role(
                    id("reason"),
                    kit::StatusKind::Attention,
                    if attention { Role::Alert } else { Role::Label },
                    stop_reason_text(reason),
                    cx,
                ))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .children(page.map(|page| {
                            self.settings_link(id("settings"), page, cx)
                                .outline()
                                .role(Role::Button)
                        }))
                        .when(attention, |row| {
                            row.child(
                                Button::new(id("dismiss"))
                                    .label("Dismiss")
                                    .ghost()
                                    .small()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.stop_reason_dismissed = Some(session.clone());
                                        cx.notify();
                                    })),
                            )
                        }),
                )
                .into_any_element(),
        )
    }

    /// Per-track capture, dropped audio and disk headroom (RT21/RT22).
    /// Abnormal lines go to Needs attention. Other diagnostics go to Capture.
    fn health_lines(
        &self,
        recording: &RecordingState,
        urgent: bool,
        cx: &App,
    ) -> Vec<Stateful<gpui::Div>> {
        let theme = cx.theme();
        let warnings: Vec<&CaptureWarning> = self.model(cx).capture_warnings().collect();
        let line = |id: (&'static str, usize), text: String, attention: bool| {
            text_line(id, if attention { Role::Alert } else { Role::Status }, text)
                .when(attention, |el| el.text_color(theme.danger))
        };
        let mut lines = if urgent {
            disk_lines(recording.health.disk.as_ref(), &warnings, true, cx)
        } else {
            Vec::new()
        };
        for health in &recording.health.tracks {
            let (text, attention) = track_status_text(health);
            let track_ix = health.track.index();
            if attention == urgent {
                lines.push(kit::status_line(
                    ("track-health", track_ix),
                    if urgent {
                        kit::StatusKind::Attention
                    } else {
                        kit::StatusKind::Info
                    },
                    text,
                    cx,
                ));
            }
            if urgent && health.dropped_frames > 0 {
                lines.push(
                    text_line(
                        ("track-dropped", track_ix),
                        Role::Label,
                        format!("{} audio frames lost before saving", health.dropped_frames),
                    )
                    .text_color(theme.danger),
                );
            }
        }
        for (ix, warning) in warnings.into_iter().enumerate() {
            // A track the state already reports as failing says so above,
            // and a low disk is always one of the disk lines above.
            let shown_above = match warning {
                CaptureWarning::TrackDown { track, .. } => recording
                    .health
                    .tracks
                    .iter()
                    .any(|h| h.track == *track && h.state != TrackState::Capturing),
                CaptureWarning::DiskLow { .. } => true,
                _ => false,
            };
            if shown_above {
                continue;
            }
            let attention = !matches!(warning, CaptureWarning::TrackResumed { .. });
            if attention == urgent {
                lines.push(line(
                    ("capture-warning", ix),
                    capture_warning_text(warning),
                    attention,
                ));
            }
        }
        lines
    }

    fn reveal_title(&mut self, session: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.daemon.read(cx).connected {
            return;
        }
        let active = self.model(cx).active_session() == Some(session);
        let Some(editor) = self.titles.get_mut(session) else {
            return;
        };
        editor.revealed = true;
        self.temporarily_expanded = true;
        if active {
            self.tab = RecordingPanelTab::Details;
            self.details_scroll.scroll_to_top_of_item(0);
        }
        let focus = editor.input.read(cx).focus_handle(cx);
        window.on_next_frame(move |window, cx| focus.focus(window, cx));
        cx.notify();
    }

    fn cancel_title(&mut self, session: &str, window: &mut Window, cx: &mut Context<Self>) {
        let active = self.model(cx).active_session() == Some(session);
        let Some(editor) = self.titles.get_mut(session) else {
            return;
        };
        editor.revealed = false;
        let value = editor.seen.clone().unwrap_or_default();
        editor
            .input
            .update(cx, |input, cx| input.set_value(value, window, cx));
        if active {
            self.more_focus.focus(window, cx);
        } else {
            editor.rename_focus.focus(window, cx);
        }
        cx.stop_propagation();
        cx.notify();
    }

    fn title_editor(&self, session: &str, cx: &Context<Self>) -> Option<AnyElement> {
        let editor = self.titles.get(session)?;
        let connected = self.daemon.read(cx).connected;
        let can_save =
            !self.pending(Control::Title, Some(session)) && self.title_draft(session, cx).is_some();
        let target = session.to_owned();
        let cancel_target = target.clone();
        let input_target = target.clone();
        let component_target = target.clone();
        let key_target = target.clone();
        Some(
            div()
                .id(SharedString::from(format!("title-editor-{session}")))
                .key_context("RecordingTitleEditor")
                .flex()
                .flex_col()
                .gap_1()
                .capture_action(cx.listener(
                    move |this, _: &crate::ui::keymap::CancelTransient, window, cx| {
                        this.cancel_title(&cancel_target, window, cx);
                    },
                ))
                .capture_action(cx.listener(
                    move |this, _: &gpui::component::input::Escape, window, cx| {
                        this.cancel_title(&input_target, window, cx);
                    },
                ))
                .on_action(
                    cx.listener(move |this, _: &gpui::base::actions::Cancel, window, cx| {
                        this.cancel_title(&component_target, window, cx);
                    }),
                )
                .on_key_down(
                    cx.listener(move |this, ev: &gpui::KeyDownEvent, window, cx| {
                        if ev.keystroke.key == "escape" && !ev.keystroke.modifiers.modified() {
                            this.cancel_title(&key_target, window, cx);
                        }
                    }),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .child(
                            div().flex_1().min_w(px(160.)).child(
                                Input::new(&editor.input)
                                    .aria_label("Meeting title")
                                    .disabled(!connected),
                            ),
                        )
                        .child(
                            Button::new(SharedString::from(format!("panel-title-{session}")))
                                .label("Rename")
                                .map(|button| kit::disable_action(button, !can_save, cx))
                                .map(|button| disconnected_button(button, connected, cx))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.submit_title(&target, window, cx)
                                })),
                        ),
                )
                .children(self.status_for(Control::Title, session, cx))
                .into_any_element(),
        )
    }

    /// Each stopped recording not saved yet, as itself: what it is doing
    /// (RT24), its own title editor, Retry once its processing failed, and
    /// Discard; then the recordings saved during this agent run.
    fn stopped_rows(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let model = self.model(cx);
        let since = model.since_state(Instant::now());
        let mut rows = Vec::new();
        let connected = self.daemon.read(cx).connected;
        for stopped in &model.stopped {
            let title = meeting_title(stopped.title.as_deref());
            let session = stopped.session_id.as_str();
            let issue = model
                .journal_issues
                .iter()
                .find(|issue| issue.session_id == session);
            let failure = save_failure(&stopped.state, issue);
            let mut row = kit::card(SharedString::from(format!("stopped-{session}")), cx)
                .when(self.notice_target.as_deref() == Some(session), |row| {
                    row.anchor_scroll(Some(self.notice_anchor.clone()))
                })
                .role(Role::Group)
                .aria_label("Stopped recording")
                .child(text_line(
                    SharedString::from(format!("stopped-title-{session}")),
                    Role::Label,
                    title.to_owned(),
                ))
                .children(self.stop_reason_row(stopped, false, cx));
            use StoppedRecordingState::*;
            let id = SharedString::from(format!("stopped-state-{session}"));
            match (&stopped.state, failure) {
                (
                    Retrying {
                        attempts,
                        retry_in_s,
                        error,
                    },
                    Some(SaveFailure::Automatic { .. }),
                ) => {
                    let remaining = retry_countdown_remaining(*retry_in_s, since);
                    let time = crate::ui::format::seconds(remaining);
                    let words = if *attempts == 0 {
                        format!("Retrying in {time}")
                    } else {
                        format!("Retrying in {time} · attempt {attempts}")
                    };
                    let cause = stopped_cause(error.as_deref()).unwrap_or_default();
                    row = row
                        .child(kit::status_line(
                            SharedString::from(format!(
                                "stopped-state-{session}-attempt-{attempts}"
                            )),
                            kit::StatusKind::Attention,
                            failure_text("Couldn't transcribe.", cause, ""),
                            cx,
                        ))
                        .child(
                            timer_line(
                                SharedString::from(format!("retry-countdown-{session}")),
                                "Retrying in",
                                spoken_seconds(remaining),
                                words,
                            )
                            .text_role(TextRole::Caption),
                        );
                }
                (Failed { error }, Some(SaveFailure::Manual)) => {
                    row = row
                        .child(kit::status_line(
                            id,
                            kit::StatusKind::Failed,
                            "Couldn't transcribe",
                            cx,
                        ))
                        .child(text_line(
                            SharedString::from(format!("stopped-cause-{session}")),
                            Role::Label,
                            failure_text(
                                "",
                                stopped_cause(error.as_deref()).unwrap_or_default(),
                                "The recording is kept.",
                            ),
                        ));
                }
                (Failed { error } | Retrying { error, .. }, Some(failure)) => {
                    row = row.child(kit::status_line(
                        id,
                        kit::StatusKind::Failed,
                        save_failure_text(failure, error.as_deref()),
                        cx,
                    ));
                }
                (Running { fraction }, _)
                    if fraction
                        .and_then(crate::ui::format::progress_value)
                        .is_some() =>
                {
                    let percent = fraction
                        .and_then(crate::ui::format::progress_value)
                        .unwrap();
                    row = row
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .gap_x_1()
                                .child(kit::status_line(
                                    id,
                                    kit::StatusKind::Pending,
                                    "Transcribing",
                                    cx,
                                ))
                                .child(
                                    text_line(
                                        SharedString::from(format!("stopped-progress-{session}")),
                                        Role::Label,
                                        format!("· {}", crate::ui::format::percent(percent)),
                                    )
                                    .text_role(TextRole::Caption),
                                ),
                        )
                        .child(kit::progress(
                            format!("stopped-progress-bar-{session}"),
                            format!("Transcribing {title}"),
                            f32::from(percent) / 100.,
                        ));
                }
                _ => {
                    row = row.child(kit::status_line(
                        id,
                        kit::StatusKind::Pending,
                        stopped_summary(&stopped.state),
                        cx,
                    ));
                }
            }
            if stopped.state.is_pending_save() {
                let mut actions = div().flex().flex_wrap().gap_2();
                // Only a failure the agent retries on request: never one it
                // retries by itself or a journal kept for repair.
                if failure == Some(SaveFailure::Manual) {
                    actions = actions.child(self.retry_button(
                        format!("retry-{session}").into(),
                        session,
                        cx,
                    ));
                }
                let target = session.to_owned();
                let rename_target = target.clone();
                let opener = self
                    .titles
                    .get(session)
                    .map(|editor| editor.discard_focus.clone());
                let focus = opener.clone();
                actions = actions
                    .when_some(self.titles.get(session), |actions, editor| {
                        actions.child(
                            Button::new(SharedString::from(format!("rename-{session}")))
                                .label("Rename")
                                .small()
                                .ghost()
                                .track_focus(&editor.rename_focus)
                                .map(|button| disconnected_button(button, connected, cx))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.reveal_title(&rename_target, window, cx);
                                })),
                        )
                    })
                    .child(
                        Button::new(SharedString::from(format!("discard-{session}")))
                            .label("Discard")
                            .small()
                            .when_some(opener, |button, opener| button.track_focus(&opener))
                            .danger()
                            .outline()
                            .disabled(
                                self.pending(Control::Discard, Some(session))
                                    || self.discard_confirm.is_some(),
                            )
                            .map(|button| disconnected_button(button, connected, cx))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                let opener =
                                    focus.clone().unwrap_or_else(|| this.more_focus.clone());
                                this.present_discard(target.clone(), opener, window, cx);
                            })),
                    );
                row = row
                    .child(actions)
                    .when(
                        self.titles.get(session).is_some_and(|editor| {
                            editor.revealed || self.title_draft(session, cx).is_some()
                        }),
                        |row| row.children(self.title_editor(session, cx)),
                    )
                    .when(
                        self.titles.get(session).is_some_and(|editor| {
                            !editor.revealed && self.title_draft(session, cx).is_none()
                        }),
                        |row| row.children(self.status_for(Control::Title, session, cx)),
                    );
            } else {
                row = row.children(self.status_for(Control::Title, session, cx));
            }
            for control in [
                Control::Session,
                Control::Retry,
                Control::Discard,
                Control::ResolveJournal,
            ] {
                row = row.children(self.status_for(control, session, cx));
            }
            rows.push(row.into_any_element());
        }
        for saved in &model.saved {
            let title = saved.title.clone();
            let session = saved.session_id.as_str();
            let mut row = kit::card(SharedString::from(format!("saved-{session}")), cx)
                .role(Role::Group)
                .aria_label("Saved recording")
                .child(text_line(
                    SharedString::from(format!("saved-recording-label-{session}")),
                    Role::Label,
                    meeting_title(title.as_deref()).to_owned(),
                ))
                .child(kit::status_line(
                    SharedString::from(format!("saved-state-{session}")),
                    kit::StatusKind::Done,
                    "Saved",
                    cx,
                ))
                .children(
                    self.live_reviews(&saved.meeting_id)
                        .filter(|count| *count > 0)
                        .map(|count| {
                            kit::status_line(
                                SharedString::from(format!("live-review-{session}")),
                                kit::StatusKind::Attention,
                                format!(
                                    "{count} {} to review",
                                    if count == 1 { "name" } else { "names" }
                                ),
                                cx,
                            )
                            .tooltip(kit::title_tooltip(
                                live_review_notice(count).unwrap_or_default(),
                            ))
                        }),
                )
                .child(self.meeting_actions(&saved.meeting_id, title, true, cx));
            for control in [
                Control::Session,
                Control::Title,
                Control::Retry,
                Control::Discard,
                Control::ResolveJournal,
            ] {
                row = row.children(self.status_for(control, session, cx));
            }
            rows.push(row.into_any_element());
        }
        rows
    }

    /// Results about recordings the agent no longer lists (a discarded one,
    /// a deleted meeting): shown on their own, never on another recording.
    fn unlisted_statuses(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        let model = self.model(cx);
        self.controls
            .keys()
            .filter(|key| key.control.per_recording())
            .filter(|key| {
                key.session.as_deref().is_none_or(|s| {
                    !model.knows_session(s)
                        && !model.saved.iter().any(|r| r.session_id == s)
                        && !(key.control == Control::ResolveJournal
                            && model
                                .journal_issues
                                .iter()
                                .any(|issue| issue.session_id == s))
                })
            })
            .filter_map(|key| self.status_line(key, cx))
            .collect()
    }

    /// The shared "Start BroScribe Agent" request's own line (starting,
    /// waiting for the agent, not installed, failed), its message as its
    /// accessible text; nothing when idle.
    pub(super) fn agent_start_notice(&self, cx: &App) -> Option<Stateful<gpui::Div>> {
        let start =
            AgentStartStatus::unavailable(self.daemon.read(cx), self.agent.read(cx).status())?;
        let text = start.message()?;
        let kind = if start.is_failure() {
            kit::StatusKind::Failed
        } else if start.is_pending() {
            kit::StatusKind::Pending
        } else {
            kit::StatusKind::Attention
        };
        Some(kit::status_line("agent-start", kind, text, cx))
    }

    fn notification_card(
        &self,
        notice: &broscribe_ipc::NotificationRequest,
        first_ask: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let ask = notice.is_recording_prompt();
        let id = &notice.id;
        let title = notice.title.clone();
        let body = notice.body.clone();
        let mut card = if ask {
            kit::confirm_box(kit::ConfirmTone::Warning, cx)
                .id(SharedString::from(format!("prompt-{id}")))
                .spoken(Role::Alert, format!("{title}. {body}"))
                .child(div().text_role(TextRole::Section).child(title))
                .child(body)
        } else {
            kit::card(SharedString::from(format!("prompt-{id}")), cx)
                .child(crate::ui::heading(
                    SharedString::from(format!("prompt-{id}-title")),
                    title,
                ))
                .child(text_line(
                    SharedString::from(format!("prompt-{id}-body")),
                    Role::Label,
                    body,
                ))
        };
        let mut actions = div().flex().flex_wrap().gap_2();
        // A native body click has an explicit accessible counterpart here.
        let implicit_open = (notice.meeting_id.is_some()
            && !notice.actions.iter().any(|action| action.id == "open"))
        .then(|| broscribe_ipc::NotificationButton {
            id: "open".into(),
            label: "Open meeting".into(),
        });
        for action in notice.actions.iter().chain(implicit_open.iter()) {
            let record = ask && action.id == "record";
            let label = if action.id == "open" && notice.meeting_id.is_some() {
                "Open meeting".to_owned()
            } else {
                action.label.clone()
            };
            let id = id.clone();
            let action_id = action.id.clone();
            actions = actions.child(
                Button::new(SharedString::from(format!("prompt-{id}-{action_id}")))
                    .map(|button| {
                        if record {
                            // The record dot sits before the words.
                            button
                                .accessibility_label(label.clone())
                                .child(kit::record_dot(true, false, cx))
                                .child(label)
                        } else {
                            button.label(label)
                        }
                    })
                    .small()
                    .when(record, |button| kit::primary(button, false, cx))
                    .when(!record, |button| button.outline())
                    .when(record && first_ask, |button| {
                        button.track_focus(&self.urgent_control)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.notifications
                            .update(cx, |host, cx| host.act(&id, &action_id, cx));
                    })),
            );
        }
        if notice.actions.is_empty() {
            let id = id.clone();
            actions = actions.child(
                Button::new(SharedString::from(format!("dismiss-{id}")))
                    .label("Dismiss")
                    .small()
                    .ghost()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.notifications
                            .update(cx, |host, cx| host.dismiss_local(&id, cx));
                    })),
            );
        }
        card = card.child(actions);
        card.into_any_element()
    }
}

/// The journal disk's lines while recording (RT21, RT22): one live alert
/// for the level, spoken when the level changes, and non-live labels for
/// the free space, headroom and stop thresholds, which follow every update.
///
/// The agent sends its one-off low-disk warning and the health snapshot
/// that reports the new level separately, in either order. The model drops
/// that warning once a later snapshot reports the disk Ok, so a warning it
/// still holds is newer than any Ok snapshot shown. The level is the
/// health's once it reports Warning or Low, else Warning while the agent's
/// warning is held (with that warning's numbers). Warning from either
/// source is the same node with the same words, so whichever arrives second
/// says nothing new.
fn disk_lines(
    health: Option<&DiskHealth>,
    warnings: &[&CaptureWarning],
    urgent: bool,
    cx: &App,
) -> Vec<Stateful<gpui::Div>> {
    let theme = cx.theme();
    let warned = warnings.iter().find_map(|w| match w {
        CaptureWarning::DiskLow {
            free_bytes,
            headroom_s,
            stop_below_bytes,
            stop_below_s,
        } => Some((
            DiskHealth {
                free_bytes: *free_bytes,
                headroom_s: *headroom_s,
                level: DiskLevel::Warning,
            },
            (*stop_below_bytes, *stop_below_s),
        )),
        _ => None,
    });
    let disk = match (health, warned) {
        (Some(health), _) if health.level != DiskLevel::Ok => *health,
        (_, Some((warned, _))) => warned,
        (Some(health), None) => *health,
        (None, None) => return Vec::new(),
    };
    if urgent != (disk.level != DiskLevel::Ok) {
        return Vec::new();
    }
    let alert = disk_alert_text(disk.level).map(|alert| {
        kit::status_line_role(
            ("disk-health-alert", 0usize),
            kit::StatusKind::Attention,
            Role::Alert,
            alert,
            cx,
        )
    });
    let space = div()
        .id(("disk-health", 0usize))
        .spoken(Role::Label, spoken_disk_space(&disk))
        .text_role(TextRole::Caption)
        .child(disk_space_text(&disk))
        .when(disk.level != DiskLevel::Ok, |el| {
            el.text_color(theme.danger)
        });
    let thresholds = warned.filter(|_| disk.level == DiskLevel::Warning).map(
        |(_, (stop_below_bytes, stop_below_s))| {
            div()
                .id(("disk-stop", 0usize))
                .spoken(
                    Role::Label,
                    disk_stop_text(
                        stop_below_bytes,
                        stop_below_s,
                        crate::ui::format::spoken_length,
                    ),
                )
                .text_role(TextRole::Caption)
                .text_color(theme.muted_foreground)
                .child(disk_stop_text(
                    stop_below_bytes,
                    stop_below_s,
                    crate::ui::format::length,
                ))
        },
    );
    alert
        .into_iter()
        .chain(Some(space))
        .chain(thresholds)
        .collect()
}

/// A live picker whose row left the preview (the agent keeps only its
/// latest turns).
const LIVE_ROW_GONE: &str = "This line left the live preview while you were choosing (only the \
     latest lines are kept); nothing was saved.";

/// The transcript's own view. Normal rendering caches its subtree across
/// unrelated meter/clock paints; active accessibility renders the real rows
/// because GPUI's cache does not retain their semantic children. It keeps
/// no state of its own: rows, scroll and picker belong to the panel.
pub(super) struct TranscriptView {
    panel: WeakEntity<RecordingPanel>,
    #[cfg(test)]
    renders: usize,
}

impl TranscriptView {
    fn new(panel: WeakEntity<RecordingPanel>) -> Self {
        Self {
            panel,
            #[cfg(test)]
            renders: 0,
        }
    }
}

impl Render for TranscriptView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let panel = self.panel.clone();
        let Some((state, page)) = panel.upgrade().map(|p| {
            let p = p.read(cx);
            (p.transcript.clone(), p.transcript_page.clone())
        }) else {
            return div().into_any_element();
        };
        let rows = panel.clone();
        let state_for_scroll = state.clone();
        div()
            .relative()
            .size_full()
            .child(
                list(state.clone(), move |ix, window, cx| {
                    rows.update(cx, |panel, cx| panel.transcript_row(ix, window, cx))
                        .unwrap_or_else(|_| div().into_any_element())
                })
                .size_full(),
            )
            .vertical_scrollbar(&state)
            .on_prepaint(move |bounds, window, cx| {
                page.set(bounds.size.height);
                // Scrollbar drags change ListState directly. Notify the parent only
                // when the follow indicator actually needs to change.
                if panel.upgrade().is_some_and(|p| {
                    p.read(cx).following_visible != state_for_scroll.is_following_tail()
                }) {
                    window.defer(cx, move |_, cx| {
                        let _ = panel.update(cx, |this, cx| {
                            let following = this.transcript.is_following_tail();
                            if this.following_visible != following {
                                this.following_visible = following;
                                cx.notify();
                            }
                        });
                    });
                }
            })
            .into_any_element()
    }
}

impl Render for RecordingPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(test)]
        {
            self.renders += 1;
        }
        let journal_rows = self.library_journal_rows(window, cx);
        let daemon = self.daemon.read(cx);
        let model = &daemon.panel;
        let connected = daemon.connected;
        let recording = model.recording.clone();
        let theme = cx.theme();
        let active = recording.is_some();
        let compact = self.compact_layout();
        let header;
        let mut tabs = None;
        let mut attention = Vec::new();
        if connected {
            attention.extend(
                kit::capture_access_warnings(
                    model.access,
                    recording.as_ref().map(|recording| recording.status),
                    &self.state,
                    cx,
                )
                .map(IntoElement::into_any_element),
            );
        }
        if let Some(recording) = &recording
            && connected
        {
            attention.extend(self.auto_stop_attention(recording, cx));
            attention.extend(
                self.health_lines(recording, true, cx)
                    .into_iter()
                    .map(IntoElement::into_any_element),
            );
        }
        if connected
            && let Some(stopped) = model.stopped.iter().max_by_key(|entry| entry.stopped_at)
            && self.stop_reason_dismissed.as_deref() != Some(&stopped.session_id)
        {
            attention.extend(self.stop_reason_row(stopped, true, cx));
        }
        let mut content = div()
            .id("panel-upper")
            .debug_selector(|| "panel-upper".into())
            .role(Role::Group)
            .aria_label("Recording")
            .text_role(TextRole::Body)
            .w_full()
            .px_4()
            .py_2()
            .flex()
            .flex_col()
            .gap_2()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .flex_wrap()
                    .child({
                        // Wraps inside a narrow panel at large text instead
                        // of running past the window edge.
                        let title =
                            crate::ui::title("recording-controls-title", "Recording controls")
                                .flex_grow(1.)
                                .flex_shrink(1.)
                                .flex_basis(gpui::rems(10.))
                                .min_w_0();
                        #[cfg(test)]
                        let title = gpui::base::TestSupportExt::test_support(title);
                        title
                    })
                    .child(
                        crate::ui::heading("panel-recording-heading", "Recording")
                            .text_role(TextRole::Caption)
                            .text_color(theme.muted_foreground),
                    )
                    .child(self.compact_button(
                        match (active, compact) {
                            (true, true) => "Show transcript",
                            (true, false) => "Hide transcript",
                            (false, true) => "Show details",
                            (false, false) => "Hide details",
                        },
                        cx,
                    )),
            )
            .children(self.compact_error.as_ref().map(|error| {
                kit::status_line(
                    "panel-compact-error",
                    kit::StatusKind::Failed,
                    error.clone(),
                    cx,
                )
            }));
        let mut urgent_action = self.grace_pending(cx);
        let mut asks = Vec::new();
        let mut informational = Vec::new();
        for notice in self.notifications.read(cx).panel_notices() {
            let ask = notice.is_recording_prompt();
            let card = self.notification_card(notice, ask && !urgent_action, cx);
            if ask || compact {
                asks.push(card);
                urgent_action |= ask;
            } else {
                informational.push(card);
            }
        }
        let agent_status = AgentStartStatus::unavailable(daemon, self.agent.read(cx).status());
        let mut agent_attention = Vec::new();
        if let Some(status) = &agent_status {
            let buttons = div()
                .flex()
                .flex_wrap()
                .gap_2()
                .when_some(status.recovery(), |row, action| {
                    row.child(
                        Button::new("start-agent")
                            .label(action.label(false))
                            .map(|button| kit::primary(button, false, cx))
                            .small()
                            .track_focus(&self.first_control)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.agent.update(cx, |agent, cx| agent.recover(cx));
                            })),
                    )
                })
                .when(status.copy_details().is_some(), |row| {
                    row.child(
                        Button::new("agent-copy-details")
                            .label("Copy details")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.agent
                                    .update(cx, |agent, cx| agent.copy_recovery_details(cx));
                            })),
                    )
                });
            agent_attention.push(kit::stack()
                .children(self.agent_start_notice(cx))
                .when(!connected && active, |row| row.child(text_line(
                    "agent-capture-disconnected", Role::Label,
                    "These controls return when BroScribe Agent reconnects. If capture is still running, it continues independently.",
                )))
                .child(buttons)
                .into_any_element());
        }
        if let Some(refusal) = &daemon.menu_start_refusal {
            agent_attention.push(
                kit::stack()
                    .child(kit::status_line(
                        "menu-start-refused",
                        kit::StatusKind::Failed,
                        refusal.words.clone(),
                        cx,
                    ))
                    .children(kit::start_recovery_button(
                        "menu-start-recovery",
                        refusal,
                        &self.state,
                    ))
                    .into_any_element(),
            );
        }
        if let Some(error) = daemon.error.clone()
            && !matches!(
                agent_status,
                Some(AgentStartStatus::Stopped | AgentStartStatus::Outdated { .. })
            )
            && (connected || error != "BroScribe Agent isn't running.")
        {
            agent_attention.push(
                kit::status_line("agent-error", kit::StatusKind::Failed, error, cx)
                    .into_any_element(),
            );
        }
        attention.extend(agent_attention);
        attention.extend(journal_rows);
        attention.extend(asks);
        if let Some(recording) = recording {
            let session = recording.session_id.clone();
            let session_busy = self.pending(Control::Session, Some(&session));
            let paused = recording.status == RecordingStatus::Paused;
            content = content
                .child(self.recording_visor(&recording, cx))
                .child(daemon.meters().clone())
                .when(connected, |header| {
                    header.children(disk_lines(
                        recording.health.disk.as_ref(),
                        &model.capture_warnings().collect::<Vec<_>>(),
                        false,
                        cx,
                    ))
                });
            let others = model.stopped.len() + model.saved.len();
            if connected
                && let Some(failed) = model.stopped.iter().rev().find(|stopped| {
                    save_failure(
                        &stopped.state,
                        model
                            .journal_issues
                            .iter()
                            .find(|issue| issue.session_id == stopped.session_id),
                    ) == Some(SaveFailure::Manual)
                })
            {
                attention.push(
                    kit::stack()
                        .child(kit::status_line(
                            "other-recording-failed",
                            kit::StatusKind::Failed,
                            format!(
                                "Couldn't transcribe “{}”. The recording is kept.",
                                meeting_title(failed.title.as_deref())
                            ),
                            cx,
                        ))
                        .child(
                            self.retry_button("attention-retry".into(), &failed.session_id, cx)
                                .outline(),
                        )
                        .into_any_element(),
                );
            }
            if others > 0 {
                let summary = if let [stopped] = model.stopped.as_slice()
                    && others == 1
                {
                    format!(
                        "{}: {}",
                        meeting_title(stopped.title.as_deref()),
                        stopped_summary(&stopped.state)
                    )
                } else {
                    let failed = model
                        .stopped
                        .iter()
                        .filter(|stopped| {
                            matches!(stopped.state, StoppedRecordingState::Failed { .. })
                        })
                        .count();
                    let noun = if others == 1 {
                        "recording"
                    } else {
                        "recordings"
                    };
                    if failed == 0 {
                        format!("{others} other {noun}")
                    } else {
                        format!("{others} other {noun} · {failed} failed")
                    }
                };
                attention.push(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(kit::status_line(
                            "other-recordings-summary",
                            kit::StatusKind::Info,
                            summary,
                            cx,
                        ))
                        .child(
                            Button::new("show-other")
                                .label("Show")
                                .small()
                                .ghost()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.temporarily_expanded = true;
                                    this.tab = RecordingPanelTab::Details;
                                    this.other_recordings_anchor.scroll_to(window, cx);
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                );
            }
            if self.consent_reminder && self.consent_dismissed.as_deref() != Some(&session) {
                let shown = session.clone();
                attention.push(
                    kit::banner(
                        "consent-reminder",
                        kit::StatusKind::Info,
                        Role::Note,
                        "Let everyone know this meeting is being recorded.",
                        cx,
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(self.settings_link(
                                "consent-settings",
                                SettingsPage::Recording,
                                cx,
                            ))
                            .child(
                                Button::new("consent-reminder-dismiss")
                                    .label("Dismiss")
                                    .small()
                                    .ghost()
                                    .self_start()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.consent_dismissed = Some(shown.clone());
                                        cx.notify();
                                    })),
                            ),
                    )
                    .into_any_element(),
                );
            }
            let pause = if recording.status == RecordingStatus::Paused {
                SessionCommand::Resume
            } else {
                SessionCommand::Pause
            };
            let grace_active = self.grace_pending(cx);
            let note_busy = self.pending(Control::Note, Some(&session));
            let pause_session = session.clone();
            let stop_session = session.clone();
            content = content
                .child(
                    div()
                        .id("panel-controls")
                        .debug_selector(|| "panel-controls".into())
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(
                            Button::new("panel-pause")
                                .icon(if paused { crate::ui::icons::PLAY } else { crate::ui::icons::PAUSE })
                                .label(if recording.status == RecordingStatus::Paused {
                                    "Resume"
                                } else {
                                    "Pause"
                                })
                                .when(connected, |button| button.track_focus(&self.first_control))
                                .when(connected && paused && !grace_active, |button| kit::primary(button, session_busy, cx))
                                .when(!paused || grace_active, |button| button.outline())
                                .map(|button| crate::ui::kit::disable_with_reason(button, session_busy, "Waiting for BroScribe Agent to answer the recording control.", cx))
                                .map(|button| disconnected_button(button, connected, cx))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if this.model(cx).active_session() == Some(&pause_session) {
                                        this.session_action(pause, window, cx);
                                    }
                                })),
                        )
                        .child(
                            Button::new("panel-bookmark")
                                .label("Bookmark")
                                .outline()
                                .tooltip_with_action("Mark this moment. Text in the note field becomes its label.", &crate::menu::AddBookmark, None)
                                .disabled(note_busy)
                                .map(|button| disconnected_button(button, connected, cx))
                                .on_click(cx.listener(|this, _, window, cx| this.bookmark(window, cx))),
                        )
                        .child(div().flex_1())
                        .child(self.more_actions(&session, session_busy, cx))
                        .child(
                            Button::new("panel-stop")
                                .label("Stop and save")
                                .when(connected && !paused && !grace_active, |button| kit::primary(button, session_busy, cx))
                                .when(paused || grace_active, |button| button.outline())
                                .map(|button| crate::ui::kit::disable_with_reason(button, session_busy, "Waiting for BroScribe Agent to answer the recording control.", cx))
                                .map(|button| disconnected_button(button, connected, cx))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if this.model(cx).active_session() == Some(&stop_session) {
                                        this.session_action(SessionCommand::Stop, window, cx);
                                    }
                                })),
                        )
                )
                .children(self.status_for(Control::Session, &session, cx))
                .children(self.status_for(Control::Discard, &session, cx))
                .child(Input::new(&self.note).aria_label("Note").disabled(!connected))
                .children(self.status_for(Control::Note, &session, cx));
            let upper = content.flex_none().border_b_1().border_color(theme.border);
            header = Some(if compact {
                upper
            } else {
                // At large text sizes or a short frame, the controls scroll
                // independently so attention and tabs stay visible.
                div()
                    .id("panel-header-scroll")
                    .debug_selector(|| "panel-header-scroll".into())
                    .w_full()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .max_h(gpui::relative(0.5))
                    .overflow_y_scroll()
                    .track_scroll(&self.header_scroll)
                    .vertical_scrollbar(&self.header_scroll)
                    .child(upper)
            });
            tabs = (!compact).then(|| {
                let title_draft = self.title_draft(&session, cx).is_some();
                div()
                    .id("panel-tabs")
                    .debug_selector(|| "panel-tabs".into())
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .flex_none()
                    .px_4()
                    .pt_2()
                    .child(
                        kit::tabs_with_names(
                            "recording-panel-tabs",
                            "Recording sections",
                            [
                                ("Transcript".into(), "Transcript".into()),
                                if title_draft {
                                    ("Details •".into(), "Details, unsaved title".into())
                                } else {
                                    ("Details".into(), "Details".into())
                                },
                            ],
                            usize::from(self.tab == RecordingPanelTab::Details),
                            cx.listener(|this, index, _, cx| {
                                this.tab = if *index == 0 {
                                    RecordingPanelTab::Transcript
                                } else {
                                    RecordingPanelTab::Details
                                };
                                cx.notify();
                            }),
                            cx,
                        )
                        .flex_none()
                        .flex_nowrap(),
                    )
            });
            let details = self.tab == RecordingPanelTab::Details;
            content = div()
                .id("recording-panel-page")
                .role(Role::TabPanel)
                .aria_label(if details { "Details" } else { "Transcript" })
                .w_full()
                .min_h_0()
                .flex_1()
                .px_4()
                .py_1()
                .flex()
                .flex_col()
                .gap_2()
                .when(details, |el| {
                    el.overflow_y_scroll()
                        .track_scroll(&self.details_scroll)
                        .vertical_scrollbar(&self.details_scroll)
                });
            if !compact && details {
                content = content
                    .child(crate::ui::subheading("panel-title-heading", "Title"))
                    .children(self.title_editor(&session, cx))
                    .child(crate::ui::subheading("panel-language-heading", "Language"))
                    .child(kit::field(
                        "panel-language-label",
                        "Language",
                        None,
                        |label| {
                            let config = self.state.read(cx).config();
                            let preferred = self
                                .language_profiles
                                .iter()
                                .find(|profile| profile.asr == config.profile.asr)
                                .map_or(&[][..], |profile| profile.languages.as_slice());
                            let selected = self.shown_language(cx);
                            let target = session.clone();
                            crate::ui::select::Picker::languages(
                                SharedString::from(format!("panel-language-{session}")),
                                label,
                                preferred,
                                selected,
                                cx,
                                move |this, value, window, cx| {
                                    if this.model(cx).active_session() == Some(&target) {
                                        this.set_language(value.to_owned(), window, cx);
                                    }
                                },
                            )
                            .disabled(!connected || self.pending(Control::Language, Some(&session)))
                        },
                        cx,
                    ))
                    .children(self.status_for(Control::Language, &session, cx));

                // People: attendance only; a voice is never learned here.
                content = content.child(self.participant_section(&recording, cx));

                content = content.child(crate::ui::subheading("panel-tags-heading", "Tags"));
                // Tags: chips with Remove, comma-separated additions.
                let tags_busy = self.pending(Control::Tag, Some(&session));
                let tags = self.shown_tags(cx);
                content = content
                    .child(
                        div()
                            .id("panel-tags")
                            .role(Role::List)
                            .aria_label("Recording tags")
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .children(tags.into_iter().enumerate().map(|(ix, tag)| {
                                let remove = tag.clone();
                                kit::tag_chip(tag.clone(), cx).child(
                                    Button::new(("panel-tag-remove", ix))
                                        .icon(crate::ui::icons::DISMISS)
                                        .accessibility_label(format!("Remove tag {tag}"))
                                        .xsmall()
                                        .tooltip(format!("Remove tag {tag}"))
                                        .disabled(tags_busy)
                                        .map(|button| disconnected_button(button, connected, cx))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.remove_tag(remove.clone(), window, cx)
                                        })),
                                )
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_2()
                            .child(
                                div().flex_1().min_w(px(160.)).child(
                                    Input::new(&self.tag)
                                        .aria_label("Recording tags to add")
                                        .disabled(!connected),
                                ),
                            )
                            .child(
                                Button::new("panel-tag")
                                    .label("Add tags")
                                    .disabled(
                                        tags_busy
                                            || typed_tags(self.tag.read(cx).value().as_ref())
                                                .is_empty(),
                                    )
                                    .map(|button| disconnected_button(button, connected, cx))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_tags(window, cx)
                                    })),
                            ),
                    )
                    .children(self.status_for(Control::Tag, &session, cx))
                    .child(crate::ui::subheading(
                        "panel-auto-stop-heading",
                        "Automatic stop",
                    ))
                    .child(self.auto_stop_section(&recording, cx))
                    .child(crate::ui::subheading("panel-capture-heading", "Capture"))
                    .child(self.capture_exclusion_section(cx))
                    .children(self.health_lines(&recording, false, cx))
                    .children(self.overlay_note.clone().map(|note| {
                        text_line("overlay-note", Role::Status, note).text_role(TextRole::Caption)
                    }))
                    .child(
                        crate::ui::subheading("panel-other-heading", "Other recordings")
                            .anchor_scroll(Some(self.other_recordings_anchor.clone())),
                    );
            } else if !compact {
                let empty = transcript_empty(connected, &model.preview);
                let announcement = |id, text: String, attention| {
                    kit::status_line(
                        id,
                        if attention {
                            kit::StatusKind::Attention
                        } else {
                            kit::StatusKind::Pending
                        },
                        text,
                        cx,
                    )
                    .text_role(TextRole::Caption)
                };
                let no_rows = self.transcript_rows.is_empty();
                let mut notices = div()
                    .id("panel-transcript-notices")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .flex_none()
                    .max_h(gpui::rems(3.))
                    .overflow_y_scroll();
                if !no_rows {
                    notices = notices.children(
                        if connected {
                            preview_notice(&model.preview)
                        } else {
                            Some(empty.clone())
                        }
                        .map(|text| {
                            announcement(
                                "live-preview",
                                text,
                                !connected
                                    || matches!(
                                        model.preview,
                                        PreviewState::Restarting { .. }
                                            | PreviewState::Unavailable { .. }
                                    ),
                            )
                        }),
                    );
                }
                if connected
                    && matches!(
                        model.preview,
                        PreviewState::Restarting {
                            reason: WorkerProblem::ModelMissing | WorkerProblem::InvalidSettings,
                            ..
                        } | PreviewState::Unavailable {
                            reason: WorkerProblem::ModelMissing | WorkerProblem::InvalidSettings
                        }
                    )
                {
                    notices = notices.child(self.settings_link(
                        "preview-settings",
                        SettingsPage::Transcription,
                        cx,
                    ));
                }
                // Reserve one line so a missed update keeps the reading viewport steady.
                let missed = div()
                    .id("live-missed-strip")
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(gpui::rems(1.25))
                    .flex_none()
                    .overflow_hidden()
                    .when_some(model.missed_updates, |row, dropped| {
                        row.child(
                            announcement("live-missed", missed_updates_notice().to_owned(), false)
                                .truncate(),
                        )
                        .child(
                            text_line(
                                "live-missed-count",
                                Role::Label,
                                missed_updates_count(dropped),
                            )
                            .text_role(TextRole::Caption)
                            .whitespace_nowrap()
                            .flex_none(),
                        )
                    });
                notices = notices.when(!no_rows || model.missed_updates.is_some(), |el| {
                    el.child(missed)
                });
                if connected {
                    notices = notices.children(identity_notice(&model.identity).map(|text| {
                        announcement(
                            "live-identity",
                            text,
                            matches!(
                                model.identity.status,
                                LiveIdentityStatus::Restarting { .. }
                                    | LiveIdentityStatus::Unavailable { .. }
                            ),
                        )
                    }));
                    let settings = match model.identity.status {
                        LiveIdentityStatus::Disabled => Some(SettingsPage::Recording),
                        LiveIdentityStatus::Restarting {
                            reason: WorkerProblem::ModelMissing | WorkerProblem::InvalidSettings,
                        }
                        | LiveIdentityStatus::Unavailable {
                            reason: WorkerProblem::ModelMissing | WorkerProblem::InvalidSettings,
                        } => Some(SettingsPage::Transcription),
                        _ => None,
                    };
                    notices = notices.children(
                        settings.map(|page| self.settings_link("identity-settings", page, cx)),
                    );
                }
                if let Some(turn) = self
                    .live_status_turn
                    .as_deref()
                    .filter(|turn| model.turn(turn).is_none())
                {
                    notices =
                        notices.children(self.status_line(&ControlKey::live(&session, turn), cx));
                }
                content = content
                .child(div().flex().items_start().gap_2()
                    .child(div().flex_1().min_w_0().child(notices))
                    .when(!model.turns.is_empty(), |el| el.child(
                        gpui::component::popover::Popover::new("live-names-info")
                            .trigger(Button::new("live-names-info-button").ghost().small()
                                .icon(gpui::assets::IconName::Info).accessibility_label("About live names"))
                            .content(|_, _, cx| div().max_w(gpui::rems(21.)).child(
                                text_line("live-explanation", Role::Label, LIVE_EXPLANATION)
                                    .text_role(TextRole::Caption)
                                    .text_color(cx.theme().muted_foreground)
                            )),
                    )))
                .child(
                    // The live transcript is the one card on the window's desk.
                    kit::desk_card("live-transcript", cx)
                        .debug_selector(|| "live-transcript".into())
                        .role(Role::List)
                        .aria_label("Live transcript")
                        .aria_keyshortcuts("ArrowUp ArrowDown Home End PageUp PageDown Enter N Backspace Delete Shift+F10 ContextMenu")
                        .track_focus(&self.transcript_focus.clone().tab_index(0).tab_stop(true))
                        .on_key_down(cx.listener(Self::transcript_key_down))
                        .relative()
                        .flex_1()
                        .min_h_0()
                        .overflow_hidden()
                        .when(no_rows, |el| el.flex_row().items_start().justify_center().overflow_y_scroll().child(
                            text_line(if matches!(model.preview, PreviewState::Loading { .. }) { "live-preview" } else { "live-transcript-empty" }, Role::Status, empty.clone())
                                .debug_selector(|| "live-transcript-status".into())
                                .w_full().min_w_0().px_4().py_2().text_center().text_role(TextRole::Caption)
                        ))
                        .when(!no_rows, |el| el.child(
                            // Cached GPUI subtrees do not rebuild their accessibility children.
                            if window.is_a11y_active() {
                                self.transcript_view.clone().into_any_element()
                            } else {
                                self.transcript_view.clone()
                                    .cached(StyleRefinement::default().size_full())
                                    .into_any_element()
                            },
                        ))
                        .when(!self.following_visible && self.live_naming.is_none() && !no_rows, |el| {
                            el.child(div().absolute().right_2().bottom_2().child(
                                Button::new("live-show-latest").label("Show latest").small()
                                    .on_click(cx.listener(|this, _, _, cx| this.show_latest(cx))),
                            ))
                        }),
                )
                .when(connected, |el| el.children(self.docked_line_picker(cx)))
                .children(self.transcript_menu_layer(window));
            }
        } else {
            if connected {
                let (visor, has_primary) = self.new_idle_visor(cx);
                let start_pending =
                    self.pending(Control::Start, None) || daemon.record_start_pending();
                let update_waiting = crate::menu::update_waiting(cx);
                let start_words = if start_pending {
                    "Starting…"
                } else {
                    "Start recording"
                };
                let idle = model.stopped.is_empty() && model.saved.is_empty();
                content = content.child(visor)
                    .when(idle && model.detection_enabled, |el| el.child(text_line("panel-idle-detection", Role::Label,
                        "Start a recording here, or let BroScribe start one when a meeting app joins a call.")))
                    .child(div().flex().flex_wrap().gap_2()
                        .child(Button::new("panel-start").track_focus(&self.first_control)
                            // The record dot sits before the words.
                            .accessibility_label(start_words)
                            .child(kit::record_dot(!has_primary, start_pending || update_waiting, cx))
                            .child(start_words)
                            .when(has_primary, |button| button.outline())
                            .when(!has_primary, |button| kit::primary(button, start_pending || update_waiting, cx))
                            .tooltip_with_action("Start recording", &crate::menu::BeginRecording, None)
                            .disabled(start_pending || update_waiting)
                            .on_click(cx.listener(|this, _, window, cx| this.start_recording(window, cx))))
                        .child(Button::new("panel-library").label("Open library").ghost()
                            .on_click(cx.listener(|this, _, _, cx| this.open_library(cx)))))
                    .when(update_waiting, |el| el
                        .child(kit::status_line("panel-update-hold", kit::StatusKind::Attention,
                            crate::views::update::RECORDING_HELD, cx))
                        .child(Button::new("panel-open-updates").label("Open update settings").outline()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.update(cx, |_, cx| cx.emit(crate::state::AppEvent::OpenSettings(
                                    Some(crate::state::SettingsPage::Updates))));
                            }))))
                    .children(self.status_line(&ControlKey::new(Control::Start, None), cx))
                    .children(self.start_refusal.as_ref().and_then(|refusal| {
                        kit::start_recovery_button("panel-start-recovery", refusal, &self.state)
                    }));
            } else {
                let connecting = matches!(agent_status, Some(AgentStartStatus::Connecting));
                content = content.child(visor(
                    if connecting {
                        kit::Face::Waiting
                    } else {
                        kit::Face::Attention
                    },
                    if connecting {
                        "Connecting…"
                    } else {
                        "Not connected"
                    },
                    None,
                    None,
                    cx,
                ));
            }
            header = Some(content.flex_none().border_b_1().border_color(theme.border));
            content = div()
                .id("panel-idle-body")
                .w_full()
                .px_4()
                .py_2()
                .flex()
                .flex_col()
                .gap_2()
                .child(crate::ui::heading("panel-recordings", "Recordings"))
                .when(model.stopped.is_empty() && model.saved.is_empty(), |el| {
                    el.child(text_line(
                        "panel-empty",
                        Role::Label,
                        "Recordings you make appear here until they’re saved.",
                    ))
                });
        }
        if !compact && !active && self.has_unsent_drafts(cx) {
            let note = self.note.read(cx).value().trim().to_owned();
            let tags = self.tag.read(cx).value().trim().to_owned();
            let participant = self.picker.read(cx).query().trim().to_owned();
            content = content
                .child(crate::ui::heading("unsent-drafts", "Unsent drafts"))
                .when(!note.is_empty(), |el| {
                    el.child(text_line(
                        "unsent-note",
                        Role::Label,
                        format!("Note: {note}"),
                    ))
                })
                .when(!tags.is_empty(), |el| {
                    el.child(text_line(
                        "unsent-tags",
                        Role::Label,
                        format!("Tags: {tags}"),
                    ))
                })
                .when(!participant.is_empty(), |el| {
                    el.child(text_line(
                        "unsent-participant",
                        Role::Label,
                        format!("Participant search: {participant}"),
                    ))
                })
                .child(
                    Button::new("clear-unsent-drafts")
                        .label("Clear drafts")
                        .self_start()
                        .on_click(
                            cx.listener(|this, _, window, cx| this.clear_unsent_drafts(window, cx)),
                        ),
                );
        }
        if let Some(warning) = self.overlay_warning.clone() {
            attention.push(
                kit::banner(
                    "overlay-warning",
                    kit::StatusKind::Attention,
                    Role::Alert,
                    warning,
                    cx,
                )
                .into_any_element(),
            );
        }
        let content = (!compact).then(|| {
            content
                .when(!active, |el| el.flex_1().min_h_0().overflow_y_scroll())
                .when(!active || self.tab == RecordingPanelTab::Details, |el| {
                    el.children(informational)
                        .children(self.stopped_rows(cx))
                        .children(self.unlisted_statuses(cx))
                })
                .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                    crate::views::import_sheet::ImportRuns::request_dialog(
                        crate::views::import_sheet::dropped_paths(paths),
                        cx,
                    );
                    this.state
                        .update(cx, |_, cx| cx.emit(crate::state::AppEvent::OpenMainWindow));
                }))
        });
        // Where the window system leaves decorations to the app: the
        // title, which moves the window, and Close, never minimise
        // (design §4).
        let theme = cx.theme();
        let panel = cx.weak_entity();
        let restore_size = self.compact_height.is_some();
        let root = div()
            .on_children_prepainted(move |bounds, window, cx| {
                if !compact
                    && !restore_size
                    && window.viewport_size().height >= px(crate::ui_state::PANEL_MIN.1)
                {
                    return;
                }
                let chrome = crate::ui::chrome::panel_chrome_height(window);
                let height = bounds
                    .iter()
                    .fold(chrome, |height, bounds| height + bounds.size.height);
                let panel = panel.clone();
                window.defer(cx, move |window, cx| {
                    let _ = panel.update(cx, |panel, cx| panel.size_for_layout(height, window, cx));
                });
            })
            .id("recording-panel")
            .role(Role::Main)
            .aria_label("Recording controls")
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(
                cx.listener(|this, _: &crate::menu::CloseWindow, window, cx| {
                    this.return_overlay_focus(window);
                    window.remove_window();
                    cx.stop_propagation();
                }),
            )
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape"
                    && !event.keystroke.modifiers.modified()
                    && !this.has_unsent_drafts(cx)
                    && this
                        .titles
                        .keys()
                        .all(|session| this.title_draft(session, cx).is_none())
                    && this.live_naming.is_none()
                    && !this.roster_open
                    && !this.more_open
                    && this.discard_confirm.is_none()
                    && this.delete.is_none()
                    && this.transcript_menu.is_none()
                    && this.return_overlay_focus(window)
                {
                    cx.stop_propagation();
                }
            }))
            .when(compact, |el| el.overflow_y_scroll())
            .children(header)
            .when(!attention.is_empty(), |el| {
                el.child(
                    div()
                        .id("panel-attention")
                        .debug_selector(|| "panel-attention".into())
                        .role(Role::Group)
                        .aria_label("Needs attention")
                        .flex()
                        .flex_col()
                        .flex_none()
                        .gap_2()
                        .px_4()
                        .py_2()
                        .when(compact, |el| el.max_h(gpui::rems(10.)))
                        .when(!compact, |el| el.max_h(gpui::relative(0.3)))
                        .overflow_y_scroll()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            crate::ui::heading("panel-attention-heading", "Needs attention")
                                .text_role(TextRole::Label),
                        )
                        .children(attention),
                )
            })
            .children(tabs)
            .children(content);
        crate::ui::chrome::framed_panel(WINDOW_TITLE, root, window)
    }
}
