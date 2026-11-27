//! "Delete meeting": a confirmation shared by the meeting header and the
//! recording panel's saved-recording notice (design §4, §7.1).
//!
//! - Opening it runs `broscribe delete <id> --dry-run --json`; the dialog
//!   lists what that plan removes (transcript and generated results, owned
//!   recording files, voice samples learned from the meeting, caches, queued
//!   jobs) and what stays (people, original files outside the library,
//!   exports). A recording moved into the library is the user's only copy:
//!   that gets an explicit warning.
//! - Only "Delete permanently" runs `delete <id> --yes --json`. Both run as a
//!   CLI child on the background executor: no deletion code runs in the GUI
//!   process, and the meeting ID is the only argument.
//! - Cancel, or closing the host, changes nothing.

use std::ffi::OsString;

use broscribe_core::MeetingId;
use gpui::component::WindowExt as _;
use gpui::{AnyWindowHandle, App, Context, Entity, EventEmitter, FocusHandle, Window};
use serde_json::Value;

use crate::jobs::{ChildKind, CliError, failure_text};
use crate::state::AppState;
use crate::ui::kit::{self, StatusKind};

/// `delete <id> --dry-run` or `delete <id> --yes` (the runner adds
/// `--library <lib> --json`).
pub fn delete_args(meeting_id: &MeetingId, confirmed: bool) -> Vec<OsString> {
    vec![
        "delete".into(),
        meeting_id.as_str().into(),
        if confirmed { "--yes" } else { "--dry-run" }.into(),
    ]
}

fn count(report: &Value, key: &str) -> u64 {
    report[key].as_u64().unwrap_or(0)
}

fn listed(report: &Value, key: &str) -> usize {
    report[key].as_array().map_or(0, Vec::len)
}

/// The consequences of deleting this meeting, as reported by the dry run.
#[derive(Clone, Debug, PartialEq)]
pub struct DeletePlan {
    pub goes: Vec<String>,
    pub changes: Vec<String>,
    pub stays: Vec<String>,
    pub warning: Option<String>,
}

fn delete_plan(report: &Value) -> DeletePlan {
    let mut goes = vec![
        "Removes the transcript, summary, decisions, action items, template results and search entries of this meeting."
            .to_owned(),
    ];
    let audio = listed(report, "owned_audio") as u64;
    let samples = count(report, "voice_samples");
    let caches = count(report, "playback_caches");
    for (n, one, many) in [
        (audio, "stored recording file", "stored recording files"),
        (
            samples,
            "voice sample learned from this meeting",
            "voice samples learned from this meeting",
        ),
        (caches, "playback cache file", "playback cache files"),
    ] {
        if n > 0 {
            goes.push(format!(
                "Removes {}.",
                crate::ui::format::count(n, one, many)
            ));
        }
    }

    let mut changes = Vec::new();
    let jobs = count(report, "jobs_cancelled");
    if jobs > 0 {
        changes.push(format!(
            "Cancels {} for this meeting.",
            crate::ui::format::count(jobs, "waiting job", "waiting jobs")
        ));
    }
    let people = count(report, "people_affected");
    if people > 0 {
        changes.push(format!(
            "Rechecks the voices of {}.",
            crate::ui::format::count(people, "person", "people")
        ));
    }

    let mut stays = vec![
        "The people and their other voice samples stay.".to_owned(),
        "Files you exported stay where you saved them.".to_owned(),
    ];
    let kept = listed(report, "referenced_originals_kept") as u64;
    if kept > 0 {
        stays.push(format!(
            "Keeps {} outside the library: BroScribe only referenced them.",
            crate::ui::format::count(kept, "original file", "original files")
        ));
    }

    DeletePlan {
        goes,
        changes,
        stays,
        warning: (report["moved_original"] == true).then(|| {
            "This recording is your only copy. It was moved into the library at import, so deleting the meeting deletes it for good.".to_owned()
        }),
    }
}

/// What a finished `--yes` report tells the user.
pub fn deleted_text(report: &Value) -> String {
    match report["state"].as_str() {
        Some("cleanup_pending") => format!(
            "Meeting deleted. {} couldn't be removed yet; BroScribe retries the cleanup.",
            crate::ui::format::count(
                listed(report, "cleanup_failures") as u64,
                "leftover file",
                "leftover files"
            )
        ),
        _ => "Meeting deleted.".to_owned(),
    }
}

