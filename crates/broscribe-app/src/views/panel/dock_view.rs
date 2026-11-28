//! The "Speaking now" dock under the live transcript ([`super::dock`] holds
//! its tested model): who is speaking, and the people of the call as one
//! row of chips. A tap or **1–4** names the turn the dock shows; *Someone
//! new…* opens the shared people picker on it; *Who's this?* on an older
//! turn retargets the dock until *Back to speaking now*.

use super::dock::{self, Chip, ChipState, DockInputs, DockTarget, KnownPerson};
use super::*;
use crate::ui::type_scale::{TextRole, TextRoleExt as _};
use gpui::component::Sizable as _;

/// What the dock shows in one frame.
pub(super) struct DockView {
    pub target: Option<LiveTurn>,
    pub retargeted: bool,
    pub chips: Vec<Chip>,
}

/// The chip a tap meant: by number key, or the person clicked on the turn
/// the dock showed.
enum DockPick {
    Key(usize),
    Click { person: String, turn: String },
}

impl RecordingPanel {
    /// Reads the registered people (and who has stored voice samples) for
    /// the dock once per people revision, on the background executor.
    pub(super) fn sync_dock_people(&mut self, cx: &mut Context<Self>) {
        let revision = self.state.read(cx).people_revision();
        if self.dock_known_revision == Some(revision) {
            return;
        }
        self.dock_known_revision = Some(revision);
        let library = self.state.read(cx).library().clone();
        let samples = self.state.update(cx, |state, cx| state.people_samples(cx));
        let people = cx.background_executor().spawn(async move {
            let root = library.root();
            match broscribe_core::index::Index::open_readonly(root) {
                Ok(index) => index.list_people().unwrap_or_default(),
                Err(_) => Vec::new(),
            }
        });
        cx.spawn(async move |this, cx| {
            let people = people.await;
            // Without samples nobody shows "known voice"; naming still works.
            let voices: BTreeSet<String> = samples
                .await
                .map(|samples| {
                    samples
                        .all()
                        .iter()
                        .filter(|p| !p.confirmed.is_empty() || !p.weak.is_empty())
                        .map(|p| p.person_id.as_str().to_owned())
                        .collect()
                })
                .unwrap_or_default();
            let known: Vec<KnownPerson> = people
                .into_iter()
                .map(|p| KnownPerson {
                    has_voice: voices.contains(p.id.as_str()),
                    id: p.id.as_str().to_owned(),
                    name: p.name,
                    emails: p.emails,
                    is_self: p.is_self,
                    hue: p.hue,
                })
                .collect();
            this.update(cx, |this, cx| {
                if this.dock_known_revision == Some(revision) && *this.dock_known != known {
                    this.dock_known = Rc::new(known);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// The dock's target and chips now; `None` without an active recording.
    pub(super) fn dock_view(&mut self, cx: &App) -> Option<DockView> {
        let model = self.model(cx);
        let recording = model.recording.as_ref()?;
        let session = &recording.session_id;
        if self.dock_order.0.as_ref() != Some(session) {
            self.dock_order = (Some(session.clone()), Vec::new());
        }
        let confirmed: Vec<(String, String)> = self
            .confirmed
            .iter()
            .map(|(id, person)| (id.clone(), person.name.clone()))
            .collect();
        let people = dock::people(DockInputs {
            observed: &recording.observed_roster,
            expected: &recording.expected_roster,
            confirmed: &confirmed,
            turns: &model.turns,
            known: &self.dock_known,
        });
        let people = dock::stable_order(&mut self.dock_order.1, people);
        let (mut target, retargeted) = dock::target_turn(&self.dock_target, &model.turns);
        // Nothing is being said while paused; an older turn can still be named.
        if recording.status == RecordingStatus::Paused && !retargeted {
            target = None;
        }
        let chips = dock::chips(people, &model.turns, target);
        Some(DockView {
            target: target.cloned(),
            retargeted,
            chips,
        })
    }

    fn dock_paused(&self, cx: &App) -> bool {
        self.model(cx)
            .recording
            .as_ref()
            .is_some_and(|r| r.status == RecordingStatus::Paused)
    }

    /// Names (or confirms) the dock's turn as the picked person.
    fn dock_pick(&mut self, pick: DockPick, window: &mut Window, cx: &mut Context<Self>) {
        let connected = self.daemon.read(cx).connected;
        let paused = self.dock_paused(cx);
        if self.live_pending() || self.live_naming.is_some() {
            return;
        }
        let Some(view) = self.dock_view(cx) else {
            return;
        };
        let Some(turn) = view.target.as_ref() else {
            return;
        };
        if dock::blocked(Some(turn), paused, connected).is_some() {
            return;
        }
        let chip = match &pick {
            DockPick::Key(ix) => view.chips.get(*ix),
            DockPick::Click {
                person,
                turn: shown,
            } => {
                // A click names the turn it was shown beside, never one
                // that arrived meanwhile.
                if *shown != turn.turn_id {
                    self.set_live_status(
                        &turn.session_id,
                        shown,
                        ControlStatus::Failed(
                            "Someone else started speaking; nothing was named. Check the turn \
                             and tap again."
                                .into(),
                        ),
                    );
                    cx.notify();
                    return;
                }
                view.chips.iter().find(|c| c.person.key == *person)
            }
        };
        let Some(chip) = chip else {
            return;
        };
        let Some((action, person_ref)) = dock::tap(&chip.person, turn) else {
            return;
        };
        self.live_decision(ShownTurn::of(turn), action, Some(person_ref), window, cx);
    }

    /// *Back to speaking now*: the dock follows the speaker again.
    pub(super) fn dock_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.live_naming.is_some() {
            self.cancel_live_naming(window, cx);
        } else {
            self.dock_target = DockTarget::SpeakingNow;
            cx.notify();
        }
    }

    /// **1–4** name the dock's turn. `true` when the key was one of them.
    pub(super) fn dock_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.live_naming.is_some() {
            return false;
        }
        let Some(ix) = dock::key_index(&event.keystroke.key, event.keystroke.modifiers.modified())
        else {
            return false;
        };
        self.dock_pick(DockPick::Key(ix), window, cx);
        true
    }

    /// The dock, pinned under the live transcript.
    pub(super) fn speaking_dock(
        &mut self,
        roomy: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let connected = self.daemon.read(cx).connected;
        let paused = self.dock_paused(cx);
        let view = self.dock_view(cx)?;
        let busy = self.live_pending() || self.live_naming.is_some();
        let target = view.target.as_ref();
        let blocked = dock::blocked(target, paused, connected);
        let muted = cx.theme().muted_foreground;
        let title = dock::target_title(target, view.retargeted);
        let line = target.map(|turn| {
            let speaker = match turn.track {
                Track::Mic => "Me".to_owned(),
                Track::System => turn_badge(turn),
            };
            format!("{} · {speaker} · {}", format_clock(turn.start_s), turn.text)
        });
        let keys = view.chips.len().min(dock::DOCK_KEYS);
        let hint = match keys {
            0 => "Nobody listed yet. Someone new… adds a name.".to_owned(),
            1 => "1 names this turn".to_owned(),
            n => format!("1–{n} name this turn"),
        };
        let shown_turn = target.map(|t| t.turn_id.clone());
        let chips = view.chips.iter().enumerate().map(|(ix, chip)| {
            let disc = match chip.state {
                ChipState::TapToConfirm | ChipState::Suggested(_) => kit::Disc::Suggested,
                _ => chip
                    .person
                    .hue
                    .map_or(kit::Disc::Voice(None), kit::Disc::Person),
            };
            let person = chip.person.key.clone();
            let turn = shown_turn.clone();
            let disabled = blocked.is_some() || busy;
            let reason = blocked.unwrap_or("Waiting for BroScribe Agent to answer.");
            Button::new(SharedString::from(format!("dock-person-{ix}")))
                .small()
                .outline()
                .when(chip.state.unconfirmed(), |button| button.border_dashed())
                .accessibility_label(chip.spoken(target))
                .when_some(chip.key.filter(|_| !disabled), |button, key| {
                    let stroke = gpui::Keystroke::parse(["1", "2", "3", "4"][key])
                        .expect("a digit is a keystroke");
                    button.child(gpui::component::kbd::Kbd::new(stroke))
                })
                .child(kit::avatar(&chip.person.name, disc, None, 16., cx))
                .child(chip.line())
                .map(|button| kit::disable_with_reason(button, disabled, reason, cx))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if let Some(turn) = turn.clone() {
                        this.dock_pick(
                            DockPick::Click {
                                person: person.clone(),
                                turn,
                            },
                            window,
                            cx,
                        );
                    }
                }))
        });
        let new_target = shown_turn.clone();
        let someone_new = Button::new("dock-someone-new")
            .label("Someone new…")
            .small()
            .ghost()
            .accessibility_label("Someone new: search people or add a name for this turn")
            .map(|button| {
                kit::disable_with_reason(
                    button,
                    blocked.is_some() || busy,
                    blocked.unwrap_or("Waiting for BroScribe Agent to answer."),
                    cx,
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(turn) = new_target.as_deref() {
                    this.open_live_naming(turn, window, cx);
                }
            }));
        let turn_words =
            line.unwrap_or_else(|| blocked.unwrap_or("Nobody is speaking yet.").to_owned());
        Some(
            div()
                .id("speaking-dock")
                .debug_selector(|| "speaking-dock".into())
                .role(Role::Group)
                .aria_label(title.clone())
                .track_focus(&self.dock_focus)
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                    if this.dock_key(event, window, cx) {
                        cx.stop_propagation();
                    }
                }))
                .flex()
                .flex_col()
                .gap_1()
                .flex_none()
                // Who is speaking, on one line beside the title.
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .min_w_0()
                        .child(
                            div()
                                .flex_none()
                                .child(crate::ui::subheading("dock-title", title)),
                        )
                        .child(
                            text_line("dock-turn", Role::Label, turn_words)
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_role(TextRole::Caption),
                        )
                        .when(view.retargeted, |el| {
                            el.child(
                                Button::new("dock-back")
                                    .label("Back to speaking now")
                                    .small()
                                    .ghost()
                                    .flex_none()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.dock_back(window, cx)
                                    })),
                            )
                        }),
                )
                // In a short window the heading and hint give their room to
                // the transcript; the list keeps its name.
                .when(roomy, |el| {
                    el.when_some(blocked.filter(|_| target.is_some()), |el, why| {
                        el.child(
                            text_line("dock-blocked", Role::Label, why)
                                .text_role(TextRole::Caption)
                                .text_color(muted),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_baseline()
                            .gap_2()
                            .child(crate::ui::subheading(
                                "dock-people-title",
                                "People in this call",
                            ))
                            .child(
                                text_line("dock-hint", Role::Label, hint)
                                    .text_role(TextRole::Caption)
                                    .text_color(muted),
                            ),
                    )
                })
                // One row of chips that scrolls sideways (at 200 % too).
                .child(
                    div()
                        .id("dock-people")
                        .role(Role::List)
                        .aria_label("People in this call")
                        .flex()
                        .items_center()
                        .gap_1()
                        .overflow_x_scroll()
                        .children(chips.map(|chip| chip.flex_none()))
                        .child(someone_new.flex_none()),
                )
                .into_any_element(),
        )
    }
}
