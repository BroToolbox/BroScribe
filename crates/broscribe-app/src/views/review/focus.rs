//! The focused "Identify voices" mode of the review queue (redesign spec
//! §5.5): the waiting voices one at a time, then the possible duplicate
//! profiles, with the keyboard.
//!
//! The mode is a state of [`ReviewView`], not a second queue: it shows the
//! queue's current row (the first waiting voice in scope that was not
//! skipped) and every answer goes through the queue's own decisions, so the
//! fences, the speaker recheck, Undo and Redo work as in the list.
//!
//! One voice: "Who is Speaker 2?" (or "Is this Ana Example?" when there is
//! a suggestion), its samples and two quotes, up to three people to choose
//! with `1`–`3` (the suggestion first, then the closest voices), the person
//! picker to search or add someone, and Can't tell (`D`), Not this person
//! (`R`, with a suggestion) and Skip (`S`). `Enter` confirms the suggestion
//! or the open "Link … in N meetings" preview; `Space` plays; `←` / `→`
//! step through samples; `Escape` leaves the search field, then the mode.
//! A voice heard in several meetings opens the queue's preview with one
//! checkbox per meeting before anything is written.
//!
//! After the voices (global mode only): "Is Ana the same person as Ana
//! S.?" for each possible duplicate, with the queue's merge (Review merge,
//! Merge profiles) or Different people, and Skip. The finish state lists
//! what was answered and offers the skipped voices again.

use std::collections::{HashMap, HashSet};

use broscribe_core::ops::PersonDuplicate;
use gpui::Div;
use gpui::component::kbd::Kbd;

use super::*;
use crate::ui::kit::Disc;

/// Most people offered as one-key choices.
pub const OPTION_KEYS: usize = 3;

/// How a voice left the queue while the mode was open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub key: RowKey,
    /// "Speaker 2 · Weekly sync".
    pub voice: String,
    /// "Named Ana Example", "Can't tell", "Named by the recheck".
    pub outcome: String,
}

/// What the mode shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The queue is still being read.
    Loading,
    /// The row at this index of the queue.
    Voice(usize),
    /// The possible duplicate at this index of the snapshot.
    Duplicate(usize),
    Finished,
}

/// The focused mode's own state; everything else is the queue's.
#[derive(Debug, Default)]
pub struct FocusMode {
    /// Only voices heard in this meeting.
    pub scope: Option<MeetingId>,
    /// Every voice in scope seen while the mode was open, first seen first.
    seen: Vec<RowKey>,
    labels: HashMap<RowKey, String>,
    skipped: HashSet<RowKey>,
    /// What the last decision asked for each voice ("Named Ana Example").
    intents: HashMap<RowKey, String>,
    answers: Vec<Answer>,
    /// Possible duplicates skipped (by their pair's IDs).
    skipped_pairs: HashSet<(String, String)>,
}

/// The pair's key in [`FocusMode::skipped_pairs`].
fn pair_key(duplicate: &PersonDuplicate) -> (String, String) {
    (
        duplicate.pair.0.as_str().to_owned(),
        duplicate.pair.1.as_str().to_owned(),
    )
}

impl FocusMode {
    pub fn new(scope: Option<MeetingId>) -> Self {
        Self {
            scope,
            ..Self::default()
        }
    }

    /// Follows a read of the queue: `rows` are the voices in scope (key and
    /// label), in queue order. A voice seen before that is gone was
    /// answered (with its intent, or by a recheck); one that is back (an
    /// undo) is no longer.
    pub fn observe(&mut self, rows: &[(RowKey, String)]) {
        let present: HashSet<&RowKey> = rows.iter().map(|(k, _)| k).collect();
        self.answers.retain(|answer| !present.contains(&answer.key));
        for (key, label) in rows {
            if !self.labels.contains_key(key) {
                self.seen.push(key.clone());
            }
            self.labels.insert(key.clone(), label.clone());
        }
        for key in &self.seen {
            if present.contains(key) || self.answers.iter().any(|a| &a.key == key) {
                continue;
            }
            let outcome = self
                .intents
                .remove(key)
                .unwrap_or_else(|| "Named by the recheck".to_owned());
            self.answers.push(Answer {
                key: key.clone(),
                voice: self.labels.get(key).cloned().unwrap_or_default(),
                outcome,
            });
            self.skipped.remove(key);
        }
    }

    /// The decision just asked for `key` ("Named Ana Example").
    pub fn intend(&mut self, key: &RowKey, outcome: impl Into<String>) {
        self.intents.insert(key.clone(), outcome.into());
    }

    pub fn skip(&mut self, key: &RowKey) {
        self.skipped.insert(key.clone());
    }

    pub fn skip_pair(&mut self, duplicate: &PersonDuplicate) {
        self.skipped_pairs.insert(pair_key(duplicate));
    }

    /// Offers the skipped voices again.
    pub fn revisit_skipped(&mut self) {
        self.skipped.clear();
    }

    pub fn answers(&self) -> &[Answer] {
        &self.answers
    }

    /// Voices in scope still waiting that were skipped.
    pub fn skipped_count(&self, rows: &[(RowKey, String)]) -> usize {
        rows.iter()
            .filter(|(k, _)| self.skipped.contains(k))
            .count()
    }

