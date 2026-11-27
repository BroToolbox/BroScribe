//! The meeting's AI results and the speaker recap that is available before generation.

use super::speakers::speech_length;
use super::*;
use crate::ui::format::{due, percent};
use crate::ui::rich_text::OpenLink;
use crate::views::templates::{ResultBlock, markdown_blocks, result_document};
use gpui::component::checkbox::Checkbox;
use gpui::component::date_picker::{DatePicker, DatePickerState};
use gpui::component::menu::{ContextMenuExt as _, PopupMenu};
use gpui::component::popover::Popover;
use gpui::component::table::{Table, TableCell, TableHead, TableHeader, TableRow};
use gpui::component::{Disableable as _, IconName};
use gpui::{FocusHandle, Pixels, relative};
use std::rc::Rc;

const READING_MEASURE: f32 = 42.;
const TABLE_MIN_REM: f32 = 40.;
const SPEAKER_BAR_HEIGHT: Pixels = px(4.);

#[derive(Clone, Copy, PartialEq, Eq)]
enum SortColumn {
    Owner,
    Due,
}

enum ActionEditorKind {
    Owner(Entity<PersonPicker>),
    Due(Entity<DatePickerState>),
}

struct ActionEditor {
    id: String,
    kind: ActionEditorKind,
    _subscription: Option<Subscription>,
}

#[derive(Default)]
pub(super) struct Overview {
    summary: Option<(String, Vec<ResultBlock>)>,
    sort: Option<(SortColumn, bool)>,
    width: Pixels,
    action_editor: Option<ActionEditor>,
    /// The options button the last action editor opened from, by action:
    /// focus goes back to it when the editor closes (ACC-24).
    action_opener: Option<(String, FocusHandle)>,
}

impl Overview {
    pub(super) fn update(&mut self, data: &MeetingData) {
        let Some(result) = &data.meeting.enrichment else {
            self.summary = None;
            return;
        };
        let source = substitute_speakers(
            &data.meeting,
            &result.line_people,
            &result.summary,
            &data.people,
        );
        if !self.summary.as_ref().is_some_and(|(old, _)| old == &source) {
            let blocks = markdown_blocks(&source);
            self.summary = Some((source, blocks));
        }
    }

    fn sort_by(&mut self, column: SortColumn) {
        let descending = matches!(self.sort, Some((current, false)) if current == column);
        self.sort = Some((column, descending));
    }

    fn rows(&self, data: &MeetingData) -> Vec<ActionRow> {
        let mut rows = action_rows(&data.meeting, &data.people);
        if let Some((column, descending)) = self.sort {
            rows.sort_by(|a, b| {
                let order = match column {
                    SortColumn::Owner => match (a.owner.is_empty(), b.owner.is_empty()) {
                        (false, false) => a.owner.to_lowercase().cmp(&b.owner.to_lowercase()),
                        (a, b) => return a.cmp(&b),
                    },
                    SortColumn::Due => match (a.due, b.due) {
                        (Some(a), Some(b)) => a.cmp(&b),
                        (a, b) => return a.is_none().cmp(&b.is_none()),
                    },
                };
                if descending { order.reverse() } else { order }
            });
        }
        rows
    }
}

impl MeetingView {
    pub(super) fn enrichment(
        &self,
        data: &MeetingData,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let weak = cx.weak_entity();
        let observed_width = self.overview.width;
        let width = if observed_width > px(0.) {
            observed_width
        } else {
            window.viewport_size().width - window.rem_size() * 2.
        };
        let mut body = div()
            .id("enrichment-content")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .gap_6()
            .p_4()
            .overflow_y_scroll()
            .track_scroll(&self.enrichment_scroll)
            .child(
                gpui::canvas(
                    move |bounds, window, cx| {
                        if bounds.size.width != observed_width {
                            let weak = weak.clone();
                            window.defer(cx, move |_, cx| {
                                weak.update(cx, |this, cx| {
                                    let width = bounds.size.width;
                                    if this.overview.width != width {
                                        this.overview.width = width;
                                        this.generate.update(cx, |panel, cx| {
                                            panel.set_options_width(width, cx)
                                        });
                                        cx.notify();
                                    }
                                })
                                .ok();
                            });
                        }
                    },
                    |_, (), _, _| {},
                )
                .w_full()
                .h(rems(0.))
                .flex_none()
                .absolute(),
            )
            .children(self.overview_review(data, cx))
            .children(speakers(data, cx));
        let mut summary = kit::stack().child(self.generate.clone());
        if let Some((_, blocks)) = &self.overview.summary {
            if blocks.is_empty() {
                summary = summary.child(text_line(
                    "summary-no-text",
                    Role::Label,
                    "The provider returned no summary text for this meeting.",
                ));
            } else {
                let templates = self.templates.downgrade();
                let open: OpenLink = Rc::new(move |url, _, cx| {
                    templates.update(cx, |tab, cx| tab.open_link(url, cx)).ok();
                });
                summary = summary.child(kit::as_ai_result(
                    kit::stack()
                        .id("summary-reading")
                        .debug_selector(|| "summary-reading".into())
                        .w_full()
                        .max_w(rems(READING_MEASURE))
                        .children(result_document("summary", 0, "Summary", blocks, open, cx)),
                    cx,
                ));
            }
        }
        body = body.child(summary);
        if let Some(result) = &data.meeting.enrichment {
            body = body
                .children(self.overview_decisions(data, result, cx))
                .children(self.overview_actions(
                    data,
                    width >= window.rem_size() * TABLE_MIN_REM,
                    cx,
                ))
                .children(topics(result, cx));
        }
        body.children(self.overview_timeline(cx)).into_any_element()
    }

