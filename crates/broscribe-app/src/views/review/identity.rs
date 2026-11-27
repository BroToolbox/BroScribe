//! Identify recurring voices and compare duplicate profiles in the review queue.
use std::collections::{HashMap, HashSet};

use broscribe_core::config::Thresholds;
use broscribe_core::ops::{
    IdentifyBatch, IdentityGroup, IdentityOccurrence, MergePeople, PersonDuplicate, PersonPair,
};
use gpui::component::checkbox::Checkbox;
use gpui::{Div, base::Disableable as _};

use super::*;
use crate::views::people::{self, Clip};

#[derive(Default)]
pub(super) struct Snapshot {
    pub groups: HashMap<RowKey, Group>,
    pub duplicates: Vec<PersonDuplicate>,
    pub pending: Option<MergePeople>,
    pub group_error: Option<String>,
    pub duplicate_error: Option<String>,
}

pub(super) struct Group {
    members: Vec<(IdentityOccurrence, Option<Clip>)>,
    meetings: usize,
}

impl Group {
    pub(super) fn caption(&self) -> String {
        if self.meetings == 1 {
            format!("{} matching speakers", self.members.len())
        } else {
            format!("{} meetings", self.meetings)
        }
    }
}

impl Snapshot {
    pub fn apply_groups(&mut self, groups: Result<Vec<IdentityGroup>>, rows: &mut Vec<ReviewRow>) {
        let groups = match groups {
            Ok(groups) => groups,
            Err(e) => {
                self.group_error = Some(core_user_text(&e));
                return;
            }
        };
        let mut ranks = HashMap::new();
        let mut folded = HashSet::new();
        let clips: HashMap<_, _> = rows
            .iter()
            .map(|row| {
                let clip = row
                    .source
                    .as_ref()
                    .zip(row.samples.first())
                    .map(|(source, sample)| clip_of(source, sample));
                (row.key(), clip)
            })
            .collect();
        for (rank, group) in groups.into_iter().enumerate() {
            let mut members = group.occurrences.into_iter().filter_map(|occurrence| {
                let key = (occurrence.meeting_id.clone(), occurrence.speaker_id);
                let clip = clips.get(&key)?.clone();
                Some((occurrence, clip))
            });
            let Some(first) = members.next() else {
                continue;
            };
            let key = (first.0.meeting_id.clone(), first.0.speaker_id);
            ranks.insert(key.clone(), rank);
            let members: Vec<_> = std::iter::once(first).chain(members).collect();
            if members.len() > 1 {
                folded.extend(
                    members
                        .iter()
                        .skip(1)
                        .map(|(o, _)| (o.meeting_id.clone(), o.speaker_id)),
                );
                self.groups.insert(
                    key,
                    Group {
                        members,
                        meetings: group.meeting_count,
                    },
                );
            }
        }
        rows.retain(|row| !folded.contains(&row.key()));
        rows.sort_by_key(|row| ranks.get(&row.key()).copied().unwrap_or(usize::MAX));
    }

    pub fn load_duplicates(&mut self, lib: &Library, model: &str, thresholds: Option<&Thresholds>) {
        let result = ops::pending_person_merge(lib).and_then(|pending| {
            self.pending = pending;
            ops::person_duplicates(lib, model, thresholds)
        });
        match result {
            Ok(duplicates) => self.duplicates = duplicates,
            Err(e) => self.duplicate_error = Some(core_user_text(&e)),
        }
    }
}

pub(super) struct Identification {
    key: RowKey,
    choice: PersonChoice,
    label: String,
    members: Vec<(IdentityOccurrence, Option<Clip>)>,
    chosen: Vec<bool>,
}

#[derive(Default)]
pub(super) struct ReviewIdentity {
    pub snapshot: Snapshot,
    pub show_duplicates: bool,
    pub saving: bool,
    pub identify: Option<Identification>,
    undo_edits: Vec<HistoryEntry>,
    redo_edits: Vec<HistoryEntry>,
    merge: Option<(PersonPair, PersonId)>,
}

