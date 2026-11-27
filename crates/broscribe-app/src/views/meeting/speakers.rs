//! Saved speakers, their sample player and the decisions attached to each row.

use super::*;
use crate::learning::{
    Focus, RecheckText, affected_links, recheck_failure, recheck_lines, recheck_outcome,
    recheck_phase, render_recheck,
};
use crate::ui::kit::{BadgeTone, OpenRowMenu};
use crate::views::review::{ReviewKey, review_key, step};
use crate::views::speaker_review::SampleControls;
use gpui::base::TestSupportExt as _;
use gpui::component::kbd::Kbd;
use gpui::component::menu::PopupMenu;
use gpui::{
    ClickEvent, DismissEvent, FocusHandle, MouseButton, Pixels, Point, anchored, deferred, point,
};

gpui::actions!(
    speaker_list,
    [
        Decide,
        Reject,
        Name,
        Dismiss,
        PlaySample,
        PreviousSample,
        NextSample
    ]
);

pub(super) type RecheckRevision = (JobId, u64, u64, Option<JobState>);

pub(super) struct OpenSpeakerMenu {
    pub(super) speaker: SpeakerId,
    pub(super) fence: SpeakerFence,
    menu: Entity<PopupMenu>,
    position: Point<Pixels>,
    opener: FocusHandle,
    _dismiss: Subscription,
}

#[derive(Clone)]
enum MenuCommand {
    Dismiss,
    Group(GroupingChange),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    Suggested,
    Automatic,
    Unnamed,
    Confirmed,
    Dismissed,
}

impl Decision {
    fn of(row: &SpeakerRow) -> Self {
        match (row.review_dismissed, row.assignment, &row.proposal) {
            (true, _, _) => Self::Dismissed,
            (_, Assignment::Confirmed, _) => Self::Confirmed,
            (_, Assignment::Auto, Some(_)) => Self::Automatic,
            (_, Assignment::Suggested, Some(_)) => Self::Suggested,
            _ => Self::Unnamed,
        }
    }

    fn needs_decision(self) -> bool {
        matches!(self, Self::Suggested | Self::Unnamed)
    }

    fn badge(self, row: &SpeakerRow) -> (BadgeTone, String) {
        let (tone, label) = match self {
            Self::Dismissed => (BadgeTone::Outline, "Dismissed from review"),
            Self::Confirmed => (BadgeTone::Strong, "Named by you"),
            Self::Automatic => (BadgeTone::Neutral, "Named automatically"),
            Self::Unnamed => (BadgeTone::Attention, "Needs a name"),
            Self::Suggested => {
                let name = &row.proposal.as_ref().expect("suggested speaker").name;
                return (BadgeTone::Suggested, format!("Suggested · {name}"));
            }
        };
        (tone, label.to_owned())
    }

    fn primary_control(self) -> SpeakerControl {
        match self {
            Self::Suggested | Self::Automatic => SpeakerControl::Accept,
            Self::Unnamed | Self::Confirmed => SpeakerControl::Name,
            Self::Dismissed => SpeakerControl::Review,
        }
    }

    fn more_dismiss(self) -> bool {
        matches!(self, Self::Suggested | Self::Automatic)
    }
}

/// Shared wording for the speaker list and for other views that show time spoken.
pub(crate) fn speech_time(seconds: f64, spoken: bool) -> String {
    format!("{} of speech", speech_length(seconds, spoken))
}

pub(crate) fn speech_length(seconds: f64, spoken: bool) -> String {
    if seconds > 0.0 && seconds < 1.0 {
        if spoken { "less than 1 second" } else { "<1 s" }.into()
    } else if spoken {
        spoken_length(seconds)
    } else {
        length(seconds)
    }
}

fn metadata(row: &SpeakerRow, spoken: bool) -> String {
    let mut parts = vec![speech_time(row.talk_s, spoken)];
    parts.extend(row.live_source.iter().cloned());
    if let Some(score) = row.proposal.as_ref().and_then(|proposal| proposal.score) {
        parts.push(format!("voice {score:.2}"));
    }
    if let Some(group) = &row.merged {
        let members = group
            .members
            .iter()
            .copied()
            .map(raw_label)
            .collect::<Vec<_>>()
            .join(", ");
        match group.origin {
            GroupOrigin::Auto { score } => {
                parts.push(format!("includes {members}"));
                parts.push("merged automatically".into());
                parts.push(format!("voice {score:.2}"));
            }
            GroupOrigin::User => parts.push(format!("includes {members}, merged by you")),
        }
    }
    parts.join(if spoken { ", " } else { " · " })
}

fn unavailable_audio(audio: &AudioSource) -> Option<&'static str> {
    Some(match audio {
        AudioSource::NoRecording => "Voice samples can't be played: this meeting has no recording.",
        AudioSource::Deleted => {
            "Voice samples can't be played: the audio was deleted. Names and the transcript stay."
        }
        AudioSource::Missing { .. } => {
            "Voice samples can't be played until the recording file is back."
        }
        AudioSource::Available { .. } => return None,
    })
}

fn speaker_count(rows: &[SpeakerRow]) -> String {
    let count = rows.len();
    let decisions = rows
        .iter()
        .filter(|row| Decision::of(row).needs_decision())
        .count();
    let status = match decisions {
        0 if rows.iter().any(|row| {
            row.review_dismissed
                && row.assignment != Assignment::Auto
                && row.assignment != Assignment::Confirmed
        }) =>
        {
            "none need a decision".into()
        }
        0 => "all named".into(),
        1 => "1 needs a decision".into(),
        _ => format!("{decisions} need a decision"),
    };
    format!(
        "{count} {} · {status}",
        crate::ui::format::plural(count as u64, "speaker", "speakers")
    )
}

fn shortcut(
    button: Button,
    selected: bool,
    action: &dyn gpui::Action,
    tooltip: &str,
    focus: &FocusHandle,
    window: &Window,
) -> Button {
    button
        .tooltip_with_action(tooltip.to_owned(), action, Some("SpeakerList"))
        .children(
            selected
                .then(|| Kbd::binding_for_action_in(action, focus, window))
                .flatten(),
        )
}

impl MeetingView {
    fn recheck_panel(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let state = self.state.read(cx);
        let recheck = state.recheck();
        let request = state
            .recheck_request()
            .filter(|request| request.source_meeting.as_ref() == Some(&self.meeting_id));
        let current = recheck.status.as_ref().filter(|status| {
            request.map_or_else(
                || status.requested_source.as_ref() == Some(&self.meeting_id),
                |request| status.job_id == request.job_id,
            )
        });
        let since = request
            .map(|request| request.generation)
            .or_else(|| current.map(|status| status.requested))?;
        let job = request
            .map(|request| &request.job_id)
            .or_else(|| current.map(|status| &status.job_id))?;
        let revision = (
            job.clone(),
            current.map_or(since, |status| status.requested.max(since)),
            current.map_or(0, |status| status.processed),
            current.map(|status| status.state),
        );
        if self.speaker_recheck_dismissed.as_ref() == Some(&revision) {
            return None;
        }
        let error = state.recheck_error();
        let text = match current {
            Some(status) => RecheckText {
                // Only for the generation requested here, never for a
                // later one requested elsewhere on the same job.
                outcome: request.and_then(|request| recheck_outcome(status, request)),
                lines: recheck_lines(
                    status,
                    Some(Focus {
                        meeting: &self.meeting_id,
                        label: "This meeting",
                    }),
                    Some(since),
                    state.recheck_progress(),
                ),
                links: affected_links(recheck, Some(since))
                    .into_iter()
                    .filter(|(id, _)| *id != self.meeting_id)
                    .collect(),
            },
            None if error.is_some() => RecheckText::default(),
            None => RecheckText {
                lines: vec!["Checking other speakers, this meeting first…".to_owned()],
                ..RecheckText::default()
            },
        };
        // A failed newer read is reported even when an older status is
        // shown; that status stays, marked as the last known state.
        let error = error.map(|cause| recheck_failure(cause, current.is_some()));
        Some(render_recheck(
            "speaker-recheck",
            text,
            error,
            recheck_phase(current, Some(since)),
            &self.state,
            move |this: &mut Self, window, cx| {
                this.speaker_recheck_dismissed = Some(revision.clone());
                this.speaker_list_focus.focus(window, cx);
                cx.notify();
            },
            cx,
        ))
    }