    fn overview_decisions(
        &self,
        data: &MeetingData,
        result: &Enrichment,
        cx: &Context<Self>,
    ) -> Option<Div> {
        if result.decisions.is_empty() {
            return None;
        }
        let lp = &result.line_people;
        let items = result.decisions.iter().enumerate().map(|(ix, decision)| {
            let at = decision.at_s;
            let text = substitute_speakers(&data.meeting, lp, &decision.text, &data.people);
            let who = enrichment_who(
                &data.meeting,
                lp,
                decision.by_speaker,
                decision.by_name.as_deref(),
                &data.people,
            );
            let time = spoken_position(at);
            let sentence = crate::jobs::sentence(&text);
            let name = who.as_ref().map_or_else(
                || format!("{sentence} At {time}."),
                |who| format!("{sentence} Decided by {who} at {time}."),
            );
            div()
                .id(("decision", ix))
                .spoken(Role::ListItem, name)
                .flex()
                .gap_2()
                .items_start()
                .child(
                    div()
                        .w(rems(4.))
                        .flex_none()
                        .child(timestamp(("decision-at", ix), at, cx)),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .max_w(rems(READING_MEASURE))
                        .child(div().text_role(TextRole::Reading).child(text))
                        .children(who.map(|name| {
                            let attribution = format!("Decided by {name}");
                            div()
                                .text_role(TextRole::Caption)
                                .text_color(cx.theme().muted_foreground)
                                .child(attribution)
                        })),
                )
        });
        Some(
            kit::stack()
                .child(crate::ui::heading("overview-decisions", "Decisions"))
                .child(
                    kit::stack()
                        .id("decisions")
                        .role(Role::List)
                        .aria_label("Decisions list")
                        .gap_2()
                        .children(items),
                ),
        )
    }

    fn show_transcript_at(&mut self, at: f64, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(data) = &self.data
            && !data.lines.is_empty()
        {
            let index = line_at(&data.lines, at).unwrap_or(0);
            self.set_tab(MeetingTab::Transcript, cx);
            self.following_transcript = false;
            self.select_transcript_row(index, window, cx);
        }
    }

    fn action_sort(&self, column: SortColumn, cx: &Context<Self>) -> Button {
        let (id, label) = match column {
            SortColumn::Owner => ("sort-action-owner", "Sort by owner"),
            SortColumn::Due => ("sort-action-due", "Sort by due date"),
        };
        let active = self
            .overview
            .sort
            .filter(|(selected, _)| *selected == column);
        let (icon, label) = match active {
            Some((_, true)) => (IconName::SortDescending, format!("{label}, descending")),
            Some((_, false)) => (IconName::SortAscending, format!("{label}, ascending")),
            None => (IconName::SortAscending, label.to_owned()),
        };
        button(id, "", true, cx, move |this: &mut Self, _, cx| {
            this.overview.sort_by(column);
            cx.notify();
        })
        .ghost()
        .xsmall()
        .icon(icon)
        .tooltip(label.clone())
        .accessibility_label(label)
    }

    fn overview_actions(&self, data: &MeetingData, wide: bool, cx: &Context<Self>) -> Option<Div> {
        let rows = self.overview.rows(data);
        if rows.is_empty() {
            return None;
        }
        let today = chrono::Local::now().date_naive();
        let due_date = |row: &ActionRow| {
            row.due.map(|day| {
                let overdue = row.open && day < today;
                let (shown, spoken) = due(day, today, overdue);
                (shown, spoken, overdue)
            })
        };
        let heading = crate::ui::heading("overview-actions", "Action items");
        if !wide {
            let list = kit::stack()
                .id("actions-list")
                .role(Role::List)
                .aria_label("Action items list")
                .children(rows.iter().enumerate().map(|(ix, row)| {
                    let mut caption = div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .items_center()
                        .text_role(TextRole::Caption);
                    if !row.owner.is_empty() {
                        caption = caption.child(text_line(
                            ("action-owner", ix),
                            Role::Label,
                            row.owner.clone(),
                        ));
                    }
                    if let Some((shown, spoken, overdue)) = due_date(row) {
                        let shown = format!("Due {shown}");
                        caption = caption
                            .child(
                                div()
                                    .id(("action-due", ix))
                                    .spoken(Role::Label, spoken)
                                    .child(shown),
                            )
                            .when(overdue, |el| el.child(attention_badge("Overdue", cx)));
                    }
                    caption = caption.child(timestamp(("action-at", ix), row.at_s, cx));
                    kit::stack()
                        .id(("action-summary", ix))
                        .role(Role::ListItem)
                        .aria_label(format!("Action {}", ix + 1))
                        .child(
                            div()
                                .flex()
                                .items_start()
                                .gap_2()
                                .child(self.action_checkbox(row, cx))
                                .child(self.action_text(ix, row, cx).flex_1())
                                .child(self.action_controls(row, cx)),
                        )
                        .child(caption)
                        .context_menu(Self::action_menu_builder(row, self.busy.is_none(), cx))
                }));
            return Some(
                kit::stack()
                    .child(heading)
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(self.action_sort(SortColumn::Owner, cx))
                            .child(self.action_sort(SortColumn::Due, cx)),
                    )
                    .child(list),
            );
        }

