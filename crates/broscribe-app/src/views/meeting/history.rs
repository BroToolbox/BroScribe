//! Meeting edits that can be replayed against the same library field.

use super::*;
use broscribe_core::lifecycle::{MeetingTitle, journal_root, restore_meeting_title_checked};
use broscribe_core::schema::{Action, UtterancePerson};
use chrono::{DateTime, FixedOffset};

type Stamp = Option<DateTime<FixedOffset>>;

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Scope {
    Title,
    Tags,
    Action(String),
    Line {
        fingerprints: Vec<SegmentFingerprint>,
        fence: SpeakerFence,
    },
    Dismiss {
        speaker: SpeakerId,
        fence: SpeakerFence,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Value {
    Title(MeetingTitle),
    Tags(Vec<String>, Vec<String>),
    Action(Box<Action>),
    Line(Vec<UtterancePerson>),
    Dismiss(Vec<(SpeakerId, Stamp)>),
}

#[derive(Clone)]
pub(super) struct Step {
    pub(super) target: EditTarget,
    pub(super) name: &'static str,
    operation: Operation,
}

#[derive(Clone)]
enum Operation {
    Fields(FieldEdit),
    Identity(Box<ops::ReviewEdit>),
}

#[derive(Clone)]
struct FieldEdit {
    scope: Scope,
    current: Value,
    restore: Value,
}

fn changed() -> Error {
    Error::Service {
        code: "meeting_edit_changed",
        message: "The edited value changed.".into(),
    }
}

impl Scope {
    pub(super) fn read(&self, meeting: &Meeting) -> Result<Value> {
        Ok(match self {
            Self::Title => Value::Title(meeting.into()),
            Self::Tags => Value::Tags(meeting.tags.clone(), meeting.tags_removed.clone()),
            Self::Action(id) => Value::Action(Box::new(
                meeting
                    .enrichment
                    .as_ref()
                    .and_then(|result| result.actions.iter().find(|action| &action.id == id))
                    .cloned()
                    .ok_or_else(changed)?,
            )),
            Self::Line {
                fingerprints,
                fence,
            } => {
                ops::check_speaker_fence(meeting, fence)?;
                // The replay must still refer to the same words and recording times.
                let actual = line_fingerprints(
                    meeting,
                    &meeting
                        .segments
                        .iter()
                        .filter(|segment| {
                            fingerprints.iter().any(|fp| {
                                fp.track == segment.track
                                    && fp.start == segment.start
                                    && fp.end == segment.end
                            })
                        })
                        .map(|segment| segment.id)
                        .collect::<Vec<_>>(),
                )
                .ok_or_else(changed)?;
                if !fingerprints.iter().all(|fp| actual.contains(fp)) {
                    return Err(changed());
                }
                Value::Line(
                    meeting
                        .utterance_people
                        .iter()
                        .filter(|overlay| {
                            fingerprints.iter().any(|fp| {
                                fp.track == overlay.track
                                    && fp.start == overlay.start
                                    && fp.end == overlay.end
                                    && fp.text_sha256 == overlay.text_sha256
                            })
                        })
                        .cloned()
                        .collect(),
                )
            }
            Self::Dismiss { speaker, fence } => {
                ops::check_speaker_fence(meeting, fence)?;
                if meeting.speaker(*speaker).is_none() {
                    return Err(changed());
                }
                let effective = ops::effective_speaker(meeting, *speaker);
                Value::Dismiss(
                    meeting
                        .speakers
                        .iter()
                        .filter(|member| ops::effective_speaker(meeting, member.id) == effective)
                        .map(|member| (member.id, member.review_dismissed_at))
                        .collect(),
                )
            }
        })
    }

    pub(super) fn load(&self, lib: &Library, id: &MeetingId) -> Result<Value> {
        self.read(&lib.load_meeting(id)?)
    }
}

impl FieldEdit {
    fn check(&self, lib: &Library, id: &MeetingId) -> Result<()> {
        if self.scope.load(lib, id).ok().as_ref() == Some(&self.current) {
            Ok(())
        } else {
            Err(changed())
        }
    }

    fn apply(&self, lib: &Library, data_dir: &Path, id: &MeetingId) -> Result<Value> {
        if let Value::Title(title) = &self.restore {
            return restore_meeting_title_checked(lib, &journal_root(data_dir), id, title, || {
                self.check(lib, id)
            })
            .map(Value::Title);
        }
        ops::with_index_checked(
            lib,
            || self.check(lib, id),
            |tx, index| {
                match (&self.scope, &self.restore) {
                    (Scope::Tags, Value::Tags(tags, removed)) => {
                        let mut meeting = tx.library().load_meeting(id)?;
                        meeting.tags.clone_from(tags);
                        meeting.tags_removed.clone_from(removed);
                        meeting.updated_at = broscribe_core::schema::now();
                        tx.save_meeting(&meeting)?;
                        index.upsert_meeting(tx, &meeting)?;
                    }
                    (Scope::Action(action), Value::Action(previous)) => {
                        ops::set_action_status(tx, index, id, action, previous.status)?;
                    }
                    (Scope::Line { fingerprints, .. }, Value::Line(overlays)) => {
                        let person = overlays.first().map(|overlay| &overlay.person_id);
                        if let Some(person) = person {
                            if overlays.len() != fingerprints.len()
                                || overlays.iter().any(|overlay| &overlay.person_id != person)
                            {
                                return Err(changed());
                            }
                            tx.library().load_person(person)?;
                        }
                        ops::set_utterance_people(
                            tx,
                            index,
                            id,
                            fingerprints,
                            person.map(PersonId::as_str),
                        )?;
                    }
                    (Scope::Dismiss { speaker, fence }, Value::Dismiss(members)) => {
                        if members.iter().all(|(_, at)| at.is_some()) {
                            ops::dismiss_review(tx, index, id, *speaker, fence)?;
                        } else if members.iter().all(|(_, at)| at.is_none()) {
                            ops::restore_review(tx, index, id, *speaker, fence)?;
                        } else {
                            return Err(changed());
                        }
                    }
                    _ => return Err(changed()),
                }
                self.scope.load(lib, id)
            },
        )
    }
}

impl MeetingView {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_reversible_write<T: Send + 'static>(
        &mut self,
        target: EditTarget,
        scope: Scope,
        name: &'static str,
        wait: &'static str,
        window: &Window,
        cx: &mut Context<Self>,
        work: impl FnOnce(&Library) -> Result<T> + Send + 'static,
        finish: impl FnOnce(&mut Self, Result<T>, &mut Window, &mut Context<Self>) -> Notice + 'static,
    ) {
        let id = self.meeting_id.clone();
        let history_target = target.clone();
        self.run_write_then(
            target,
            wait,
            window,
            cx,
            move |lib| {
                let before = scope.load(lib, &id).ok();
                let result = work(lib)?;
                let step = before.and_then(|restore| {
                    let current = scope.load(lib, &id).ok()?;
                    (current != restore).then_some(Step {
                        target: history_target,
                        name,
                        operation: Operation::Fields(FieldEdit {
                            scope,
                            current,
                            restore,
                        }),
                    })
                });
                Ok((result, step))
            },
            move |view, result, window, cx| match result {
                Ok((result, step)) => {
                    let notice = finish(view, Ok(result), window, cx);
                    if matches!(notice, Notice::Info(_))
                        && let Some(step) = step
                    {
                        view.undo_edits.push(step);
                        view.redo_edits.clear();
                    }
                    notice
                }
                Err(error) => finish(view, Err(error), window, cx),
            },
        );
    }

    pub(super) fn remember_identity(
        &mut self,
        target: EditTarget,
        name: &'static str,
        edit: Option<ops::ReviewEdit>,
    ) {
        if let Some(edit) = edit {
            self.undo_edits.push(Step {
                target,
                name,
                operation: Operation::Identity(Box::new(edit)),
            });
            self.redo_edits.clear();
        }
    }

    pub(super) fn replay_edit(&mut self, redo: bool, window: &Window, cx: &mut Context<Self>) {
        let step = if redo {
            self.redo_edits.last()
        } else {
            self.undo_edits.last()
        };
        let Some(mut step) = step.cloned() else {
            if !redo && let Some(naming) = self.latest_live_naming() {
                let id = self.meeting_id.clone();
                self.run_write_then(
                    EditTarget::Speaker(naming.speaker_id),
                    "Wait until the edit is saved.",
                    window,
                    cx,
                    move |lib| {
                        ops::with_index(lib, |tx, index| {
                            ops::undo_live_promotion(tx, index, &id, &naming)
                        })
                    },
                    |view, result, _, cx| match result {
                        Ok(()) => {
                            view.state.update(cx, |state, cx| {
                                state.kick_jobs(cx);
                                state.refresh_recheck(cx);
                            });
                            Notice::Info("Undid the name given while recording.".into())
                        }
                        Err(error) => Notice::Error(meeting_user_text(&error)),
                    },
                );
                return;
            }
            cx.propagate();
            return;
        };
        let id = self.meeting_id.clone();
        let data_dir = self.state.read(cx).paths().data_dir.clone();
        self.run_write_then(
            step.target.clone(),
            "Wait until the edit is saved.",
            window,
            cx,
            move |lib| {
                let result = match &mut step.operation {
                    Operation::Fields(edit) => edit.apply(lib, &data_dir, &id).map(Some),
                    Operation::Identity(edit) => edit.replay(lib, redo).map(|()| None),
                };
                Ok((step, result))
            },
            move |view, result, _, cx| {
                let (from, to) = if redo {
                    (&mut view.redo_edits, &mut view.undo_edits)
                } else {
                    (&mut view.undo_edits, &mut view.redo_edits)
                };
                let (mut step, result) = match result {
                    Ok(result) => result,
                    Err(error) => return Notice::Error(meeting_user_text(&error)),
                };
                from.pop();
                match result {
                    Ok(current) => {
                        if let (Operation::Fields(edit), Some(current)) =
                            (&mut step.operation, current)
                        {
                            // Replaying a field write can assign a new timestamp.
                            for other in from.iter_mut().chain(to.iter_mut()) {
                                if let Operation::Fields(other) = &mut other.operation
                                    && other.scope == edit.scope
                                    && other.current == edit.restore
                                {
                                    other.current = current.clone();
                                }
                            }
                            edit.restore = std::mem::replace(&mut edit.current, current);
                        } else {
                            view.state.update(cx, |state, cx| {
                                state.kick_jobs(cx);
                                state.refresh_recheck(cx);
                            });
                        }
                        to.push(step);
                        Notice::Info(if redo { "Redone." } else { "Undone." }.to_owned())
                    }
                    Err(error)
                        if matches!(
                            error.code(),
                            "meeting_edit_changed" | "identity_edit_changed"
                        ) =>
                    {
                        Notice::Error(
                            if redo {
                                "This changed since. Nothing was redone."
                            } else {
                                "This changed since. Nothing was undone."
                            }
                            .to_owned(),
                        )
                    }
                    Err(error) => {
                        from.push(step);
                        Notice::Error(failure_text(
                            if redo {
                                "Couldn't redo the edit."
                            } else {
                                "Couldn't undo the edit."
                            },
                            &meeting_user_text(&error),
                            "",
                        ))
                    }
                }
            },
        );
    }

    fn latest_live_naming(&self) -> Option<broscribe_core::live_identity::LiveSpeakerNaming> {
        broscribe_core::live_identity::speaker_namings(&self.data.as_ref()?.meeting)
            .into_iter()
            .max_by_key(|naming| naming.promotion.at)
    }

    pub(super) fn speaker_history_buttons(&self, cx: &Context<Self>) -> gpui::Div {
        let render = |redo: bool, id, label: &'static str, step: Option<&Step>| {
            let live = !redo && step.is_none() && self.latest_live_naming().is_some();
            let action = if live {
                "Undo name given while recording".to_owned()
            } else {
                step.map_or_else(|| label.to_owned(), |step| format!("{label} {}", step.name))
            };
            let hint = if step.is_some() || live {
                action.clone()
            } else if redo {
                "No edits to redo".to_owned()
            } else {
                "No edits to undo".to_owned()
            };
            button(
                id,
                label,
                self.busy.is_none() && (step.is_some() || live),
                cx,
                move |this: &mut Self, window, cx| this.replay_edit(redo, window, cx),
            )
            .small()
            .outline()
            .accessibility_label(action)
            .tooltip(hint)
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(render(
                false,
                "speakers-undo",
                "Undo",
                self.undo_edits.last(),
            ))
            .child(render(
                true,
                "speakers-redo",
                "Redo",
                self.redo_edits.last(),
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_core::lifecycle::{
        SESSION_MANIFEST, read_session_title, session_dir, set_meeting_title,
    };
    use broscribe_core::schema::{ProcessingStatus, Source, TitleSource};

    #[test]
    fn rename_undo_and_redo_restore_exact_title_and_session_sidecar() {
        for (title, source, stamp) in [
            (None, None, None),
            (
                Some("Calendar title".into()),
                Some(TitleSource::Calendar),
                None,
            ),
            (Some("File title".into()), Some(TitleSource::File), None),
            (
                Some("Earlier chosen title".into()),
                Some(TitleSource::User),
                Some(DateTime::parse_from_rfc3339("2026-11-01T08:00:00Z").unwrap()),
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let lib = Library::open(dir.path().join("library")).unwrap();
            let journal_root = journal_root(dir.path());
            let session = "s-synthetic";
            let journal = session_dir(&journal_root, session).unwrap();
            std::fs::create_dir_all(&journal).unwrap();
            std::fs::write(journal.join(SESSION_MANIFEST), "{}").unwrap();
            let mut meeting = Meeting::new(
                MeetingId::random(),
                Source::Recording,
                broscribe_core::schema::now(),
            );
            meeting.source_id = Some(format!("recording:{session}"));
            meeting.processing.status = ProcessingStatus::Provisional;
            meeting.title = title;
            meeting.title_source = source;
            meeting.title_set_at = stamp;
            lib.write(|tx| {
                tx.create_meeting_dir(&meeting)?;
                tx.save_meeting(&meeting)
            })
            .unwrap();
            let scope = Scope::Title;
            let original = scope.read(&meeting).unwrap();
            set_meeting_title(&lib, &journal_root, &meeting.id, "Edited title").unwrap();
            let renamed = scope.load(&lib, &meeting.id).unwrap();
            let renamed_sidecar = read_session_title(&journal).unwrap().unwrap();
            let undo = FieldEdit {
                scope: scope.clone(),
                current: renamed.clone(),
                restore: original.clone(),
            };
            assert_eq!(undo.apply(&lib, dir.path(), &meeting.id).unwrap(), original);
            assert_eq!(scope.load(&lib, &meeting.id).unwrap(), original);
            let restored_sidecar = read_session_title(&journal).unwrap();
            assert_eq!(restored_sidecar.as_ref().map(|title| title.set_at), stamp);
            if let Some(restored) = restored_sidecar {
                assert_eq!(Some(restored.title), meeting.title);
            }

            let redo = FieldEdit {
                scope,
                current: original,
                restore: renamed.clone(),
            };
            assert_eq!(redo.apply(&lib, dir.path(), &meeting.id).unwrap(), renamed);
            assert_eq!(read_session_title(&journal).unwrap(), Some(renamed_sidecar));
        }
    }
}
