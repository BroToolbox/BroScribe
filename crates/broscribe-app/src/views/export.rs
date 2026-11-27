//! The meeting view's Export… form (design §4 meeting view, §6): which
//! sections (transcript, summary, decisions, action items, each template
//! result), timestamps on or off, and the format (Markdown, plain text,
//! JSON). A section the meeting does not have is shown disabled; a stale one
//! is marked, and the export marks it too. Export asks for a file with the
//! native save dialog and runs `broscribe export <m> --include … --format …
//! [--no-timestamps] --spec-stdin` with `{"out": <path>}` on stdin, so the
//! chosen path (often the meeting's title) never reaches argv; the CLI
//! writes it atomically with mode 0600 (it holds a transcript). The CLI
//! renders; the app never assembles the text itself.
//!
//! Mounting: [`crate::views::meeting::MeetingView`] creates the form when
//! Export… is pressed and presents it as a modal dialog.

use crate::ui::kit::{StatusKind, button, status_line};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use broscribe_core::MeetingId;
use broscribe_core::config::Config;
use broscribe_core::export::{
    ExportFormat, ExportSection, available_sections, outside_library_parent,
};
use broscribe_core::ops;
use broscribe_core::schema::Meeting;
use broscribe_platform::paths::{AppPaths, FileActions, SavePath};
use broscribe_platform_native::NativePlatform;
use gpui::base::ElementExt as _;
use gpui::component::button::Button;
use gpui::component::checkbox::Checkbox;
use gpui::component::{ActiveTheme as _, Disableable as _, Sizable as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Context, EventEmitter, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    Role, SharedString, StatefulInteractiveElement as _, Styled as _, Task, Window, div, rems,
};

use crate::jobs::{ChildKind, CliError, failure_text, sentence};
use crate::state::AppState;
use crate::ui::text_line;
use crate::ui::type_scale::{TextRole, TextRoleExt as _};

/// One section the form offers.
#[derive(Clone, Debug, PartialEq)]
pub struct SectionChoice {
    pub section: ExportSection,
    pub label: String,
    /// The meeting has it (absent ones are shown disabled).
    pub available: bool,
    /// It predates transcript or name changes.
    pub stale: bool,
    pub checked: bool,
}

/// The sections of `meeting`: transcript, summary, decisions, action items,
/// then each template result; every available one checked.
pub fn section_choices(meeting: &Meeting) -> Vec<SectionChoice> {
    let available = available_sections(meeting);
    let enrichment_stale = meeting.enrichment.is_some() && ops::enrichment_is_stale(meeting);
    let mut out = vec![
        (ExportSection::Transcript, "Transcript".to_owned(), false),
        (
            ExportSection::Summary,
            "Summary".to_owned(),
            enrichment_stale,
        ),
        (
            ExportSection::Decisions,
            "Decisions".to_owned(),
            enrichment_stale,
        ),
        (
            ExportSection::Actions,
            "Action items".to_owned(),
            enrichment_stale,
        ),
        (
            ExportSection::Notes,
            "Bookmarks and notes".to_owned(),
            false,
        ),
    ];
    for result in &meeting.template_results {
        out.push((
            ExportSection::Template(result.template_id.clone()),
            result.template_name.clone(),
            ops::template_is_stale(meeting, result),
        ));
    }
    out.into_iter()
        .map(|(section, label, stale)| {
            let has = available.contains(&section);
            SectionChoice {
                section,
                label,
                available: has,
                stale: has && stale,
                checked: has,
            }
        })
        .collect()
}

/// `export <meeting> --include a,b,… --format md|txt|json [--no-timestamps]
/// --spec-stdin` (the runner adds `--library <lib> --json`); the
/// destination goes on stdin ([`export_spec`]), never in argv.
pub fn export_args(
    meeting: &MeetingId,
    sections: &[ExportSection],
    format: ExportFormat,
    timestamps: bool,
) -> Vec<OsString> {
    let include = sections
        .iter()
        .map(ExportSection::name)
        .collect::<Vec<_>>()
        .join(",");
    let mut args: Vec<OsString> = vec![
        "export".into(),
        meeting.as_str().into(),
        "--include".into(),
        include.into(),
        "--format".into(),
        format.extension().into(),
    ];
    if !timestamps {
        args.push("--no-timestamps".into());
    }
    args.push("--spec-stdin".into());
    args
}