        let header = TableHeader::new().child(
            TableRow::new().children(ACTION_COLUMNS.iter().enumerate().map(
                |(ix, (name, width))| {
                    let content = div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(text_line(("action-column", ix), Role::Label, *name))
                        .when(ix == 2, |el| {
                            el.child(self.action_sort(SortColumn::Owner, cx))
                        })
                        .when(ix == 3, |el| {
                            el.child(self.action_sort(SortColumn::Due, cx))
                        });
                    let cell = TableHead::new().child(content).px_1();
                    match width {
                        Some(w) => cell.w(rems(*w)).flex_none(),
                        None => cell.flex_1().min_w(rems(12.)),
                    }
                },
            )),
        );
        let table = Table::new()
            .accessibility_label("Action items")
            .child(header)
            .children(rows.iter().enumerate().map(|(ix, row)| {
                let due_date = due_date(row);
                let due_cell = match due_date {
                    Some((shown, spoken, overdue)) => kit::stack()
                        .child(
                            div()
                                .id(("action-due", ix))
                                .spoken(Role::Label, spoken)
                                .child(shown),
                        )
                        .when(overdue, |el| el.child(attention_badge("Overdue", cx))),
                    None => kit::stack().child(text_line(
                        ("action-due", ix),
                        Role::Label,
                        "No due date",
                    )),
                };
                let owner = if row.owner.is_empty() {
                    "No owner"
                } else {
                    &row.owner
                };
                TableRow::new()
                    .id(SharedString::from(format!("action-row-{}", row.id)))
                    .child(
                        TableCell::new()
                            .w(rems(2.5))
                            .flex_none()
                            .px_1()
                            .child(self.action_checkbox(row, cx)),
                    )
                    .child(
                        TableCell::new().flex_1().min_w(rems(12.)).px_1().child(
                            self.action_text(ix, row, cx)
                                .id(("action-context", ix))
                                .context_menu(Self::action_menu_builder(
                                    row,
                                    self.busy.is_none(),
                                    cx,
                                )),
                        ),
                    )
                    .child(
                        TableCell::new()
                            .w(rems(9.))
                            .flex_none()
                            .px_1()
                            .child(text_line(
                                ("action-owner", ix),
                                Role::Label,
                                owner.to_owned(),
                            )),
                    )
                    .child(
                        TableCell::new()
                            .w(rems(7.))
                            .flex_none()
                            .px_1()
                            .child(due_cell),
                    )
                    .child(
                        TableCell::new().w(rems(6.5)).flex_none().px_1().child(
                            div()
                                .flex()
                                .items_center()
                                .child(timestamp(("action-at", ix), row.at_s, cx))
                                .child(self.action_controls(row, cx)),
                        ),
                    )
            }));
        Some(
            kit::stack()
                .child(heading)
                .child(div().id("actions-table").child(table)),
        )
    }

    fn action_menu_builder(
        row: &ActionRow,
        enabled: bool,
        cx: &Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static + use<>
    {
        let view = cx.weak_entity();
        let row = row.clone();
        move |menu, _, _| {
            let item = |label: &str, status| {
                let id = row.id.clone();
                let view = view.clone();
                let label = SharedString::from(label.to_owned());
                PopupMenuItem::new(label)
                    .disabled(!enabled)
                    .on_click(move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.set_action_status(id.clone(), status, window, cx)
                        });
                    })
            };
            let edit = |label, owner| {
                let id = row.id.clone();
                let view = view.clone();
                let due = row.due;
                PopupMenuItem::new(label)
                    .disabled(!enabled)
                    .on_click(move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.open_action_editor(id.clone(), owner, due, window, cx)
                        });
                    })
            };
            menu.when(!row.dropped, |menu| {
                menu.item(if row.done {
                    item("Reopen", ActionStatus::Open)
                } else {
                    item("Mark as done", ActionStatus::Done)
                })
            })
            .item(if row.dropped {
                item("Restore", ActionStatus::Open)
            } else {
                item("Drop", ActionStatus::Dropped)
            })
            .separator()
            .item(edit("Change owner…", true))
            .item(edit("Change due date…", false))
        }
    }

    fn action_controls(&self, row: &ActionRow, cx: &Context<Self>) -> AnyElement {
        let trigger = Button::new(SharedString::from(format!("action-menu-{}", row.id)))
            .icon(IconName::Ellipsis)
            .ghost()
            .small()
            .tooltip("Action options")
            .accessibility_label(format!("Options for {}", row.action))
            .when_some(
                self.overview
                    .action_opener
                    .as_ref()
                    .filter(|(id, _)| id == &row.id),
                |trigger, (_, focus)| trigger.track_focus(focus),
            );
        if !self
            .overview
            .action_editor
            .as_ref()
            .is_some_and(|editor| editor.id == row.id)
        {
            return trigger
                .dropdown_menu(Self::action_menu_builder(row, self.busy.is_none(), cx))
                .into_any_element();
        }
        let view = cx.weak_entity();
        let close = cx.listener(|this, open: &bool, window, cx| {
            if !open {
                this.close_action_editor(window, cx);
            }
        });
        Popover::new(format!("action-editor-{}", row.id))
            .trigger(trigger)
            .open(true)
            .overlay_closable(false)
            .on_open_change(close)
            .content(move |_, _, cx| {
                view.update(cx, |this, cx| this.action_editor_panel(cx))
                    .unwrap_or_else(|_| div())
            })
            .into_any_element()
    }

    fn open_action_editor(
        &mut self,
        id: String,
        owner: bool,
        due: Option<chrono::NaiveDate>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        let (kind, subscription) = if owner {
            let state = self.state.clone();
            let picker = cx.new(|cx| {
                let mut picker = PersonPicker::new(state, "Action owner", window, cx);
                picker.set_existing_only();
                picker
            });
            let action = id.clone();
            let subscription = cx.subscribe_in(
                &picker,
                window,
                move |this: &mut Self, _, event: &PersonPickerEvent, window, cx| {
                    if !this
                        .overview
                        .action_editor
                        .as_ref()
                        .is_some_and(|e| e.id == action)
                    {
                        return;
                    }
                    match event {
                        PersonPickerEvent::Picked(PersonChoice::Existing(owner)) => this
                            .save_action_edit(
                                action.clone(),
                                ops::ActionEdit {
                                    owner: Some(owner.clone()),
                                    ..Default::default()
                                },
                                window,
                                cx,
                            ),
                        PersonPickerEvent::Cancelled => this.close_action_editor(window, cx),
                        PersonPickerEvent::Picked(PersonChoice::Typed(_)) => {}
                    }
                },
            );
            (ActionEditorKind::Owner(picker), Some(subscription))
        } else {
            let date = cx.new(|cx| {
                let mut date = DatePickerState::new(window, cx);
                if let Some(due) = due {
                    date.set_date(due, window, cx);
                }
                date
            });
            (ActionEditorKind::Due(date), None)
        };
        if self
            .overview
            .action_opener
            .as_ref()
            .is_none_or(|(opener, _)| opener != &id)
        {
            self.overview.action_opener = Some((id.clone(), cx.focus_handle()));
        }
        self.overview.action_editor = Some(ActionEditor {
            id,
            kind,
            _subscription: subscription,
        });
        cx.notify();
    }

    fn action_editor_panel(&self, cx: &Context<Self>) -> Div {
        let Some(editor) = &self.overview.action_editor else {
            return div();
        };
        let id = editor.id.clone();
        let mut panel = div().flex().flex_col().gap_2().w(rems(22.));
        match &editor.kind {
            ActionEditorKind::Owner(person) => {
                panel = panel.child(person.clone()).child(
                    button(
                        "action-clear-owner",
                        "Clear owner",
                        true,
                        cx,
                        move |this, window, cx| {
                            this.save_action_edit(
                                id.clone(),
                                ops::ActionEdit {
                                    clear_owner: true,
                                    ..Default::default()
                                },
                                window,
                                cx,
                            );
                        },
                    )
                    .ghost()
                    .small(),
                );
            }
            ActionEditorKind::Due(date) => {
                let date = date.clone();
                panel = panel
                    .child(text_line("action-due-label", Role::Label, "Due date"))
                    .child(
                        DatePicker::new(&date)
                            .placeholder("Choose a date")
                            .cleanable(true),
                    )
                    .child(kit::confirm_actions(
                        button(
                            "action-cancel-due",
                            "Cancel",
                            true,
                            cx,
                            |this, window, cx| {
                                this.close_action_editor(window, cx);
                            },
                        )
                        .outline(),
                        button(
                            "action-save-due",
                            "Save",
                            true,
                            cx,
                            move |this, window, cx| {
                                let due = date.read(cx).date_time().start().map(|date| date.date());
                                this.save_action_edit(
                                    id.clone(),
                                    ops::ActionEdit {
                                        due,
                                        clear_due: due.is_none(),
                                        ..Default::default()
                                    },
                                    window,
                                    cx,
                                );
                            },
                        )
                        .map(|button| kit::primary(button, false, cx)),
                        cx,
                    ));
            }
        }
        panel
    }

    /// Closes the action editor and gives focus back to the options button
    /// it opened from.
    fn close_action_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.overview.action_editor.take().is_none() {
            return;
        }
        if let Some((_, focus)) = &self.overview.action_opener {
            focus.focus(window, cx);
        }
        cx.notify();
    }

    fn save_action_edit(
        &mut self,
        action: String,
        edit: ops::ActionEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_action_editor(window, cx);
        let id = self.meeting_id.clone();
        self.run_write_then(
            EditTarget::Action(action.clone()),
            "Wait until the action item is saved.",
            window,
            cx,
            move |lib| {
                ops::with_index(lib, |tx, index| {
                    ops::edit_action(tx, index, &id, &action, edit)
                })
            },
            |_, result, _, _| match result {
                Ok(_) => Notice::Info("Action updated.".into()),
                Err(error) => Notice::Error(failure_text(
                    "Couldn't update the action.",
                    &meeting_user_text(&error),
                    "",
                )),
            },
        );
    }

    fn action_checkbox(&self, row: &ActionRow, cx: &Context<Self>) -> Checkbox {
        let id = row.id.clone();
        let pending = self
            .busy
            .as_ref()
            .is_some_and(|(active, _)| active == &EditTarget::Action(id.clone()));
        Checkbox::new(SharedString::from(format!("action-done-{id}")))
            .checked(row.done)
            .disabled(pending || row.dropped)
            .when(row.dropped, |checkbox| {
                checkbox.tooltip("Restore the action to mark it done.")
            })
            .accessibility_label(format!("Done: {}", row.action))
            .on_change(cx.listener(move |this, checked, window, cx| {
                let status = if *checked {
                    ActionStatus::Done
                } else {
                    ActionStatus::Open
                };
                this.set_action_status(id.clone(), status, window, cx);
            }))
    }

    fn action_text(&self, ix: usize, row: &ActionRow, cx: &Context<Self>) -> Div {
        let spoken = if row.stale {
            format!("{}, not in the latest summary", row.action)
        } else if row.dropped {
            format!("{}, dropped", row.action)
        } else {
            row.action.clone()
        };
        kit::stack()
            .min_w_0()
            .gap_1()
            .child(
                div()
                    .id(("action-text", ix))
                    .spoken(Role::Label, spoken)
                    .text_role(TextRole::Reading)
                    .when(row.done || row.dropped, |el| {
                        el.line_through().text_color(cx.theme().muted_foreground)
                    })
                    .child(row.action.clone()),
            )
            .when(row.stale, |el| {
                el.child(kit::badge(
                    kit::BadgeTone::Outline,
                    "Not in latest summary",
                    cx,
                ))
            })
            .when(row.dropped, |el| {
                el.child(kit::badge(kit::BadgeTone::Outline, "Dropped", cx))
            })
            .children(self.edit_line(
                &EditTarget::Action(row.id.clone()),
                ("action-status", ix),
                cx,
            ))
    }
}