    /// "Play sample" / "Stop" for the speaker shown as `sid` (`label` names
    /// it for assistive technology).
    fn sample_button(
        &self,
        id: impl Into<ElementId>,
        sid: SpeakerId,
        label: &str,
        play_text: &str,
        cx: &Context<Self>,
    ) -> Button {
        let playing = self.sample_playing(sid);
        let playable = playing || self.speaker_sample(sid).is_some();
        button(
            id,
            if playing { "Stop" } else { play_text }.to_owned(),
            playable,
            cx,
            move |this, _, cx| this.toggle_sample(sid, cx),
        )
        .ghost()
        .small()
        .accessibility_label(if playing {
            format!("Stop {label}'s sample")
        } else {
            format!("Play a sample of {label}")
        })
    }

    /// "Possible duplicates": pairs that may be one voice, each with Play
    /// (both), Merge and "Not the same person".
    fn duplicates_list(&self, data: &MeetingData, cx: &Context<Self>) -> Option<impl IntoElement> {
        let visible = |id| data.speakers.iter().any(|row| row.id == id);
        let mut duplicates = data
            .grouping
            .duplicates
            .iter()
            .filter(|d| visible(d.a) && visible(d.b))
            .peekable();
        let playable = matches!(data.audio, AudioSource::Available { .. });
        duplicates.peek()?;
        let theme = cx.theme();
        let label = |id: SpeakerId| speaker_label(&data.meeting, Some(id), &data.people);
        let rows = duplicates.enumerate().map(|(ix, d)| {
            let (a, b) = (d.a, d.b);
            let (a_label, b_label) = (label(a), label(b));
            let merge = button(
                ("duplicate-merge", ix),
                format!("Merge into {a_label}"),
                true,
                cx,
                move |this, window, cx| {
                    this.change_grouping(
                        GroupingChange::Merge {
                            kept: a,
                            members: vec![b],
                        },
                        window,
                        cx,
                    )
                },
            )
            .small()
            .accessibility_label(format!("Merge {b_label} into {a_label}"));
            let not_same = button(
                ("duplicate-not-same", ix),
                "Not the same person",
                true,
                cx,
                move |this, window, cx| {
                    this.change_grouping(GroupingChange::NotSame { a, b }, window, cx)
                },
            )
            .ghost()
            .small()
            .accessibility_label(format!("{b_label} and {a_label} are not the same person"));
            div()
                .id(("duplicate", ix))
                .role(Role::ListItem)
                .aria_label(d.text.clone())
                .flex()
                .flex_col()
                .gap_1()
                .py_1()
                .child(div().text_role(TextRole::Reading).child(d.text.clone()))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .when(playable, |actions| {
                            actions
                                .child(self.sample_button(
                                    ("duplicate-play-b", ix),
                                    b,
                                    &b_label,
                                    &format!("Play {b_label}"),
                                    cx,
                                ))
                                .child(self.sample_button(
                                    ("duplicate-play-a", ix),
                                    a,
                                    &a_label,
                                    &format!("Play {a_label}"),
                                    cx,
                                ))
                        })
                        .child(merge)
                        .child(not_same),
                )
        });
        // The heading sits beside the list it names, not in it, so no node
        // nests another with its text (INS-01).
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p_3()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.warning)
                .child(crate::ui::heading(
                    "duplicates-heading",
                    "Possible duplicates",
                ))
                .child(text_line(
                    "duplicates-explanation", Role::Label,
                    "Merging shows two speakers as one. No line changes, and Unmerge undoes it.",
                ).text_role(TextRole::Caption).text_color(theme.muted_foreground))
                .child(
                    div()
                        .id("possible-duplicates")
                        .role(Role::List)
                        .aria_label("Possible duplicate pairs")
                        .flex()
                        .flex_col()
                        .gap_1()
                        .children(rows),
                ),
        )
    }

    fn select_speaker(&mut self, speaker: SpeakerId, cx: &mut Context<Self>) {
        if self.selected_speaker != Some(speaker) {
            self.selected_speaker = Some(speaker);
            cx.notify();
        }
    }

    fn reveal_speaker(&self, index: usize) {
        if let Some(bounds) = self.speaker_rows_bounds.bounds_for_item(index) {
            let viewport = self.speaker_scroll.bounds();
            let mut offset = self.speaker_scroll.offset();
            if bounds.top() < viewport.top() {
                offset.y += viewport.top() - bounds.top();
            } else if bounds.bottom() > viewport.bottom() {
                offset.y -=
                    (bounds.bottom() - viewport.bottom()).min(bounds.top() - viewport.top());
            }
            self.speaker_scroll.set_offset(offset);
        }
    }

    fn page_speaker(&self, index: usize, last: usize, down: bool) -> usize {
        let Some(current) = self.speaker_rows_bounds.bounds_for_item(index) else {
            return step(index, last + 1, down);
        };
        let page = self.speaker_scroll.bounds().size.height.max(px(1.));
        let distance = |other| {
            self.speaker_rows_bounds
                .bounds_for_item(other)
                .map_or(Pixels::ZERO, |bounds| (bounds.top() - current.top()).abs())
        };
        if down {
            ((index + 1)..=last)
                .find(|&other| distance(other) >= page)
                .unwrap_or(last)
        } else {
            (0..index)
                .rev()
                .find(|&other| distance(other) >= page)
                .unwrap_or(0)
        }
    }

    fn speaker_key(&mut self, key: ReviewKey, window: &mut Window, cx: &mut Context<Self>) {
        if self.naming.is_some() || self.speaker_menu.is_some() {
            return;
        }
        let Some(data) = &self.data else { return };
        let Some(index) = data
            .speakers
            .iter()
            .position(|row| Some(row.id) == self.selected_speaker)
        else {
            return;
        };
        let row = &data.speakers[index];
        let speaker = row.id;
        let decision = Decision::of(row);
        match key {
            ReviewKey::Up | ReviewKey::Down => {
                let next = step(index, data.speakers.len(), key == ReviewKey::Down);
                self.select_speaker(data.speakers[next].id, cx);
                self.reveal_speaker(next);
                self.speaker_list_focus.focus(window, cx);
            }
            ReviewKey::Accept if decision == Decision::Dismissed => {
                self.set_dismissed(speaker, false, window, cx);
            }
            ReviewKey::Accept
                if decision == Decision::Unnamed || decision == Decision::Confirmed =>
            {
                self.start_naming(speaker, window, cx);
            }
            ReviewKey::Accept | ReviewKey::Reject if decision != Decision::Dismissed => {
                if let Some(person) = row.proposal.as_ref().map(|p| p.person_id.clone()) {
                    if key == ReviewKey::Accept {
                        self.accept_proposal(speaker, person, window, cx);
                    } else {
                        self.reject_proposal(speaker, person, window, cx);
                    }
                }
            }
            ReviewKey::Name if decision != Decision::Dismissed => {
                self.start_naming(speaker, window, cx)
            }
            ReviewKey::Dismiss if decision.needs_decision() || decision == Decision::Automatic => {
                self.set_dismissed(speaker, true, window, cx);
            }
            ReviewKey::Play => self.toggle_sample(speaker, cx),
            ReviewKey::Previous | ReviewKey::Next => {
                self.step_sample(speaker, key == ReviewKey::Next, cx)
            }
            _ => {}
        }
    }

    fn on_speaker_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.naming.is_some() || self.speaker_menu.is_some() {
            return;
        }
        if !ev.keystroke.modifiers.modified()
            && matches!(
                ev.keystroke.key.as_str(),
                "home" | "end" | "pageup" | "pagedown"
            )
            && let Some(data) = &self.data
            && let Some(index) = data
                .speakers
                .iter()
                .position(|row| Some(row.id) == self.selected_speaker)
        {
            let last = data.speakers.len() - 1;
            let next = match ev.keystroke.key.as_str() {
                "home" => 0,
                "end" => last,
                key => self.page_speaker(index, last, key == "pagedown"),
            };
            self.select_speaker(data.speakers[next].id, cx);
            self.reveal_speaker(next);
            self.speaker_list_focus.focus(window, cx);
            cx.stop_propagation();
            return;
        }
        let Some(key) = review_key(&ev.keystroke) else {
            return;
        };
        if matches!(
            key,
            ReviewKey::First | ReviewKey::Last | ReviewKey::PageUp | ReviewKey::PageDown
        ) {
            return;
        }
        if matches!(key, ReviewKey::Accept | ReviewKey::Play)
            && !self.speaker_list_focus.is_focused(window)
        {
            return;
        }
        cx.stop_propagation();
        self.speaker_key(key, window, cx);
    }

    pub(super) fn speaker_has_menu(&self, row: &SpeakerRow, data: &MeetingData) -> bool {
        Decision::of(row).more_dismiss()
            || row.merged.is_some()
            || data
                .grouping
                .merge_targets
                .get(&row.id)
                .is_some_and(|targets| data.speakers.iter().any(|row| targets.contains(&row.id)))
    }

    pub(super) fn close_speaker_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(open) = self.speaker_menu.take() {
            if open.menu.focus_handle(cx).contains_focused(window, cx) {
                open.opener.focus(window, cx);
            }
            cx.notify();
        }
    }

    fn open_speaker_menu(
        &mut self,
        speaker: SpeakerId,
        position: Option<Point<Pixels>>,
        from_button: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(data) = &self.data else { return };
        let Some(index) = data.speakers.iter().position(|row| row.id == speaker) else {
            return;
        };
        let row = &data.speakers[index];
        if !self.speaker_has_menu(row, data) {
            return;
        }
        let opener = if from_button {
            self.speaker_focus[&(speaker, SpeakerControl::More)].clone()
        } else {
            self.speaker_list_focus.clone()
        };
        let viewport = self.speaker_scroll.bounds();
        let position = position.unwrap_or_else(|| {
            self.speaker_rows_bounds
                .bounds_for_item(index)
                .map_or(viewport.origin, |bounds| {
                    point(
                        bounds.left() + px(12.),
                        bounds.bottom().clamp(viewport.top(), viewport.bottom()),
                    )
                })
        });
        let fence = SpeakerFence::of(&data.meeting);
        let dismiss_speaker = Decision::of(row).more_dismiss();
        let source_name = row.label.clone();
        let group = row.merged.as_ref().map(|group| group.group_id.clone());
        let targets = data
            .grouping
            .merge_targets
            .get(&speaker)
            .into_iter()
            .flatten()
            .filter_map(|target| data.speakers.iter().find(|row| row.id == *target))
            .map(|row| (row.id, row.label.clone(), row.talk_s))
            .collect::<Vec<_>>();
        self.end_naming(cx);
        self.select_speaker(speaker, cx);
        let view = cx.weak_entity();
        let command = {
            let fence = fence.clone();
            let opener = opener.clone();
            move |command: MenuCommand| {
                let view = view.clone();
                let fence = fence.clone();
                let opener = opener.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let _ = view.update(cx, |this, cx| {
                        if this.tab == MeetingTab::Speakers
                            && this.shown_fence() == Some(fence.clone())
                        {
                            opener.focus(window, cx);
                            this.close_speaker_menu(window, cx);
                            match &command {
                                MenuCommand::Dismiss => {
                                    this.set_dismissed(speaker, true, window, cx)
                                }
                                MenuCommand::Group(change) => {
                                    this.change_grouping(change.clone(), window, cx)
                                }
                            }
                        }
                    });
                }
            }
        };
        let menu = PopupMenu::build(window, cx, |menu, window, cx| {
            menu.action_context(opener.clone())
                .when(dismiss_speaker, |menu| {
                    menu.item(
                        PopupMenuItem::new("Can't tell").on_click(command(MenuCommand::Dismiss)),
                    )
                })
                .when(!targets.is_empty(), |menu| {
                    let command = command.clone();
                    menu.submenu("Merge into", window, cx, move |menu, _, _| {
                        targets.iter().fold(menu, |menu, (target, name, time)| {
                            let label = format!("{name} · {}", speech_time(*time, false));
                            let aria = format!("Merge {source_name} into {name}");
                            let kept = *target;
                            menu.item(
                                PopupMenuItem::element(move |_, _| {
                                    let aria = aria.clone();
                                    div()
                                        .id(("speaker-merge-target-label", kept.number() as usize))
                                        .role(Role::Group)
                                        .a11y_synthetic_children(move |builder| {
                                            builder.parent_node().set_role(Role::GenericContainer);
                                            if let Some(item) =
                                                builder.ancestor_node(Role::MenuItem)
                                            {
                                                item.set_label(aria.clone());
                                            }
                                        })
                                        .child(label.clone())
                                })
                                .on_click(command(
                                    MenuCommand::Group(GroupingChange::Merge {
                                        kept,
                                        members: vec![speaker],
                                    }),
                                )),
                            )
                        })
                    })
                })
                .when_some(group, |menu, group| {
                    menu.item(
                        PopupMenuItem::new("Unmerge").on_click(command(MenuCommand::Group(
                            GroupingChange::Unmerge { group },
                        ))),
                    )
                })
        });
        let dismiss = cx.subscribe_in(&menu, window, |this, _, _: &DismissEvent, window, cx| {
            this.close_speaker_menu(window, cx);
        });
        menu.focus_handle(cx).focus(window, cx);
        self.speaker_menu = Some(OpenSpeakerMenu {
            speaker,
            fence,
            menu,
            opener,
            position,
            _dismiss: dismiss,
        });
        cx.notify();
    }

    fn speaker_row(
        &self,
        row: &SpeakerRow,
        index: usize,
        data: &MeetingData,
        window: &Window,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let speaker = row.id;
        let key = sample_key(speaker);
        let id = speaker.number() as usize;
        let selected = self.selected_speaker == Some(speaker);
        let focused = self.speaker_list_focus.is_focused(window);
        let selection_focused = self.speaker_list_focus.contains_focused(window, cx);
        let focus = |control| &self.speaker_focus[&(speaker, control)];
        let decision = Decision::of(row);
        let (tone, state) = decision.badge(row);
        let primary = |button: Button, control| {
            button.when(
                selected && decision.primary_control() == control,
                |button| kit::primary(button, false, cx),
            )
        };
        let label = &row.label;
        let aria = format!("{label}, {state}, {}", metadata(row, true));
        let caption = kit::row_metadata_colour(selected, cx);
        let playable = matches!(data.audio, AudioSource::Available { .. });
        let samples = SampleControls {
            key: &key,
            speaker: label,
            clips: &row.samples,
            playable,
            tab_stop: selected,
            keyboard: Some(crate::views::speaker_review::SampleKeyboard {
                focus: &self.speaker_list_focus,
                window,
                actions: [&PlaySample, &PreviousSample, &NextSample],
                context: "SpeakerList",
            }),
        };
        let picker = self
            .naming
            .as_ref()
            .filter(|n| n.target == NamingTarget::Speaker(speaker))
            .map(|n| n.picker.clone());
        let hint = |button, action: &dyn gpui::Action, tooltip: &str| {
            shortcut(
                button,
                selected,
                action,
                tooltip,
                &self.speaker_list_focus,
                window,
            )
        };
        let mut actions = div()
            .flex()
            .flex_wrap()
            .gap_2()
            .items_center()
            .key_context("SpeakerActions");
        if playable && !row.samples.is_empty() {
            actions = actions.child(samples.buttons(
                ("speaker-samples", id),
                &self.sample_cursor,
                &self.samples,
                cx,
                move |this: &mut Self, _, cx| {
                    this.select_speaker(speaker, cx);
                    this.toggle_sample(speaker, cx);
                },
                move |this: &mut Self, forward, _, cx| {
                    this.select_speaker(speaker, cx);
                    this.step_sample(speaker, forward, cx);
                },
            ));
        }
        if let Some(proposal) = row
            .proposal
            .as_ref()
            .filter(|_| decision != Decision::Dismissed)
        {
            let accept = proposal.person_id.clone();
            let reject = accept.clone();
            let accept_name = if decision == Decision::Automatic {
                format!("Accept {}, named automatically", proposal.name)
            } else {
                format!("Accept {} for {}", proposal.name, raw_label(speaker))
            };
            let reject_tip = format!(
                "BroScribe won't suggest {} for this speaker again",
                proposal.name
            );
            actions = actions
                .child(hint(
                    primary(
                        button(
                            ("speaker-accept", id),
                            "Accept",
                            true,
                            cx,
                            move |this, window, cx| {
                                this.select_speaker(speaker, cx);
                                this.accept_proposal(speaker, accept.clone(), window, cx);
                            },
                        )
                        .small()
                        .icon(gpui::assets::IconName::Check)
                        .tab_stop(selected)
                        .track_focus(focus(SpeakerControl::Accept))
                        .accessibility_label(accept_name)
                        .when(decision == Decision::Automatic, |button| button.ghost()),
                        SpeakerControl::Accept,
                    ),
                    &Decide,
                    "BroScribe learns this voice from the speaker's samples",
                ))
                .child(hint(
                    button(
                        ("speaker-reject", id),
                        "Reject",
                        true,
                        cx,
                        move |this, window, cx| {
                            this.select_speaker(speaker, cx);
                            this.reject_proposal(speaker, reject.clone(), window, cx);
                        },
                    )
                    .small()
                    .ghost()
                    .tab_stop(selected)
                    .track_focus(focus(SpeakerControl::Reject))
                    .accessibility_label(format!(
                        "Reject {} for {}",
                        proposal.name,
                        raw_label(speaker)
                    )),
                    &Reject,
                    &reject_tip,
                ));
        }
        if picker.is_none() && decision != Decision::Dismissed {
            let named = matches!(decision, Decision::Confirmed | Decision::Automatic);
            let (visible, name) = if named {
                (
                    "Change person…",
                    format!("Name {label}'s speaker as someone else…"),
                )
            } else {
                ("Name…", format!("Name {label}…"))
            };
            let name_button = button(
                ("speaker-name", id),
                visible,
                true,
                cx,
                move |this, window, cx| {
                    this.select_speaker(speaker, cx);
                    this.start_naming(speaker, window, cx);
                },
            )
            .small()
            .tab_stop(selected)
            .track_focus(focus(SpeakerControl::Name))
            .accessibility_label(name.clone())
            .when(decision != Decision::Unnamed, |button| button.ghost());
            actions = actions.child(hint(
                primary(name_button, SpeakerControl::Name),
                &Name,
                &name,
            ));
        }
        if matches!(decision, Decision::Dismissed | Decision::Unnamed) {
            let restore = decision == Decision::Dismissed;
            let (visible, name) = if restore {
                ("Restore", format!("Restore {label} to the review queue"))
            } else {
                (
                    "Can't tell",
                    format!("Dismiss {label} from review without naming it"),
                )
            };
            let review_button = button(
                (
                    if restore {
                        "speaker-restore"
                    } else {
                        "speaker-dismiss"
                    },
                    id,
                ),
                visible,
                true,
                cx,
                move |this, window, cx| {
                    this.select_speaker(speaker, cx);
                    this.set_dismissed(speaker, !restore, window, cx);
                },
            )
            .small()
            .ghost()
            .tab_stop(selected)
            .track_focus(focus(SpeakerControl::Review))
            .accessibility_label(name.clone());
            actions = actions.child(hint(
                primary(review_button, SpeakerControl::Review),
                if restore {
                    &Decide as &dyn gpui::Action
                } else {
                    &Dismiss
                },
                &name,
            ));
        }
        let has_menu = self.speaker_has_menu(row, data);
        if has_menu {
            let name = format!("More actions for {label}");
            let more = Button::new(("speaker-more", id))
                .icon(gpui::assets::IconName::Ellipsis)
                .small()
                .ghost()
                .tab_stop(selected)
                .track_focus(focus(SpeakerControl::More))
                .accessibility_label(name.clone())
                .aria_expanded(
                    self.speaker_menu
                        .as_ref()
                        .is_some_and(|menu| menu.speaker == speaker),
                )
                .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                    this.open_speaker_menu(speaker, Some(ev.position()), true, window, cx);
                    cx.stop_propagation();
                }));
            actions = actions.child(hint(more, &OpenRowMenu, &name));
        }
        if let Some(segment) = row
            .transcript_segment
            .filter(|_| !playable || row.samples.is_empty())
        {
            actions = actions.child(
                button(
                    ("speaker-transcript", id),
                    "Go to transcript",
                    true,
                    cx,
                    move |this: &mut Self, window, cx| {
                        this.set_tab(MeetingTab::Transcript, cx);
                        this.reveal_transcript_segment(segment, true, window, cx);
                        cx.stop_propagation();
                    },
                )
                .ghost()
                .small()
                .tab_stop(selected)
                .accessibility_label(format!("Go to the transcript for {label}")),
            );
        }
        let base = div()
            .id(("speaker", id))
            .role(Role::ListItem)
            .aria_label(aria)
            .aria_position_in_set(index + 1)
            .aria_size_of_set(data.speakers.len())
            .when(selected && focused, |row| row.aria_active_descendant())
            .flex()
            .flex_col()
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_speaker(speaker, cx);
                this.speaker_list_focus.focus(window, cx);
            }))
            .when(has_menu, |row| {
                row.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, ev: &gpui::MouseDownEvent, window, cx| {
                        this.open_speaker_menu(speaker, Some(ev.position), false, window, cx);
                        cx.stop_propagation();
                        window.prevent_default();
                    }),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &gpui::MouseDownEvent, window, cx| {
                        if ev.modifiers.control
                            && NativePlatform::host_platform()
                                == broscribe_platform::HostPlatform::MacOs
                        {
                            this.open_speaker_menu(speaker, Some(ev.position), false, window, cx);
                            cx.stop_propagation();
                            window.prevent_default();
                        }
                    }),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(label.clone()))
                    .child(kit::badge(tone, state, cx)),
            )
            .child(
                div()
                    .id(("speaker-talk", id))
                    .spoken(Role::Label, metadata(row, true))
                    .truncate()
                    .text_role(TextRole::Caption)
                    .text_color(caption)
                    .child(metadata(row, false)),
            )
            .children(row.identity_conflict.iter().map(|note| {
                text_line(("speaker-identity-conflict", id), Role::Label, note.clone())
                    .text_role(TextRole::Caption)
                    .text_color(caption)
            }))
            .child(actions)
            .when(playable && row.samples.is_empty(), |row| {
                row.child(
                    text_line(
                        ("speaker-no-sample", id),
                        Role::Label,
                        "No voice sample for this speaker.",
                    )
                    .text_role(TextRole::Caption)
                    .text_color(caption),
                )
            })
            .children(samples.status(("speaker-sample-status", id), &self.samples, cx))
            .children(samples.line(("speaker-sample-line", id), &self.sample_cursor, cx))
            .children(self.edit_line(&EditTarget::Speaker(speaker), ("speaker-status", id), cx))
            .children(
                self.undo
                    .as_ref()
                    .filter(|undo| undo.speaker == speaker)
                    .map(|undo| {
                        button(
                            ("undo-dismiss", id),
                            "Undo",
                            true,
                            cx,
                            |this, window, cx| this.undo_dismiss(window, cx),
                        )
                        .ghost()
                        .small()
                        .tab_stop(selected)
                        .accessibility_label(format!(
                            "Undo: return {} to the review queue",
                            undo.label
                        ))
                    }),
            )
            .children(picker);
        kit::selected_row(base, selected, selection_focused, cx)
    }

    fn speakers_without_evidence(
        &self,
        data: &MeetingData,
        cx: &Context<Self>,
    ) -> Option<Stateful<Div>> {
        let rows = &data.speakers_without_evidence;
        if rows.is_empty() {
            return None;
        }
        let title = "Voices without a transcript";
        let heading = format!("{title} ({})", rows.len());
        Some(
            div()
                .id("speakers-without-evidence")
                .role(Role::Group)
                .aria_label(title)
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    kit::disclosure(
                        "speakers-without-evidence-toggle",
                        heading,
                        self.show_speakers_without_evidence,
                        cx,
                        |this, _, cx| {
                            this.show_speakers_without_evidence =
                                !this.show_speakers_without_evidence;
                            cx.notify();
                        },
                    )
                    .track_focus(&self.speaker_disclosure_focus),
                )
                .when(self.show_speakers_without_evidence, |section| {
                    section
                        .child(
                            text_line(
                                "speakers-without-evidence-hint",
                                Role::Label,
                                "No playable samples for these speakers.",
                            )
                            .text_role(TextRole::Caption)
                            .text_color(cx.theme().muted_foreground),
                        )
                        .child(
                            div()
                                .id("speakers-without-evidence-list")
                                .role(Role::List)
                                .aria_label(title)
                                .children(rows.iter().map(|row| {
                                    text_line(
                                        ("speaker-without-evidence", row.id.number() as usize),
                                        Role::ListItem,
                                        format!(
                                            "{} · {}",
                                            row.label,
                                            speech_time(row.talk_s, false)
                                        ),
                                    )
                                    .aria_label(format!(
                                        "{}, {}",
                                        row.label,
                                        speech_time(row.talk_s, true)
                                    ))
                                    .px_3()
                                    .py_2()
                                })),
                        )
                }),
        )
    }

    pub(super) fn speakers(
        &self,
        data: &MeetingData,
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let empty = data.speakers.is_empty() && data.speakers_without_evidence.is_empty();
        let caption = |id, role, words: String| {
            text_line(id, role, words)
                .text_role(TextRole::Caption)
                .text_color(cx.theme().muted_foreground)
        };
        let list = div()
            .id("speaker-list")
            .role(Role::List)
            .aria_label("Meeting speakers")
            .aria_keyshortcuts("Enter R N D ArrowUp ArrowDown ArrowLeft ArrowRight Space")
            .track_focus(&self.speaker_list_focus.clone().tab_index(0).tab_stop(true))
            .track_scroll(&self.speaker_rows_bounds)
            .key_context("SpeakerList")
            .flex()
            .flex_col()
            .capture_key_down(cx.listener(Self::on_speaker_key))
            .on_action(cx.listener(|this, _: &Decide, window, cx| {
                if this.speaker_list_focus.is_focused(window) {
                    this.speaker_key(ReviewKey::Accept, window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &PlaySample, window, cx| {
                if this.speaker_list_focus.is_focused(window) {
                    this.speaker_key(ReviewKey::Play, window, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &Reject, window, cx| {
                this.speaker_key(ReviewKey::Reject, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Name, window, cx| {
                this.speaker_key(ReviewKey::Name, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Dismiss, window, cx| {
                this.speaker_key(ReviewKey::Dismiss, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PreviousSample, window, cx| {
                this.speaker_key(ReviewKey::Previous, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NextSample, window, cx| {
                this.speaker_key(ReviewKey::Next, window, cx)
            }))
            .on_action(cx.listener(|this, _: &OpenRowMenu, window, cx| {
                if this.naming.is_none()
                    && let Some(speaker) = this.selected_speaker
                {
                    this.open_speaker_menu(speaker, None, false, window, cx);
                }
            }))
            .when(empty, |list| {
                list.child(
                    caption(
                        "speakers-empty",
                        Role::Status,
                        if data.meeting.processing.status == ProcessingStatus::Done {
                            "No speakers were found in this recording.".into()
                        } else {
                            "Speakers appear when the final transcript is ready.".into()
                        },
                    )
                    .p_2(),
                )
            })
            .children(data.speakers.iter().enumerate().map(|(index, row)| {
                self.speaker_row(row, index, data, window, cx)
                    .test_support()
            }));
        let menu = self.speaker_menu.as_ref().map(|open| {
            deferred(
                anchored()
                    .position(open.position)
                    .snap_to_window_with_margin(px(8.))
                    .child(open.menu.clone()),
            )
            .with_priority(gpui::base::POPUP_PRIORITY)
        });
        div()
            .id("speakers")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .id("speakers-scroll")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap_2()
                    .px_4()
                    .py_2()
                    .overflow_y_scroll()
                    .track_scroll(&self.speaker_scroll)
                    .when_some(
                        unavailable_audio(&data.audio).filter(|_| !data.speakers.is_empty()),
                        |view, reason| {
                            view.child(caption("speakers-no-audio", Role::Status, reason.into()))
                        },
                    )
                    .when(!data.speakers.is_empty(), |view| {
                        view.child(caption(
                            "speakers-count",
                            Role::Status,
                            speaker_count(&data.speakers),
                        ))
                    })
                    .child(self.speaker_history_buttons(cx))
                    .children(self.recheck_panel(cx))
                    .children(self.edit_line(&EditTarget::Speakers, "speakers-edit-status", cx))
                    .children(self.last_edit.iter().filter_map(|target| {
                        let EditTarget::Speaker(id) = target else {
                            return None;
                        };
                        (!data.speakers.iter().any(|row| row.id == *id))
                            .then(|| {
                                self.edit_line(
                                    target,
                                    ("removed-speaker-status", id.number() as usize),
                                    cx,
                                )
                            })
                            .flatten()
                    }))
                    .children(self.duplicates_list(data, cx))
                    .when(empty || !data.speakers.is_empty(), |view| {
                        view.child(list.test_support())
                    })
                    .children(self.speakers_without_evidence(data, cx))
                    .test_support(),
            )
            .children(menu)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{
        click_meeting_control, meeting_control_bounds, open_edited, speakers_tab, spoken_nodes,
        with_recording,
    };
    use super::*;
    use crate::ui::{a11y_tree::draw_and_read, spoken_log::node};
    use crate::views::person_picker::fixtures::{self, sid};
    use broscribe_core::schema::{Segment, SpeakerGroup};
    use broscribe_platform::{HostPlatform, fakes::FakeAudioPlayer};
    use gpui::{TestAppContext, VisualTestContext};

    #[gpui::test]
    fn unavailable_audio_gets_one_status_and_no_sample_controls(cx: &mut TestAppContext) {
        let (handle, app, _, _, _dir) = open_edited(cx, |meeting, _| {
            fixtures::set_voice(meeting, 1, &fixtures::voice(1.0));
            fixtures::set_voice(meeting, 3, &fixtures::voice(0.86));
        });
        app.update(cx, |state, cx| {
            let mut config = state.config().clone();
            config.identity.insert(
                fixtures::voice_model(),
                Thresholds {
                    merge: Some(0.5),
                    auto_merge: Some(0.95),
                    ..Default::default()
                },
            );
            state.set_config(config, cx);
        });
        speakers_tab(&handle, cx);
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        for (audio, reason) in [
            (
                AudioSource::NoRecording,
                "Voice samples can't be played: this meeting has no recording.",
            ),
            (
                AudioSource::Deleted,
                "Voice samples can't be played: the audio was deleted. Names and the transcript stay.",
            ),
            (
                AudioSource::Missing { path: None },
                "Voice samples can't be played until the recording file is back.",
            ),
        ] {
            handle
                .update(&mut visual, |view, _, cx| {
                    let data = view.data.as_mut().unwrap();
                    assert_eq!(data.grouping.duplicates.len(), 1);
                    data.audio = audio;
                    cx.notify();
                })
                .unwrap();
            let tree = visual.update(draw_and_read);
            assert_eq!(
                tree.read(tree.find("speakers-no-audio")),
                ("Status", reason)
            );
            assert!(
                tree.nodes
                    .iter()
                    .all(|node| !node
                        .element_id
                        .as_deref()
                        .is_some_and(|id| id.starts_with("sample-play-")
                            || id.starts_with("duplicate-play-")))
            );
            assert!(tree.all("speakers-no-audio").len() == 1);
        }
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("duplicates-heading")),
            ("Heading", "Possible duplicates")
        );
        assert_eq!(
            tree.read(tree.find("possible-duplicates")),
            ("List", "Possible duplicate pairs")
        );
        assert_eq!(
            tree.read(tree.find("duplicate-0")).1,
            "Speaker 3 may be the same person as Speaker 1 · voice 0.86"
        );
        assert_eq!(
            tree.read(tree.find("duplicate-merge-0")),
            ("Button", "Merge Speaker 3 into Speaker 1")
        );
        assert!(
            tree.read(tree.find("duplicates-explanation"))
                .1
                .contains("Unmerge undoes it")
        );
        assert!(
            meeting_control_bounds(&mut visual, "duplicate-merge-0").top()
                < meeting_control_bounds(&mut visual, "speaker-list").top()
        );
    }

    #[gpui::test]
    fn decision_badges_and_actions_follow_assignment(cx: &mut TestAppContext) {
        use broscribe_core::schema::Speaker;
        let (handle, _, _, _, _dir) = open_edited(cx, |meeting, people| {
            meeting.speakers[0].assignment = Assignment::Confirmed;
            meeting.speakers[0].person_id = Some(people.ana.clone());
            meeting.speakers[2].assignment = Assignment::Auto;
            meeting.speakers[2].person_id = Some(people.ana_other.clone());
            meeting.speakers[2].score = Some(0.90);
            for (n, talk) in [(4, 9.), (5, 8.)] {
                let mut speaker = Speaker::new(sid(n), "t0");
                speaker.talk_s = talk;
                if n == 5 {
                    speaker.review_dismissed_at = Some(meeting.started_at);
                }
                meeting.speakers.push(speaker);
            }
            meeting.segments.push(Segment {
                speech_overlap: Vec::new(),
                id: 100,
                track: "t0".into(),
                speaker: Some(sid(4)),
                start: 18.0,
                end: 19.0,
                text: "Yes".into(),
                words: Vec::new(),
            });
        });
        speakers_tab(&handle, cx);
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        visual.simulate_resize(gpui::size(px(900.), px(950.)));
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("speakers-count")),
            ("Status", "5 speakers · 2 need a decision")
        );
        handle
            .update(&mut visual, |view, _, _| {
                let data = view.data.as_ref().unwrap();
                for (row, (tone, words, primary)) in data.speakers.iter().zip([
                    // Named by you is teal with a check; a suggestion is
                    // dashed, like every unconfirmed mark.
                    (BadgeTone::Strong, "Named by you", SpeakerControl::Name),
                    (
                        BadgeTone::Suggested,
                        "Suggested · José Pereira",
                        SpeakerControl::Accept,
                    ),
                    (
                        BadgeTone::Neutral,
                        "Named automatically",
                        SpeakerControl::Accept,
                    ),
                    (BadgeTone::Attention, "Needs a name", SpeakerControl::Name),
                    (
                        BadgeTone::Outline,
                        "Dismissed from review",
                        SpeakerControl::Review,
                    ),
                ]) {
                    let state = Decision::of(row);
                    assert_eq!(state.badge(row), (tone, words.into()));
                    assert_eq!(state.primary_control(), primary);
                    assert!(
                        tree.read(tree.find(&format!("speaker-{}", row.id.number())))
                            .1
                            .contains(words)
                    );
                }
            })
            .unwrap();
        for n in [1, 3] {
            assert_eq!(
                tree.read(tree.find(&format!("speaker-name-{n}"))),
                ("Button", "Name Ana Silva's speaker as someone else…")
            );
        }
        assert_eq!(
            tree.read(tree.find("speaker-name-4")),
            ("Button", "Name Speaker 4…")
        );
        assert_eq!(
            tree.read(tree.find("speaker-accept-2")),
            ("Button", "Accept José Pereira for Speaker 2")
        );
        assert_eq!(
            tree.read(tree.find("speaker-accept-3")),
            ("Button", "Accept Ana Silva, named automatically")
        );
        assert_eq!(
            tree.read(tree.find("speaker-restore-5")),
            ("Button", "Restore Speaker 5 to the review queue")
        );
        assert!(
            tree.all("speaker-dismiss-2").is_empty() && tree.all("speaker-dismiss-3").is_empty()
        );
        assert!(tree.all("speaker-name-5").is_empty());
    }

    #[gpui::test]
    fn metadata_uses_one_line_and_rows_align_in_talk_order(cx: &mut TestAppContext) {
        use broscribe_core::config::ReadingSize;
        let (handle, app, _, _, _dir) = open_edited(cx, |meeting, _| {
            meeting.speakers[1].talk_s = 42.0;
            meeting.speaker_groups.push(SpeakerGroup::new(
                sid(2),
                [sid(3)],
                GroupOrigin::Auto { score: 0.93 },
            ));
        });
        speakers_tab(&handle, cx);
        handle.update(cx, |view, _, _| {
            let rows = &view.data.as_ref().unwrap().speakers;
            assert_eq!(rows.iter().map(|row| row.id).collect::<Vec<_>>(), [sid(2), sid(1)]);
            assert_eq!(metadata(&rows[0], false), "52 s of speech · voice 0.70 · includes Speaker 3 · merged automatically · voice 0.93");
            let mut row = rows[0].clone();
            row.merged.as_mut().unwrap().origin = GroupOrigin::User;
            assert_eq!(metadata(&row, false), "52 s of speech · voice 0.70 · includes Speaker 3, merged by you");
        }).unwrap();
        let named = spoken_nodes(&handle, cx);
        assert_eq!(
            node(&named, "speaker-talk-2"),
            (
                Role::Label,
                "52 seconds of speech, voice 0.70, includes Speaker 3, merged automatically, voice 0.93"
            )
        );
        assert_eq!(
            node(&named, "speaker-talk-1"),
            (Role::Label, "30 seconds of speech")
        );
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        visual.simulate_resize(gpui::size(px(900.), px(950.)));
        for size in [ReadingSize::Standard, ReadingSize::Largest] {
            app.update(&mut visual, |app, cx| {
                let mut config = app.config().clone();
                config.appearance.reading_size = size;
                app.set_config(config, cx);
            });
            visual.update(draw_and_read);
            let a = meeting_control_bounds(&mut visual, "speaker-2");
            let b = meeting_control_bounds(&mut visual, "speaker-1");
            let panel = meeting_control_bounds(&mut visual, "speakers-scroll");
            let tabs = meeting_control_bounds(&mut visual, "meeting-tab-strip");
            assert_eq!((a.left(), a.right()), (b.left(), b.right()));
            assert_eq!(a.left() - panel.left(), tabs.left() - panel.left());
            assert!(a.top() < b.top());
        }
    }

    #[gpui::test]
    fn empty_tab_names_processing_and_final_empty_states(cx: &mut TestAppContext) {
        for (phase, words) in [
            (
                ProcessingStatus::Queued,
                "Speakers appear when the final transcript is ready.",
            ),
            (
                ProcessingStatus::Done,
                "No speakers were found in this recording.",
            ),
        ] {
            let (handle, _, _, _, _dir) = open_edited(cx, |meeting, _| {
                meeting.speakers.clear();
                meeting.segments.clear();
                meeting.processing.status = phase;
            });
            speakers_tab(&handle, cx);
            assert_eq!(
                node(&spoken_nodes(&handle, cx), "speakers-empty"),
                (Role::Status, words)
            );
        }
    }

    #[gpui::test]
    fn speakers_keep_playable_short_voices_and_disclose_only_missing_evidence(
        cx: &mut TestAppContext,
    ) {
        let (handle, _, lib, fixture, _dir) = open_edited(cx, |meeting, _| {
            meeting.speakers[0].talk_s = 0.5;
            meeting.segments[0].end = meeting.segments[0].start + 0.5;
            meeting
                .segments
                .retain(|segment| segment.speaker == Some(sid(1)));
            fixtures::set_voice(meeting, 3, &fixtures::voice(0.8));
            meeting.speakers[2].centroid.as_mut().unwrap().ranges = vec![[18., 18.5]];
        });
        let recording = lib.meeting_dir(&fixture.meeting).unwrap().join("audio.m4a");
        std::fs::write(&recording, b"synthetic").unwrap();
        lib.write(|tx| {
            let mut meeting = tx.library().load_meeting(&fixture.meeting)?;
            with_recording(&mut meeting, &fixture);
            tx.save_meeting(&meeting)
        })
        .unwrap();
        speakers_tab(&handle, cx);
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        visual.simulate_resize(gpui::size(px(900.), px(950.)));
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("speakers-count")).1,
            "2 speakers · 2 need a decision"
        );
        assert_eq!(
            tree.read(tree.find("speaker-talk-1")).1,
            "less than 1 second of speech"
        );
        for id in ["S1", "S3"] {
            assert!(
                tree.read(tree.find(&format!("sample-play-speaker-{id}")))
                    .1
                    .starts_with("Play")
            );
        }
        assert!(tree.all("speaker-2").is_empty());
        assert!(tree.all("speaker-without-evidence-2").is_empty());
        click_meeting_control(&mut visual, "speakers-without-evidence-toggle");
        let expanded = visual.update(draw_and_read);
        assert_eq!(
            expanded.read(expanded.find("speaker-without-evidence-2")).0,
            "ListItem"
        );

        handle
            .update(&mut visual, |view, _, cx| {
                view.data.as_mut().unwrap().audio = AudioSource::Deleted;
                cx.notify();
            })
            .unwrap();
        visual.update(draw_and_read);
        click_meeting_control(&mut visual, "speaker-transcript-1");
        handle
            .update(&mut visual, |view, _, _| {
                assert_eq!(view.tab, MeetingTab::Transcript);
                assert_eq!(view.transcript_cursor, view.transcript_row(0));
            })
            .unwrap();
    }

    #[gpui::test]
    fn sample_controls_have_stable_ids_and_row_local_status(cx: &mut TestAppContext) {
        let (handle, app, lib, fixture, _dir) = open_edited(cx, |meeting, people| {
            with_recording(meeting, people);
            meeting
                .segments
                .extend([(1, 24.), (2, 30.)].map(|(n, start)| Segment {
                    speech_overlap: Vec::new(),
                    id: n + 3,
                    track: "t0".into(),
                    speaker: Some(sid(n)),
                    start,
                    end: start + 2.0,
                    text: format!("sample {n}"),
                    words: Vec::new(),
                }));
            meeting
                .segments
                .retain(|segment| segment.speaker != Some(sid(3)));
        });
        let recording = lib.meeting_dir(&fixture.meeting).unwrap().join("audio.m4a");
        std::fs::write(&recording, b"synthetic").unwrap();
        lib.write(|tx| {
            let mut meeting = tx.library().load_meeting(&fixture.meeting)?;
            with_recording(&mut meeting, &fixture);
            tx.save_meeting(&meeting)
        })
        .unwrap();
        app.update(cx, |state, _| {
            let mut player = FakeAudioPlayer::new(60.0);
            player.native_extensions.clear();
            state.set_player(Box::new(player));
        });
        speakers_tab(&handle, cx);
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        let tree = visual.update(draw_and_read);
        let mut ids = std::collections::BTreeSet::new();
        for n in [1, 2] {
            for action in ["play", "previous", "next"] {
                let suffix = format!("-S{n}");
                let children = tree
                    .nodes
                    .iter()
                    .filter_map(|node| node.element_id.as_deref())
                    .filter(|id| {
                        id.starts_with(&format!("sample-{action}-")) && id.ends_with(&suffix)
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    children.len(),
                    1,
                    "sample {action}, speaker {n}: {:?}",
                    tree.nodes
                        .iter()
                        .filter_map(|node| node.element_id.as_deref())
                        .filter(|id| id.contains("sample"))
                        .collect::<Vec<_>>()
                );
                assert!(ids.insert(children[0]));
            }
        }
        assert_eq!(ids.len(), 6);
        assert!(tree.all("sample-play-speaker-S3").is_empty());
        assert!(tree.all("speaker-3").is_empty());
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|node| node.role == "Status" && node.name == "1 of 2")
                .count(),
            2
        );
        // Render immediately after start, while path resolution is still queued.
        handle
            .update(&mut visual, |view, window, cx| {
                view.toggle_sample(sid(2), cx);
                let named = crate::ui::spoken_log::while_building(|| {
                    view.speakers(view.data.as_ref().unwrap(), window, cx)
                })
                .1;
                assert_eq!(
                    node(&named, "speaker-sample-status-2"),
                    (Role::Status, "Loading…")
                );
                assert!(
                    named
                        .iter()
                        .all(|(id, _, _)| id != "speaker-sample-status-1")
                );
            })
            .unwrap();
        visual.run_until_parked();
        let tree = visual.update(draw_and_read);
        assert_eq!(tree.read(tree.find("speaker-sample-status-2")).0, "Alert");
        assert!(tree.all("speaker-sample-status-1").is_empty());
        for id in ids {
            assert_eq!(tree.all(id).len(), 1);
        }
        app.update(&mut visual, |state, _| {
            state.set_player(Box::new(FakeAudioPlayer::new(60.0)))
        });
        bind_and_focus(handle, &mut visual);
        for (key, position) in [("space", "1 of 2"), ("right", "2 of 2"), ("left", "1 of 2")] {
            visual.simulate_keystrokes(key);
            visual.run_until_parked();
            let tree = visual.update(draw_and_read);
            assert_eq!(
                tree.read(tree.find("sample-position-speaker-S1")),
                ("Status", position)
            );
            handle
                .update(&mut visual, |view, _, _| {
                    assert!(view.sample_playing(sid(1)))
                })
                .unwrap();
        }
        visual.simulate_keystrokes("space");
        handle
            .update(&mut visual, |view, _, _| {
                assert!(!view.sample_playing(sid(1)));
                assert!(!view.playback.playing);
            })
            .unwrap();
    }

    fn bind_and_focus(handle: gpui::WindowHandle<MeetingView>, visual: &mut VisualTestContext) {
        handle
            .update(visual, |view, window, cx| {
                let keys = crate::ui::keymap::key_bindings_for(HostPlatform::Windows, cx);
                cx.bind_keys(keys);
                view.speaker_list_focus.focus(window, cx);
            })
            .unwrap();
        visual.update(draw_and_read);
    }

    #[gpui::test]
    fn keyboard_selection_rejects_only_the_speaker_shown_and_persists(cx: &mut TestAppContext) {
        let (handle, _, lib, fixture, _dir) = open_edited(cx, |_, _| {});
        speakers_tab(&handle, cx);
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        bind_and_focus(handle, &mut visual);
        visual.simulate_keystrokes("down");
        handle
            .update(&mut visual, |view, window, _| {
                assert_eq!(view.selected_speaker, Some(sid(2)));
                assert!(view.speaker_list_focus.is_focused(window));
            })
            .unwrap();
        visual.update(draw_and_read);
        handle
            .update(&mut visual, |view, window, cx| {
                window.focus_next(cx);
                assert!(view.speaker_focus[&(sid(2), SpeakerControl::Accept)].is_focused(window));
                view.speaker_list_focus.focus(window, cx);
            })
            .unwrap();
        visual.simulate_keystrokes("r");
        visual.run_until_parked();
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("speaker-status-2")),
            (
                "Status",
                "Speaker 2: José Pereira rejected. BroScribe won't suggest them for this speaker again."
            )
        );
        let saved = lib.load_meeting(&fixture.meeting).unwrap();
        assert_eq!(
            saved.speaker(sid(2)).unwrap().rejected_person_ids,
            [fixture.jose]
        );
        assert!(
            saved
                .speaker(sid(1))
                .unwrap()
                .rejected_person_ids
                .is_empty()
        );
        assert!(tree.all("speaker-accept-2").is_empty());
        handle
            .update(&mut visual, |view, _, _| {
                assert_eq!(view.selected_speaker, Some(sid(2)))
            })
            .unwrap();
        visual.simulate_keystrokes("d");
        visual.run_until_parked();
        assert!(ops::review_dismissed(
            &lib.load_meeting(&fixture.meeting).unwrap(),
            sid(2)
        ));
        visual.update(draw_and_read);
        visual.simulate_keystrokes("enter");
        visual.run_until_parked();
        assert!(!ops::review_dismissed(
            &lib.load_meeting(&fixture.meeting).unwrap(),
            sid(2)
        ));
        visual.update(draw_and_read);
        visual.simulate_keystrokes("n");
        handle
            .update(&mut visual, |view, _, _| {
                assert_eq!(
                    view.naming.as_ref().and_then(NamingSession::speaker),
                    Some(sid(2))
                );
            })
            .unwrap();
        visual.update(draw_and_read);
        visual.simulate_keystrokes("escape");
        handle
            .update(&mut visual, |view, window, _| {
                assert!(view.naming.is_none());
                assert!(view.speaker_list_focus.is_focused(window));
            })
            .unwrap();
    }

    #[gpui::test]
    fn more_right_click_and_keyboard_share_one_merge_submenu(cx: &mut TestAppContext) {
        let (handle, _, _, _, _dir) = open_edited(cx, |_, _| {});
        speakers_tab(&handle, cx);
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        bind_and_focus(handle, &mut visual);
        visual.simulate_keystrokes("down");
        let commands = |visual: &mut VisualTestContext| {
            visual
                .update(draw_and_read)
                .nodes
                .into_iter()
                .filter(|node| node.role == "MenuItem" && !node.name.is_empty())
                .map(|node| node.name)
                .collect::<Vec<_>>()
        };
        for opener in ["shift-f10", "right-click", "more"] {
            match opener {
                "right-click" => {
                    let row = meeting_control_bounds(&mut visual, "speaker-2");
                    visual.simulate_mouse_down(
                        gpui::point(row.right() - px(6.), row.top() + px(3.)),
                        MouseButton::Right,
                        Default::default(),
                    );
                }
                "more" => click_meeting_control(&mut visual, "speaker-more-2"),
                chord => visual.simulate_keystrokes(chord),
            }
            assert_eq!(
                commands(&mut visual),
                ["Can't tell", "Merge into"],
                "{opener}"
            );
            for key in ["down", "down", "enter"] {
                visual.simulate_keystrokes(key);
                visual.update(draw_and_read);
            }
            let submenu = commands(&mut visual);
            assert!(
                submenu
                    .iter()
                    .any(|label| label == "Merge Speaker 2 into Speaker 1"),
                "{submenu:?}"
            );
            visual.simulate_keystrokes("escape");
            handle
                .update(&mut visual, |view, window, _| {
                    let expected = if opener == "more" {
                        &view.speaker_focus[&(sid(2), SpeakerControl::More)]
                    } else {
                        &view.speaker_list_focus
                    };
                    assert!(expected.is_focused(window), "{opener}");
                    assert!(view.speaker_menu.is_none());
                })
                .unwrap();
        }
    }

    #[gpui::test]
    fn recheck_dismisses_one_generation_and_reads_the_next_from_disk(cx: &mut TestAppContext) {
        use broscribe_core::learning;
        let (handle, app, lib, fixture, _dir) = open_edited(cx, |_, _| {});
        speakers_tab(&handle, cx);
        let model = fixtures::voice_model();
        let next = || {
            lib.write(|tx| learning::request(tx, &model, Some(&fixture.meeting)))
                .unwrap()
        };
        next();
        app.update(cx, |state, cx| state.refresh_recheck(cx));
        cx.run_until_parked();
        let mut visual = VisualTestContext::from_window(handle.into(), cx);
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("speaker-recheck")),
            ("Group", "Speaker recheck")
        );
        assert_eq!(tree.read(tree.find("speaker-recheck-line-0")).0, "Label");
        assert_eq!(
            tree.read(tree.find("speaker-recheck-hide")),
            ("Button", "Dismiss the speaker recheck's result")
        );
        click_meeting_control(&mut visual, "speaker-recheck-hide");
        assert!(
            visual
                .update(draw_and_read)
                .all("speaker-recheck")
                .is_empty()
        );
        next();
        app.update(&mut visual, |state, cx| state.refresh_recheck(cx));
        visual.run_until_parked();
        assert_eq!(visual.update(draw_and_read).all("speaker-recheck").len(), 1);
    }
}
