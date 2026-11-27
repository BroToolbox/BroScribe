//! The Settings confirmation uses the same plan and execution as the CLI.

use super::*;
use gpui::component::{WindowExt as _, checkbox::Checkbox};
use serde::Deserialize;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Selection {
    data: bool,
    library: bool,
}

impl Selection {
    fn command(self, apply: bool) -> Vec<OsString> {
        let mut args = super::args([
            "uninstall",
            if apply { "--yes" } else { "--dry-run" },
            "--from-settings",
        ]);
        if self.data {
            args.push("--remove-data".into());
        }
        if self.library {
            args.push("--remove-library".into());
        }
        args
    }
}

#[derive(Clone, Deserialize)]
struct Report {
    ready: bool,
    steps: Vec<Step>,
    manual_steps: Vec<String>,
}

impl Report {
    fn requires_quit(&self) -> bool {
        self.steps
            .iter()
            .any(|step| matches!(step.status.as_str(), "done" | "pending"))
    }
}

#[derive(Clone, Deserialize)]
struct Step {
    action: String,
    path: Option<String>,
    status: String,
    detail: Option<String>,
}

impl Step {
    fn text(&self) -> String {
        let path = self
            .path
            .as_deref()
            .map_or_else(String::new, |path| format!(" ({path})"));
        let action = match self.status.as_str() {
            "done" => format!("Done. {}", self.action),
            "failed" => format!("Failed. {}", self.action),
            "pending" => format!("Pending. {}", self.action),
            "partial" => format!("Partially completed. {}", self.action),
            "warning" => format!("Warning. {}", self.action),
            "skipped" => format!("Skip {}", self.action.to_lowercase()),
            _ => self.action.clone(),
        };
        let detail = self
            .detail
            .as_deref()
            .map_or_else(String::new, |detail| format!(" · {detail}"));
        format!("{action}{path}{detail}")
    }
}

#[derive(Default)]
pub(super) struct RemovalDialog {
    selection: Selection,
    revision: u64,
    phase: Phase,
}

#[derive(Clone, Default)]
enum Phase {
    #[default]
    Loading,
    Preview(Report),
    Applying,
    Result(Report),
    Failed(String),
}

impl RemovalDialog {
    pub(super) fn applying(&self) -> bool {
        matches!(self.phase, Phase::Applying)
    }
}

impl SettingsView {
    pub(super) fn uninstall_button(&self, cx: &mut Context<Self>) -> Button {
        Button::new("settings-uninstall")
            .label("Uninstall BroScribe…")
            .danger()
            .outline()
            .self_start()
            .track_focus(&self.uninstall_focus)
            .on_click(cx.listener(|this, _, window, cx| this.open_uninstall(window, cx)))
    }

    pub(super) fn open_uninstall(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_active_dialog(cx) {
            return;
        }
        self.uninstall.selection = Selection::default();
        self.load_uninstall(false, window, cx);
        let owner = cx.weak_entity();
        let cancel = cx.focus_handle();
        let first = Cell::new(true);
        let focus_result = Cell::new(true);
        window.open_alert_dialog(cx, move |dialog, window, cx| {
            if first.replace(false) { cancel.focus(window, cx); }
            let Some(entity) = owner.upgrade() else { return dialog; };
            let state = &entity.read(cx).uninstall;
            let selection = state.selection;
            let phase = state.phase.clone();
            let applying = matches!(phase, Phase::Applying);
            let preview = matches!(phase, Phase::Loading | Phase::Preview(_));
            let ready = matches!(&phase, Phase::Preview(report) if report.ready);
            let finished = matches!(phase, Phase::Result(_));
            let quit = matches!(&phase, Phase::Result(report) if report.requires_quit());
            let mut body = div().flex().flex_col().gap_2();
            match &phase {
                Phase::Loading => body = body.child(text_line("uninstall-checking", Role::Status, "Checking what BroScribe can remove…")),
                Phase::Applying => body = body.child(text_line("uninstall-applying", Role::Status, "Uninstalling BroScribe…")),
                Phase::Failed(message) => body = body.child(text_line("uninstall-failed", Role::Alert, message.clone())),
                Phase::Preview(report) | Phase::Result(report) => {
                    let mut steps = div().id("uninstall-steps").max_h(gpui::rems(13.75)).overflow_y_scroll()
                        .flex().flex_col().gap_2();
                    for (i, step) in report.steps.iter().enumerate() {
                        steps = steps.child(text_line(("uninstall-step", i), Role::Label, step.text()));
                    }
                    body = body.child(steps);
                    for (i, line) in report.manual_steps.iter().enumerate() {
                        body = body.child(text_line(("uninstall-manual", i), Role::Label, line.clone()));
                    }
                }
            }
            if preview {
                for (id, label, checked, data) in [
                    ("uninstall-data", "Delete BroScribe-created data and settings", selection.data, true),
                    ("uninstall-library", "Delete the meeting library", selection.library, false),
                ] {
                    let owner = owner.clone();
                    body = body.child(Checkbox::new(id).label(label).checked(checked).on_change(move |_, window, cx| {
                        let _ = owner.update(cx, |this, cx| {
                            if data { this.uninstall.selection.data = !this.uninstall.selection.data; }
                            else { this.uninstall.selection.library = !this.uninstall.selection.library; }
                            this.load_uninstall(false, window, cx);
                        });
                    }));
                }
            }
            let parent_focus = entity.read(cx).uninstall_focus.clone();
            let dismiss = Button::new("uninstall-dismiss")
                .label(if quit { "Quit BroScribe" } else if preview { "Cancel" } else { "Close" })
                .outline()
                .disabled(applying)
                .track_focus(&cancel)
                .on_click(move |_, window, cx| {
                    if quit { cx.quit(); }
                    else { window.close_dialog(cx); parent_focus.focus(window, cx); }
                });
            let mut footer = div().flex().justify_end().gap(kit::CONTROL_GAP);
            if preview {
                let owner = owner.clone();
                footer = footer.child(kit::confirm_actions(
                    dismiss,
                    Button::new("uninstall-confirm").label("Uninstall BroScribe").danger().outline().disabled(!ready)
                        .on_click(move |_, window, cx| {
                            let _ = owner.update(cx, |this, cx| {
                                if matches!(&this.uninstall.phase, Phase::Preview(report) if report.ready) {
                                    this.load_uninstall(true, window, cx);
                                }
                            });
                        }),
                    cx,
                ));
            } else {
                if !applying && focus_result.replace(false) { cancel.focus(window, cx); }
                footer = footer.child(dismiss);
            }
            dialog.title(if finished { "BroScribe removal" } else { "Uninstall BroScribe?" })
                .bg(cx.theme().popover)
                .width(px(530.))
                .entrance(gpui::component::dialog::DialogEntrance::None)
                .keyboard(!applying && !quit)
                .on_ok(|_, _, _| false)
                .child(body)
                .footer(footer)
        });
    }