fn timestamp(id: impl Into<ElementId>, at: f64, cx: &Context<MeetingView>) -> Button {
    button(
        id,
        crate::views::generate::elapsed_text(at.max(0.) as i64),
        true,
        cx,
        move |this, window, cx| this.show_transcript_at(at, window, cx),
    )
    .link()
    .small()
    .map(tabular)
    .accessibility_label(format!("Show in transcript at {}", spoken_position(at)))
}

fn speakers(data: &MeetingData, cx: &App) -> Option<Div> {
    if data.speakers.is_empty() {
        return None;
    }
    let mut ordered: Vec<_> = data.speakers.iter().collect();
    ordered.sort_by(|a, b| b.talk_s.total_cmp(&a.talk_s));
    let total: f64 = data
        .speakers
        .iter()
        .chain(&data.speakers_without_evidence)
        .map(|speaker| speaker.talk_s.max(0.))
        .sum();
    let items = ordered.into_iter().enumerate().map(|(ix, speaker)| {
        let share = if total > 0. {
            speaker.talk_s.max(0.) / total
        } else {
            0.
        };
        let unnamed = matches!(speaker.assignment, Assignment::None | Assignment::Suggested);
        let talk = speech_length(speaker.talk_s, true);
        let percent = percent((share * 100.).round() as u8);
        let state = if unnamed { ", needs a name" } else { "" };
        div()
            .id(("overview-speaker", ix))
            .spoken(
                Role::ListItem,
                format!(
                    "{}{state}, {talk} of speech, {percent} of all speech",
                    speaker.label
                ),
            )
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(speaker.label.clone())
                    .when(unnamed, |el| el.child(attention_badge("Needs a name", cx)))
                    .child(
                        div()
                            .text_role(TextRole::Caption)
                            .child(speech_length(speaker.talk_s, false)),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .h(SPEAKER_BAR_HEIGHT)
                    .rounded_full()
                    .bg(cx.theme().muted)
                    .child(
                        div()
                            .h_full()
                            .w(relative(share as f32))
                            .rounded_full()
                            .bg(cx.theme().muted_foreground),
                    ),
            )
    });
    Some(
        kit::stack()
            .child(crate::ui::heading("overview-speakers", "Speakers"))
            .child(
                kit::stack()
                    .id("overview-speaker-list")
                    .role(Role::List)
                    .aria_label("Speakers by talk time")
                    .children(items),
            ),
    )
}

