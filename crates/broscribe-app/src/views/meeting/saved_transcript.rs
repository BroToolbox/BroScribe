//! The saved meeting's variable-height transcript, find bar and line actions.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;

use crate::ui::select::{OptionItem, Picker};
use broscribe_core::names::fold_search_text;
use gpui::assets::IconName;
use gpui::base::ElementExt as _;
use gpui::component::input::Copy;
use gpui::component::menu::PopupMenu;
use gpui::component::popover::Popover;
use gpui::{
    HighlightStyle, InteractiveText, ListAlignment, MouseButton, StyledText, UnderlineStyle,
};

use super::*;

gpui::actions!(saved_transcript, [NameTranscriptLine, PlayTranscriptLine]);

pub(super) struct LineMenu {
    menu: Entity<PopupMenu>,
    position: gpui::Point<gpui::Pixels>,
    focus_first: Rc<Cell<bool>>,
    _dismiss: Subscription,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CopyStatus {
    Line,
    Transcript,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum Entry {
    Line(usize),
    Marker(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Marker {
    pub(super) at: f64,
    kind: &'static str,
    icon: &'static str,
    text: Option<String>,
}

pub(super) fn markers(meeting: &Meeting) -> Vec<Marker> {
    let mut markers: Vec<_> = meeting
        .bookmarks
        .iter()
        .map(|mark| Marker {
            at: mark.at_s,
            kind: "Bookmark",
            icon: "icons/bookmark.svg",
            text: mark.label.clone(),
        })
        .chain(meeting.notes.iter().map(|note| Marker {
            at: note.at_s,
            kind: "Note",
            icon: "icons/sticky-note.svg",
            text: Some(note.text.clone()),
        }))
        .collect();
    markers.sort_by(|left, right| left.at.total_cmp(&right.at));
    markers
}

impl Marker {
    fn label(&self, time: String) -> String {
        let mut words = format!("{} · {time}", self.kind);
        if let Some(text) = self.text.as_deref().filter(|text| !text.is_empty()) {
            words.push_str(" · ");
            words.push_str(text);
        }
        words
    }

    fn content(&self, index: usize, cx: &Context<MeetingView>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(kit::glyph(self.icon, cx.theme().muted_foreground))
            .child(
                div()
                    .id(("marker", index))
                    .spoken(Role::Note, self.label(spoken_position(self.at)))
                    .text_role(TextRole::Caption)
                    .child(self.label(format_clock(self.at))),
            )
    }
}

/// The disc before a line's speaker: their hue when the line names someone,
/// a dashed ring for an unnamed speaker (suggested or not).
fn line_disc(line: &TranscriptLine, data: &MeetingData) -> kit::Disc {
    if let Some(person) = line.person_id.as_ref().and_then(|id| data.people.get(id)) {
        return kit::Disc::for_person(Some(person));
    }
    let suggested = line.speaker.is_some_and(|id| {
        data.meeting
            .speakers
            .iter()
            .any(|speaker| speaker.id == id && speaker.suggestion.is_some())
    });
    if suggested {
        kit::Disc::Suggested
    } else {
        kit::Disc::Voice(None)
    }
}

fn line_speaker_key(line: &TranscriptLine) -> String {
    if let Some(person) = &line.person_id {
        person.to_string()
    } else if let Some(speaker) = line.speaker {
        speaker.to_string()
    } else {
        format!("label:{}", line.label)
    }
}

fn segment_ranges(line: &TranscriptLine, data: &MeetingData) -> Vec<(Range<usize>, f64, f64)> {
    let mut offset = 0;
    line.segment_ids
        .iter()
        .filter_map(|id| {
            let segment = data.meeting.segments.get(*data.segment_indices.get(id)?)?;
            let end = offset + segment.text.trim().len();
            let bytes = offset..end;
            offset = end + 1;
            (!bytes.is_empty() && end <= line.text.len()).then_some((
                bytes,
                segment.start,
                segment.end,
            ))
        })
        .collect()
}

impl MeetingView {
    pub(super) fn new_transcript_list(cx: &Context<Self>) -> ListState {
        let list = ListState::new(0, ListAlignment::Top, px(400.)).measure_all();
        let view = cx.weak_entity();
        list.set_scroll_handler(move |_, window, cx| {
            let view = view.clone();
            // GPUI computes the event's range from the old anchor. Read the
            // new anchor after it releases its mutable list borrow.
            window.defer(cx, move |window, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.following_transcript = this
                        .playback
                        .current_line
                        .and_then(|ix| this.transcript_row(ix))
                        .is_some_and(|row| !this.transcript_row_outside(row));
                    if this.naming.is_none()
                        && this.focused_entry(window).is_none()
                        && let Some(active) = this.active_transcript_row()
                        && this.transcript_row_outside(active)
                    {
                        this.transcript_cursor =
                            Some(this.transcript_list.logical_scroll_top().item_ix);
                    }
                    cx.notify();
                });
            });
        });
        list
    }

    pub(super) fn sync_transcript_rows(
        &mut self,
        lines: Option<&[TranscriptLine]>,
        markers: Vec<Marker>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let old = self.data.as_ref().map_or(&[][..], |data| &data.lines);
        if lines.is_some_and(|lines| old == lines) && self.timeline == markers {
            return;
        }
        let lines = lines.unwrap_or(old);
        if !self.transcript_speaker.is_empty()
            && !lines
                .iter()
                .any(|line| line_speaker_key(line) == self.transcript_speaker)
        {
            self.transcript_speaker.clear();
        }
        let key = |entry: Entry, lines: &[TranscriptLine]| match entry {
            Entry::Line(ix) => lines[ix]
                .segment_ids
                .first()
                .copied()
                .map(|id| (false, id as usize)),
            Entry::Marker(ix) => Some((true, ix)),
        };
        let old_key = |ix: usize| {
            self.transcript_entries
                .get(ix)
                .and_then(|&entry| key(entry, old))
        };
        let scroll = self.transcript_list.logical_scroll_top();
        let top = old_key(scroll.item_ix);
        let cursor = self.transcript_cursor.and_then(old_key);
        let focused = self
            .transcript_rows
            .iter()
            .position(|row| row.is_focused(window))
            .and_then(old_key);
        let mut saved = self
            .transcript_entries
            .iter()
            .copied()
            .zip(self.transcript_rows.drain(..))
            .filter_map(|(entry, handle)| Some((key(entry, old)?, handle)))
            .collect::<HashMap<_, _>>();
        let old_len = self.transcript_entries.len();
        self.transcript_entries.clear();
        self.transcript_line_rows.clear();
        let mut upcoming = markers.iter().enumerate().peekable();
        for (ix, line) in lines.iter().enumerate() {
            while let Some((marker, _)) = upcoming.next_if(|(_, mark)| mark.at <= line.start) {
                self.transcript_entries.push(Entry::Marker(marker));
            }
            if self.transcript_speaker.is_empty()
                || line_speaker_key(line) == self.transcript_speaker
            {
                self.transcript_line_rows
                    .push(Some(self.transcript_entries.len()));
                self.transcript_entries.push(Entry::Line(ix));
            } else {
                self.transcript_line_rows.push(None);
            }
        }
        self.transcript_entries
            .extend(upcoming.map(|(ix, _)| Entry::Marker(ix)));
        self.timeline = markers;
        self.transcript_rows = self
            .transcript_entries
            .iter()
            .map(|&entry| {
                key(entry, lines)
                    .and_then(|key| saved.remove(&key))
                    .unwrap_or_else(|| cx.focus_handle())
            })
            .collect();
        let index_of = |row_key| {
            self.transcript_entries
                .iter()
                .position(|&entry| key(entry, lines) == Some(row_key))
        };
        self.transcript_list
            .splice_focusable(0..old_len, self.transcript_rows.iter().cloned().map(Some));
        self.transcript_cursor = cursor.and_then(index_of);
        if let Some(ix) = top.and_then(index_of) {
            self.transcript_list.scroll_to(gpui::ListOffset {
                item_ix: ix,
                offset_in_item: scroll.offset_in_item,
            });
        }
        if let Some(ix) = focused.and_then(index_of) {
            self.transcript_rows[ix].focus(window, cx);
        }
        self.transcript_menu = None;
    }

    pub(super) fn transcript_row(&self, line: usize) -> Option<usize> {
        self.transcript_line_rows.get(line).copied().flatten()
    }

    pub(super) fn reveal_transcript_line(&self, ix: usize) {
        if let Some(row) = self.transcript_row(ix) {
            self.transcript_list.scroll_to_reveal_item(row);
        }
    }

    pub(super) fn set_transcript_speaker(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.transcript_speaker == key {
            return;
        }
        self.transcript_speaker = key.to_owned();
        self.sync_transcript_rows(None, self.timeline.clone(), window, cx);
        self.update_find(cx);
        cx.notify();
    }

    fn transcript_row_outside(&self, ix: usize) -> bool {
        self.transcript_list.item_is_above_viewport(ix) == Some(true)
            || self.transcript_list.item_is_below_viewport(ix) == Some(true)
    }

    pub(super) fn active_transcript_row(&self) -> Option<usize> {
        let last = self.transcript_rows.len().checked_sub(1)?;
        Some(
            self.transcript_cursor
                .or(self
                    .playback
                    .current_line
                    .and_then(|ix| self.transcript_row(ix)))
                .unwrap_or(0)
                .min(last),
        )
    }

    pub(super) fn select_transcript_row(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.transcript_row(ix) {
            self.select_entry(row, window, cx);
        }
    }

    fn select_entry(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.transcript_cursor = Some(ix);
        self.transcript_list.scroll_to_reveal_item(ix);
        self.transcript_rows[ix].focus(window, cx);
        cx.notify();
    }

    fn focused_entry(&self, window: &Window) -> Option<usize> {
        self.active_transcript_row()
            .filter(|&ix| self.transcript_rows[ix].is_focused(window))
    }

    fn focused_transcript_row(&self, window: &Window) -> Option<usize> {
        match self.transcript_entries[self.focused_entry(window)?] {
            Entry::Line(ix) => Some(ix),
            Entry::Marker(_) => None,
        }
    }

    pub(super) fn focused_playback_position(&self, window: &Window) -> Option<f64> {
        match self.transcript_entries[self.focused_entry(window)?] {
            Entry::Line(ix) => Some(self.data.as_ref()?.lines[ix].start),
            Entry::Marker(ix) => Some(self.timeline[ix].at),
        }
    }

    pub(crate) fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_tab(MeetingTab::Transcript, cx);
        self.find_open = true;
        self.find_input.read(cx).focus_handle(cx).focus(window, cx);
        self.update_find(cx);
    }