/// The stdin of [`export_args`]: `{"out": <path>}`. A path that is not
/// UTF-8 cannot travel in the JSON spec and is refused.
pub fn export_spec(out: &Path) -> std::result::Result<Vec<u8>, String> {
    let out = out
        .to_str()
        .ok_or_else(|| crate::jobs::unsendable_path(out, "used"))?;
    Ok(serde_json::to_vec(&serde_json::json!({ "out": out })).expect("a spec serializes"))
}

/// “Couldn't export. {Cause}. Choose another folder and try again.”
/// (WRT-08).
pub fn export_failure(error: &CliError) -> String {
    failure_text(
        "Couldn't export.",
        &error.user_text(),
        "Choose another folder and try again.",
    )
}

/// Starts the export of `sections` of `meeting` to `out` through the CLI
/// (destination on stdin); `Err` when `out` cannot travel in the spec.
pub fn run_export(
    state: &AppState,
    meeting: &MeetingId,
    sections: &[ExportSection],
    format: ExportFormat,
    timestamps: bool,
    out: &Path,
    cx: &gpui::App,
) -> std::result::Result<Task<std::result::Result<serde_json::Value, CliError>>, String> {
    if outside_library_parent(state.library().root(), out).is_none() {
        return Err("Choose an existing folder outside the BroScribe library.".to_owned());
    }
    let spec = export_spec(out)?;
    Ok(state.cli().run_with_stdin(
        export_args(meeting, sections, format, timestamps),
        ChildKind::Command,
        spec,
        cx.background_executor(),
    ))
}

/// The file name the save dialog suggests: the meeting's title (or ID)
/// without path separators, with the format's extension.
pub fn suggested_file_name(meeting: &Meeting, format: ExportFormat) -> String {
    let base = meeting
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or(meeting.id.as_str());
    export_file_name(base, format)
}

pub(super) fn export_file_name(base: &str, format: ExportFormat) -> String {
    let base: String = base
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '-',
            c => c,
        })
        .collect();
    format!("{base}.{}", format.extension())
}

fn save_dir(library: &Path, last_dir: Option<&Path>, paths: &dyn AppPaths) -> Option<PathBuf> {
    let library = library.canonicalize().ok()?;
    [
        last_dir.map(Path::to_owned),
        Some(paths.documents_dir()),
        library.parent().map(Path::to_owned),
        Some(std::env::temp_dir()),
    ]
    .into_iter()
    .flatten()
    .filter_map(|path| path.canonicalize().ok())
    .find(|path| path.is_dir() && !path.starts_with(&library))
}

struct GpuiFileActions;

impl FileActions<App> for GpuiFileActions {
    fn choose_save_path(&self, cx: &App, dir: &Path, name: &str) -> SavePath {
        let picked = cx.prompt_for_new_path(dir, Some(name));
        Box::pin(async move {
            match picked.await {
                Ok(Ok(path)) => Ok(path),
                Ok(Err(_)) => Err("Couldn't open the save dialog.".to_owned()),
                Err(_) => Ok(None),
            }
        })
    }

    fn reveal(&self, cx: &App, path: &Path) {
        cx.reveal_path(path);
    }
}

pub(super) fn choose_export_path(
    state: &AppState,
    name: &str,
    cx: &App,
) -> impl std::future::Future<Output = Result<Option<PathBuf>, String>> + 'static {
    let dir = save_dir(
        state.library().root(),
        state.config().export.last_dir.as_deref(),
        NativePlatform::paths().as_ref(),
    );
    let picked = dir.map(|dir| GpuiFileActions.choose_save_path(cx, &dir, name));
    async move {
        match picked {
            Some(picked) => picked.await,
            None => {
                Err("Couldn't find an existing folder outside the BroScribe library.".to_owned())
            }
        }
    }
}