fn topics(result: &Enrichment, cx: &App) -> Option<Div> {
    if result.topics.is_empty() {
        return None;
    }
    Some(
        kit::stack()
            .child(crate::ui::heading("overview-topics", "Topics"))
            .child(
                div()
                    .id("overview-topic-list")
                    .role(Role::List)
                    .aria_label("Topics list")
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .children(result.topics.iter().enumerate().map(|(ix, topic)| {
                        kit::tag_chip(topic.clone(), cx)
                            .id(("overview-topic", ix))
                            .spoken(Role::ListItem, topic.clone())
                    })),
            ),
    )
}

fn attention_badge(label: &str, cx: &App) -> Div {
    let warning = cx.theme().warning;
    kit::badge(kit::BadgeTone::Outline, label.to_owned(), cx)
        .border_color(warning)
        .text_color(warning)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::a11y_tree::draw_and_read;
    use crate::ui::kit::dialog_gate::{click, draw};
    use crate::ui::spoken_log::{assert_distinct, assert_shown, node, while_building};
    use gpui::{TestAppContext, VisualTestContext};

    fn result() -> Enrichment {
        serde_json::from_value(json!({
            "provider": "synthetic",
            "created_at": "2026-11-15T00:00:00Z",
            "summary": "## Release recap\n\nThe team **agreed**.\n\n- Plan the release.",
            "instructions": "Use synthetic examples.",
            "stale": true,
            "decisions": [
                {"id":"d1","text":"Ship the beta","at_s":0.0,"by_name":"Liro Sample"},
                {"id":"d2","text":"Review the draft","at_s":10.0}
            ],
            "actions": [
                {"id":"a1","text":"Draft the plan","at_s":10.0,"owner_name":"Zeno Sample","due":"2000-01-01","stale":true},
                {"id":"a2","text":"Book the room","at_s":0.0,"owner_name":"Cato Sample","due":"2099-01-01","status":"done"},
                {"id":"a3","text":"Send the notes","at_s":0.0}
            ],
            "topics": ["Beta planning", "Release notes"]
        })).unwrap()
    }

    #[gpui::test]
    fn overview_reading_order_markdown_credit_and_copy(cx: &mut TestAppContext) {
        let (handle, _, lib, fixture, dir) = super::super::tests::open_edited(cx, |meeting, _| {
            meeting.enrichment = Some(result());
        });
        super::super::tests::reply(dir.path(), json!({"providers": []}));
        handle
            .update(cx, |view, window, cx| {
                window.resize(gpui::size(px(1200.), px(1140.)));
                gpui::init(cx);
                view.set_tab(MeetingTab::Overview, cx);
            })
            .unwrap();
        cx.run_until_parked();
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        draw(&mut visual);
        let named = handle
            .update(&mut visual, |view, window, cx| {
                while_building(|| view.enrichment(view.data.as_ref().unwrap(), window, cx)).1
            })
            .unwrap();
        assert_distinct(&named, &["action-owner-*", "action-due-*"]);
        assert_shown(
            &named,
            &["Release recap", "The team agreed.", "Plan the release."],
        );
        assert_eq!(
            node(&named, "decision-0"),
            (
                Role::ListItem,
                "Ship the beta. Decided by Liro Sample at 0 seconds."
            )
        );
        assert_eq!(
            node(&named, "overview-topic-1"),
            (Role::ListItem, "Release notes")
        );

        let tree = visual.update(draw_and_read);
        let outline = [
            "overview-speakers",
            "overview-summary",
            "overview-decisions",
            "overview-actions",
            "overview-topics",
        ];
        assert!(
            outline
                .map(|id| tree.find(id))
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        assert!(tree.find("overview-summary") < tree.find("summary-h-0-0"));
        assert_eq!(
            tree.read(tree.find("summary-ai-note")),
            ("Label", crate::views::generate::AI_RESULT_NOTE)
        );
        assert_eq!(tree.all("summary-stale").len(), 1);
        assert!(tree.find("enrichment-credit") < tree.find("summary-stale"));
        assert!(tree.find("summary-stale") < tree.find("summary-h-0-0"));
        assert!(
            tree.read(tree.find("enrichment-credit"))
                .1
                .starts_with("AI summary by unknown provider (synthetic)")
        );
        assert_eq!(
            tree.read(tree.find("decisions")),
            ("List", "Decisions list")
        );
        assert_eq!(
            tree.read(tree.find("overview-topic-list")),
            ("List", "Topics list")
        );
        click(&mut visual, "credit-instructions", Role::Link);
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("credit-instructions-text")),
            ("Label", "Use synthetic examples.")
        );
        visual.simulate_keystrokes("escape");
        click(&mut visual, "overview-copy", Role::Button);
        let current = lib.load_meeting(&fixture.meeting).unwrap();
        let expected = broscribe_core::export::render_export(
            &current,
            &lib.people_lookup().unwrap(),
            &[
                broscribe_core::export::ExportSection::Summary,
                broscribe_core::export::ExportSection::Decisions,
                broscribe_core::export::ExportSection::Actions,
            ],
            broscribe_core::export::ExportFormat::Markdown,
            true,
        )
        .unwrap();
        assert_eq!(
            visual.read_from_clipboard().unwrap().text().unwrap(),
            expected.content
        );
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("overview-copied")),
            ("Status", "Copied.")
        );
    }

    /// ACC-24: the action editor (owner or due date) closes on Escape and on
    /// Cancel, and focus goes back to the action's options button.
    #[gpui::test]
    fn action_editor_closes_on_escape_back_to_its_options_button(cx: &mut TestAppContext) {
        let (handle, _, _, _, _dir) = super::super::tests::open_edited(cx, |meeting, _| {
            meeting.enrichment = Some(result());
        });
        cx.simulate_window_resize(handle.into(), gpui::size(px(1180.), px(1300.)));
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        handle
            .update(&mut visual, |view, _, cx| {
                gpui::init(cx);
                view.set_tab(MeetingTab::Overview, cx);
            })
            .unwrap();
        visual.run_until_parked();
        draw(&mut visual);
        let open = |visual: &mut VisualTestContext, owner: bool| {
            handle
                .update(visual, |view, window, cx| {
                    view.open_action_editor("a1".into(), owner, None, window, cx)
                })
                .unwrap();
            visual.run_until_parked();
            draw(visual);
            draw(visual);
            handle
                .update(visual, |view, window, _| {
                    assert!(view.overview.action_editor.is_some(), "the editor opened");
                    let (id, opener) = view.overview.action_opener.clone().unwrap();
                    assert_eq!(id, "a1");
                    assert!(!opener.is_focused(window), "focus moved into the editor");
                })
                .unwrap();
        };
        let closed_on_opener = |visual: &mut VisualTestContext| {
            visual.run_until_parked();
            draw(visual);
            handle
                .update(visual, |view, window, _| {
                    assert!(view.overview.action_editor.is_none(), "the editor closed");
                    let (_, opener) = view.overview.action_opener.as_ref().unwrap();
                    assert!(
                        opener.is_focused(window),
                        "focus is back on the options button"
                    );
                })
                .unwrap();
            let tree = visual.update(draw_and_read);
            assert_eq!(
                tree.read(tree.find("action-menu-a1")),
                ("Button", "Options for Draft the plan")
            );
        };
        for owner in [false, true] {
            open(&mut visual, owner);
            visual.simulate_keystrokes("escape");
            closed_on_opener(&mut visual);
        }
        open(&mut visual, false);
        click(&mut visual, "action-cancel-due", Role::Button);
        closed_on_opener(&mut visual);
    }

    #[gpui::test]
    fn overview_table_sort_checkbox_dates_and_timestamp_jumps(cx: &mut TestAppContext) {
        let (handle, _, lib, fixture, _dir) = super::super::tests::open_edited(cx, |meeting, _| {
            meeting.enrichment = Some(result());
        });
        cx.simulate_window_resize(handle.into(), gpui::size(px(1180.), px(1300.)));
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        handle
            .update(&mut visual, |view, _, cx| {
                gpui::init(cx);
                view.set_tab(MeetingTab::Overview, cx);
            })
            .unwrap();
        visual.run_until_parked();
        draw(&mut visual);
        let tree = visual.update(draw_and_read);
        assert!(
            tree.nodes
                .iter()
                .any(|n| n.role == "Table" && n.name == "Action items")
        );
        for (index, label) in ["Done", "Action", "Owner", "Due", "Said at"]
            .into_iter()
            .enumerate()
        {
            let node = tree.find(&format!("action-column-{index}"));
            assert_eq!(tree.read(node), ("Label", label));
            assert_eq!(tree.read(tree.parent(node).unwrap()).0, "ColumnHeader");
        }
        assert_eq!(
            tree.read(tree.find("action-text-0")),
            ("Label", "Draft the plan, not in the latest summary")
        );
        assert_eq!(
            tree.read(tree.find("action-due-0")),
            ("Label", "Due Saturday 1 January 2000, overdue")
        );
        let checkbox = |visual: &mut VisualTestContext, id: &str| {
            visual.update(|window, _| {
                gpui::base::test_support::snapshots(window)
                    .into_iter()
                    .find(|node| {
                        node.path()
                            .last()
                            .is_some_and(|part| part.to_string() == id)
                    })
                    .unwrap()
            })
        };
        let checked = |visual: &mut VisualTestContext, id| checkbox(visual, id).checked();
        assert_eq!(
            checkbox(&mut visual, "action-done-a1").label(),
            Some("Done: Draft the plan")
        );
        assert_eq!(checked(&mut visual, "action-done-a1"), Some(false));
        assert_eq!(checked(&mut visual, "action-done-a2"), Some(true));
        handle
            .update(&mut visual, |view, _, cx| {
                view.busy = Some((EditTarget::Action("a1".into()), "Saving…"));
                cx.notify();
            })
            .unwrap();
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("action-status-0")),
            ("Status", "Saving…")
        );
        assert_eq!(
            checkbox(&mut visual, "action-done-a1").disabled(),
            Some(true)
        );
        assert_ne!(
            checkbox(&mut visual, "action-done-a2").disabled(),
            Some(true)
        );
        handle
            .update(&mut visual, |view, _, cx| {
                view.busy = None;
                cx.notify();
            })
            .unwrap();
        click(&mut visual, "sort-action-owner", Role::Button);
        let tree = visual.update(draw_and_read);
        assert_eq!(tree.read(tree.find("action-text-0")).1, "Book the room");
        click(&mut visual, "sort-action-owner", Role::Button);
        let tree = visual.update(draw_and_read);
        assert!(
            tree.read(tree.find("action-text-0"))
                .1
                .starts_with("Draft the plan")
        );
        click(&mut visual, "sort-action-due", Role::Button);
        let tree = visual.update(draw_and_read);
        assert_eq!(tree.read(tree.find("action-owner-0")).1, "Zeno Sample");
        click(&mut visual, "sort-action-due", Role::Button);
        let tree = visual.update(draw_and_read);
        assert_eq!(tree.read(tree.find("action-owner-0")).1, "Cato Sample");
        for _ in 0..48 {
            if checkbox(&mut visual, "action-done-a1").focused() == Some(true) {
                break;
            }
            visual.update(|window, cx| window.focus_next(cx));
            draw(&mut visual);
        }
        assert_eq!(
            checkbox(&mut visual, "action-done-a1").focused(),
            Some(true)
        );
        let space = gpui::Keystroke::parse("space").unwrap();
        visual.simulate_event(gpui::KeyDownEvent {
            keystroke: space.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        visual.simulate_event(gpui::KeyUpEvent { keystroke: space });
        visual.run_until_parked();
        draw(&mut visual);
        assert_eq!(checked(&mut visual, "action-done-a1"), Some(true));
        assert_eq!(
            checkbox(&mut visual, "action-done-a1").focused(),
            Some(true)
        );
        assert_eq!(
            lib.load_meeting(&fixture.meeting)
                .unwrap()
                .enrichment
                .unwrap()
                .actions[0]
                .status,
            ActionStatus::Done
        );
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("action-status-1")),
            ("Status", "Marked done.")
        );
        handle
            .update(&mut visual, |view, window, cx| {
                view.replay_edit(false, window, cx)
            })
            .unwrap();
        visual.run_until_parked();
        draw(&mut visual);
        assert_eq!(checked(&mut visual, "action-done-a1"), Some(false));

        click(&mut visual, "decision-at-0", Role::Link);
        let assert_jump = |visual: &mut VisualTestContext, index: usize| {
            handle
                .update(visual, |view, window, _| {
                    assert_eq!(view.tab(), MeetingTab::Transcript);
                    assert!(
                        view.transcript_rows[view.transcript_row(index).unwrap()]
                            .is_focused(window)
                    );
                    assert!(!view.playback.playing);
                })
                .unwrap();
        };
        assert_jump(&mut visual, 0);
        handle
            .update(&mut visual, |view, _, cx| {
                view.set_tab(MeetingTab::Overview, cx)
            })
            .unwrap();
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("action-at-1")),
            ("Link", "Show in transcript at 10 seconds")
        );
        click(&mut visual, "action-at-1", Role::Link);
        assert_jump(&mut visual, 1);
    }

    #[gpui::test]
    fn overview_has_speakers_without_ai_and_adapts_reading_and_actions(cx: &mut TestAppContext) {
        let (handle, _, lib, fixture, _dir) = super::super::tests::open_edited(cx, |meeting, _| {
            meeting.speakers[0].talk_s = 30.;
            meeting.speakers[1].talk_s = 90.;
            meeting.speakers[2].talk_s = 0.;
        });
        cx.simulate_window_resize(handle.into(), gpui::size(px(1400.), px(1000.)));
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        handle
            .update(&mut visual, |view, _, cx| {
                view.set_tab(MeetingTab::Overview, cx)
            })
            .unwrap();
        visual.run_until_parked();
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("overview-speaker-list")),
            ("List", "Speakers by talk time")
        );
        let longest = tree.read(tree.find("overview-speaker-0")).1;
        assert!(longest.starts_with("Speaker 2, needs a name"), "{longest}");
        assert!(longest.ends_with("75% of all speech"), "{longest}");
        assert!(tree.all("enrichment-credit").is_empty());
        assert!(tree.all("overview-topics").is_empty());
        lib.write(|tx| {
            let mut meeting = tx.library().load_meeting(&fixture.meeting)?;
            meeting.enrichment = Some(Enrichment {
                summary: String::new(),
                ..result()
            });
            tx.save_meeting(&meeting)
        })
        .unwrap();
        handle
            .update(&mut visual, |view, window, cx| view.reload(window, cx))
            .unwrap();
        visual.run_until_parked();
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("summary-no-text")),
            (
                "Label",
                "The provider returned no summary text for this meeting."
            )
        );
        assert_eq!(
            tree.read(tree.find("overview-topic-0")),
            ("ListItem", "Beta planning")
        );
        handle
            .update(&mut visual, |view, _, cx| {
                let mut data = view.data.take().unwrap();
                data.meeting.enrichment = Some(result());
                view.overview.update(&data);
                view.data = Some(data);
                cx.notify();
            })
            .unwrap();
        for rem in [16., 20.] {
            visual.update(|window, cx| {
                window.set_rem_size(px(rem));
                window.refresh();
                window.draw(cx).clear(cx);
            });
            let width = visual.debug_bounds("summary-reading").unwrap().size.width;
            assert_eq!(width, px(rem * READING_MEASURE));
        }
        visual.simulate_resize(gpui::size(px(550.), px(1000.)));
        visual.run_until_parked();
        draw(&mut visual);
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("actions-list")),
            ("List", "Action items list")
        );
    }
}