/// Which request of the dialog failed: the dry run that opens it, or the
/// deletion the user confirmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteStep {
    /// `delete --dry-run`: it never deletes anything.
    Preview,
    /// `delete --yes`, after the user confirmed.
    Delete,
}

/// What a refused plan or deletion tells the user: by its code, else why in
/// words, never the code. "Nothing was deleted" is said only where the app
/// knows it: a refusal the CLI makes before it deletes anything, or a
/// failed dry run. A deletion that failed otherwise may have begun, so it
/// promises nothing.
pub fn delete_refusal(step: DeleteStep, error: &CliError) -> String {
    match error.code.as_str() {
        "recording_active" => "This meeting is still being recorded or saved. Use Discard in \
                               the recording controls instead. Nothing was deleted."
            .to_owned(),
        "job_running" => "A job is working on this meeting (import, reprocessing, summary…). \
                          Try again when it finishes; nothing was deleted."
            .to_owned(),
        "only_copy_in_flight" => "The recording is still being moved into the library, and it \
                                  is your only copy. Try again when the import finishes; \
                                  nothing was deleted."
            .to_owned(),
        "library_busy" => {
            "The library is busy; try again in a moment. Nothing was deleted.".to_owned()
        }
        "not_found" => "This meeting no longer exists.".to_owned(),
        _ => match step {
            DeleteStep::Preview => failure_text(
                "Couldn't check what would be deleted.",
                &error.user_text(),
                "Nothing was deleted.",
            ),
            DeleteStep::Delete => {
                failure_text("Couldn't delete the meeting.", &error.user_text(), "")
            }
        },
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum DeleteEvent {
    /// The meeting is gone (`text` says whether cleanup is still pending).
    Deleted {
        meeting_id: MeetingId,
        text: String,
    },
    Cancelled,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DeletePhase {
    Planning,
    Planned(DeletePlan),
    Deleting,
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeleteRecovery {
    CheckAgain,
    RecordingControls,
}

pub struct DeleteMeetingPrompt {
    state: gpui::Entity<AppState>,
    meeting_id: MeetingId,
    title: String,
    phase: DeletePhase,
    /// Bumped by every request; only the current one's answer applies.
    generation: u64,
    recovery: Option<DeleteRecovery>,
    dialog: Option<(AnyWindowHandle, FocusHandle)>,
}

impl EventEmitter<DeleteEvent> for DeleteMeetingPrompt {}

impl DeleteMeetingPrompt {
    /// Opens the confirmation and starts the dry run.
    pub fn new(
        state: gpui::Entity<AppState>,
        meeting_id: MeetingId,
        title: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            state,
            meeting_id,
            title,
            phase: DeletePhase::Planning,
            generation: 0,
            recovery: None,
            dialog: None,
        };
        this.run(false, cx);
        this
    }

    pub fn meeting_id(&self) -> &MeetingId {
        &self.meeting_id
    }

    pub fn phase(&self) -> &DeletePhase {
        &self.phase
    }

    fn run(&mut self, confirmed: bool, cx: &mut Context<Self>) {
        self.recovery = None;
        self.generation += 1;
        let generation = self.generation;
        self.phase = if confirmed {
            DeletePhase::Deleting
        } else {
            DeletePhase::Planning
        };
        cx.notify();
        let cli = self.state.read(cx).cli().clone();
        let args = delete_args(&self.meeting_id, confirmed);
        let task = cx
            .background_executor()
            .spawn(async move { cli.run_blocking(&args, ChildKind::Command) });
        cx.spawn(async move |this, cx| {
            let result: Result<Value, CliError> = task.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(report) if confirmed => {
                        let text = deleted_text(&report);
                        if let Some((window, opener)) = this.dialog.take() {
                            let _ = window.update(cx, |_, window, cx| {
                                window.close_dialog(cx);
                                opener.focus(window, cx);
                            });
                        }
                        let cleanup = listed(&report, "cleanup_failures") as u64;
                        this.state.update(cx, |state, cx| {
                            state.meeting_deleted(
                                this.meeting_id.clone(),
                                &this.title,
                                cleanup,
                                cx,
                            );
                            state.refresh_all(cx);
                        });
                        cx.emit(DeleteEvent::Deleted {
                            meeting_id: this.meeting_id.clone(),
                            text,
                        });
                    }
                    Ok(report) => {
                        this.phase = DeletePhase::Planned(delete_plan(&report));
                    }
                    Err(e) => {
                        let step = if confirmed {
                            DeleteStep::Delete
                        } else {
                            DeleteStep::Preview
                        };
                        this.recovery = match e.code.as_str() {
                            "recording_active" => Some(DeleteRecovery::RecordingControls),
                            "library_busy" | "job_running" | "only_copy_in_flight" => {
                                Some(DeleteRecovery::CheckAgain)
                            }
                            _ => None,
                        };
                        this.phase = DeletePhase::Failed(delete_refusal(step, &e));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// "Delete permanently": only after a plan was shown.
    pub fn confirm(&mut self, cx: &mut Context<Self>) {
        if matches!(self.phase, DeletePhase::Planned(_)) {
            self.run(true, cx);
        }
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if self.phase != DeletePhase::Deleting {
            self.generation += 1;
            cx.emit(DeleteEvent::Cancelled);
        }
    }

    /// Displays the prompt in its owner's window, without coupling the dry
    /// run or deletion to any particular view.
    pub fn present(prompt: &Entity<Self>, opener: FocusHandle, window: &mut Window, cx: &mut App) {
        prompt.update(cx, |prompt, _| {
            prompt.dialog = Some((window.window_handle(), opener.clone()))
        });
        let prompt = prompt.downgrade();
        let model = prompt.clone();
        let action = prompt.clone();
        kit::confirm_dialog(
            opener,
            window,
            cx,
            move |cx| {
                model.upgrade().map(|prompt| {
                    let value = prompt.read(cx);
                    let mut spec = value.spec();
                    if let Some(recovery) = value.recovery {
                        let prompt = model.clone();
                        spec.recovery = Some((
                            kit::confirm_line(
                                "delete-recovery",
                                match recovery {
                                    DeleteRecovery::CheckAgain => "Check again",
                                    DeleteRecovery::RecordingControls => "Show recording controls",
                                },
                            ),
                            std::rc::Rc::new(move |window, cx| match recovery {
                                DeleteRecovery::CheckAgain => {
                                    let _ = prompt.update(cx, |prompt, cx| prompt.run(false, cx));
                                }
                                DeleteRecovery::RecordingControls => {
                                    if Self::try_cancel(&prompt, cx) {
                                        window.close_dialog(cx);
                                        window.dispatch_action(
                                            Box::new(crate::menu::ShowRecordingControls),
                                            cx,
                                        );
                                    }
                                }
                            }),
                        ));
                    }
                    spec
                })
            },
            move |_, cx| Self::try_cancel(&prompt, cx),
            move |_, cx| {
                let _ = action.update(cx, |this, cx| this.confirm(cx));
                false
            },
        );
    }

    fn try_cancel(prompt: &gpui::WeakEntity<Self>, cx: &mut App) -> bool {
        prompt
            .update(cx, |this, cx| {
                if this.phase == DeletePhase::Deleting {
                    return false;
                }
                this.cancel(cx);
                true
            })
            .unwrap_or(true)
    }

    fn spec(&self) -> kit::ConfirmSpec {
        use kit::confirm_line as line;

        let mut spec = kit::ConfirmSpec::new(
            line(
                "delete-meeting-title",
                format!("Delete “{}” permanently?", self.title),
            ),
            "delete-meeting",
            "delete-meeting-footer",
            "delete-meeting-cancel",
            line("delete-meeting-confirm", "Delete meeting"),
        );
        spec.action_enabled = false;
        match &self.phase {
            DeletePhase::Planning => {
                spec.status = Some((
                    line("delete-status", "Checking what deleting removes…"),
                    StatusKind::Pending,
                ));
            }
            DeletePhase::Deleting => {
                spec.cancel_enabled = false;
                spec.status = Some((
                    line("delete-status", "Deleting the meeting…"),
                    StatusKind::Pending,
                ));
            }
            DeletePhase::Planned(plan) => {
                spec.action_enabled = true;
                spec.warning = plan
                    .warning
                    .clone()
                    .map(|words| line("delete-moved-warning", words));
                let mut index = 0usize;
                let mut lines = |words: &[String]| {
                    words
                        .iter()
                        .map(|text| {
                            let id = ("delete-plan-line", index);
                            index += 1;
                            line(id, text.clone())
                        })
                        .collect()
                };
                spec.goes = lines(&plan.goes);
                spec.changes = lines(&plan.changes);
                spec.stays = lines(&plan.stays);
            }
            DeletePhase::Failed(text) => {
                spec.status = Some((line("delete-error", text.clone()), StatusKind::Failed));
            }
        }
        spec
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plan_lists_scope_and_warns_about_a_moved_only_copy() {
        let report = json!({
            "meeting_id": "m-synthetic", "dry_run": true,
            "owned_audio": ["m-synthetic/audio.m4a"],
            "referenced_originals_kept": ["/synthetic/original.wav", "/synthetic/second.wav"],
            "moved_original": true, "voice_samples": 3, "people_affected": 2,
            "playback_caches": 1, "jobs_cancelled": 0, "state": "planned",
            "cleanup_failures": []
        });
        let plan = delete_plan(&report);
        let goes = plan.goes.join("\n");
        assert!(goes.contains("Removes 1 stored recording file."));
        assert!(goes.contains("Removes 3 voice samples learned from this meeting."));
        assert!(goes.contains("Removes 1 playback cache file."));
        assert_eq!(plan.changes, ["Rechecks the voices of 2 people."]);
        let stays = plan.stays.join("\n");
        assert!(stays.contains("Keeps 2 original files outside the library"));
        assert!(stays.contains("Files you exported stay"));
        assert!(plan.warning.unwrap().contains("your only copy"));

        let no_audio = delete_plan(&json!({ "moved_original": false }));
        assert!(no_audio.warning.is_none());
        assert_eq!(no_audio.goes.len(), 1);
        assert!(no_audio.changes.is_empty());
    }

    #[gpui::test]
    fn delete_meeting_dialog_keeps_its_consequences_and_opener(cx: &mut gpui::TestAppContext) {
        use crate::ui::kit::dialog_gate::{Expected, cancel_both_ways, click};
        use gpui::component::{
            Root,
            button::{Button, ButtonVariants as _},
        };
        use gpui::{AppContext as _, IntoElement, ParentElement as _, Render, Role, div};

        struct Probe {
            prompt: Entity<DeleteMeetingPrompt>,
            opener: FocusHandle,
        }
        impl Render for Probe {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let prompt = self.prompt.clone();
                let opener = self.opener.clone();
                div().child(
                    Button::new("delete-open")
                        .label("Delete meeting")
                        .danger()
                        .outline()
                        .track_focus(&self.opener)
                        .on_click(move |_, window, cx| {
                            DeleteMeetingPrompt::present(&prompt, opener.clone(), window, cx)
                        }),
                )
            }
        }
        cx.update(crate::ui::theme::init);
        let dir = tempfile::tempdir().unwrap();
        let (state, _, _) = crate::views::person_picker::fixtures::state_with_cli(
            cx,
            dir.path(),
            dir.path().join("unused-cli"),
        );
        let prompt = cx.new(|_| DeleteMeetingPrompt {
            state,
            meeting_id: MeetingId::random(),
            title: "Synthetic sync".into(),
            phase: DeletePhase::Planned(delete_plan(&json!({
                "owned_audio": ["synthetic.wav"], "jobs_cancelled": 1, "moved_original": true
            }))),
            generation: 0,
            recovery: None,
            dialog: None,
        });
        let opener = cx.update(|cx| cx.focus_handle());
        let window = cx.add_window(|window, cx| {
            let view = cx.new(|_| Probe {
                prompt,
                opener: opener.clone(),
            });
            Root::new(view, window, cx)
        });
        let vcx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        cancel_both_ways(
            vcx,
            "delete-open",
            Expected {
                title: "Delete “Synthetic sync” permanently?",
                cancel_id: "delete-meeting-cancel",
                action_id: "delete-meeting-confirm",
                action: "Delete meeting",
                lines: &[
                    ("delete-plan-line-1", "Removes 1 stored recording file."),
                    (
                        "delete-plan-line-2",
                        "Cancels 1 waiting job for this meeting.",
                    ),
                    (
                        "delete-plan-line-3",
                        "The people and their other voice samples stay.",
                    ),
                    ("confirmation-irreversible", "This can't be undone."),
                ],
                headings: &["What goes", "What changes", "What stays"],
            },
            |vcx| click(vcx, "delete-open", Role::Button),
        );
    }

    /// Opening the dialog runs the dry run: its failure says the check
    /// failed and that nothing was deleted, never that the deletion failed.
    #[gpui::test]
    fn a_failed_preview_says_nothing_was_deleted(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        let dir = tempfile::tempdir().unwrap();
        let lib = broscribe_core::Library::open(dir.path().join("lib")).unwrap();
        let paths = broscribe_platform::resolve_paths(&broscribe_platform::fakes::FakePaths::new(
            dir.path(),
        ))
        .unwrap();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    paths,
                    Default::default(),
                    lib,
                    dir.path().join("missing-cli"),
                ),
                cx,
            )
        });
        let prompt = cx
            .new(|cx| DeleteMeetingPrompt::new(state, MeetingId::random(), "Synthetic".into(), cx));
        cx.run_until_parked();
        prompt.read_with(cx, |prompt, _| {
            assert_eq!(
                prompt.phase(),
                &DeletePhase::Failed(
                    "Couldn't check what would be deleted. BroScribe's command-line tool \
                     couldn't start. Nothing was deleted."
                        .into()
                )
            );
        });
    }

    #[test]
    fn refusals_and_results_explain_what_happened() {
        let refusal = |code: &str, message: &str| {
            delete_refusal(DeleteStep::Delete, &CliError::new(code, message))
        };
        assert!(refusal("recording_active", "").contains("Discard"));
        assert!(refusal("only_copy_in_flight", "").contains("only copy"));
        // Refusals keep saying that nothing was deleted (§4.2).
        for code in [
            "recording_active",
            "job_running",
            "only_copy_in_flight",
            "library_busy",
        ] {
            assert!(refusal(code, "x").contains("othing was deleted"), "{code}");
        }
        assert_eq!(
            refusal("invalid_input", "invalid input: bad id."),
            "Couldn't delete the meeting. Bad id."
        );
        // A failed deletion may have begun: it promises nothing.
        assert!(!refusal("invalid_input", "x").contains("othing was deleted"));
        // The dry run that opens the dialog never deletes: its failure says
        // so, and never that the deletion failed.
        let preview = |code: &str, message: &str| {
            delete_refusal(DeleteStep::Preview, &CliError::new(code, message))
        };
        assert_eq!(
            preview("exit", "the CLI exited with exit status: 1"),
            "Couldn't check what would be deleted. BroScribe's command-line tool stopped \
             unexpectedly. Nothing was deleted."
        );
        assert_eq!(
            preview("internal", ""),
            "Couldn't check what would be deleted. Nothing was deleted."
        );
        assert!(!preview("invalid_input", "x").contains("Couldn't delete"));
        assert!(preview("job_running", "x").contains("nothing was deleted"));
        // WRT-04: no code reaches the line.
        assert_eq!(
            refusal("exit", "the CLI exited with exit status: 1"),
            "Couldn't delete the meeting. BroScribe's command-line tool stopped unexpectedly."
        );
        assert_eq!(
            deleted_text(&json!({ "state": "deleted" })),
            "Meeting deleted."
        );
        assert!(
            deleted_text(&json!({ "state": "cleanup_pending", "cleanup_failures": ["a"] }))
                .contains("1 leftover file")
        );
        let id = MeetingId::random();
        assert_eq!(
            delete_args(&id, false),
            vec![
                OsString::from("delete"),
                id.as_str().into(),
                "--dry-run".into()
            ]
        );
        assert_eq!(delete_args(&id, true)[2], OsString::from("--yes"));
    }
}