pub(super) fn reveal_file(path: &Path, cx: &App) {
    GpuiFileActions.reveal(cx, path);
}

pub(super) async fn remember_export_dir(
    data_dir: PathBuf,
    library: PathBuf,
    file: PathBuf,
    executor: gpui::BackgroundExecutor,
) -> Option<Config> {
    executor
        .spawn(async move {
            let dir = outside_library_parent(&library, &file)?;
            Config::update(&data_dir, |config| {
                config.export.last_dir = Some(dir);
                Ok(())
            })
            .ok()
            .map(|(config, ())| config)
        })
        .await
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExportEvent {
    /// The form was closed.
    Closed,
    Saved(PathBuf),
}

pub struct ExportForm {
    state: gpui::Entity<AppState>,
    meeting: Meeting,
    sections: Vec<SectionChoice>,
    timestamps: bool,
    format: ExportFormat,
    running: bool,
    focus_first: bool,
    notice: Option<(String, bool)>,
}

impl EventEmitter<ExportEvent> for ExportForm {}

impl ExportForm {
    pub fn new(state: gpui::Entity<AppState>, meeting: Meeting) -> Self {
        Self {
            state,
            sections: section_choices(&meeting),
            meeting,
            timestamps: true,
            format: ExportFormat::Markdown,
            running: false,
            focus_first: true,
            notice: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn sections(&self) -> &[SectionChoice] {
        &self.sections
    }

    #[cfg(test)]
    pub(crate) fn notice(&self) -> Option<&(String, bool)> {
        self.notice.as_ref()
    }

    pub fn toggle_section(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(choice) = self.sections.get_mut(ix).filter(|c| c.available) {
            choice.checked = !choice.checked;
            cx.notify();
        }
    }

    pub fn set_timestamps(&mut self, on: bool, cx: &mut Context<Self>) {
        self.timestamps = on;
        cx.notify();
    }

    pub fn set_format(&mut self, format: ExportFormat, cx: &mut Context<Self>) {
        self.format = format;
        cx.notify();
    }

    /// The checked sections, in the form's order.
    pub fn chosen(&self) -> Vec<ExportSection> {
        self.sections
            .iter()
            .filter(|c| c.available && c.checked)
            .map(|c| c.section.clone())
            .collect()
    }

    /// Asks for a file, then writes the export there through the CLI.
    pub fn export(&mut self, cx: &mut Context<Self>) {
        let sections = self.chosen();
        if self.running || sections.is_empty() {
            return;
        }
        let state = self.state.read(cx);
        let data_dir = state.paths().data_dir.clone();
        let library = state.library().root().to_owned();
        let executor = cx.background_executor().clone();
        let picked =
            choose_export_path(state, &suggested_file_name(&self.meeting, self.format), cx);
        self.running = true;
        self.notice = None;
        cx.notify();
        let (meeting, format, timestamps) = (self.meeting.id.clone(), self.format, self.timestamps);
        cx.spawn(async move |this, cx| {
            let path = match picked.await {
                Ok(Some(path)) => path,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        this.running = false;
                        this.notice = Some((error, true));
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                _ => {
                    this.update(cx, |this, cx| {
                        this.running = false;
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let Ok(started) = this.update(cx, |this, cx| {
                run_export(
                    this.state.read(cx),
                    &meeting,
                    &sections,
                    format,
                    timestamps,
                    &path,
                    cx,
                )
            }) else {
                return;
            };
            let result = match started {
                Ok(task) => task.await.map_err(|e| export_failure(&e)),
                Err(refusal) => Err(failure_text("Couldn't export.", &sentence(&refusal), "")),
            };
            let config = if result.is_ok() {
                remember_export_dir(data_dir, library, path.clone(), executor).await
            } else {
                None
            };
            this.update(cx, |this, cx| {
                this.running = false;
                if let Some(config) = config {
                    this.state
                        .update(cx, |state, cx| state.set_config(config, cx));
                }
                match result {
                    Ok(_) => cx.emit(ExportEvent::Saved(path)),
                    Err(line) => this.notice = Some((line, true)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn running(&self) -> bool {
        self.running
    }

    pub fn close(&mut self, cx: &mut Context<Self>) {
        if !self.running {
            cx.emit(ExportEvent::Closed);
        }
    }
}

impl Render for ExportForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let idle = !self.running;
        let mut sections = div()
            .id("export-sections")
            .role(Role::Group)
            .aria_label("Sections")
            .flex()
            .flex_wrap()
            .gap_2();
        for (ix, choice) in self.sections.iter().enumerate() {
            let mut text = choice.label.clone();
            let mut aria = choice.label.clone();
            if !choice.available {
                text.push_str(" (none yet)");
                aria.push_str(", not available for this meeting");
            } else if choice.stale {
                text.push_str(" (predates transcript changes)");
                aria.push_str(", predates transcript changes");
            }
            sections = sections.child(
                Checkbox::new(("export-section", ix))
                    .label(SharedString::from(text))
                    .accessibility_label(aria)
                    .checked(choice.checked && choice.available)
                    .disabled(!idle || !choice.available)
                    .on_change(cx.listener(move |this, _, _, cx| this.toggle_section(ix, cx))),
            );
        }
        let formats = [
            ("export-md", "Markdown (.md)", ExportFormat::Markdown),
            ("export-txt", "Plain text (.txt)", ExportFormat::Text),
            ("export-json", "JSON (.json)", ExportFormat::Json),
        ];
        let mut format_row = crate::ui::kit::ChoiceGroup::new(
            div().flex().flex_wrap().gap_2().items_center(),
            "export-formats",
            "Format",
        );
        for (id, label, format) in formats {
            let selected = self.format == format;
            format_row = format_row.option(crate::ui::kit::radio_option(
                id,
                label,
                selected,
                idle,
                cx,
                move |this, _, cx| this.set_format(format, cx),
            ));
        }
        let timestamps = self.timestamps;
        let chosen = !self.chosen().is_empty();
        let initial_focus = std::mem::take(&mut self.focus_first);
        let mut el = div().id("export-form")
            .role(Role::Group)
            .aria_label("Export options")
            .flex().flex_col().gap_3()
            .max_h(rems(32.5))
            .overflow_y_scroll()
            .when(initial_focus, |el| el.on_prepaint(|_, window, cx| {
                window.defer(cx, |window, cx| window.focus_next(cx));
            }))
            .child(crate::ui::heading("export-include", "Include"))
            .child(sections)
            .child(
                Checkbox::new("export-timestamps")
                    .label("Include timestamps")
                    .checked(timestamps)
                    .disabled(!idle)
                    .on_change(cx.listener(move |this, checked, _, cx| this.set_timestamps(*checked, cx))),
            )
            .child(crate::ui::kit::field("export-format-label", "Format", None, |_| format_row, cx))
            .when(self.sections.iter().any(|section| section.section == ExportSection::Transcript && section.checked), |el| {
                el.child(text_line("export-local-note", Role::Note,
                    "The file contains the transcript. It stays on this computer unless you move or share it.")
                    .text_role(TextRole::Caption).text_color(theme.muted_foreground))
            })
            .when(!chosen, |el| el.child(text_line("export-no-sections", Role::Label, "Choose at least one section.")
                .text_role(TextRole::Caption)));
        if let Some((text, error)) = &self.notice {
            el = el.child(status_line(
                "export-notice",
                if *error {
                    StatusKind::Failed
                } else {
                    StatusKind::Done
                },
                text.clone(),
                cx,
            ));
        }
        el.child(
            div()
                .flex()
                .gap_2()
                .justify_end()
                .child(
                    button(
                        "export-close",
                        "Cancel",
                        idle,
                        cx,
                        |this: &mut Self, _, cx| this.close(cx),
                    )
                    .outline(),
                )
                .child(
                    Button::new("export-save")
                        .label(if self.running {
                            "Exporting…"
                        } else {
                            "Export…"
                        })
                        .map(|button| crate::ui::kit::primary(button, !idle || !chosen, cx))
                        .small()
                        .disabled(!idle || !chosen)
                        .on_click(cx.listener(|this, _, _, cx| this.export(cx))),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use broscribe_core::Library;
    use gpui::{AppContext as _, TestAppContext};
    use serde_json::json;

    use super::*;
    use crate::views::person_picker::fixtures;

    #[test]
    fn save_location_prefers_the_last_directory_and_refuses_library_aliases() {
        use broscribe_platform::fakes::{FakeFileActions, FakePaths};
        let root = tempfile::tempdir().unwrap();
        let library = root.path().join("Library");
        let inside = library.join("exports");
        let previous = root.path().join("Previous");
        let paths = FakePaths::new(root.path());
        let documents = paths.documents_dir();
        for dir in [&inside, &previous, &documents] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let alias = previous.join("..").join("Library").join("exports");
        assert_eq!(
            save_dir(&library, Some(&alias), &paths),
            Some(documents.canonicalize().unwrap())
        );
        assert_eq!(
            save_dir(&library, Some(&previous), &paths),
            Some(previous.canonicalize().unwrap())
        );
        assert!(outside_library_parent(&library, &alias.join("Synthetic.md")).is_none());
        let dest = previous.join("Synthetic.md");
        let fake = FakeFileActions {
            selected: Some(dest.clone()),
            ..Default::default()
        };
        let dir = save_dir(&library, Some(&alias), &paths).unwrap();
        let picked = fake.choose_save_path(&(), &dir, "Synthetic.md");
        assert_eq!(
            *fake.prompts.borrow(),
            [(documents.canonicalize().unwrap(), "Synthetic.md".to_owned())]
        );
        drop(picked);
        fake.reveal(&(), &dest);
        assert_eq!(*fake.revealed.borrow(), [dest]);
    }

    #[test]
    fn export_destination_travels_on_stdin_only() {
        let m = MeetingId::parse("m-0000a1").unwrap();
        let out = Path::new("/tmp/Synthetic private title.txt");
        let got = export_args(
            &m,
            &[
                ExportSection::Transcript,
                ExportSection::Actions,
                ExportSection::Template("synthetic-notes".into()),
            ],
            ExportFormat::Text,
            false,
        );
        let want: Vec<OsString> = [
            "export",
            "m-0000a1",
            "--include",
            "transcript,actions,template:synthetic-notes",
            "--format",
            "txt",
            "--no-timestamps",
            "--spec-stdin",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        assert_eq!(got, want);
        let spec: serde_json::Value = serde_json::from_slice(&export_spec(out).unwrap()).unwrap();
        assert_eq!(spec, json!({ "out": "/tmp/Synthetic private title.txt" }));
    }

    #[test]
    fn a_destination_that_is_not_utf8_is_refused_before_any_child() {
        #[cfg(unix)]
        let out = {
            use std::os::unix::ffi::OsStrExt as _;
            std::ffi::OsStr::from_bytes(b"/tmp/synthetic-\xff.md").to_owned()
        };
        // Windows paths are UTF-16: an unpaired surrogate is not Unicode.
        #[cfg(windows)]
        let out = {
            use std::os::windows::ffi::OsStringExt as _;
            let mut wide: Vec<u16> = "C:\\synthetic-".encode_utf16().collect();
            wide.push(0xD800);
            wide.extend(".md".encode_utf16());
            std::ffi::OsString::from_wide(&wide)
        };
        assert!(export_spec(Path::new(&out)).is_err());
    }

    /// The seeded meeting with an enrichment and one template result.
    fn generated_meeting(lib: &Library, id: &MeetingId) -> Meeting {
        let mut meeting = lib.load_meeting(id).unwrap();
        meeting.title = Some("Synthetic: weekly/sync".into());
        meeting.enrichment = Some(
            serde_json::from_value(json!({
                "provider": "agent-a", "created_at": "2026-11-15T00:00:00Z",
                "summary": "Synthetic summary.",
                "decisions": [{"id": "d1", "text": "Ship the synthetic build", "at_s": 1.0}],
                "actions": [{"id": "a1", "text": "Write the synthetic notes", "at_s": 2.0}]
            }))
            .unwrap(),
        );
        meeting.template_results = vec![
            serde_json::from_value(json!({
                "template_id": "synthetic-notes", "template_name": "Synthetic notes",
                "instructions_sha256": "00", "provider": "agent-a",
                "created_at": "2026-11-15T00:00:00Z", "markdown": "# Notes\n\nSynthetic."
            }))
            .unwrap(),
        ];
        meeting
    }

    #[gpui::test]
    fn form_offers_present_sections_and_exports_through_the_cli(cx: &mut TestAppContext) {
        use crate::test_support::{ps_path, ps_str, write_cli};
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display();
        let reply = r#"{"path":"x","sections":[],"omitted":[]}"#;
        let cli = write_cli(
            &dir.path().join("synthetic-cli"),
            &format!(
                "printf '%s\\n' \"$*\" >> '{d}/args.log'\n\
                 cat > '{d}/stdin.json'\n\
                 printf '{reply}'\n"
            ),
            &format!(
                "fx_log_args {};fx_save_stdin {};fx_print {}",
                ps_path(&dir.path().join("args.log")),
                ps_path(&dir.path().join("stdin.json")),
                ps_str(reply),
            ),
        );
        let (app, lib, s) = fixtures::state_with_cli(cx, dir.path(), cli);

        // Without generated content only the transcript can be exported.
        let bare = lib.load_meeting(&s.meeting).unwrap();
        let choices = section_choices(&bare);
        let summary = choices
            .iter()
            .find(|c| c.section == ExportSection::Summary)
            .unwrap();
        assert!(!summary.available && !summary.checked);

        let mut meeting = generated_meeting(&lib, &s.meeting);
        meeting.notes.push(broscribe_core::schema::Note {
            at_s: 4.0,
            text: "Check the synthetic memo.".into(),
        });
        assert_eq!(
            suggested_file_name(&meeting, ExportFormat::Markdown),
            "Synthetic- weekly-sync.md"
        );
        let form = cx.new(|_| ExportForm::new(app.clone(), meeting));
        form.update(cx, |f, cx| {
            let labels: Vec<&str> = f.sections().iter().map(|c| c.label.as_str()).collect();
            assert_eq!(
                labels,
                [
                    "Transcript",
                    "Summary",
                    "Decisions",
                    "Action items",
                    "Bookmarks and notes",
                    "Synthetic notes"
                ]
            );
            assert!(f.sections().iter().all(|c| c.available && c.checked));
            // Untick the decisions, plain text, no timestamps.
            f.toggle_section(2, cx);
            f.set_format(ExportFormat::Text, cx);
            f.set_timestamps(false, cx);
            f.export(cx);
        });
        assert!(cx.did_prompt_for_new_path());
        let out = dir.path().join("synthetic-export.txt");
        let chosen = out.clone();
        cx.simulate_new_path_selection(move |_| Some(chosen));
        cx.run_until_parked();
        let log = std::fs::read_to_string(dir.path().join("args.log")).unwrap();
        assert_eq!(
            log.trim_end(),
            format!(
                "--library {} --json export {} --include transcript,summary,actions,notes,template:synthetic-notes --format txt --no-timestamps --spec-stdin",
                lib.root().display(),
                s.meeting,
            )
        );
        let spec: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("stdin.json")).unwrap()).unwrap();
        assert_eq!(spec, json!({ "out": out.to_str().unwrap() }));
        form.read_with(cx, |f, _| assert!(f.notice().is_none()));
        app.read_with(cx, |app, _| {
            assert_eq!(
                app.config().export.last_dir,
                Some(dir.path().canonicalize().unwrap())
            )
        });
    }
}
