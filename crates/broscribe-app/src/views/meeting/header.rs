//! Meeting facts, header actions, processing feedback and playback transport.

use broscribe_core::queue::{
    IMPORT_KIND, MANIFEST_KIND, QueueSnapshot, RECORD_FINAL_KIND, RECORD_PROVISIONAL_KIND,
    REPROCESS_KIND,
};
use broscribe_core::schema::Source;
use gpui::assets::IconName;
use gpui::base::ElementExt as _;
use gpui::component::menu::DropdownMenu as _;

use super::*;
use crate::menu::{
    BackFifteenSecondsShortcut, ExportMeeting, ForwardFifteenSecondsShortcut,
    ToggleMeetingPlaybackShortcut,
};
use crate::ui::format::{
    count, language_name, length, local_date_time, progress_value, spoken_date_time,
};
use crate::ui::select::{OptionItem, Picker};

gpui::actions!(meeting_header, [RemoveMeetingTag]);

#[derive(Clone, Debug)]
pub(super) enum HeaderJob {
    Waiting(usize),
    Running,
}

fn transcribes(kind: &str) -> bool {
    matches!(
        kind,
        IMPORT_KIND | MANIFEST_KIND | RECORD_FINAL_KIND | RECORD_PROVISIONAL_KIND | REPROCESS_KIND
    )
}

pub(super) fn meeting_job(queue: &QueueSnapshot, meeting: &Meeting) -> Option<HeaderJob> {
    let matching = queue.jobs.iter().filter(|job| {
        transcribes(&job.kind)
            && (job.meeting_id().as_ref() == Some(&meeting.id)
                || (job.kind == RECORD_FINAL_KIND
                    && meeting.source == Source::Recording
                    && meeting.source_id.as_deref() == Some(job.key.as_str())))
    });
    // The current pass takes precedence over work still waiting to start.
    let mut waiting = None;
    for job in matching {
        if queue.is_waiting(job) {
            waiting.get_or_insert_with(|| HeaderJob::Waiting(queue.jobs_ahead(&job.id)));
        } else if matches!(job.state, JobState::Running | JobState::Analyzed) {
            return Some(HeaderJob::Running);
        }
    }
    waiting
}

/// Only changes that affect this view's status wording can repaint it.
#[derive(Default, PartialEq, Eq)]
pub(super) struct JobDisplay {
    processing: Option<Option<u8>>,
    preparing_cache: bool,
}

impl JobDisplay {
    pub(super) fn state_changed(&self, old: &Self) -> bool {
        self.processing.is_some() != old.processing.is_some()
            || self.preparing_cache != old.preparing_cache
    }
}

pub(super) fn header_facts(
    meeting: &Meeting,
    app: Option<&str>,
    audio: &AudioSource,
) -> (String, String) {
    let mut facts = vec![local_date_time(&meeting.started_at)];
    let mut label = vec![format!(
        "Started {} local time",
        spoken_date_time(&meeting.started_at)
    )];
    if meeting.duration_s > 0. {
        facts.push(length(meeting.duration_s));
        label.push(format!("{} long", spoken_length(meeting.duration_s)));
    }
    let mut add = |fact: &str| {
        facts.push(fact.to_owned());
        label.push(fact.to_owned());
    };
    if let Some(app) = app {
        add(app);
    }
    add(match meeting.source {
        Source::Recording => "Recorded",
        Source::Import => "Imported",
    });
    match audio {
        AudioSource::NoRecording => add("No recording"),
        AudioSource::Deleted => add("Audio deleted"),
        AudioSource::Missing { .. } => add("Recording missing"),
        AudioSource::Available { .. } => {}
    }
    let mut languages = Vec::new();
    for span in &meeting.languages {
        let name = language_name(&span.lang);
        if !languages.contains(&name) {
            languages.push(name);
        }
    }
    if !languages.is_empty() {
        add(&languages.join(", "));
    }
    match meeting.processing.status {
        ProcessingStatus::Queued => add("Queued"),
        ProcessingStatus::Analyzed => add("Identifying speakers…"),
        ProcessingStatus::Provisional => add("Draft transcript"),
        ProcessingStatus::Failed => add("Couldn't transcribe"),
        ProcessingStatus::Done => {}
    }
    (facts.join(" · "), label.join(", "))
}