    pub(super) fn schedule_find(&mut self, cx: &mut Context<Self>) {
        if !self.find_open {
            return;
        }
        let timer = cx.background_executor().timer(Duration::from_millis(150));
        self.find_debounce = Some(cx.spawn(async move |view, cx| {
            timer.await;
            view.update(cx, |this, cx| this.update_find(cx)).ok();
        }));
    }

    pub(super) fn update_find(&mut self, cx: &mut Context<Self>) {
        self.find_query = fold_search_text(self.find_input.read(cx).value().trim());
        let previous = self
            .find_index
            .and_then(|ix| self.find_hits.get(ix))
            .copied();
        self.find_hits.clear();
        if !self.find_query.is_empty()
            && let Some(data) = &self.data
        {
            self.find_hits.extend(
                data.lines
                    .iter()
                    .zip(&self.transcript_line_rows)
                    .enumerate()
                    .filter_map(|(ix, (line, row))| {
                        (row.is_some() && transcript::line_matches(line, &self.find_query))
                            .then_some(ix)
                    }),
            );
        }
        self.find_index = previous.and_then(|ix| self.find_hits.iter().position(|hit| *hit == ix));
        if self.find_index.is_none() {
            self.step_find(true, cx);
        }
        cx.notify();
    }

    pub(crate) fn step_find(&mut self, next: bool, cx: &mut Context<Self>) {
        if !self.find_open {
            return;
        }
        let len = self.find_hits.len();
        if len > 0 {
            let index = match (self.find_index, next) {
                (Some(index), true) => (index + 1) % len,
                (Some(index), false) => (index + len - 1) % len,
                (None, true) => 0,
                (None, false) => len - 1,
            };
            self.find_index = Some(index);
            let ix = self.find_hits[index];
            self.following_transcript = false;
            self.transcript_cursor = self.transcript_row(ix);
            self.reveal_transcript_line(ix);
        }
        cx.notify();
    }