impl ReviewIdentity {
    pub fn clear_preview(&mut self) {
        self.identify = None;
        self.merge = None;
    }

    pub fn duplicate_summary(&self, loading: bool) -> String {
        if let Some(cause) = &self.snapshot.duplicate_error {
            return failure_text("Couldn't compare people.", cause, "");
        }
        if loading {
            return "Comparing voices…".into();
        }
        crate::ui::format::count(
            self.snapshot.duplicates.len() as u64,
            "possible duplicate",
            "possible duplicates",
        )
    }
}

#[derive(Clone)]
struct HistoryEntry {
    label: &'static str,
    edit: ops::ReviewEdit,
}

impl ReviewIdentity {
    pub(super) fn can_replay(&self, redo: bool) -> bool {
        !self.saving
            && if redo {
                !self.redo_edits.is_empty()
            } else {
                !self.undo_edits.is_empty()
            }
    }

    pub(super) fn remember(&mut self, label: &'static str, edit: Option<ops::ReviewEdit>) {
        self.redo_edits.clear();
        if let Some(edit) = edit {
            if self.undo_edits.len() == 20 {
                self.undo_edits.remove(0);
            }
            self.undo_edits.push(HistoryEntry { label, edit });
        }
    }
}

enum Write {
    Identify(IdentifyBatch),
    Merge(MergePeople),
    Separate(PersonPair),
}

impl ReviewView {
    pub(super) fn show_duplicates(&mut self, cx: &mut Context<Self>) {
        if !self.identity.show_duplicates {
            self.identity.show_duplicates = true;
            self.identity.clear_preview();
            self.playback.stop(&self.state, cx);
            self.end_naming(cx);
            self.row_popup = None;
            self.scroll.set_offset(gpui::Point::default());
            cx.notify();
        }
    }

    pub(super) fn preview_identification(
        &mut self,
        key: RowKey,
        choice: PersonChoice,
        name: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = self.identity.snapshot.groups.get(&key) else {
            return;
        };
        let (label, person) = match &choice {
            PersonChoice::Typed(name) => (name.clone(), None),
            PersonChoice::Existing(id) => {
                let label = self
                    .rows
                    .iter()
                    .find(|row| row.key() == key)
                    .filter(|row| accept_target(&row.entry.item) == Some(id))
                    .and_then(|row| proposal(&row.entry.item))
                    .map(|(_, name, _)| name.to_owned());
                (label.unwrap_or_default(), Some(id))
            }
        };
        let chosen = group
            .members
            .iter()
            .map(|(o, _)| o.allows(person))
            .collect();
        self.identity.identify = Some(Identification {
            key,
            choice,
            label: name.unwrap_or(label),
            members: group.members.clone(),
            chosen,
        });
        cx.notify();
    }

    #[cfg(test)]
    pub(super) fn select_identification(&mut self, key: &RowKey) {
        let preview = self
            .identity
            .identify
            .as_mut()
            .expect("identification preview");
        for ((occurrence, _), chosen) in preview.members.iter().zip(&mut preview.chosen) {
            *chosen = occurrence.meeting_id == key.0 && occurrence.speaker_id == key.1;
        }
    }

    pub(super) fn save_identification(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = self.identity.identify.as_ref() else {
            return;
        };
        let occurrences = preview
            .members
            .iter()
            .zip(&preview.chosen)
            .filter(|(_, chosen)| **chosen)
            .map(|((occurrence, _), _)| occurrence.clone())
            .collect();
        self.write_identity(
            Write::Identify(IdentifyBatch {
                person: preview.choice.person_ref().into(),
                occurrences,
            }),
            cx,
        );
    }