fn waiting_text(ahead: usize) -> String {
    if ahead == 0 {
        "next".to_owned()
    } else {
        count(ahead as u64, "job ahead", "jobs ahead")
    }
}

fn display_path(path: &Path) -> String {
    if NativePlatform::host_platform() != broscribe_platform::HostPlatform::Windows
        && let Some(home) = std::env::home_dir()
        && let Ok(relative) = path.strip_prefix(home)
    {
        return Path::new("~").join(relative).display().to_string();
    }
    path.display().to_string()
}

impl MeetingView {
    pub(super) fn job_display(&self, cx: &App) -> JobDisplay {
        let jobs = &self.state.read(cx).jobs().active;
        JobDisplay {
            processing: jobs
                .iter()
                .find(|job| {
                    job.meeting_id.as_ref() == Some(&self.meeting_id)
                        && job.kind.as_deref().is_some_and(transcribes)
                })
                .map(|job| job.fraction.and_then(progress_value)),
            preparing_cache: self
                .playback
                .cache_job
                .as_ref()
                .is_some_and(|id| jobs.iter().any(|job| job.job_id == id.as_str())),
        }
    }

    pub(super) fn load_status(&self, cx: &Context<Self>) -> impl IntoElement {
        let (kind, role, text) = match &self.load_error {
            Some(text) if self.meeting_not_found => (StatusKind::Info, Role::Status, text.as_str()),
            Some(text) => (StatusKind::Failed, Role::Alert, text.as_str()),
            None => (StatusKind::Pending, Role::Status, "Loading the meeting…"),
        };
        div()
            .p_4()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                crate::ui::title("meeting-title", "Meeting").aria_level(if self.embedded {
                    2
                } else {
                    1
                }),
            )
            .child(kit::status_line_role(
                "meeting-status",
                kind,
                role,
                text.to_owned(),
                cx,
            ))
            .when(self.load_error.is_some(), |body| {
                body.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .when(!self.meeting_not_found, |row| {
                            row.child(
                                button(
                                    "meeting-load-retry",
                                    "Try again",
                                    true,
                                    cx,
                                    |this, window, cx| {
                                        this.load_error = None;
                                        this.reload(window, cx);
                                        cx.notify();
                                    },
                                )
                                .small(),
                            )
                        })
                        .child(
                            button(
                                "meeting-load-back",
                                "Back to Library",
                                true,
                                cx,
                                |this, _, cx| {
                                    this.state.update(cx, |state, cx| {
                                        state.navigate(Route::library(), cx)
                                    });
                                },
                            )
                            .small(),
                        ),
                )
            })
    }

    pub(super) fn processing_notice(
        &self,
        data: &MeetingData,
        cx: &Context<Self>,
    ) -> Option<impl IntoElement> {
        if !matches!(self.tab, MeetingTab::Overview | MeetingTab::Transcript) {
            return None;
        }
        let phase = data.meeting.processing.status;
        let live = self.job_display(cx).processing;
        let (kind, text) = match (phase, &data.processing_job, live) {
            (ProcessingStatus::Provisional, _, _) => (
                StatusKind::Attention,
                "Draft transcript · the final transcript replaces it when it’s ready".into(),
            ),
            (ProcessingStatus::Analyzed, _, _) => {
                (StatusKind::Pending, "Identifying speakers…".into())
            }
            (_, _, Some(Some(pct))) => (
                StatusKind::Pending,
                format!("Transcribing · {}", crate::ui::format::percent(pct)),
            ),
            (_, Some(HeaderJob::Running), _) | (_, _, Some(None)) => {
                (StatusKind::Pending, "Transcribing…".into())
            }
            (_, Some(HeaderJob::Waiting(ahead)), _) => (
                StatusKind::Pending,
                format!("Queued · {}", waiting_text(*ahead)),
            ),
            (ProcessingStatus::Queued, _, _) => (StatusKind::Pending, "Queued".into()),
            (ProcessingStatus::Failed, _, _) => {
                let cause = data
                    .meeting
                    .processing
                    .error
                    .as_deref()
                    .and_then(crate::jobs::stored_error)
                    .map_or_else(
                        || "The background job couldn't finish.".to_owned(),
                        |error| meeting_user_text(&error),
                    );
                (
                    StatusKind::Failed,
                    failure_text("Couldn't transcribe this meeting.", &cause, ""),
                )
            }
            (ProcessingStatus::Done, _, _) => return None,
        };
        let failed = kind == StatusKind::Failed;
        let can_retry = matches!(data.audio, AudioSource::Available { .. });
        Some(
            div()
                .mx_4()
                .my_2()
                .flex()
                .flex_col()
                .gap_1()
                .child(kit::status_line(
                    if failed {
                        "processing-failed"
                    } else {
                        "meeting-processing"
                    },
                    kind,
                    text,
                    cx,
                ))
                .children(self.edit_line(
                    &EditTarget::Processing,
                    "meeting-transcription-write",
                    cx,
                ))
                .when(failed && can_retry, |body| {
                    body.child(
                        button(
                            "processing-retry",
                            "Try again",
                            true,
                            cx,
                            Self::retry_transcription,
                        )
                        .small(),
                    )
                })
                .when(failed && !can_retry, |body| {
                    body.child(
                        text_line(
                            "processing-audio-gone",
                            Role::Note,
                            "The recording is gone, so this meeting can't be transcribed again.",
                        )
                        .text_role(TextRole::Caption),
                    )
                }),
        )
    }

    fn retry_transcription(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cli = self.state.read(cx).cli().clone();
        let args = vec![
            "reprocess".into(),
            self.meeting_id.as_str().into(),
            "--queue".into(),
        ];
        self.run_write_then(
            EditTarget::Processing,
            "Wait until transcription is queued.",
            window,
            cx,
            move |_| Ok(cli.run_blocking(&args, crate::jobs::ChildKind::Command)),
            |view, result, _, cx| match result {
                Ok(Ok(_)) => {
                    view.state.update(cx, |state, cx| state.kick_jobs(cx));
                    Notice::Info("Transcription queued.".to_owned())
                }
                Ok(Err(error)) => Notice::Error(failure_text(
                    "Couldn't queue transcription.",
                    &error.user_text(),
                    "",
                )),
                Err(error) => Notice::Error(failure_text(
                    "Couldn't queue transcription.",
                    &meeting_user_text(&error),
                    "",
                )),
            },
        );
    }

    fn more_menu(&self, narrow: bool, data: &MeetingData, cx: &Context<Self>) -> impl IntoElement {
        let reveal = matches!(data.audio, AudioSource::Available { .. });
        let copy = !data.lines.is_empty();
        let delete = self.delete.is_none() && self.export.is_none();
        let view = cx.weak_entity();
        Button::new("meeting-more")
            .label("More")
            .ghost()
            .small()
            .dropdown_caret(true)
            .track_focus(&self.delete_focus)
            .accessibility_label("More meeting actions")
            .dropdown_menu_with_anchor(gpui::Anchor::TopRight, move |menu, _, cx| {
                let action = |label, run: fn(&mut Self, &mut Window, &mut Context<Self>)| {
                    let view = view.clone();
                    PopupMenuItem::new(label).on_click(move |_, window, cx| {
                        let _ = view.update(cx, |this, cx| run(this, window, cx));
                    })
                };
                menu.when(narrow, |menu| {
                    menu.item(action("Export…", Self::present_export))
                })
                .item(action("Rename", |this, window, cx| {
                    this.start_rename_from(this.delete_focus.clone(), window, cx);
                }))
                .item(
                    action("Copy transcript", |this, _, cx| this.copy_transcript(cx))
                        .disabled(!copy),
                )
                .when(reveal, |menu| {
                    let view = view.clone();
                    let label = NativePlatform::host_platform().reveal_object_label("recording");
                    menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        if let Some(view) = view.upgrade() {
                            view.read(cx).reveal_recording(cx);
                        }
                    }))
                })
                .separator()
                .item(
                    action("Delete meeting", Self::present_delete)
                        .icon(crate::ui::icons::icon(IconName::Trash).text_color(cx.theme().danger))
                        .disabled(!delete),
                )
            })
    }

    pub(super) fn header(
        &self,
        data: &MeetingData,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let narrow = self.header_narrow;
        let meeting = &data.meeting;
        let (facts, spoken) = header_facts(meeting, data.app_name.as_deref(), &data.audio);
        let view = cx.weak_entity();
        let min = window.rem_size() * 40.;
        let can_save = self.busy.is_none() && self.rename_draft(cx).is_some();
        div()
            .id("meeting-header")
            .role(Role::Group)
            .aria_label("Meeting details")
            .flex()
            .flex_none()
            .flex_col()
            .gap_2()
            .p_4()
            .key_context("MeetingControls")
            .on_prepaint(move |bounds, window, cx| {
                let actual = bounds.size.width < min;
                if actual != narrow {
                    let view = view.clone();
                    window.defer(cx, move |_, cx| {
                        let _ = view.update(cx, |this, cx| {
                            if this.header_narrow != actual {
                                this.header_narrow = actual;
                                cx.notify();
                            }
                        });
                    });
                }
            })
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .map(|row| match &self.rename {
                        Some((input, _)) => row
                            .track_focus(&self.rename_form_focus)
                            .child(
                                div().flex_1().min_w_0().flex_basis(rems(12.)).child(
                                    Input::new(input)
                                        .text_role(TextRole::Title)
                                        .aria_label("Meeting title"),
                                ),
                            )
                            .child(
                                button("rename-save", "Save", can_save, cx, Self::submit_rename)
                                    .when_some(self.busy.as_ref(), |button, (_, reason)| {
                                        button.tooltip(*reason)
                                    })
                                    .track_focus(&self.rename_save_focus)
                                    .map(|button| kit::primary(button, !can_save, cx))
                                    .small(),
                            )
                            .child(
                                button("rename-cancel", "Cancel", true, cx, Self::cancel_rename)
                                    .outline()
                                    .small(),
                            ),
                        None => row.child(
                            crate::ui::title("meeting-title", shown_title(meeting))
                                .aria_level(if self.embedded { 2 } else { 1 })
                                .track_focus(&self.title_focus.clone().tab_index(-1))
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(
                                        |this, event: &gpui::MouseDownEvent, window, cx| {
                                            if event.click_count == 2 {
                                                this.start_rename(window, cx);
                                                cx.stop_propagation();
                                            }
                                        },
                                    ),
                                )
                                .flex_1()
                                .min_w_0()
                                .flex_basis(rems(12.))
                                .map(|title| {
                                    #[cfg(test)]
                                    let title = gpui::base::TestSupportExt::test_support(title);
                                    title
                                }),
                        ),
                    })
                    .when(!narrow, |row| {
                        row.child(
                            button("export-open", "Export…", true, cx, Self::present_export)
                                .outline()
                                .small()
                                .track_focus(&self.export_focus)
                                .tooltip_with_action("Export meeting", &ExportMeeting, None),
                        )
                    })
                    .child(self.more_menu(narrow, data, cx)),
            )
            .children(self.edit_line(&EditTarget::Title, "meeting-title-status", cx))
            .when_some(self.export_result.as_ref(), |header, path| {
                let file = path.file_name().unwrap_or_default().to_string_lossy();
                let path = path.clone();
                header.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(kit::status_line(
                            "meeting-exported",
                            StatusKind::Done,
                            format!("Exported “{file}”."),
                            cx,
                        ))
                        .child(
                            Button::new("show-export")
                                .label(NativePlatform::host_platform().reveal_label())
                                .ghost()
                                .small()
                                .on_click(move |_, _, cx| {
                                    crate::views::export::reveal_file(&path, cx)
                                }),
                        ),
                )
            })
            .when(
                self.transcript_copy_status == Some(saved_transcript::CopyStatus::Transcript),
                |header| {
                    header.child(kit::status_line(
                        "meeting-copy-status",
                        StatusKind::Done,
                        "Transcript copied.",
                        cx,
                    ))
                },
            )
            .child(
                div()
                    .id("meeting-facts")
                    .spoken(Role::Label, spoken)
                    .text_role(TextRole::Caption)
                    .text_color(theme.muted_foreground)
                    .child(facts),
            )
            .children(meeting.language_effective.as_deref().map(|language| {
                text_line(
                    "meeting-detected-language",
                    Role::Label,
                    format!("Detected language: {}", language_name(language)),
                )
                .text_role(TextRole::Caption)
                .text_color(theme.muted_foreground)
            }))
            .when(matches!(data.audio, AudioSource::Deleted), |header| {
                header.child(
                    text_line(
                        "audio-deleted-note",
                        Role::Note,
                        "The transcript and speakers remain.",
                    )
                    .text_role(TextRole::Caption)
                    .text_color(theme.muted_foreground),
                )
            })
            .child(
                div()
                    .id("meeting-people-tags")
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x_3()
                    .gap_y_2()
                    .when(!data.participants.is_empty(), |row| {
                        row.child(self.participants(data, cx))
                    })
                    .child(self.tags(meeting, cx)),
            )
            .children(self.playback_bar(data, cx))
            .children(self.recording_notice(data, cx))
    }

    fn participants(&self, data: &MeetingData, cx: &Context<Self>) -> impl IntoElement {
        let count = data.participants.len();
        let cap = if self.header_narrow { 3 } else { 6 };
        div()
            .id("participants")
            .role(Role::List)
            .aria_label("Meeting participants")
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .child(
                text_line("participants-label", Role::Label, "Participants")
                    .text_role(TextRole::Caption)
                    .text_color(cx.theme().muted_foreground),
            )
            .children(
                data.participants
                    .iter()
                    .take(if self.participants_expanded {
                        count
                    } else {
                        cap
                    })
                    .enumerate()
                    .map(|(ix, participant)| {
                        div()
                            .id(("participant-row", ix))
                            .role(Role::ListItem)
                            .aria_label(format!("Participant {}", ix + 1))
                            .aria_position_in_set(ix + 1)
                            .aria_size_of_set(count)
                            .child(if let Some(person) = &participant.person {
                                let person = person.clone();
                                button(
                                    ("participant", ix),
                                    participant.name.clone(),
                                    true,
                                    cx,
                                    move |this, _, cx| {
                                        this.state.update(cx, |state, cx| {
                                            state.navigate(
                                                Route::People {
                                                    person: Some(person.clone()),
                                                },
                                                cx,
                                            )
                                        });
                                    },
                                )
                                .ghost()
                                .xsmall()
                                .accessibility_label(format!("Open {} in People", participant.name))
                                .into_any_element()
                            } else {
                                div()
                                    .id(("participant", ix))
                                    .spoken(Role::Label, participant.name.clone())
                                    .child(self.chip(participant.name.clone(), cx))
                                    .into_any_element()
                            })
                    }),
            )
            .when(count > cap, |row| {
                let remaining = count - cap;
                let (label, spoken) = if self.participants_expanded {
                    (
                        "Show fewer".to_owned(),
                        "Show fewer participants".to_owned(),
                    )
                } else {
                    (
                        format!("+{remaining} more"),
                        format!("Show {remaining} more participants"),
                    )
                };
                row.child(
                    button("participants-more", label, true, cx, |this, _, cx| {
                        this.participants_expanded = !this.participants_expanded;
                        cx.notify();
                    })
                    .ghost()
                    .xsmall()
                    .accessibility_label(spoken),
                )
            })
    }

    fn tags(&self, meeting: &Meeting, cx: &Context<Self>) -> impl IntoElement {
        div()
            .id("tags")
            .role(Role::Group)
            .aria_label("Meeting tags")
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                text_line("tags-label", Role::Label, "Tags")
                    .text_role(TextRole::Caption)
                    .text_color(cx.theme().muted_foreground),
            )
            .children(
                meeting
                    .effective_tags()
                    .into_iter()
                    .enumerate()
                    .map(|(ix, tag)| {
                        let click_tag = tag.clone();
                        let key_tag = tag.clone();
                        let suggestion = !meeting.tags.contains(&tag);
                        let remove = Button::new(("tag-remove", ix))
                            .ghost()
                            .xsmall()
                            .icon(crate::ui::icons::DISMISS)
                            .accessibility_label(format!("Remove tag {tag}"))
                            .tooltip("Remove tag")
                            .track_focus(&self.tag_focus[&tag])
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.remove_tag(click_tag.clone(), window, cx)
                            }))
                            .key_context("MeetingTag")
                            .on_action(cx.listener(
                                move |this, _: &RemoveMeetingTag, window, cx| {
                                    this.remove_tag(key_tag.clone(), window, cx);
                                },
                            ));
                        kit::tag_chip(tag.clone(), cx)
                            .id(("tag", ix))
                            .spoken(Role::Label, format!("Tag: {tag}"))
                            .when(suggestion, |chip| {
                                chip.tooltip(|window, cx| {
                                    Tooltip::new("Suggested by the summary").build(window, cx)
                                })
                            })
                            .child(remove)
                    }),
            )
            .child(if self.tag_open {
                let suggestions = self.tag_suggestions(cx);
                div()
                    .w(rems(10.))
                    .max_w_full()
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        let count = this.tag_suggestions(cx).len();
                        if count > 0 {
                            match event.keystroke.key.as_str() {
                                "up" => {
                                    this.tag_choice = Some(
                                        this.tag_choice
                                            .map_or(count - 1, |index| (index + count - 1) % count),
                                    )
                                }
                                "down" => {
                                    this.tag_choice =
                                        Some(this.tag_choice.map_or(0, |index| (index + 1) % count))
                                }
                                _ => return,
                            }
                            cx.stop_propagation();
                            cx.notify();
                        }
                    }))
                    .child(
                        gpui::base::Select::new("meeting-tag-combobox")
                            .open(!suggestions.is_empty())
                            .accessibility_label("Add tag")
                            .content_focus_handle(&self.tag_input.read(cx).focus_handle(cx))
                            .on_dismiss({
                                let view = cx.weak_entity();
                                move |window, cx| {
                                    let _ = view.update(cx, |view, cx| view.close_tag(window, cx));
                                }
                            })
                            .flex()
                            .flex_col()
                            .child(Input::new(&self.tag_input).aria_label("Tag"))
                            .when(!suggestions.is_empty(), |field| {
                                field.child(
                                    div()
                                        .id("meeting-tag-suggestions")
                                        .role(Role::ListBox)
                                        .aria_label("Suggested tags")
                                        .flex()
                                        .flex_col()
                                        .children(suggestions.into_iter().enumerate().map(
                                            |(index, tag)| {
                                                let tag = tag.to_owned();
                                                kit::list_row(
                                                    ("suggested-tag", index),
                                                    self.tag_choice == Some(index),
                                                    false,
                                                    cx,
                                                )
                                                .role(Role::ListBoxOption)
                                                .aria_label(tag.clone())
                                                .aria_selected(self.tag_choice == Some(index))
                                                .child(tag.clone())
                                                .on_click(cx.listener(
                                                    move |this, _, window, cx| {
                                                        this.choose_tag(&tag, window, cx)
                                                    },
                                                ))
                                            },
                                        )),
                                )
                            }),
                    )
                    .into_any_element()
            } else {
                button("meeting-add-tag", "Add tag", true, cx, Self::open_tag)
                    .ghost()
                    .xsmall()
                    .track_focus(&self.add_tag_focus)
                    .icon(IconName::TagPlus)
                    .into_any_element()
            })
            .children(self.edit_line(&EditTarget::Tags, "meeting-tags-status", cx))
    }

    fn recording_notice(&self, data: &MeetingData, cx: &Context<Self>) -> Option<AnyElement> {
        if let AudioSource::Missing { path } = &data.audio {
            let text = path.as_ref().map_or_else(
                || "Recording missing.".to_owned(),
                |path| {
                    format!(
                        "Recording missing. BroScribe expects it at {}.",
                        display_path(path)
                    )
                },
            );
            let folder = data.missing_folder.clone();
            return Some(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(kit::status_line_role(
                        "audio-missing",
                        StatusKind::Attention,
                        Role::Alert,
                        text,
                        cx,
                    ))
                    .children(folder.map(|folder| {
                        button(
                            "recording-show-folder",
                            NativePlatform::host_platform().reveal_object_label("folder"),
                            true,
                            cx,
                            move |_, _, cx| crate::views::export::reveal_file(&folder, cx),
                        )
                        .small()
                    }))
                    .child(
                        button(
                            "recording-check-again",
                            "Check again",
                            true,
                            cx,
                            |this, window, cx| this.reload(window, cx),
                        )
                        .small(),
                    )
                    .into_any_element(),
            );
        }
        if !matches!(data.audio, AudioSource::Available { .. }) {
            return None;
        }
        if let Some(cause) = &self.playback.cache_failed {
            let text = failure_text(
                "Couldn't prepare this recording for playback.",
                &sentence(cause),
                "",
            );
            return Some(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(kit::status_line(
                        "audio-unsupported",
                        StatusKind::Failed,
                        text,
                        cx,
                    ))
                    .child(
                        button(
                            "audio-cache-retry",
                            "Try again",
                            true,
                            cx,
                            |this, window, cx| {
                                this.play_from(this.playback.position_s, window, cx);
                            },
                        )
                        .small(),
                    )
                    .into_any_element(),
            );
        }
        self.playback.error.as_ref().map(|cause| {
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .child(kit::status_line(
                    "audio-error",
                    StatusKind::Failed,
                    playback_failure(&sentence(cause)),
                    cx,
                ))
                .child(
                    button(
                        "audio-playback-retry",
                        "Try again",
                        true,
                        cx,
                        |this, window, cx| {
                            this.play_from(this.playback.position_s, window, cx);
                        },
                    )
                    .small(),
                )
                .into_any_element()
        })
    }

    fn playback_state_label(&self, data: &MeetingData, cx: &App) -> Option<String> {
        if self.playback.pending_start.is_some() {
            if !self.job_display(cx).preparing_cache
                && let Some(ahead) = data.cache_ahead
            {
                return Some(format!(
                    "Preparing audio for playback · {}",
                    waiting_text(ahead)
                ));
            }
            Some("Preparing audio for playback…".into())
        } else if self.playback.preparing {
            Some("Opening the recording…".into())
        } else if self.playback.loaded && self.playback.playing {
            Some("Playing".into())
        } else if self.playback.loaded {
            Some("Paused".into())
        } else if self.playback.finished {
            Some("Finished".into())
        } else {
            None
        }
    }

    fn playback_bar(&self, data: &MeetingData, cx: &Context<Self>) -> Option<impl IntoElement> {
        if !matches!(data.audio, AudioSource::Available { .. }) {
            return None;
        }
        let starting = self.playback.pending_start.is_some() || self.playback.preparing;
        let (label, icon) = if starting {
            ("Cancel", crate::ui::icons::STOP)
        } else if self.playback.playing {
            ("Pause", crate::ui::icons::PAUSE)
        } else {
            ("Play", crate::ui::icons::PLAY)
        };
        let narrow = self.header_narrow;
        let position = self.scrub.unwrap_or(self.playback.position_s);
        let elapsed = format_clock(position);
        let position_text = if narrow {
            elapsed
        } else {
            format!("{elapsed} / {}", format_clock(data.meeting.duration_s))
        };
        let skip = |id, label, delta, icon, action: &dyn gpui::Action| {
            button(id, "15 s", !starting, cx, move |this, window, cx| {
                this.seek_relative(delta, window, cx)
            })
            .ghost()
            .small()
            .icon(icon)
            .accessibility_label(label)
            .tooltip_with_action(label, action, None)
        };
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .id("playback-bar")
                        .role(Role::Toolbar)
                        .aria_label("Playback")
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .min_h(rems(2.))
                        .child(
                            button("play-pause", label, true, cx, Self::toggle_playback)
                                .small()
                                .min_w(rems(6.))
                                .icon(icon)
                                .tooltip_with_action(label, &ToggleMeetingPlaybackShortcut, None),
                        )
                        .when(!narrow, |row| {
                            row.child(skip(
                                "playback-back",
                                "Back 15 seconds",
                                -15.,
                                IconName::RotateCcw,
                                &BackFifteenSecondsShortcut,
                            ))
                            .child(skip(
                                "playback-forward",
                                "Forward 15 seconds",
                                15.,
                                IconName::RotateCw,
                                &ForwardFifteenSecondsShortcut,
                            ))
                        })
                        .child(
                            div().w(rems(4.5)).flex_none().child(Picker::new(
                                "playback-speed",
                                "Playback speed",
                                ["0.75", "1", "1.25", "1.5", "1.75", "2"]
                                    .into_iter()
                                    .map(|rate| OptionItem::new(rate, format!("{rate}×")))
                                    .collect(),
                                self.state.read(cx).playback_rate.to_string(),
                                cx,
                                |this, value, _, cx| {
                                    if let Ok(rate) = value.parse() {
                                        this.set_playback_rate(rate, cx);
                                    }
                                },
                            )),
                        )
                        .children(self.slider.as_ref().map(|(slider, ..)| {
                            let duration = data.meeting.duration_s;
                            div()
                                .flex_1()
                                .min_w(rems(5.))
                                .relative()
                                .child(
                                    Slider::new(slider)
                                        .horizontal()
                                        .accessibility_label("Playback position"),
                                )
                                .when(duration > 0., |slider| {
                                    slider.child(
                                        div().absolute().left_2().right_2().bottom_0().children(
                                            self.timeline.iter().enumerate().map(
                                                |(index, marker)| {
                                                    let tick = div()
                                                        .id(("scrub-marker", index))
                                                        .absolute()
                                                        .left(gpui::relative(
                                                            (marker.at / duration).clamp(0., 1.)
                                                                as f32,
                                                        ))
                                                        .bottom_0()
                                                        .w(px(2.))
                                                        .h(rems(0.5))
                                                        .bg(cx.theme().foreground);
                                                    #[cfg(test)]
                                                    let tick =
                                                        gpui::base::TestSupportExt::test_support(
                                                            tick,
                                                        );
                                                    tick
                                                },
                                            ),
                                        ),
                                    )
                                })
                        }))
                        .child(
                            tabular(div())
                                .id("playback-position")
                                .spoken(
                                    Role::Label,
                                    format!(
                                        "{} of {}",
                                        spoken_position(position),
                                        spoken_position(data.meeting.duration_s)
                                    ),
                                )
                                .child(position_text)
                                .text_role(TextRole::Caption)
                                .text_color(cx.theme().muted_foreground),
                        ),
                )
                .children(self.playback_state_label(data, cx).map(|state| {
                    text_line("playback-state", Role::Status, state)
                        .text_role(TextRole::Caption)
                        .text_color(cx.theme().muted_foreground)
                })),
        )
    }
}