    fn load_uninstall(&mut self, apply: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.uninstall.revision += 1;
        let revision = self.uninstall.revision;
        self.uninstall.phase = if apply {
            Phase::Applying
        } else {
            Phase::Loading
        };
        let selection = self.uninstall.selection;
        let args = selection.command(apply);
        let suspended = apply && selection.data;
        let flush = if suspended {
            crate::ui_state::suspend_for_uninstall(cx)
        } else {
            Task::ready(())
        };
        cx.notify();
        window.refresh();
        cx.spawn_in(window, async move |this, cx| {
            flush.await;
            let Ok(task) = this.update(cx, |this, cx| {
                this.state.read(cx).run_cli(args, ChildKind::Command, cx)
            }) else {
                if suspended {
                    let _ = cx.update(|_, cx| crate::ui_state::resume_after_failed_uninstall(cx));
                }
                return;
            };
            let answer = task.await;
            let refused = answer
                .as_ref()
                .is_err_and(|e| e.from_cli || e.code == "spawn");
            let result = answer.map_err(|e| e.user_text()).and_then(|value| {
                serde_json::from_value::<Report>(value)
                    .map_err(|_| "BroScribe returned an unreadable uninstall report.".to_owned())
            });
            if suspended && (refused || matches!(&result, Ok(report) if !report.requires_quit())) {
                let _ = cx.update(|_, cx| crate::ui_state::resume_after_failed_uninstall(cx));
            }
            let _ = this.update_in(cx, |this, window, cx| {
                if this.uninstall.revision != revision {
                    return;
                }
                this.uninstall.phase = match result {
                    Ok(report) if apply => Phase::Result(report),
                    Ok(report) => Phase::Preview(report),
                    Err(message) => Phase::Failed(message),
                };
                cx.notify();
                window.refresh();
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uninstall_only_requires_quit_when_a_step_ran_or_is_pending() {
        for (status, quit) in [
            ("failed", false),
            ("skipped", false),
            ("done", true),
            ("pending", true),
        ] {
            let report = Report {
                ready: false,
                steps: vec![Step {
                    action: "Synthetic removal step".into(),
                    path: None,
                    status: status.into(),
                    detail: None,
                }],
                manual_steps: Vec::new(),
            };
            assert_eq!(report.requires_quit(), quit);
        }
    }

    #[test]
    fn deleting_data_and_library_are_independent_explicit_choices() {
        let command = Selection::default().command(false);
        assert!(command.contains(&"--dry-run".into()));
        assert!(!command.contains(&"--yes".into()));
        assert!(!command.contains(&"--remove-data".into()));
        assert!(!command.contains(&"--remove-library".into()));
        let command = Selection {
            data: true,
            library: false,
        }
        .command(true);
        assert!(command.contains(&"--yes".into()));
        assert!(command.contains(&"--remove-data".into()));
        assert!(!command.contains(&"--remove-library".into()));
        let command = Selection {
            data: false,
            library: true,
        }
        .command(true);
        assert!(command.contains(&"--remove-library".into()));
        assert!(!command.contains(&"--remove-data".into()));
    }
}