    pub(crate) fn step_find_command(
        &mut self,
        next: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.find_open && !self.find_hits.is_empty() {
            self.set_tab(MeetingTab::Transcript, cx);
            self.step_find(next, cx);
            if !self.find_input.read(cx).focus_handle(cx).is_focused(window)
                && let Some(ix) = self.active_transcript_row()
            {
                self.select_entry(ix, window, cx);
            }
        }
    }

    pub(super) fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find_open = false;
        self.find_debounce = None;
        self.find_query.clear();
        self.find_hits.clear();
        self.find_index = None;
        self.find_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        if let Some(ix) = self.active_transcript_row() {
            self.select_transcript_row(ix, window, cx);
        }
        cx.notify();
    }

    fn copy_line(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(line) = self.data.as_ref().and_then(|data| data.lines.get(ix)) {
            let name = NameStyle::Plain.apply(&line.label);
            let text = format!("{} {name}: {}", format_clock(line.start), line.text);
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.transcript_copy_status = Some(CopyStatus::Line);
            cx.notify();
        }
    }

    pub(crate) fn copy_transcript(&mut self, cx: &mut Context<Self>) {
        if let Some(data) = self.data.as_ref().filter(|data| !data.lines.is_empty()) {
            let text = broscribe_core::export::transcript_lines(
                &data.meeting,
                &data.people,
                true,
                NameStyle::Plain,
            )
            .join("\n");
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.transcript_copy_status = Some(CopyStatus::Transcript);
            cx.notify();
        }
    }

    fn transcript_tools(&self, cx: &Context<Self>) -> Stateful<Div> {
        let lines = self
            .data
            .as_ref()
            .map_or(&[][..], |data| data.lines.as_slice());
        let mut options = vec![OptionItem::new("", "Everyone")];
        let mut seen = std::collections::HashSet::new();
        let mut selected_label = None;
        for line in lines {
            let key = line_speaker_key(line);
            if seen.insert(key.clone()) {
                if key == self.transcript_speaker {
                    selected_label = Some(line.label.as_str());
                }
                options.push(OptionItem::new(key, line.label.clone()));
            }
        }
        div()
            .id("transcript-tools")
            .debug_selector(|| "transcript-tools".into())
            .key_context("FindBar MeetingControls")
            .flex_none()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .p_2()
            .child(div().w(rems(12.)).max_w_full().child(Picker::new(
                "transcript-speaker-filter",
                "Speaker",
                options,
                &self.transcript_speaker,
                cx,
                Self::set_transcript_speaker,
            )))
            .when_some(selected_label, |tools, label| {
                let count = self
                    .transcript_line_rows
                    .iter()
                    .filter(|row| row.is_some())
                    .count();
                tools
                    .child(kit::status_line(
                        "transcript-speaker-status",
                        StatusKind::Info,
                        format!("Showing {count} of {} lines · {label}", lines.len()),
                        cx,
                    ))
                    .child(
                        button(
                            "transcript-show-all",
                            "Show all",
                            true,
                            cx,
                            |this, window, cx| {
                                this.set_transcript_speaker("", window, cx);
                            },
                        )
                        .ghost()
                        .small(),
                    )
            })
            .when(!self.find_open, |el| {
                el.child(
                    Button::new("find-transcript-open")
                        .label("Find…")
                        .ghost()
                        .on_click(cx.listener(|this, _, window, cx| this.open_find(window, cx))),
                )
            })
            .when(self.find_open, |el| {
                let status = self
                    .find_index
                    .map(|ix| format!("{} of {}", ix + 1, self.find_hits.len()))
                    .or_else(|| (!self.find_query.is_empty()).then(|| "No matches".to_owned()));
                el.child(
                    div().w(rems(15.)).max_w_full().child(
                        Input::new(&self.find_input)
                            .prefix(crate::ui::icons::icon(crate::ui::icons::SEARCH))
                            .cleanable(true)
                            .aria_label("Find in transcript"),
                    ),
                )
                .children(
                    [
                        (false, "Previous", IconName::ChevronUp),
                        (true, "Next", IconName::ChevronDown),
                    ]
                    .map(|(next, label, icon)| {
                        Button::new(("find-transcript-step", usize::from(next)))
                            .label(label)
                            .icon(icon)
                            .ghost()
                            .small()
                            .map(|button| {
                                kit::disable_with_reason(
                                    button,
                                    self.find_hits.is_empty(),
                                    "There are no matching transcript lines.",
                                    cx,
                                )
                            })
                            .on_click(cx.listener(move |this, _, _, cx| this.step_find(next, cx)))
                    }),
                )
                .child(
                    Button::new("find-transcript-close")
                        .icon(crate::ui::icons::DISMISS)
                        .accessibility_label("Close find")
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, window, cx| this.close_find(window, cx))),
                )
                .children(status.map(|text| {
                    kit::status_line("find-transcript-status", StatusKind::Info, text, cx)
                }))
            })
    }

    fn on_transcript_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(current) = self.focused_entry(window) else {
            return;
        };
        if event.keystroke.modifiers.modified() {
            return;
        }
        let last = self.transcript_rows.len() - 1;
        let next = match event.keystroke.key.as_str() {
            "up" => current.saturating_sub(1),
            "down" => (current + 1).min(last),
            "home" => 0,
            "end" => last,
            "pageup" | "pagedown" => {
                let top = self.transcript_list.logical_scroll_top().item_ix;
                let end = (top..=last)
                    .find(|&ix| self.transcript_list.item_is_below_viewport(ix) != Some(false))
                    .unwrap_or(last + 1);
                let page = end.saturating_sub(top).max(1);
                if event.keystroke.key == "pageup" {
                    current.saturating_sub(page)
                } else {
                    (current + page).min(last)
                }
            }
            _ => return,
        };
        self.following_transcript = self
            .playback
            .current_line
            .and_then(|ix| self.transcript_row(ix))
            == Some(next);
        self.select_entry(next, window, cx);
        cx.stop_propagation();
    }

    fn play_transcript_row(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(start) = self
            .data
            .as_ref()
            .and_then(|data| data.lines.get(ix))
            .map(|line| line.start)
        {
            self.select_transcript_row(ix, window, cx);
            self.play_from(start, window, cx);
        }
    }

    fn open_transcript_menu(
        &mut self,
        ix: usize,
        keyboard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(data) = &self.data else { return };
        let Some(line) = data.lines.get(ix) else {
            return;
        };
        let can_play = !matches!(self.playable(), Playable::Nothing);
        let speaker = line.speaker;
        let name_label = speaker.map(|speaker| {
            if data.speakers.iter().any(|row| {
                row.id == speaker
                    && !matches!(row.assignment, Assignment::None | Assignment::Suggested)
            }) {
                "Change person…"
            } else {
                "Name speaker…"
            }
        });
        let segments = line.segment_ids.clone();
        let clear = line_restore_label(line);
        self.select_transcript_row(ix, window, cx);
        self.following_transcript = false;
        let Some(row) = self.transcript_row(ix) else {
            return;
        };
        let viewport = self.transcript_list.viewport_bounds();
        let position = self.transcript_list.bounds_for_item(row).map_or_else(
            || viewport.origin,
            |bounds| {
                gpui::point(
                    bounds.left(),
                    bounds.bottom().clamp(viewport.top(), viewport.bottom()),
                )
            },
        );
        let view = cx.weak_entity();
        let command =
            |label: SharedString, run: fn(&mut Self, usize, &mut Window, &mut Context<Self>)| {
                let view = view.clone();
                let segments = segments.clone();
                PopupMenuItem::new(label).on_click(move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        if this
                            .data
                            .as_ref()
                            .and_then(|data| data.lines.get(ix))
                            .is_some_and(|line| line.segment_ids == segments)
                            && this.tab == MeetingTab::Transcript
                        {
                            run(this, ix, window, cx);
                        }
                    });
                })
            };
        let focus = self.transcript_rows[row].clone();
        let menu = PopupMenu::build(window, cx, |menu, _, _| {
            menu.action_context(focus.clone())
                .when(can_play, |menu| {
                    menu.item(command("Play from here".into(), Self::play_transcript_row))
                })
                .when_some(name_label, |menu, label| {
                    menu.item(command(label.into(), |this, ix, window, cx| {
                        this.open_line_naming(
                            ix,
                            Some(NamingScope::SpeakerLines),
                            false,
                            window,
                            cx,
                        )
                    }))
                })
                .item(command(
                    "Name this line only…".into(),
                    |this, ix, window, cx| {
                        this.open_line_naming(ix, Some(NamingScope::ThisLine), false, window, cx)
                    },
                ))
                .when_some(clear, |menu, label| {
                    menu.item(command(label, Self::clear_line))
                })
                .separator()
                .item(command("Copy line".into(), |this, ix, _, cx| {
                    this.copy_line(ix, cx)
                }))
        });
        let dismiss = cx.subscribe_in(
            &menu,
            window,
            move |this, menu, _: &gpui::DismissEvent, window, cx| {
                if menu.focus_handle(cx).contains_focused(window, cx) {
                    focus.focus(window, cx);
                }
                this.transcript_menu = None;
                cx.notify();
            },
        );
        menu.focus_handle(cx).focus(window, cx);
        self.transcript_menu = Some(LineMenu {
            menu,
            position,
            focus_first: Rc::new(Cell::new(keyboard)),
            _dismiss: dismiss,
        });
        cx.notify();
    }

    pub(super) fn dismiss_line_naming(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let line_open = self
            .naming
            .as_ref()
            .is_some_and(|session| session.line().is_some());
        self.end_naming(cx);
        if line_open && let Some(ix) = self.active_transcript_row() {
            self.select_entry(ix, window, cx);
            // An outside press can focus the surface behind the popup while
            // the same event is still being dispatched.
            let view = cx.weak_entity();
            let generation = self.naming_generation;
            window.defer(cx, move |window, cx| {
                if let Some(view) = view.upgrade() {
                    let view = view.read(cx);
                    if view.tab == MeetingTab::Transcript
                        && view.naming_generation == generation
                        && view.active_transcript_row() == Some(ix)
                        && view.state.read(cx).route().selected_meeting() == Some(&view.meeting_id)
                    {
                        let focus = view.transcript_rows[ix].clone();
                        focus.focus(window, cx);
                    }
                }
            });
        }
    }

    fn line_naming_popover(
        &self,
        ix: usize,
        line: &TranscriptLine,
        session: &NamingSession,
        cx: &Context<Self>,
    ) -> Popover {
        let scope = self.naming_scope();
        let mut group = kit::ChoiceGroup::new(
            div().flex().flex_wrap().gap_2(),
            ("line-scope", ix),
            "Name which lines",
        );
        if let Some(speaker) = session.speaker() {
            let data = self
                .data
                .as_ref()
                .expect("line belongs to the loaded meeting");
            let count = data
                .lines
                .iter()
                .filter(|row| row.speaker == Some(speaker) && row.line_person.is_none())
                .count();
            let label = format!(
                "Every line of {} · {}",
                raw_label(speaker),
                crate::ui::format::count(count as u64, "line", "lines"),
            );
            group = group.option(kit::radio_option(
                ("scope-speaker", ix),
                label,
                scope == Some(NamingScope::SpeakerLines),
                true,
                cx,
                |this, _, cx| this.set_naming_scope(NamingScope::SpeakerLines, cx),
            ));
        }
        group = group.option(kit::radio_option(
            ("scope-line", ix),
            "Only this line",
            scope == Some(NamingScope::ThisLine),
            true,
            cx,
            |this, _, cx| this.set_naming_scope(NamingScope::ThisLine, cx),
        ));
        let body = div()
            .id("line-naming-content")
            .w_full()
            .max_h(rems(26.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2()
            .child(crate::ui::heading(
                ("line-naming-heading", ix),
                format!("Who is speaking at {}?", format_clock(line.start)),
            ))
            .child(group)
            .children(session.speaker().map(|speaker| {
                text_line(
                    ("line-voice-note", ix),
                    Role::Note,
                    format!("Closest voices use {}'s voice.", raw_label(speaker)),
                )
                .text_role(TextRole::Caption)
            }))
            .children(line_restore_label(line).map(|label| {
                button(
                    ("line-clear", ix),
                    label,
                    true,
                    cx,
                    move |this, window, cx| this.clear_line(ix, window, cx),
                )
                .small()
            }))
            .child(session.picker.clone())
            .children(self.line_refusal(ix, line, cx));
        Popover::new(("line-naming", ix))
            .w(rems(22.5))
            .max_w_full()
            .max_h(rems(27.5))
            .open(true)
            .overlay_closable(true)
            .track_focus(&session.picker.read(cx).search_focus(cx))
            .on_open_change(cx.listener(|this, open, window, cx| {
                if !open {
                    this.dismiss_line_naming(window, cx);
                }
            }))
            .child(body)
    }

    fn line_refusal(
        &self,
        ix: usize,
        line: &TranscriptLine,
        cx: &Context<Self>,
    ) -> Option<Stateful<Div>> {
        self.edit_line(
            &EditTarget::Line(line.segment_ids.clone()),
            ("line-edit-status", ix),
            cx,
        )
    }

    pub(super) fn overview_timeline(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
        (!self.timeline.is_empty()).then(|| {
            div()
                .id("overview-timeline")
                .flex()
                .flex_col()
                .gap_2()
                .child(crate::ui::heading(
                    "overview-bookmarks",
                    format!("Bookmarks and notes ({})", self.timeline.len()),
                ))
                .children(self.timeline.iter().enumerate().map(|(index, marker)| {
                    let at = marker.at;
                    let content = marker.content(index, cx);
                    if matches!(self.playable(), Playable::Nothing) {
                        content.into_any_element()
                    } else {
                        Button::new(("overview-marker", index))
                            .ghost()
                            .small()
                            .accessibility_label(format!("Play from {}", spoken_position(at)))
                            .child(content)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.play_from(at, window, cx)
                            }))
                            .into_any_element()
                    }
                }))
        })
    }

    fn line_live_names(
        &self,
        ix: usize,
        line: &TranscriptLine,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(data) = &self.data else {
            return Vec::new();
        };
        data.live_review
            .iter()
            .filter_map(|item| {
                let decision = item.decision;
                let segments = self.live_review_segments.get(&decision)?;
                if !line.segment_ids.iter().any(|id| segments.contains(id)) {
                    return None;
                }
                let name = item
                    .person_id
                    .as_ref()
                    .and_then(|person| data.people.display_name(person));
                let caption = name.map_or_else(
                    || "Named while recording · not applied".to_owned(),
                    |name| format!("Named {name} while recording · not applied"),
                );
                let name_action = name.map(|name| {
                    button(
                        ("line-apply-live-name", decision),
                        format!("Name as {name}"),
                        true,
                        cx,
                        move |this, window, cx| {
                            this.select_transcript_row(ix, window, cx);
                            this.apply_live_review(decision, window, cx);
                        },
                    )
                    .ghost()
                    .small()
                    .tab_stop(false)
                });
                Some(
                    div()
                        .id(("line-live-name-group", decision))
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(kit::glyph(
                            kit::StatusKind::Attention.glyph_path(),
                            cx.theme().warning,
                        ))
                        .child(
                            text_line(("line-live-name", decision), Role::Note, caption)
                                .text_role(TextRole::Caption),
                        )
                        .children(name_action)
                        .child(
                            button(
                                ("line-leave-live-name", decision),
                                "Leave as transcribed",
                                true,
                                cx,
                                move |this, window, cx| {
                                    this.select_transcript_row(ix, window, cx);
                                    this.leave_live_review(decision, window, cx);
                                },
                            )
                            .ghost()
                            .small()
                            .tab_stop(false),
                        )
                        .children(self.edit_line(
                            &EditTarget::LiveReview(decision),
                            ("line-live-review-status", decision),
                            cx,
                        ))
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn marker_row(
        &self,
        row: usize,
        index: usize,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let marker = &self.timeline[index];
        let at = marker.at;
        let focus = &self.transcript_rows[row];
        let selected = self.active_transcript_row() == Some(row);
        let can_play = !matches!(self.playable(), Playable::Nothing);
        kit::list_row(
            ("marker-item", index),
            selected,
            focus.is_focused(window) && window.last_input_was_keyboard(),
            cx,
        )
        .role(Role::ListItem)
        .aria_label(format!("Recording marker {}", index + 1))
        .aria_selected(selected)
        .aria_position_in_set(row + 1)
        .aria_size_of_set(self.transcript_entries.len())
        .track_focus(&focus.clone().tab_index(0).tab_stop(selected))
        .min_h(rems(1.75))
        .child(marker.content(index, cx))
        .when(can_play, |el| {
            el.aria_description("Plays from this position")
                .aria_keyshortcuts("Enter")
        })
        .on_click(
            cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                if !event.is_keyboard() {
                    this.select_entry(row, window, cx);
                    if can_play {
                        this.play_from(at, window, cx);
                    }
                }
            }),
        )
        .into_any_element()
    }

    fn render_transcript_row(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        self.transcript_row_builds.record();
        let Some(data) = &self.data else {
            return div().into_any_element();
        };
        let row_ix = ix;
        let ix = match self.transcript_entries[row_ix] {
            Entry::Line(ix) => ix,
            Entry::Marker(index) => return self.marker_row(row_ix, index, window, cx),
        };
        let line = &data.lines[ix];
        let current = self.playback.current_line == Some(ix)
            && (self.playback.loaded || self.playback.pending_start.is_some());
        let focus = &self.transcript_rows[row_ix];
        let focused = focus.is_focused(window);
        let selected = self.active_transcript_row() == Some(row_ix);
        let found = self.find_hits.binary_search(&ix).is_ok();
        let current_hit = self.find_index.and_then(|index| self.find_hits.get(index)) == Some(&ix);
        let library_match = self
            .library_match
            .as_ref()
            .filter(|(segment, _)| line.segment_ids.contains(segment));
        let can_play = !matches!(self.playable(), Playable::Nothing);
        let label = NameStyle::Plain.apply(&line.label);
        let spoken_time = spoken_position(line.start);
        let correction = match (line.line_person.is_some(), line.overlapping_speech) {
            (true, true) => ", corrected, overlapping speech",
            (true, false) => ", corrected",
            (false, _) => "",
        };
        let playing = if current && self.playback.playing {
            "Playing. "
        } else {
            ""
        };
        let described = format!(
            "{playing}{label} at {spoken_time}{correction}: {}",
            line.text
        );
        let muted = kit::row_metadata_colour(current || selected, cx);
        let ring = cx.theme().ring;
        let mono = cx.theme().mono_font_family.clone();
        let session = self
            .naming
            .as_ref()
            .filter(|n| n.line() == Some(line.segment_ids.as_slice()));
        let button_anchor = session.is_some_and(|session| {
            matches!(
                session.target,
                NamingTarget::Line {
                    button_anchor: true,
                    ..
                }
            )
        });
        let show_name = current || focused || session.is_some() || window.is_a11y_active();
        let name = Button::new(("line-name", ix))
            .label("Name…")
            .ghost()
            .small()
            .tab_index(-1)
            .tab_stop(false)
            .accessibility_label(format!("Name the line at {spoken_time}"))
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.open_line_naming(ix, None, true, window, cx);
            }));
        let highlights = |text: &str| find_highlight(text, &self.find_query, cx);
        let name_text = if found {
            highlights(&label).into_any_element()
        } else {
            label.into_any_element()
        };
        let words = if can_play && line.segment_ids.len() > 1 {
            let ranges = segment_ranges(line, data);
            let current_range = (current && self.playback.playing)
                .then(|| {
                    ranges.iter().rev().find(|(_, start, end)| {
                        *start <= self.playback.position_s && self.playback.position_s < *end
                    })
                })
                .flatten()
                .map(|(range, _, _)| range.clone());
            let mut highlighted = if found {
                broscribe_core::names::folded_match_ranges(&line.text, &self.find_query)
                    .into_iter()
                    .map(|range| (range, find_style(cx)))
                    .collect()
            } else if let Some((_, words)) = library_match {
                crate::ui::rich_text::word_highlights(&line.text, words, cx)
            } else {
                Vec::new()
            };
            if let Some(range) = current_range {
                highlighted
                    .retain(|(other, _)| other.end <= range.start || other.start >= range.end);
                highlighted.push((range, highlight_style(cx.theme().selection)));
                highlighted.sort_by_key(|(range, _)| range.start);
            }
            let text = StyledText::new(line.text.clone()).with_highlights(highlighted);
            let view = cx.weak_entity();
            let segments = line.segment_ids.clone();
            InteractiveText::new(("seek-segment-text", ix), text)
                .on_click(
                    ranges.iter().map(|(range, _, _)| range.clone()).collect(),
                    move |part, window, cx| {
                        let _ = view.update(cx, |this, cx| {
                            if this.transcript_menu.is_none()
                                && this.tab == MeetingTab::Transcript
                                && this
                                    .data
                                    .as_ref()
                                    .and_then(|data| data.lines.get(ix))
                                    .is_some_and(|line| line.segment_ids == segments)
                            {
                                cx.stop_propagation();
                                this.select_transcript_row(ix, window, cx);
                                this.play_from(ranges[part].1, window, cx);
                            }
                        });
                    },
                )
                .into_any_element()
        } else if found {
            highlights(&line.text).into_any_element()
        } else if let Some((_, words)) = library_match {
            crate::ui::rich_text::highlight_words(&line.text, words, cx).into_any_element()
        } else {
            line.text.clone().into_any_element()
        };
        let row = kit::list_row(
            ("line-item", ix),
            current || selected,
            focused && window.last_input_was_keyboard(),
            cx,
        )
        .spoken(Role::ListItem, described)
        .aria_selected(selected)
        .aria_position_in_set(row_ix + 1)
        .aria_size_of_set(self.transcript_entries.len())
        .when(can_play, |row| {
            row.aria_description("Plays from this line")
                .aria_keyshortcuts("Enter")
        })
        .track_focus(&focus.clone().tab_index(0).tab_stop(selected))
        .group("transcript-line")
        .py(rems(0.375))
        .items_start()
        .text_role(TextRole::Reading)
        .when(
            current_hit
                || library_match.is_some()
                || self
                    .reviewed_line
                    .is_some_and(|id| line.segment_ids.contains(&id)),
            |row| row.border_1().border_color(ring),
        )
        .on_click(
            cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                if !event.is_keyboard() {
                    this.play_transcript_row(ix, window, cx);
                }
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |this, _, window, cx| {
                this.open_transcript_menu(ix, false, window, cx);
                cx.stop_propagation();
            }),
        )
        .child(div().w(rems(0.75)).flex_none().children(current.then(|| {
            crate::ui::icons::icon(crate::ui::icons::PLAY)
                .size(rems(0.75))
                .text_color(muted)
        })))
        .child(
            tabular(div())
                .id(("line-time", ix))
                .font_family(mono)
                .text_role(TextRole::Mono)
                .w(rems(if data.meeting.duration_s >= 3600. {
                    4.25
                } else {
                    3.
                }))
                .flex_none()
                .text_right()
                .text_color(muted)
                .child(format_clock(line.start))
                .map(|time| {
                    #[cfg(test)]
                    let time = gpui::base::TestSupportExt::test_support(time);
                    time
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(rems(0.125))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_1()
                        .items_baseline()
                        .child(
                            // The speaker's colour, before their name; the
                            // name says who, so the disc has no node.
                            kit::speaker_disc(line_disc(line, data), cx).self_center(),
                        )
                        .child(
                            div()
                                .text_role(TextRole::Label)
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(name_text),
                        )
                        .when(line.line_person.is_some(), |el| {
                            let tip: SharedString = match line.speaker {
                                Some(speaker) => {
                                    format!(
                                        "Named for this line only. The speaker is {}.",
                                        raw_label(speaker)
                                    )
                                }
                                None => "Named for this line only.".to_owned(),
                            }
                            .into();
                            el.child(
                                text_line(("line-corrected", ix), Role::Label, "corrected")
                                    .text_role(TextRole::Caption)
                                    .text_color(muted)
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(tip.clone()).build(window, cx)
                                    }),
                            )
                        })
                        .when(
                            line.overlapping_speech && line.line_person.is_some(),
                            |el| {
                                el.child(
                                    text_line(
                                        ("line-overlap", ix),
                                        Role::Label,
                                        broscribe_core::transcript::OVERLAP_LABEL,
                                    )
                                    .text_role(TextRole::Caption)
                                    .text_color(muted),
                                )
                            },
                        ),
                )
                .child(div().id(("line-words", ix)).child(words).map(|words| {
                    #[cfg(test)]
                    let words = gpui::base::TestSupportExt::test_support(words);
                    words
                }))
                .children(self.line_live_names(ix, line, cx))
                .when(session.is_none(), |el| {
                    el.children(self.line_refusal(ix, line, cx))
                }),
        )
        .child(
            div()
                .id(("line-actions", ix))
                .w(rems(5.5))
                .flex_none()
                .when(!show_name, |actions| {
                    actions
                        .opacity(0.)
                        .group_hover("transcript-line", |style| style.opacity(1.))
                })
                .child(if let Some(session) = session.filter(|_| button_anchor) {
                    self.line_naming_popover(ix, line, session, cx)
                        .trigger(name)
                        .into_any_element()
                } else {
                    name.into_any_element()
                })
                .map(|actions| {
                    #[cfg(test)]
                    let actions = gpui::base::TestSupportExt::test_support(actions);
                    actions
                }),
        );
        let row = row.children(session.filter(|_| !button_anchor).map(|session| {
            div().absolute().left_0().bottom_0().child(
                self.line_naming_popover(ix, line, session, cx).trigger(
                    Button::new("line-naming-anchor")
                        .tab_index(-1)
                        .tab_stop(false)
                        .invisible()
                        .size(px(0.)),
                ),
            )
        }));
        #[cfg(test)]
        let row = gpui::base::TestSupportExt::test_support(row);
        row.into_any_element()
    }

    pub(super) fn transcript(&self, data: &MeetingData, cx: &Context<Self>) -> AnyElement {
        if self.transcript_entries.is_empty() {
            let text = if data.meeting.processing.status == ProcessingStatus::Done {
                "No speech was transcribed in this meeting."
            } else {
                "The transcript appears when processing finishes."
            };
            return text_line("transcript-empty", Role::Status, text)
                .p_4()
                .text_role(TextRole::Caption)
                .into_any_element();
        }
        let menu = self.transcript_menu.as_ref().map(|open| {
            let focus_first = open.focus_first.clone();
            let menu = open.menu.clone();
            gpui::deferred(
                gpui::anchored()
                    .position(open.position)
                    .snap_to_window_with_margin(px(8.))
                    .child(div().child(menu.clone()).on_prepaint(move |_, window, cx| {
                        if focus_first.replace(false) {
                            let menu = menu.clone();
                            window.defer(cx, move |window, cx| {
                                if menu.focus_handle(cx).is_focused(window) {
                                    window.dispatch_action(
                                        Box::new(gpui::base::actions::SelectDown),
                                        cx,
                                    );
                                }
                            });
                        }
                    })),
            )
            .with_priority(gpui::base::POPUP_PRIORITY)
        });
        let away = self.playback.playing
            && self
                .playback
                .current_line
                .and_then(|ix| self.transcript_row(ix))
                .is_some_and(|row| self.transcript_row_outside(row));
        let list = div()
            .id("transcript")
            .debug_selector(|| "transcript-list".into())
            .role(Role::List)
            .aria_label("Transcript lines")
            .aria_keyshortcuts("ArrowUp ArrowDown Home End PageUp PageDown Enter N")
            .key_context("Transcript")
            .relative()
            .flex_1()
            .min_h_0()
            .on_action(cx.listener(|this, _: &NameTranscriptLine, window, cx| {
                if let Some(ix) = this.focused_transcript_row(window) {
                    this.start_line_naming(ix, window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &PlayTranscriptLine, window, cx| {
                if let Some(at) = this.focused_playback_position(window) {
                    this.play_from(at, window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &Copy, window, cx| {
                if let Some(ix) = this.focused_transcript_row(window) {
                    this.copy_line(ix, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &kit::OpenRowMenu, window, cx| {
                if let Some(ix) = this.focused_transcript_row(window) {
                    this.open_transcript_menu(ix, true, window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_key_down(
                cx.listener(|this, event, window, cx| this.on_transcript_key(event, window, cx)),
            )
            .child(
                gpui::list(
                    self.transcript_list.clone(),
                    cx.processor(|this, ix, window, cx| this.render_transcript_row(ix, window, cx)),
                )
                .size_full(),
            )
            .children(menu)
            .map(|list| {
                #[cfg(test)]
                let list = gpui::base::TestSupportExt::test_support(list);
                list
            });
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .px_4()
            .child(self.transcript_tools(cx))
            .when(
                self.transcript_copy_status == Some(CopyStatus::Line),
                |tools| {
                    tools.child(kit::status_line(
                        "transcript-copy-status",
                        StatusKind::Done,
                        "Line copied.",
                        cx,
                    ))
                },
            )
            .child(list)
            .when(away, |body| {
                body.child(
                    div()
                        .absolute()
                        .bottom(rems(1.))
                        .right(rems(1.))
                        .occlude()
                        .child(
                            Button::new("transcript-back-current")
                                .label("Back to current line")
                                .icon(crate::ui::icons::DISCLOSURE_OPEN)
                                .outline()
                                .small()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if let Some(ix) = this.playback.current_line {
                                        this.following_transcript = true;
                                        this.select_transcript_row(ix, window, cx);
                                        cx.stop_propagation();
                                    }
                                })),
                        ),
                )
            })
            .relative()
            .into_any_element()
    }
}

fn line_restore_label(line: &TranscriptLine) -> Option<SharedString> {
    line.line_person.as_ref()?;
    Some(match line.speaker {
        Some(speaker) => format!("Use {}'s name again", raw_label(speaker)).into(),
        None => "Use the speaker's name again".into(),
    })
}

fn find_highlight(text: &str, query: &str, cx: &App) -> StyledText {
    let ranges = broscribe_core::names::folded_match_ranges(text, query);
    let style = find_style(cx);
    StyledText::new(text.to_owned())
        .with_highlights(ranges.into_iter().map(|range: Range<usize>| (range, style)))
}

/// A find hit in the transcript: the find yellow with its ink, underlined.
fn find_style(cx: &App) -> HighlightStyle {
    let colours = crate::ui::brand::brand(cx);
    HighlightStyle {
        color: Some(colours.find_foreground),
        ..highlight_style(colours.find)
    }
}

/// The playing word: the selection tint, underlined.
fn highlight_style(background: gpui::Hsla) -> HighlightStyle {
    HighlightStyle {
        background_color: Some(background),
        underline: Some(UnderlineStyle {
            thickness: px(1.),
            color: None,
            wavy: false,
        }),
        ..Default::default()
    }
}

// Empty in the app. The headless fixture counts actual list processor calls.
#[derive(Default)]
pub(super) struct RowBuildCount {
    #[cfg(test)]
    count: usize,
}

impl RowBuildCount {
    fn record(&mut self) {
        #[cfg(test)]
        {
            self.count += 1;
        }
    }

    #[cfg(test)]
    pub(super) fn reset(&mut self) {
        self.count = 0;
    }

    #[cfg(test)]
    pub(super) fn get(&self) -> usize {
        self.count
    }
}