    fn write_identity(&mut self, write: Write, cx: &mut Context<Self>) {
        if self.saving.is_some() || self.identity.saving {
            self.wait_for_write(cx);
            return;
        }
        self.identity.saving = true;
        self.message = None;
        self.undo = None;
        self.playback.stop(&self.state, cx);
        let app = self.state.read(cx);
        let lib = app.library().clone();
        let model = app.config().profile.speaker_embedding.clone();
        let thresholds = app.config().thresholds(&model).copied();
        let focus = match &write {
            Write::Identify(batch) => batch.occurrences.first().map(|occurrence| {
                (
                    occurrence.meeting_id.clone(),
                    meeting_title(occurrence.meeting_title.as_deref()).to_owned(),
                )
            }),
            Write::Merge(_) | Write::Separate(_) => None,
        };
        let task = cx.background_executor().spawn(async move {
            let thresholds = thresholds.as_ref();
            ops::with_index(&lib, |tx, index| {
                ops::record_review_edit(tx, index, &model, |tx, index| match write {
                    Write::Identify(batch) => {
                        ops::identify_batch(tx, index, &batch, &model, thresholds).map(|result| {
                            let meetings = crate::ui::format::count(
                                result.meeting_count as u64,
                                "meeting",
                                "meetings",
                            );
                            let learning = result.named.last().map(|named| named.learning.clone());
                            (
                                "identification",
                                format!(
                                    "Linked {} in {meetings}. Checking other speakers.",
                                    result.person_name
                                ),
                                learning,
                                true,
                            )
                        })
                    }
                    Write::Merge(spec) => ops::merge_people(tx, index, &spec, &model, thresholds)
                        .map(|result| {
                            let meetings = crate::ui::format::count(
                                result.meetings_changed as u64,
                                "meeting",
                                "meetings",
                            );
                            (
                                "profile merge",
                                format!("Profiles merged. Updated {meetings}."),
                                None,
                                true,
                            )
                        }),
                    Write::Separate(pair) => ops::dismiss_person_duplicate(tx, &pair).map(|()| {
                        (
                            "different people decision",
                            "Marked as different people.".into(),
                            None,
                            false,
                        )
                    }),
                })
            })
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            view.update(cx, |view, cx| {
                view.identity.saving = false;
                match result {
                    Ok(((label, message, learning, changed), edit)) => {
                        view.identity.remember(label, edit);
                        view.identity.clear_preview();
                        view.message = Some(message);
                        view.message_kind = StatusKind::Done;
                        if let Some(focus) = focus {
                            view.recheck_focus = Some(focus);
                            view.recheck_hidden = None;
                        }
                        view.state.update(cx, |state, cx| {
                            if let Some(request) = learning {
                                state.recheck_requested(request, cx);
                            } else if changed {
                                state.kick_jobs(cx);
                            }
                            state.refresh_review_count(cx);
                        });
                    }
                    Err(e) => {
                        view.message_kind = StatusKind::Failed;
                        view.message = Some(failure_text(
                            "Couldn't save the decision.",
                            &core_user_text(&e),
                            "",
                        ));
                    }
                }
                view.reload(cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub(super) fn replay_identity(&mut self, redo: bool, cx: &mut Context<Self>) {
        if self.saving.is_some() || self.identity.saving {
            self.wait_for_write(cx);
            return;
        }
        let from = if redo {
            &mut self.identity.redo_edits
        } else {
            &mut self.identity.undo_edits
        };
        let Some(mut entry) = from.pop() else {
            if !redo && let Some(entry) = self.latest_live_naming.clone() {
                self.undo_recording_name(entry, cx);
            }
            return;
        };
        self.identity.saving = true;
        self.identity.clear_preview();
        self.end_naming(cx);
        self.playback.stop(&self.state, cx);
        self.message_kind = StatusKind::Pending;
        self.message = Some(if redo { "Redoing…" } else { "Undoing…" }.into());
        let lib = self.state.read(cx).library().clone();
        let task = cx.background_executor().spawn(async move {
            let result = entry.edit.replay(&lib, redo);
            (entry, result)
        });
        cx.spawn(async move |view, cx| {
            let (entry, result) = task.await;
            view.update(cx, |view, cx| {
                view.identity.saving = false;
                let label = entry.label;
                match result {
                    Ok(()) => {
                        let to = if redo {
                            &mut view.identity.undo_edits
                        } else {
                            &mut view.identity.redo_edits
                        };
                        to.push(entry);
                        view.message_kind = StatusKind::Done;
                        view.message = Some(if redo {
                            format!("Redid the {label}.")
                        } else {
                            format!("Undid the {label}.")
                        });
                        view.state.update(cx, |state, cx| {
                            state.kick_jobs(cx);
                            state.refresh_recheck(cx);
                            state.refresh_review_count(cx);
                        });
                    }
                    Err(e) => {
                        if e.code() != "identity_edit_changed" {
                            let from = if redo {
                                &mut view.identity.redo_edits
                            } else {
                                &mut view.identity.undo_edits
                            };
                            from.push(entry);
                        }
                        view.message_kind = StatusKind::Failed;
                        view.message = Some(failure_text(
                            if redo {
                                "Couldn't redo the decision."
                            } else {
                                "Couldn't undo the decision."
                            },
                            &core_user_text(&e),
                            "",
                        ));
                    }
                }
                view.reload(cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn undo_recording_name(&mut self, entry: super::RecordedSpeakerNaming, cx: &mut Context<Self>) {
        self.identity.saving = true;
        self.message_kind = StatusKind::Pending;
        self.message = Some("Undoing…".into());
        let lib = self.state.read(cx).library().clone();
        let task = cx.background_executor().spawn(async move {
            ops::with_index(&lib, |tx, index| {
                ops::undo_live_promotion(tx, index, &entry.meeting_id, &entry.naming)
            })
        });
        cx.spawn(async move |view, cx| {
            let result = task.await;
            view.update(cx, |view, cx| {
                view.identity.saving = false;
                let (kind, text) = match result {
                    Ok(()) => {
                        view.state.update(cx, |state, cx| {
                            state.kick_jobs(cx);
                            state.refresh_recheck(cx);
                            state.refresh_review_count(cx);
                        });
                        (
                            StatusKind::Done,
                            "Undid the name given while recording.".into(),
                        )
                    }
                    Err(error) => (
                        StatusKind::Failed,
                        failure_text("Couldn't undo the decision.", &core_user_text(&error), ""),
                    ),
                };
                view.message_kind = kind;
                view.message = Some(text);
                view.reload(cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    pub(super) fn render_identity_history(&self, cx: &Context<Self>) -> Div {
        let button_for = |redo: bool, id, label: &'static str, entry: Option<&HistoryEntry>| {
            let live = !redo && entry.is_none() && self.latest_live_naming.is_some();
            let enabled = self.saving.is_none()
                && !self.identity.saving
                && (self.identity.can_replay(redo) || live);
            let action = if live {
                "Undo name given while recording".to_owned()
            } else {
                entry.map_or_else(
                    || label.to_owned(),
                    |entry| format!("{label} {}", entry.label),
                )
            };
            let hint = if entry.is_some() || live {
                action.clone()
            } else if redo {
                "No decisions to redo".to_owned()
            } else {
                "No decisions to undo".to_owned()
            };
            button(id, label, enabled, cx, move |this: &mut Self, _, cx| {
                this.replay_identity(redo, cx)
            })
            .small()
            .outline()
            .accessibility_label(action)
            .tooltip(hint)
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(button_for(
                false,
                "review-identity-undo",
                "Undo",
                self.identity.undo_edits.last(),
            ))
            .child(button_for(
                true,
                "review-identity-redo",
                "Redo",
                self.identity.redo_edits.last(),
            ))
    }

    pub(super) fn render_identity_preview(&self, key: &RowKey, cx: &Context<Self>) -> Option<Div> {
        let preview = self
            .identity
            .identify
            .as_ref()
            .filter(|preview| &preview.key == key)?;
        let enabled = self.saving.is_none() && !self.identity.saving;
        let meetings = preview
            .members
            .iter()
            .zip(&preview.chosen)
            .filter(|(_, chosen)| **chosen)
            .map(|((o, _), _)| &o.meeting_id)
            .collect::<HashSet<_>>()
            .len();
        let count = crate::ui::format::count(meetings as u64, "meeting", "meetings");
        let confirm = if preview.label.is_empty() {
            format!("Link in {count}")
        } else {
            format!("Link {} in {count}", preview.label)
        };
        Some(
            crate::ui::kit::as_inset(div(), cx)
                .flex()
                .flex_col()
                .gap_2()
                .px_3()
                .py_2()
                .child(crate::ui::text_line(
                    "identify-preview-heading",
                    Role::Heading,
                    "Confirm matching voices",
                ))
                .children(preview.members.iter().zip(&preview.chosen).enumerate().map(
                    |(ix, ((o, clip), chosen))| {
                        let allowed = match &preview.choice {
                            PersonChoice::Existing(id) => o.allows(Some(id)),
                            PersonChoice::Typed(_) => o.allows(None),
                        };
                        let title = meeting_title(o.meeting_title.as_deref());
                        let meeting = o.meeting_id.clone();
                        let label = format!("{} · {title}", speaker_name(o.speaker_id));
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                Checkbox::new(("identify-include", ix))
                                    .label(label)
                                    .checked(*chosen)
                                    .disabled(!enabled || !allowed)
                                    .on_change(cx.listener(move |this, checked: &bool, _, cx| {
                                        if let Some(preview) = this.identity.identify.as_mut()
                                            && let Some(chosen) = preview.chosen.get_mut(ix)
                                        {
                                            *chosen = *checked;
                                            cx.notify();
                                        }
                                    })),
                            )
                            .children((!allowed).then(|| {
                                crate::ui::text_line(
                                    ("identify-rejected", ix),
                                    Role::Label,
                                    "Person was rejected for this speaker",
                                )
                            }))
                            .child(
                                button(
                                    ("identify-meeting", ix),
                                    "Open meeting",
                                    enabled,
                                    cx,
                                    move |this: &mut Self, _, cx| {
                                        this.state.update(cx, |state, cx| {
                                            state.navigate(Route::meeting(meeting.clone()), cx)
                                        })
                                    },
                                )
                                .small()
                                .link(),
                            )
                            .children(clip.clone().map(|clip| {
                                self.identity_play(
                                    ("identify-sample", ix),
                                    format!("identify/{}/{}", o.meeting_id, o.speaker_id).into(),
                                    clip,
                                    cx,
                                )
                            }))
                    },
                ))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            button(
                                "identify-confirm",
                                confirm,
                                enabled && meetings > 0,
                                cx,
                                |this: &mut Self, _, cx| this.save_identification(cx),
                            )
                            .outline(),
                        )
                        .child(
                            button(
                                "identify-cancel",
                                "Cancel",
                                enabled,
                                cx,
                                |this: &mut Self, _, cx| {
                                    this.identity.identify = None;
                                    this.playback.stop(&this.state, cx);
                                    cx.notify();
                                },
                            )
                            .outline(),
                        ),
                ),
        )
    }

    fn identity_play(
        &self,
        id: (&'static str, usize),
        key: SharedString,
        clip: Clip,
        cx: &Context<Self>,
    ) -> Div {
        let label = if self.playback.is_playing(&key) {
            "Stop sample"
        } else {
            "Play sample"
        };
        let playing_key = key.clone();
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                button(
                    id,
                    label,
                    !self.identity.saving,
                    cx,
                    move |this: &mut Self, _, cx| {
                        this.playback.toggle(
                            playing_key.clone(),
                            clip.clone(),
                            &this.state,
                            cx,
                            playback_of,
                        );
                    },
                )
                .small()
                .outline(),
            )
            .children(
                self.playback
                    .status_of(&key)
                    .map(|status| div().child(status.to_owned())),
            )
    }

    pub(super) fn render_duplicates(&self, cx: &Context<Self>) -> Div {
        let enabled = !self.identity.saving && self.saving.is_none();
        div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .children(self.identity.snapshot.pending.as_ref().map(|pending| {
                let pending = pending.clone();
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(crate::ui::text_line(
                        "resume-profile-merge-status",
                        Role::Status,
                        "A profile merge is waiting to finish.",
                    ))
                    .child(
                        button(
                            "resume-profile-merge",
                            "Resume merge",
                            enabled,
                            cx,
                            move |this: &mut Self, _, cx| {
                                this.write_identity(Write::Merge(pending.clone()), cx)
                            },
                        )
                        .outline(),
                    )
            }))
            .children(self.identity.snapshot.duplicates.iter().enumerate().map(
                |(ix, duplicate)| {
                    self.render_duplicate(
                        ix,
                        duplicate,
                        enabled && self.identity.snapshot.pending.is_none(),
                        cx,
                    )
                },
            ))
    }

    fn render_duplicate(
        &self,
        ix: usize,
        duplicate: &PersonDuplicate,
        enabled: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let pair = duplicate.pair.clone();
        let chosen = self
            .identity
            .merge
            .as_ref()
            .filter(|(p, _)| p == &pair)
            .map(|(_, keep)| keep);
        let suggested = duplicate.keep.clone();
        let separate = pair.clone();
        let compare = pair.clone();
        let names = format!("{} and {}", duplicate.names[0], duplicate.names[1]);
        let meetings =
            crate::ui::format::count(duplicate.meeting_count as u64, "meeting", "meetings");
        div().id(("duplicate-profile-pair", ix)).flex().flex_col().gap_2().py_2()
            .border_b_1().border_color(cx.theme().border)
            .child(crate::ui::text_line(("duplicate-names", ix), Role::Heading, names))
            .child(crate::ui::text_line(("duplicate-voice", ix), Role::Label,
                format!("Voice similarity {:.2} · {meetings}", duplicate.score)).text_role(TextRole::Caption))
            .children([&pair.0, &pair.1].into_iter().enumerate().map(|(side, person)| {
                let at = ix * 2 + side;
                let name = if duplicate.names[0].eq_ignore_ascii_case(&duplicate.names[1]) {
                    format!("{} · {person}", duplicate.names[side])
                } else {
                    duplicate.names[side].clone()
                };
                let clip = duplicate.samples[side].as_ref().and_then(|sample| people::clip_of(person, sample));
                let about = person.clone();
                let select = person.clone();
                let pair = pair.clone();
                let playback = format!("duplicate/{}/{}/{}", pair.0, pair.1, side);
                crate::ui::kit::as_inset(div(), cx).flex().flex_wrap().items_center().gap_2().px_3().py_2()
                    .child(button(("duplicate-profile", at), name.clone(), enabled, cx, move |this: &mut Self, _, cx| {
                        this.state.update(cx, |state, cx| state.navigate(Route::People { person: Some(about.clone()) }, cx))
                    }).link().small())
                    .children(clip.map(|clip| self.identity_play(("duplicate-sample", at),
                        playback.into(), clip, cx)))
                    .children(chosen.map(|chosen| {
                        let label = if chosen == person { "Keep this profile ✓".into() } else { format!("Keep {name}") };
                        button(("duplicate-keep", at), label, enabled && !duplicate.keep_locked, cx,
                            move |this: &mut Self, _, cx| {
                                this.identity.merge = Some((pair.clone(), select.clone()));
                                cx.notify();
                            }).small().outline()
                    }))
            }))
            .child(div().flex().flex_wrap().items_center().gap_2().map(|el| {
                if let Some(keep) = chosen {
                    let source = if keep == &pair.0 { pair.1 } else { pair.0 };
                    let spec = MergePeople { keep: keep.clone(), source, revision: duplicate.revision.clone() };
                    let name = &duplicate.names[usize::from(keep == &duplicate.pair.1)];
                    let name = if duplicate.names[0].eq_ignore_ascii_case(&duplicate.names[1]) {
                        format!("{name} · {keep}")
                    } else {
                        name.clone()
                    };
                    el.child(crate::ui::text_line(("duplicate-merge-description", ix), Role::Label, format!("Keep {name} and transfer the other profile’s meetings, notes, and voice samples.")))
                        .child(button(("duplicate-confirm", ix), "Merge profiles", enabled, cx,
                            move |this: &mut Self, _, cx| this.write_identity(Write::Merge(spec.clone()), cx)).outline())
                        .child(button(("duplicate-cancel", ix), "Cancel", enabled, cx, |this: &mut Self, _, cx| {
                            this.identity.merge = None;
                            cx.notify();
                        }).outline())
                } else {
                    el.child(button(("duplicate-review", ix), "Review merge", enabled, cx, move |this: &mut Self, _, cx| {
                        this.identity.merge = Some((compare.clone(), suggested.clone()));
                        cx.notify();
                    }).outline())
                    .child(button(("duplicate-separate", ix), "Different people", enabled, cx,
                        move |this: &mut Self, _, cx| this.write_identity(Write::Separate(separate.clone()), cx)).outline())
                }
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::kit::dialog_gate::{click, draw};
    use crate::views::person_picker::fixtures::{self, sid};
    use broscribe_core::schema::Source;
    use gpui::{TestAppContext, VisualTestContext};

    #[gpui::test]
    fn matching_voices_are_previewed_before_a_single_confirmation(cx: &mut TestAppContext) {
        let second = MeetingId::random();
        let (window, _, lib, fixture, _dir) =
            super::super::tests::open_review_with(cx, |lib, fixture| {
                let mut first = lib.load_meeting(&fixture.meeting).unwrap();
                fixtures::set_voice(&mut first, 1, &[1.0, 0.0]);
                first.speakers[0].talk_s = 45.0;
                let mut other = Meeting::new(second.clone(), Source::Import, first.started_at);
                other.title = Some("A different meeting".into());
                other.processing = first.processing.clone();
                other.speakers = vec![first.speakers[0].clone()];
                other.speakers[0].talk_s = 5.0;
                other.segments = vec![first.segments[0].clone()];
                ops::with_index(lib, |tx, index| {
                    tx.save_meeting(&first)?;
                    index.upsert_meeting(tx, &first)?;
                    tx.create_meeting_dir(&other)?;
                    tx.save_meeting(&other)?;
                    index.upsert_meeting(tx, &other)
                })
                .unwrap();
            });
        window
            .update(cx, |view, _, cx| {
                let key = view.rows[0].key();
                assert_eq!(view.identity.snapshot.groups.get(&key).unwrap().meetings, 2);
                view.preview_identification(
                    key,
                    PersonChoice::Existing(fixture.ana.clone()),
                    Some("Ana Silva".to_owned()),
                    cx,
                );
            })
            .unwrap();
        for id in [&fixture.meeting, &second] {
            assert_eq!(
                lib.load_meeting(id)
                    .unwrap()
                    .speaker(sid(1))
                    .unwrap()
                    .assignment,
                Assignment::None
            );
        }
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        draw(&mut visual);
        click(&mut visual, "identify-confirm", Role::Button);
        for id in [&fixture.meeting, &second] {
            let meeting = lib.load_meeting(id).unwrap();
            let speaker = meeting.speaker(sid(1)).unwrap();
            assert_eq!(speaker.assignment, Assignment::Confirmed);
            assert_eq!(speaker.person_id.as_ref(), Some(&fixture.ana));
        }
    }
}