    /// The position of `rows[current]` among the voices seen ("2 of 7"):
    /// the answered and skipped voices come before it.
    pub fn progress(&self, rows: &[(RowKey, String)]) -> (usize, usize) {
        let total = self.seen.len().max(self.answers.len() + rows.len());
        let done = self.answers.len() + self.skipped_count(rows);
        ((done + 1).min(total.max(1)), total)
    }

    /// What to show: the first voice in scope not skipped (`rows` are
    /// `(index in the queue, key)` in queue order), else the first
    /// duplicate not skipped (global mode only), else the finish.
    pub fn phase(
        &self,
        loaded: bool,
        rows: &[(usize, RowKey)],
        duplicates: &[PersonDuplicate],
    ) -> Phase {
        if !loaded {
            return Phase::Loading;
        }
        if let Some((ix, _)) = rows.iter().find(|(_, k)| !self.skipped.contains(k)) {
            return Phase::Voice(*ix);
        }
        if self.scope.is_none()
            && let Some(ix) = duplicates
                .iter()
                .position(|d| !self.skipped_pairs.contains(&pair_key(d)))
        {
            return Phase::Duplicate(ix);
        }
        Phase::Finished
    }
}

/// What a one-key choice offers.
#[derive(Clone, Debug, PartialEq)]
pub struct FocusOption {
    pub person: PersonId,
    pub name: String,
    /// "Suggested", "Named automatically", "Close voice".
    pub why: &'static str,
    pub score: Option<f32>,
    /// The suggestion (or automatic name): choosing it accepts it.
    pub suggested: bool,
}

/// The one-key choices: the row's suggestion first, then the closest voices
/// (`closest`, best first), nobody twice, at most [`OPTION_KEYS`].
pub fn options(item: &ReviewItem, closest: &[(PersonId, String, f32)]) -> Vec<FocusOption> {
    let mut out = Vec::new();
    if let (Some(person), Some((kind, name, score))) = (accept_target(item), proposal(item)) {
        out.push(FocusOption {
            person: person.clone(),
            name: name.to_owned(),
            why: if kind == "Suggested" {
                "Suggested"
            } else {
                "Named automatically"
            },
            score,
            suggested: true,
        });
    }
    for (person, name, score) in closest {
        if out.len() == OPTION_KEYS {
            break;
        }
        if out.iter().any(|o| &o.person == person) {
            continue;
        }
        out.push(FocusOption {
            person: person.clone(),
            name: name.clone(),
            why: "Close voice",
            score: Some(*score),
            suggested: false,
        });
    }
    out
}

/// "Who is Speaker 2?", or "Is this Ana Example?" with a suggestion.
pub fn question(item: &ReviewItem) -> String {
    match proposal(item) {
        Some((_, name, _)) if accept_target(item).is_some() => format!("Is this {name}?"),
        _ => format!("Who is {}?", speaker_name(item.speaker_id)),
    }
}

/// What a key does in the focused mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusKey {
    Option(usize),
    Confirm,
    Search,
    CantTell,
    NotThisPerson,
    Skip,
    Play,
    Previous,
    Next,
    Leave,
}

/// The focused mode's key; `None` for other keys and for Command, Control,
/// Option and Fn chords (so Undo and Redo reach their actions).
pub fn focus_key(keystroke: &Keystroke) -> Option<FocusKey> {
    let m = &keystroke.modifiers;
    if m.control || m.alt || m.platform || m.function {
        return None;
    }
    Some(match keystroke.key.as_str() {
        "1" => FocusKey::Option(0),
        "2" => FocusKey::Option(1),
        "3" => FocusKey::Option(2),
        "enter" => FocusKey::Confirm,
        "n" | "/" => FocusKey::Search,
        "d" => FocusKey::CantTell,
        "r" => FocusKey::NotThisPerson,
        "s" => FocusKey::Skip,
        "space" => FocusKey::Play,
        "left" if !m.shift => FocusKey::Previous,
        "right" if !m.shift => FocusKey::Next,
        "escape" => FocusKey::Leave,
        _ => return None,
    })
}

fn key_hint(key: &'static str) -> Option<Kbd> {
    Keystroke::parse(key).ok().map(Kbd::new)
}

impl ReviewView {
    /// Opens the focused mode on every waiting voice, or (`Some`) on the
    /// voices heard in one meeting.
    pub(crate) fn enter_focus(&mut self, scope: Option<MeetingId>, cx: &mut Context<Self>) {
        self.end_naming(cx);
        self.identity.clear_preview();
        self.identity.show_duplicates = false;
        self.show_dismissed = false;
        self.row_popup = None;
        self.message = None;
        self.focus = Some(FocusMode::new(scope));
        self.focus_sync(cx);
        cx.notify();
    }

    /// Leaves the focused mode for the list, on the voice it showed.
    pub(crate) fn leave_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.focus.take().is_some() {
            self.end_naming(cx);
            self.identity.clear_preview();
            self.playback.stop(&self.state, cx);
            self.reveal_selection = true;
            self.focus_handle.focus(window, cx);
            cx.notify();
        }
    }

    /// Whether the focused mode is open.
    #[cfg(test)]
    pub(crate) fn focused_mode(&self) -> Option<&FocusMode> {
        self.focus.as_ref()
    }

    /// The voices in scope: (index in the queue, key, label).
    fn scoped_rows(&self) -> Vec<(usize, RowKey, String)> {
        let Some(focus) = &self.focus else {
            return Vec::new();
        };
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                let key = row.key();
                focus.scope.as_ref().is_none_or(|scope| {
                    key.0 == *scope
                        || self
                            .identity
                            .snapshot
                            .groups
                            .get(&key)
                            .is_some_and(|g| g.has_meeting(scope))
                })
            })
            .map(|(ix, row)| {
                let item = &row.entry.item;
                let label = format!(
                    "{} · {}",
                    speaker_name(item.speaker_id),
                    meeting_title(item.meeting_title.as_deref())
                );
                (ix, row.key(), label)
            })
            .collect()
    }

    pub(crate) fn focus_phase(&self) -> Phase {
        let Some(focus) = &self.focus else {
            return Phase::Finished;
        };
        let rows: Vec<_> = self
            .scoped_rows()
            .into_iter()
            .map(|(ix, key, _)| (ix, key))
            .collect();
        focus.phase(self.loaded, &rows, &self.identity.snapshot.duplicates)
    }

    /// After a read of the queue: records answered voices and selects the
    /// voice the mode shows.
    pub(super) fn focus_sync(&mut self, cx: &mut Context<Self>) {
        if self.focus.is_none() || !self.loaded {
            return;
        }
        let rows: Vec<_> = self
            .scoped_rows()
            .into_iter()
            .map(|(_, key, label)| (key, label))
            .collect();
        if let Some(focus) = self.focus.as_mut() {
            focus.observe(&rows);
        }
        if let Phase::Voice(ix) = self.focus_phase()
            && ix != self.selected
        {
            self.selected = ix;
            self.end_naming(cx);
            self.identity.clear_preview();
            self.playback.stop(&self.state, cx);
        }
    }

    /// Records what a decision on `key` asked for, for the finish list.
    pub(super) fn focus_intend(&mut self, key: &RowKey, outcome: impl Into<String>) {
        if let Some(focus) = self.focus.as_mut() {
            focus.intend(key, outcome);
        }
    }

    fn focus_row(&self) -> Option<&ReviewRow> {
        match self.focus_phase() {
            Phase::Voice(ix) => self.rows.get(ix),
            _ => None,
        }
    }

    /// The one-key choices of the current voice.
    fn focus_options(&self, row: &ReviewRow, cx: &App) -> Vec<FocusOption> {
        let closest: Vec<_> = self
            .naming
            .as_ref()
            .filter(|n| n.key == row.key())
            .map(|n| {
                n.picker
                    .read(cx)
                    .closest_people(OPTION_KEYS + 1)
                    .into_iter()
                    .map(|(p, score)| (p.id.clone(), p.name.clone(), score))
                    .collect()
            })
            .unwrap_or_default();
        options(&row.entry.item, &closest)
    }

    /// Chooses the `ix`-th one-key option for the current voice.
    fn focus_choose(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.focus_row() else {
            return;
        };
        let Some(option) = self.focus_options(row, cx).into_iter().nth(ix) else {
            return;
        };
        let key = row.key();
        let Some(fence) = row.fence.clone() else {
            return;
        };
        let action = if option.suggested {
            Action::Accept(option.person)
        } else {
            Action::Name {
                choice: PersonChoice::Existing(option.person),
                label: Some(option.name),
                fence,
            }
        };
        self.end_naming(cx);
        self.run(key, action, cx);
    }

    fn focus_skip(&mut self, cx: &mut Context<Self>) {
        match self.focus_phase() {
            Phase::Voice(ix) => {
                if let (Some(key), Some(focus)) =
                    (self.rows.get(ix).map(ReviewRow::key), self.focus.as_mut())
                {
                    focus.skip(&key);
                }
                self.focus_sync(cx);
            }
            Phase::Duplicate(ix) => {
                if let (Some(duplicate), Some(focus)) = (
                    self.identity.snapshot.duplicates.get(ix),
                    self.focus.as_mut(),
                ) {
                    focus.skip_pair(duplicate);
                }
                self.identity.merge = None;
            }
            Phase::Loading | Phase::Finished => {}
        }
        cx.notify();
    }

    fn focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(picker) = self.naming.as_ref().map(|n| n.picker.clone()) {
            picker.update(cx, |picker, cx| picker.focus_search(window, cx));
        }
    }

    /// Opens the picker on the current voice once it is shown without one.
    fn ensure_focus_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.focus_row() else {
            return;
        };
        let current = self
            .naming
            .as_ref()
            .is_some_and(|n| n.key == row.key() && row.fence.as_deref() == Some(n.fence.as_ref()));
        if current
            || row.fence.is_none()
            || self.saving.is_some()
            || self.identity.saving
            || self.identity.identify.is_some()
        {
            return;
        }
        self.end_naming(cx);
        self.open_picker(false, window, cx);
    }

    pub(super) fn handle_focus_key(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let in_search = self
            .naming
            .as_ref()
            .is_some_and(|n| n.picker.read(cx).search_focused(window, cx));
        if in_search {
            // Keys go to the search field; Escape returns to the voice.
            if ev.keystroke.key == "escape" {
                self.focus_handle.focus(window, cx);
                cx.stop_propagation();
            }
            return;
        }
        let Some(key) = focus_key(&ev.keystroke) else {
            return;
        };
        // Buttons take Enter and Space themselves.
        if matches!(key, FocusKey::Confirm | FocusKey::Play)
            && !self.focus_handle.is_focused(window)
        {
            return;
        }
        cx.stop_propagation();
        if key == FocusKey::Leave {
            self.leave_focus(window, cx);
            return;
        }
        let writing = self.saving.is_some() || self.identity.saving;
        match (self.focus_phase(), key) {
            (Phase::Voice(_), FocusKey::Play) => self.play_selected(cx),
            (Phase::Voice(_), FocusKey::Previous | FocusKey::Next) => {
                self.step_selected(key == FocusKey::Next, cx)
            }
            (Phase::Voice(_), FocusKey::Search) => self.focus_search(window, cx),
            (_, FocusKey::Skip) if !writing => self.focus_skip(cx),
            (Phase::Voice(_) | Phase::Duplicate(_), _) if writing => self.wait_for_write(cx),
            (Phase::Voice(_), FocusKey::Option(ix)) => self.focus_choose(ix, cx),
            (Phase::Voice(_), FocusKey::Confirm) => {
                if self.identity.identify.is_some() {
                    self.save_identification(cx);
                } else if self
                    .focus_row()
                    .is_some_and(|row| accept_target(&row.entry.item).is_some())
                {
                    self.accept_selected(window, cx);
                } else {
                    self.focus_search(window, cx);
                }
            }
            (Phase::Voice(_), FocusKey::CantTell) => self.dismiss_selected(cx),
            (Phase::Voice(_), FocusKey::NotThisPerson) => self.reject_selected(cx),
            _ => {}
        }
    }

    pub(super) fn render_focus(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let phase = self.focus_phase();
        if let Phase::Voice(_) = phase {
            let view = cx.weak_entity();
            window.defer(cx, move |window, cx| {
                let _ = view.update(cx, |view, cx| view.ensure_focus_picker(window, cx));
            });
        }
        let theme = cx.theme();
        let scope_title = self.focus.as_ref().and_then(|f| f.scope.clone()).map(|m| {
            self.rows
                .iter()
                .find(|r| r.entry.item.meeting_id == m)
                .and_then(|r| r.entry.item.meeting_title.clone())
                .unwrap_or_else(|| "this meeting".to_owned())
        });
        let rows: Vec<_> = self
            .scoped_rows()
            .into_iter()
            .map(|(_, key, label)| (key, label))
            .collect();
        let heading = match phase {
            Phase::Voice(_) => {
                let (at, total) = self
                    .focus
                    .as_ref()
                    .map_or((1, 1), |focus| focus.progress(&rows));
                format!("Identify voices · {at} of {total}")
            }
            Phase::Duplicate(ix) => {
                let total = self.identity.snapshot.duplicates.len();
                format!("Possible duplicates · {} of {total}", ix + 1)
            }
            Phase::Loading => "Identify voices".to_owned(),
            Phase::Finished => "Identify voices · finished".to_owned(),
        };
        let leave = button(
            "identify-leave",
            "Done for now",
            true,
            cx,
            |this: &mut Self, window, cx| this.leave_focus(window, cx),
        )
        .ghost()
        .small()
        .tooltip("Back to the review queue (Esc)")
        .children(key_hint("escape"));
        let legend = match phase {
            Phase::Voice(_) => Some(
                "1–3 choose · Enter confirm · N search · D can’t tell · R not this person · S skip · Space play",
            ),
            Phase::Duplicate(_) => Some("S skip"),
            _ => None,
        };
        let header = div()
            .id("identify-header")
            .role(Role::Group)
            .aria_label("Identify voices")
            .flex()
            .flex_wrap()
            .items_center()
            .gap_3()
            .child(leave)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(title("identify-title", heading))
                    .children(scope_title.map(|t| {
                        crate::ui::text_line("identify-scope", Role::Label, t)
                            .text_role(TextRole::Caption)
                            .text_color(theme.muted_foreground)
                    })),
            )
            .children(legend.map(|text| {
                div()
                    .text_role(TextRole::Caption)
                    .text_color(theme.muted_foreground)
                    .child(text)
            }));
        let body = match phase {
            Phase::Loading => kit::status_line(
                "identify-loading",
                StatusKind::Pending,
                "Reading the review queue…",
                cx,
            )
            .into_any_element(),
            Phase::Voice(ix) => self.render_focus_voice(ix, window, cx).into_any_element(),
            Phase::Duplicate(ix) => self.render_focus_duplicate(ix, cx).into_any_element(),
            Phase::Finished => self.render_focus_finish(&rows, cx).into_any_element(),
        };
        div()
            .id("identify")
            .role(Role::Main)
            .aria_label("Identify voices")
            .key_context("IdentifyVoices")
            .track_focus(&self.focus_handle.clone().tab_index(0).tab_stop(true))
            .capture_key_down(cx.listener(Self::handle_focus_key))
            .on_action(cx.listener(|this, _: &Undo, _, cx| {
                if this.identity.can_replay(false) {
                    this.replay_identity(false, cx);
                } else if this.undo.is_some() {
                    this.undo_dismiss(cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &Redo, _, cx| {
                if this.identity.can_replay(true) {
                    this.replay_identity(true, cx);
                } else {
                    cx.propagate();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p(kit::DESK_PADDING)
            .text_color(theme.foreground)
            .child(header)
            .child(self.render_identity_history(cx))
            .children(self.render_activity(window, cx))
            .child(
                kit::desk_card("identify-card", cx)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(div().p_4().flex().flex_col().gap_4().child(body)),
            )
    }

    fn render_focus_voice(
        &self,
        ix: usize,
        window: &Window,
        cx: &Context<Self>,
    ) -> gpui::Stateful<Div> {
        let theme = cx.theme();
        let Some(row) = self.rows.get(ix) else {
            return div().id("identify-empty");
        };
        let item = &row.entry.item;
        let key = row.key();
        let readable = row.fence.is_some();
        let writing = self.saving.is_some() || self.identity.saving;
        let enabled = readable && !writing;
        let group = self.identity.snapshot.groups.get(&key);
        let context = match group {
            Some(group) if group.meeting_count() > 1 => format!(
                "{} · heard in {} meetings · {} of speech",
                speaker_name(item.speaker_id),
                group.meeting_count(),
                speech_length(item.talk_s, false)
            ),
            _ => format!(
                "{} · {} · {} · {} of speech",
                speaker_name(item.speaker_id),
                meeting_title(item.meeting_title.as_deref()),
                local_date(&item.started_at),
                speech_length(item.talk_s, false)
            ),
        };
        let label = label_of(key.1, item.meeting_title.as_deref());
        let pkey = playback_key(&key);
        let samples = SampleControls {
            key: &pkey,
            speaker: &label,
            clips: &row.samples,
            playable: row.source.is_some(),
            tab_stop: true,
            keyboard: Some(SampleKeyboard {
                focus: &self.focus_handle,
                window,
                actions: [&PlaySample, &PreviousSample, &NextSample],
                context: "IdentifyVoices",
            }),
        };
        let sample_buttons = samples.buttons(
            "identify-samples",
            &self.cursor,
            &self.playback,
            cx,
            |this: &mut Self, _, cx| this.play_selected(cx),
            |this: &mut Self, forward, _, cx| this.step_selected(forward, cx),
        );
        let quotes: Vec<_> = row
            .entry
            .sample_text
            .iter()
            .take(2)
            .enumerate()
            .map(|(qi, text)| {
                crate::ui::kit::as_inset(div(), cx)
                    .id(("identify-quote", qi))
                    .px_3()
                    .py_2()
                    .text_role(TextRole::Body)
                    .line_clamp(3)
                    .child(format!("“{}”", text.trim()))
            })
            .collect();
        let options = self.focus_options(row, cx);
        let option_rows = options.iter().enumerate().map(|(oi, option)| {
            let score = option
                .score
                .map(crate::views::person_picker::voice_score)
                .map_or_else(String::new, |s| format!(" · {s}"));
            let why = format!("{}{score}", option.why);
            let aria = format!("{}, {why}", option.name);
            let choose = button(
                ("identify-option", oi),
                option.name.clone(),
                enabled,
                cx,
                move |this: &mut Self, _, cx| this.focus_choose(oi, cx),
            )
            .when_else(
                option.suggested,
                |b| kit::primary(b, !enabled, cx),
                |b| b.outline(),
            )
            .accessibility_label(aria)
            .children(key_hint(["1", "2", "3"][oi]));
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(kit::avatar(&option.name, Disc::Suggested, None, 32., cx))
                .child(choose)
                .child(
                    div()
                        .text_role(TextRole::Caption)
                        .text_color(theme.muted_foreground)
                        .child(why),
                )
        });
        let choose_heading = if options.is_empty() {
            "Search or add a person"
        } else if accept_target(item).is_some() {
            "Is it…"
        } else {
            "No suggestion. Closest voices:"
        };
        let has_suggestion = accept_target(item).is_some();
        let foot = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                button(
                    "identify-cant-tell",
                    "Can’t tell",
                    enabled,
                    cx,
                    |this: &mut Self, _, cx| this.dismiss_selected(cx),
                )
                .outline()
                .tooltip("Set this voice aside without naming it. Restore it from Dismissed.")
                .children(key_hint("d")),
            )
            .when(has_suggestion, |el| {
                el.child(
                    button(
                        "identify-reject",
                        "Not this person",
                        enabled,
                        cx,
                        |this: &mut Self, _, cx| this.reject_selected(cx),
                    )
                    .outline()
                    .tooltip("BroScribe won’t suggest this person for this voice again")
                    .children(key_hint("r")),
                )
            })
            .child(
                button(
                    "identify-skip",
                    "Skip",
                    !writing,
                    cx,
                    |this: &mut Self, _, cx| this.focus_skip(cx),
                )
                .ghost()
                .tooltip("Leave it for later; it stays in the review queue")
                .children(key_hint("s")),
            );
        div()
            .id("identify-voice")
            .flex()
            .flex_col()
            .gap_4()
            .max_w(gpui::rems(44.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        crate::ui::text_line("identify-context", Role::Label, context)
                            .text_role(TextRole::Caption)
                            .text_color(theme.muted_foreground),
                    )
                    .child(crate::ui::heading("identify-question", question(item))),
            )
            .when(!readable, |el| {
                el.child(kit::retry_line_with_id(
                    "identify-unreadable",
                    "identify-retry",
                    "Can’t read this meeting",
                    cx,
                    |this, _, cx| this.reload(cx),
                ))
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(sample_buttons),
                    )
                    .children(samples.line("identify-sample-line", &self.cursor, cx))
                    .children(self.playback.status_of(&pkey).map(|words| {
                        kit::status_line(
                            "identify-sample-status",
                            StatusKind::Info,
                            words.to_owned(),
                            cx,
                        )
                    }))
                    .when(row.source.is_none() || row.samples.is_empty(), |el| {
                        el.child(
                            crate::ui::text_line(
                                "identify-no-sample",
                                Role::Label,
                                if row.source.is_some() {
                                    "No voice sample for this speaker."
                                } else {
                                    "Recording unavailable."
                                },
                            )
                            .text_role(TextRole::Caption),
                        )
                    }),
            )
            .child(div().flex().flex_col().gap_2().children(quotes))
            .children(row.entry.identity_conflict.clone().map(|text| {
                crate::ui::text_line("identify-conflict", Role::Label, text)
                    .text_role(TextRole::Caption)
            }))
            .child(
                div()
                    .id("identify-choose")
                    .role(Role::Group)
                    .aria_label("Who is it")
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(crate::ui::subheading(
                        "identify-choose-heading",
                        choose_heading,
                    ))
                    .children(option_rows)
                    .children(self.render_identity_preview(&key, cx))
                    .children(
                        self.naming
                            .as_ref()
                            .filter(|n| n.key == key && self.identity.identify.is_none())
                            .map(|n| {
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        crate::ui::text_line(
                                            "identify-search-label",
                                            Role::Label,
                                            "Someone else, or someone new (N)",
                                        )
                                        .text_role(TextRole::Caption)
                                        .text_color(theme.muted_foreground),
                                    )
                                    .child(n.picker.clone())
                            }),
                    ),
            )
            .when(self.saving.as_ref().is_some_and(|(k, _)| *k == key), |el| {
                el.child(kit::status_line(
                    "identify-saving",
                    StatusKind::Pending,
                    "Saving…",
                    cx,
                ))
            })
            .child(foot)
    }

    fn render_focus_duplicate(&self, ix: usize, cx: &Context<Self>) -> gpui::Stateful<Div> {
        let Some(duplicate) = self.identity.snapshot.duplicates.get(ix) else {
            return div().id("identify-empty");
        };
        let enabled = !self.identity.saving
            && self.saving.is_none()
            && self.identity.snapshot.pending.is_none();
        div()
            .id("identify-duplicate")
            .flex()
            .flex_col()
            .gap_3()
            .max_w(gpui::rems(44.))
            .child(crate::ui::heading(
                "identify-duplicate-question",
                format!(
                    "Is {} the same person as {}?",
                    duplicate.names[0], duplicate.names[1]
                ),
            ))
            .child(
                crate::ui::text_line(
                    "identify-duplicate-hint",
                    Role::Label,
                    "Merging keeps one profile and moves the other’s meetings, notes and voice samples to it.",
                )
                .text_role(TextRole::Caption)
                .text_color(cx.theme().muted_foreground),
            )
            .child(self.render_duplicate(ix, duplicate, enabled, cx))
            .child(
                button(
                    "identify-duplicate-skip",
                    "Skip",
                    enabled,
                    cx,
                    |this: &mut Self, _, cx| this.focus_skip(cx),
                )
                .ghost()
                .tooltip("Leave it for later; it stays under Possible duplicates")
                .children(key_hint("s")),
            )
    }

    fn render_focus_finish(
        &self,
        rows: &[(RowKey, String)],
        cx: &Context<Self>,
    ) -> gpui::Stateful<Div> {
        let Some(focus) = &self.focus else {
            return div().id("identify-empty");
        };
        let answers = focus.answers();
        let skipped = focus.skipped_count(rows);
        let heading = if answers.is_empty() && skipped == 0 {
            if focus.scope.is_some() {
                "No voices to identify in this meeting"
            } else {
                "No voices to identify"
            }
        } else if skipped == 0 {
            "All voices answered"
        } else {
            "Done for now"
        };
        let mut words = vec![crate::ui::format::count(
            answers.len() as u64,
            "voice answered",
            "voices answered",
        )];
        if skipped > 0 {
            words.push(format!("{skipped} skipped, still in the review queue"));
        }
        let back = button(
            "identify-finish-back",
            "Back to the review queue",
            true,
            cx,
            |this: &mut Self, window, cx| this.leave_focus(window, cx),
        );
        let again = (skipped > 0).then(|| {
            button(
                "identify-finish-skipped",
                format!("Go over skipped voices ({skipped})"),
                true,
                cx,
                |this: &mut Self, _, cx| {
                    if let Some(focus) = this.focus.as_mut() {
                        focus.revisit_skipped();
                    }
                    this.focus_sync(cx);
                    cx.notify();
                },
            )
        });
        div()
            .id("identify-finish")
            .flex()
            .flex_col()
            .gap_3()
            .child(kit::empty_state(
                "identify-finish-title",
                heading,
                crate::ui::text_line("identify-finish-summary", Role::Status, words.join(" · ")),
                true,
                Some(back),
                again,
                cx,
            ))
            .when(!answers.is_empty(), |el| {
                el.child(
                    div()
                        .id("identify-results")
                        .role(Role::List)
                        .aria_label("Answered voices")
                        .flex()
                        .flex_col()
                        .children(answers.iter().enumerate().map(|(ai, answer)| {
                            div()
                                .id(("identify-result", ai))
                                .role(Role::ListItem)
                                .aria_label(format!("{}: {}", answer.voice, answer.outcome))
                                .flex()
                                .flex_wrap()
                                .gap_2()
                                .px_3()
                                .py_2()
                                .border_b_1()
                                .border_color(cx.theme().border)
                                .child(div().text_role(TextRole::Label).child(answer.voice.clone()))
                                .child(
                                    div()
                                        .text_role(TextRole::Caption)
                                        .text_color(cx.theme().muted_foreground)
                                        .child(answer.outcome.clone()),
                                )
                        })),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u32) -> RowKey {
        thread_local! {
            static MEETING: MeetingId = MeetingId::random();
        }
        (MEETING.with(Clone::clone), SpeakerId::new(n).unwrap())
    }

    fn rows(ns: &[u32]) -> Vec<(RowKey, String)> {
        ns.iter()
            .map(|&n| (key(n), format!("Speaker {n}")))
            .collect()
    }

    fn indexed(ns: &[u32]) -> Vec<(usize, RowKey)> {
        ns.iter().enumerate().map(|(i, &n)| (i, key(n))).collect()
    }

    #[test]
    fn answered_voices_are_recorded_with_their_intent_and_undo_brings_them_back() {
        let mut focus = FocusMode::new(None);
        focus.observe(&rows(&[1, 2, 3]));
        assert_eq!(focus.progress(&rows(&[1, 2, 3])), (1, 3));
        focus.intend(&key(1), "Named Ana Example");
        focus.observe(&rows(&[2, 3]));
        assert_eq!(focus.answers().len(), 1);
        assert_eq!(focus.answers()[0].outcome, "Named Ana Example");
        assert_eq!(focus.progress(&rows(&[2, 3])), (2, 3));
        // A recheck named speaker 3 on its own.
        focus.observe(&rows(&[2]));
        assert_eq!(focus.answers()[1].outcome, "Named by the recheck");
        // Undo: speaker 1 waits again.
        focus.observe(&rows(&[1, 2]));
        assert_eq!(focus.answers().len(), 1);
        assert_eq!(focus.progress(&rows(&[1, 2])), (2, 3));
    }

    #[test]
    fn skipping_moves_to_the_next_voice_then_the_finish_and_can_be_revisited() {
        let mut focus = FocusMode::new(None);
        focus.observe(&rows(&[1, 2]));
        assert_eq!(focus.phase(true, &indexed(&[1, 2]), &[]), Phase::Voice(0));
        focus.skip(&key(1));
        assert_eq!(focus.phase(true, &indexed(&[1, 2]), &[]), Phase::Voice(1));
        assert_eq!(focus.progress(&rows(&[1, 2])), (2, 2));
        focus.skip(&key(2));
        assert_eq!(focus.phase(true, &indexed(&[1, 2]), &[]), Phase::Finished);
        assert_eq!(focus.skipped_count(&rows(&[1, 2])), 2);
        focus.revisit_skipped();
        assert_eq!(focus.phase(true, &indexed(&[1, 2]), &[]), Phase::Voice(0));
        assert_eq!(focus.phase(false, &indexed(&[1, 2]), &[]), Phase::Loading);
    }

    fn item(assignment: Assignment, suggested: Option<(&PersonId, &str)>) -> ReviewItem {
        ReviewItem {
            meeting_id: MeetingId::random(),
            meeting_title: Some("Weekly sync".into()),
            started_at: chrono::DateTime::parse_from_rfc3339("2026-03-02T10:00:00+00:00").unwrap(),
            speaker_id: SpeakerId::new(2).unwrap(),
            talk_s: 30.0,
            assignment,
            person_id: None,
            person_name: None,
            score: None,
            suggested_person_id: suggested.map(|(id, _)| id.clone()),
            suggested_name: suggested.map(|(_, name)| name.to_owned()),
            suggested_score: suggested.map(|_| 0.81),
        }
    }

    #[test]
    fn the_suggestion_leads_the_options_and_nobody_appears_twice() {
        let ana = PersonId::random();
        let suggested = item(Assignment::Suggested, Some((&ana, "Ana Example")));
        assert_eq!(question(&suggested), "Is this Ana Example?");
        let closest = vec![
            (ana.clone(), "Ana Example".to_owned(), 0.81),
            (PersonId::random(), "Ben Sample".to_owned(), 0.6),
            (PersonId::random(), "Cleo Test".to_owned(), 0.5),
            (PersonId::random(), "Dan Mock".to_owned(), 0.4),
        ];
        let options = options(&suggested, &closest);
        let names: Vec<_> = options.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["Ana Example", "Ben Sample", "Cleo Test"]);
        assert!(options[0].suggested && !options[1].suggested);
        assert_eq!(options[0].why, "Suggested");

        let unnamed = item(Assignment::None, None);
        assert_eq!(question(&unnamed), "Who is Speaker 2?");
        assert!(super::options(&unnamed, &[]).is_empty());
    }

    #[test]
    fn keys_map_to_focus_actions_and_chords_pass_through() {
        let k = |s: &'static str| focus_key(&Keystroke::parse(s).unwrap());
        assert_eq!(k("1"), Some(FocusKey::Option(0)));
        assert_eq!(k("3"), Some(FocusKey::Option(2)));
        assert_eq!(k("4"), None);
        assert_eq!(k("enter"), Some(FocusKey::Confirm));
        assert_eq!(k("d"), Some(FocusKey::CantTell));
        assert_eq!(k("r"), Some(FocusKey::NotThisPerson));
        assert_eq!(k("s"), Some(FocusKey::Skip));
        assert_eq!(k("escape"), Some(FocusKey::Leave));
        assert_eq!(k("cmd-z"), None);
        assert_eq!(k("ctrl-z"), None);
    }

    mod flow {
        use super::super::super::tests::{dismissed_at, named, open_review};
        use super::*;
        use crate::views::person_picker::fixtures::{self, sid};
        use gpui::{TestAppContext, VisualTestContext};

        fn phase(window: &gpui::WindowHandle<ReviewView>, cx: &mut TestAppContext) -> Phase {
            window.update(cx, |view, _, _| view.focus_phase()).unwrap()
        }

        fn current(window: &gpui::WindowHandle<ReviewView>, cx: &mut TestAppContext) -> SpeakerId {
            window
                .update(cx, |view, _, _| match view.focus_phase() {
                    Phase::Voice(ix) => view.rows()[ix].entry.item.speaker_id,
                    other => panic!("no voice shown: {other:?}"),
                })
                .unwrap()
        }

        fn press(window: &gpui::WindowHandle<ReviewView>, cx: &mut TestAppContext, keys: &str) {
            window
                .update(cx, |view, window, cx| {
                    view.focus_handle.focus(window, cx);
                    view.ensure_focus_picker(window, cx);
                })
                .unwrap();
            cx.run_until_parked();
            let visual = &mut VisualTestContext::from_window((*window).into(), cx);
            visual.simulate_keystrokes(keys);
            visual.run_until_parked();
        }

        #[gpui::test]
        fn one_voice_at_a_time_with_the_keyboard(cx: &mut TestAppContext) {
            let (window, app, lib, s, _dir) = open_review(cx);
            app.update(cx, |state, cx| state.open_identify(None, cx));
            cx.run_until_parked();
            assert!(
                window
                    .update(cx, |view, _, _| view.focused_mode().is_some())
                    .unwrap()
            );
            assert_eq!(current(&window, cx), sid(1));

            // D: can't tell. (A meeting with a queued confirmation job
            // refuses it, so this comes before the confirmation below.)
            press(&window, cx, "d");
            let message = window
                .update(cx, |view, _, _| view.message.clone())
                .unwrap();
            assert!(dismissed_at(&lib, &s, 1), "{message:?}");
            assert_eq!(current(&window, cx), sid(2));

            // Enter confirms speaker 2's suggestion.
            let suggested = window
                .update(cx, |view, _, _| {
                    let row = &view.rows()[view.selected];
                    accept_target(&row.entry.item).cloned()
                })
                .unwrap();
            assert!(suggested.is_some());
            press(&window, cx, "enter");
            assert_eq!(named(&lib, &s, 2), (Assignment::Confirmed, suggested));
            assert_eq!(current(&window, cx), sid(3));

            // Skip leaves speaker 3 waiting.
            press(&window, cx, "s");
            assert_eq!(named(&lib, &s, 3).0, Assignment::None);
            let answers = window
                .update(cx, |view, _, _| {
                    view.focused_mode()
                        .unwrap()
                        .answers()
                        .iter()
                        .map(|a| a.outcome.clone())
                        .collect::<Vec<_>>()
                })
                .unwrap();
            assert_eq!(answers.len(), 2, "{answers:?}");
            assert_eq!(answers[0], "Can’t tell");
            assert!(answers[1].starts_with("Named "), "{answers:?}");

            // Possible duplicates come next in the global mode; skip them.
            while let Phase::Duplicate(_) = phase(&window, cx) {
                press(&window, cx, "s");
            }
            assert_eq!(phase(&window, cx), Phase::Finished);

            press(&window, cx, "escape");
            assert!(
                window
                    .update(cx, |view, _, _| view.focused_mode().is_none())
                    .unwrap()
            );
        }

        #[gpui::test]
        fn a_number_key_names_the_voice_with_a_closest_person(cx: &mut TestAppContext) {
            let (window, app, lib, s, _dir) = open_review(cx);
            lib.write(|tx| {
                let mut m = tx.library().load_meeting(&s.meeting)?;
                fixtures::set_voice(&mut m, 1, &fixtures::voice(1.0));
                tx.save_meeting(&m)
            })
            .unwrap();
            fixtures::enroll(&lib, &s.jose, &fixtures::voice(1.0));
            fixtures::enroll(&lib, &s.ana, &fixtures::voice(0.6));
            app.update(cx, |st, cx| {
                st.library_changed(
                    &crate::watch::Changes {
                        index: true,
                        people: true,
                        ..Default::default()
                    },
                    cx,
                )
            });
            cx.run_until_parked();
            app.update(cx, |state, cx| state.open_identify(None, cx));
            cx.run_until_parked();
            assert_eq!(current(&window, cx), sid(1));
            press(&window, cx, "");
            let first = window
                .update(cx, |view, _, cx| {
                    let row = &view.rows()[view.selected];
                    view.focus_options(row, cx)
                        .first()
                        .map(|o| o.person.clone())
                })
                .unwrap();
            let first = first.expect("the closest voices are offered");
            assert_eq!(first, s.jose);
            press(&window, cx, "1");
            assert_eq!(named(&lib, &s, 1), (Assignment::Confirmed, Some(first)));
        }

        #[gpui::test]
        fn a_meeting_scope_shows_only_its_voices(cx: &mut TestAppContext) {
            let (window, app, _lib, s, _dir) = open_review(cx);
            app.update(cx, |state, cx| {
                state.open_identify(Some(MeetingId::random()), cx)
            });
            cx.run_until_parked();
            assert_eq!(phase(&window, cx), Phase::Finished);

            app.update(cx, |state, cx| {
                state.open_identify(Some(s.meeting.clone()), cx)
            });
            cx.run_until_parked();
            assert_eq!(current(&window, cx), sid(1));
            // Duplicates belong to the global mode only.
            press(&window, cx, "s s s");
            assert_eq!(phase(&window, cx), Phase::Finished);
        }
    }
}
