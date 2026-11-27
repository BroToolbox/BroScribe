//! Templates (design §4 meeting view and Settings, §8.4): instructions run
//! against a meeting's transcript by an AI provider, each producing one
//! Markdown result stored with the meeting.
//!
//! - [`TemplatesTab`], the meeting view's Templates tab, lists the union of
//!   the current definitions and the meeting's stored results: a result
//!   whose definition was deleted keeps its stored name, stays readable,
//!   copyable and exportable, and cannot be run. Run opens the shared
//!   [`GeneratePanel`] options popover. The disclosure opens the result
//!   (names substituted through the
//!   result's own line attribution) drawn as one named node per Markdown
//!   block ([`markdown_blocks`], parsed by the `markdown` crate: headings,
//!   paragraphs, code blocks, lists with their nested lists, and tables,
//!   all selectable in reading order, so assistive technology reads it and
//!   a selection runs across it), Copy and Export (both of the Markdown
//!   source). Struck-out text is drawn struck through and read with
//!   "(deleted)" after it, code spans are in the monospace font, strong and
//!   emphasised text bold and italic, and a link to a web address is a
//!   `Link` node that a click or Enter opens through the platform's
//!   `BrowserLauncher` ([`TemplatesTab::open_link`]; a failure is an alert
//!   with no address, [`LINK_FAILED`]). Each
//!   open result is parsed once per change of its Markdown, not per frame.
//! - [`TemplatesSettings`], Settings → Templates: the list, New, Edit (name,
//!   description, instructions), Duplicate, Delete (user templates), Reset
//!   (built-ins the user overrode) and "Run automatically after each
//!   meeting" (`[templates] auto`).
//!
//! Definitions are read with `broscribe_core::templates::list` on the
//! background executor; every change goes through the CLI (`templates new |
//! edit <id> --stdin` with `{"name","description","body"}` on stdin, `delete`,
//! `reset`, `auto add | remove`), so instructions never reach argv. The
//! editor keeps the definition it opened on as its baseline and sends it
//! with an edit (a new template is create-only): the CLI writes only while
//! the definition still matches, under the library write lock, and
//! otherwise nothing; the editor then shows the conflict, keeping the draft
//! until the user saves over the other version or takes it. A save's reply
//! only touches the editor that started it, never one opened after a Cancel.

use std::collections::HashMap;
use std::ffi::OsString;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use broscribe_core::export::ExportFormat;
use broscribe_core::ops;
use broscribe_core::queue::JobState;
use broscribe_core::render::{PeopleLookup, template_result_md};
use broscribe_core::schema::{Meeting, TemplateResult};
use broscribe_core::templates::{self, Template};
use broscribe_core::{Error, MeetingId};
use broscribe_platform::system::check_web_url;
use gpui::ScrollHandle;
use gpui::base::SelectableText;
use gpui::component::IconName;
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::checkbox::Checkbox;
use gpui::component::input::{Input, InputState, Textarea, TextareaState};
use gpui::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, ElementId, Entity, EventEmitter,
    Focusable as _, FontStyle, FontWeight, HighlightStyle, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, Role, SharedString, StatefulInteractiveElement as _,
    StrikethroughStyle, Styled as _, Subscription, UnderlineStyle, Window, div, px,
};
use markdown::mdast;

use crate::generation::GenerationTarget;
use crate::jobs::{ChildKind, core_user_text, failure_text, sentence};
use crate::state::{AppEvent, AppState, SettingsPage};
use crate::ui::format::{local_date, local_date_time, spoken_date_time, spoken_local_date};
use crate::ui::kit::{self, BadgeTone, StatusKind, button};
use crate::ui::rich_text::{OpenLink, RichLink, RichText};
use crate::ui::type_scale::{TextRole, TextRoleExt as _};
use crate::ui::{Spoken as _, text_line};
use crate::views::export::run_export;
use crate::views::generate::{
    GenerateEvent, GeneratePanel, Phase, ProviderLookup, is_usable_provider, job_state,
    providers_fingerprint,
};
use crate::views::meeting::credit_text;
use crate::views::settings::{args, muted, reload_config};

// ---- pure helpers (tested) ----

/// One row of the Templates tab.
#[derive(Clone, Debug, PartialEq)]
pub struct TemplateRow {
    pub id: String,
    /// The definition's name, or the stored one when it was deleted.
    pub name: String,
    pub description: String,
    /// A definition exists (built-in or user): Run is possible.
    pub defined: bool,
    pub result: Option<TemplateResult>,
    pub stale: bool,
}

/// The current definitions in their order, then results whose definition
/// is gone.
pub fn template_rows(definitions: &[Template], meeting: &Meeting) -> Vec<TemplateRow> {
    let result = |id: &str| {
        meeting
            .template_results
            .iter()
            .find(|r| r.template_id == id)
    };
    let mut rows: Vec<TemplateRow> = definitions
        .iter()
        .map(|t| {
            let stored = result(&t.id);
            TemplateRow {
                id: t.id.clone(),
                name: t.name.clone(),
                description: t.description.clone(),
                defined: true,
                stale: stored.is_some_and(|r| ops::template_is_stale(meeting, r)),
                result: stored.cloned(),
            }
        })
        .collect();
    for stored in &meeting.template_results {
        if !definitions.iter().any(|t| t.id == stored.template_id) {
            rows.push(TemplateRow {
                id: stored.template_id.clone(),
                name: stored.template_name.clone(),
                description: String::new(),
                defined: false,
                stale: ops::template_is_stale(meeting, stored),
                result: Some(stored.clone()),
            });
        }
    }
    rows
}

/// `{"name","description","body"[,"baseline"]}`: what `templates new | edit
/// <id> --stdin` reads. `baseline` (edit only) is the definition the editor
/// opened on: the CLI writes only while the definition still matches it,
/// under the library write lock (`template_changed` otherwise).
pub fn template_stdin(
    name: &str,
    description: &str,
    body: &str,
    baseline: Option<&Template>,
) -> Vec<u8> {
    let mut input = serde_json::json!({
        "name": name,
        "description": description,
        "body": body,
    });
    if let Some(baseline) = baseline {
        input["baseline"] = serde_json::to_value(baseline).expect("a template serializes");
    }
    serde_json::to_vec(&input).expect("a template serializes")
}

/// The definition a successful `templates new | edit` wrote, read from its
/// reply (`{"template": {…}}`): the baseline of a draft that stays open.
fn saved_template(reply: &serde_json::Value) -> Option<Template> {
    let t = &reply["template"];
    let text = |key: &str| t[key].as_str().map(str::to_owned);
    Some(Template {
        id: text("id")?,
        name: text("name")?,
        description: text("description")?,
        body: text("body")?,
        builtin: t["builtin"].as_bool()?,
        overridden: t["overridden"].as_bool()?,
    })
}

/// Error codes of a save refused because the definition is not the one
/// the editor opened on: edited, reset or deleted since (`edit`), or
/// created meanwhile (`new`).
const CONFLICT_CODES: [&str; 2] = ["template_changed", "already_exists"];

/// `templates edit <id> --stdin` for an existing template, else `templates
/// new <id> --stdin`.
pub fn save_template_args(id: &str, existing: bool) -> Vec<OsString> {
    args([
        "templates",
        if existing { "edit" } else { "new" },
        id,
        "--stdin",
    ])
}

/// `templates auto add | remove <id>`.
pub fn auto_args(id: &str, on: bool) -> Vec<OsString> {
    args(["templates", "auto", if on { "add" } else { "remove" }, id])
}

/// A free ID for a copy of `id`: `<id>-copy`, `<id>-copy-2`, … within the
/// 40-character limit.
pub fn duplicate_id(id: &str, taken: &[String]) -> String {
    let stem: String = id.chars().take(30).collect();
    let mut candidate = format!("{stem}-copy");
    let mut n = 2;
    while taken.contains(&candidate) {
        candidate = format!("{stem}-copy-{n}");
        n += 1;
    }
    candidate
}

fn list_templates(data_dir: &std::path::Path) -> std::result::Result<Vec<Template>, String> {
    templates::list(data_dir).map_err(|e| {
        tracing::warn!(code = e.code(), "reading the templates");
        failure_text("Couldn't load the templates.", &core_user_text(&e), "")
    })
}

// ---- the meeting tab ----

/// The tab's alert when a result's link does not open. It names no
/// address: the launcher's error may hold one, and a link's words may hide
/// a query string.
pub const LINK_FAILED: &str = "Couldn't open this link in your browser.";

/// What one load of the tab read.
struct TabLoad {
    definitions: std::result::Result<Vec<Template>, String>,
    jobs: HashMap<String, Option<JobState>>,
}

struct TemplatePanel {
    entity: Entity<GeneratePanel>,
    _subscriptions: [Subscription; 2],
}

#[derive(Default)]
struct ResultActions {
    exporting: bool,
    feedback: Option<ResultFeedback>,
}

enum ResultFeedback {
    Copied,
    Exporting,
    Exported(PathBuf),
    Failed(String),
}

const DELETED_RUN_REASON: &str = "The template was deleted, so it can’t run again.";
const TEMPLATES_INTRO: &str = "Templates use an AI provider you turned on to write notes, an outline, a follow-up email and more from the transcript.";
const NO_TEMPLATE_PROVIDER: &str =
    "Templates are written by an AI provider. Turn one on to run them.";

pub struct TemplatesTab {
    state: Entity<AppState>,
    meeting_id: MeetingId,
    meeting: Option<(Meeting, PeopleLookup)>,
    definitions: Option<std::result::Result<Vec<Template>, String>>,
    jobs: HashMap<String, Option<JobState>>,
    load_generation: u64,
    open: HashMap<String, bool>,
    panels: HashMap<String, TemplatePanel>,
    result_actions: HashMap<String, ResultActions>,
    notice: Option<(String, bool)>,
    catalog: Option<(String, ProviderLookup)>,
    auto: Vec<String>,
    window: Option<gpui::AnyWindowHandle>,
    subscriptions: Vec<Subscription>,
    /// The Templates tab is shown.
    visible: bool,
    /// Each open result's blocks, with the Markdown (names substituted) they
    /// were parsed from: a frame parses again only a result whose Markdown
    /// changed, never one redrawn for something else (playback ticks).
    parsed: HashMap<String, (String, Rc<Vec<ResultBlock>>)>,
    scroll: ScrollHandle,
}

impl EventEmitter<GenerateEvent> for TemplatesTab {}

impl TemplatesTab {
    pub fn new(state: Entity<AppState>, meeting_id: MeetingId) -> Self {
        Self {
            state,
            meeting_id,
            meeting: None,
            definitions: None,
            jobs: HashMap::new(),
            load_generation: 0,
            open: HashMap::new(),
            panels: HashMap::new(),
            result_actions: HashMap::new(),
            notice: None,
            catalog: None,
            auto: Vec::new(),
            window: None,
            subscriptions: Vec::new(),
            visible: false,
            parsed: HashMap::new(),
            scroll: ScrollHandle::new(),
        }
    }

    /// The tab's scroll position.
    pub fn scroll_handle(&self) -> &ScrollHandle {
        &self.scroll
    }

    /// The meeting as the view loaded it; reads the definitions and the
    /// template jobs again.
    pub fn set_meeting(&mut self, meeting: Meeting, people: PeopleLookup, cx: &mut Context<Self>) {
        self.meeting = Some((meeting, people));
        self.reload(cx);
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some((meeting, _)) = &self.meeting else {
            return;
        };
        self.load_generation += 1;
        let generation = self.load_generation;
        let (lib, data_dir) = {
            let s = self.state.read(cx);
            (s.library().clone(), s.paths().data_dir.clone())
        };
        let meeting_id = self.meeting_id.clone();
        let stored: Vec<String> = meeting
            .template_results
            .iter()
            .map(|r| r.template_id.clone())
            .collect();
        let task = cx.background_executor().spawn(async move {
            let definitions = list_templates(&data_dir);
            let mut ids: Vec<String> = definitions
                .as_ref()
                .map(|d| d.iter().map(|t| t.id.clone()).collect())
                .unwrap_or_default();
            ids.extend(stored);
            let jobs = ids
                .into_iter()
                .map(|id| {
                    let job = GenerationTarget::Template {
                        id: id.clone(),
                        name: String::new(),
                    }
                    .job_id(&meeting_id);
                    let state = job_state(lib.root(), &job).ok().flatten().map(|(s, _)| s);
                    (id, state)
                })
                .collect();
            TabLoad { definitions, jobs }
        });
        cx.spawn(async move |this, cx| {
            let load = task.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.definitions = Some(load.definitions);
                this.jobs = load.jobs;
                this.sync_panels(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The rows shown: definitions and stored results.
    pub fn rows(&self) -> Vec<TemplateRow> {
        let Some((meeting, _)) = &self.meeting else {
            return Vec::new();
        };
        let definitions = match &self.definitions {
            Some(Ok(d)) => d.as_slice(),
            _ => &[],
        };
        template_rows(definitions, meeting)
    }

    #[cfg(test)]
    pub fn panel(&self, id: &str) -> Option<&Entity<GeneratePanel>> {
        self.panels.get(id).map(|panel| &panel.entity)
    }

    fn is_open(&self, id: &str) -> bool {
        self.open.get(id).copied().unwrap_or_else(|| {
            self.meeting.as_ref().is_some_and(|(meeting, _)| {
                meeting
                    .template_results
                    .iter()
                    .any(|result| result.template_id == id)
            })
        })
    }

    #[cfg(test)]
    pub fn notice(&self) -> Option<&(String, bool)> {
        self.notice.as_ref()
    }

    fn toggle(&mut self, id: &str, cx: &mut Context<Self>) {
        self.open.insert(id.to_owned(), !self.is_open(id));
        cx.notify();
    }

    /// In-flight jobs remain visible in the header of a collapsed row.
    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible != visible {
            self.visible = visible;
            for panel in self.panels.values() {
                panel.entity.update(cx, |p, cx| {
                    if !visible {
                        p.dismiss_template_options(self.window, cx);
                    }
                    p.set_visible(visible, cx);
                });
            }
            if visible {
                self.reload(cx);
            }
        }
    }

    fn mount_rows(&mut self, rows: &[TemplateRow], window: &mut Window, cx: &mut Context<Self>) {
        self.panels.retain(|id, panel| {
            if rows.iter().any(|row| &row.id == id) {
                true
            } else {
                panel
                    .entity
                    .update(cx, |p, cx| p.dismiss_template_options(self.window, cx));
                false
            }
        });
        for row in rows {
            let has_previous = self.meeting.as_ref().is_some_and(|(meeting, _)| {
                meeting.previous_template_results.contains_key(&row.id)
            });
            if (!row.defined && !has_previous) || self.panels.contains_key(&row.id) {
                continue;
            }
            let target = GenerationTarget::Template {
                id: row.id.clone(),
                name: row.name.clone(),
            };
            let state = self.state.clone();
            let meeting = self.meeting_id.clone();
            let entity = cx.new(|cx| GeneratePanel::new(state, meeting, target, window, cx));
            entity.update(cx, |p, cx| p.set_visible(self.visible, cx));
            Self::sync_panel(
                &entity,
                row,
                self.jobs.get(&row.id).copied().flatten(),
                has_previous,
                cx,
            );
            let subscriptions = [
                cx.observe(&entity, |_, _, cx| cx.notify()),
                cx.subscribe(&entity, |_, _, event, cx| {
                    if matches!(event, GenerateEvent::Finished) {
                        cx.emit(GenerateEvent::Finished);
                    }
                }),
            ];
            self.panels.insert(
                row.id.clone(),
                TemplatePanel {
                    entity,
                    _subscriptions: subscriptions,
                },
            );
        }
    }

    fn sync_panels(&mut self, cx: &mut Context<Self>) {
        for row in self.rows() {
            if let Some(panel) = self.panels.get(&row.id) {
                Self::sync_panel(
                    &panel.entity,
                    &row,
                    self.jobs.get(&row.id).copied().flatten(),
                    self.meeting.as_ref().is_some_and(|(meeting, _)| {
                        meeting.previous_template_results.contains_key(&row.id)
                    }),
                    cx,
                );
            }
        }
    }

    fn sync_panel(
        panel: &Entity<GeneratePanel>,
        row: &TemplateRow,
        job: Option<JobState>,
        can_restore: bool,
        cx: &mut Context<Self>,
    ) {
        panel.update(cx, |p, cx| {
            p.set_definition_missing(!row.defined, cx);
            p.set_template_name(&row.name);
            p.set_result_selection(
                row.result
                    .as_ref()
                    .map(|result| (result.provider.as_str(), result.requested_model.as_deref())),
                cx,
            );
            p.set_can_restore(can_restore, cx);
            p.set_meeting(row.result.as_ref().map(|r| r.created_at), false, job, cx);
        });
    }

    fn load_catalog(&mut self, cx: &mut Context<Self>) {
        let fingerprint = providers_fingerprint(self.state.read(cx).config());
        if self
            .catalog
            .as_ref()
            .is_some_and(|(loaded, _)| loaded == &fingerprint)
        {
            return;
        }
        let lookup = self
            .state
            .update(cx, |state, cx| state.generation_providers(cx));
        self.catalog = Some((fingerprint, lookup.clone()));
        cx.spawn(async move |tab, cx| {
            let _ = lookup.await;
            tab.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }

    fn open_settings(&mut self, page: SettingsPage, cx: &mut Context<Self>) {
        self.state
            .update(cx, |_, cx| cx.emit(AppEvent::OpenSettings(Some(page))));
    }

    /// The result as shown and copied: Markdown with names substituted.
    pub fn result_markdown(&self, id: &str) -> Option<String> {
        let (meeting, people) = self.meeting.as_ref()?;
        let result = meeting
            .template_results
            .iter()
            .find(|r| r.template_id == id)?;
        Some(template_result_md(meeting, result, people))
    }

    /// Copies the result's Markdown to the clipboard.
    pub fn copy(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(text) = self.result_markdown(id) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
            self.result_actions
                .entry(id.to_owned())
                .or_default()
                .feedback = Some(ResultFeedback::Copied);
            cx.notify();
        }
    }

    /// Opens a web link of a result (http or https only, which the launcher
    /// checks again) through the platform's browser launcher, off the UI
    /// thread. A failure shows as the tab's alert, [`LINK_FAILED`], which
    /// holds no address: a launcher's error may quote it (macOS's does),
    /// and a link's words can hide a query string. Only the error's code
    /// is logged, never the address or the error's text.
    pub fn open_link(&mut self, url: &str, cx: &mut Context<Self>) {
        let Some(browser) = self.state.read(cx).browser() else {
            tracing::warn!("no browser launcher; a template link was not opened");
            self.notice = Some((LINK_FAILED.to_owned(), true));
            cx.notify();
            return;
        };
        let url = url.to_owned();
        let opened = cx
            .background_executor()
            .spawn(async move { browser.open(&url) });
        cx.spawn(async move |this, cx| {
            if let Err(e) = opened.await {
                tracing::warn!(code = e.code(), "a template link could not be opened");
                this.update(cx, |this, cx| {
                    this.notice = Some((LINK_FAILED.to_owned(), true));
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// The blocks of each open result as parsed from its current Markdown,
    /// parsing only those whose Markdown changed; closed results are
    /// forgotten.
    fn refresh_parsed(&mut self, rows: &[TemplateRow]) {
        let open: Vec<String> = rows
            .iter()
            .filter(|row| row.result.is_some() && self.is_open(&row.id))
            .map(|row| row.id.clone())
            .collect();
        self.parsed.retain(|id, _| open.contains(id));
        for id in open {
            let source = self.result_markdown(&id).unwrap_or_default();
            if self
                .parsed
                .get(&id)
                .is_some_and(|(parsed, _)| *parsed == source)
            {
                continue;
            }
            let blocks = Rc::new(markdown_blocks(&source));
            self.parsed.insert(id, (source, blocks));
        }
    }

    /// Exports the result as Markdown: save dialog, then `broscribe export
    /// <m> --include template:<id> --format md --spec-stdin` (the path on
    /// stdin).
    pub fn export(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some((meeting, _)) = &self.meeting else {
            return;
        };
        if self
            .result_actions
            .get(id)
            .is_some_and(|actions| actions.exporting)
        {
            return;
        }
        let Some(result) = meeting
            .template_results
            .iter()
            .find(|r| r.template_id == id)
        else {
            return;
        };
        let title = meeting.title.as_deref().unwrap_or(meeting.id.as_str());
        let name = crate::views::export::export_file_name(
            &format!("{title} – {}", result.template_name),
            ExportFormat::Markdown,
        );
        let state = self.state.read(cx);
        let data_dir = state.paths().data_dir.clone();
        let library = state.library().root().to_owned();
        let executor = cx.background_executor().clone();
        let picked = crate::views::export::choose_export_path(state, &name, cx);
        let id = id.to_owned();
        let section = broscribe_core::export::ExportSection::Template(id.clone());
        let meeting_id = self.meeting_id.clone();
        self.result_actions.insert(
            id.clone(),
            ResultActions {
                exporting: true,
                feedback: Some(ResultFeedback::Exporting),
            },
        );
        cx.notify();
        cx.spawn(async move |this, cx| {
            let path = match picked.await {
                Ok(Some(path)) => path,
                result => {
                    this.update(cx, |this, cx| {
                        this.finish_export(
                            &id,
                            result
                                .err()
                                .map(|cause| ResultFeedback::Failed(Self::export_refusal(&cause))),
                            cx,
                        );
                    })
                    .ok();
                    return;
                }
            };
            let Ok(started) = this.update(cx, |this, cx| {
                run_export(
                    this.state.read(cx),
                    &meeting_id,
                    &[section],
                    ExportFormat::Markdown,
                    true,
                    &path,
                    cx,
                )
            }) else {
                return;
            };
            let result = match started {
                Ok(task) => task
                    .await
                    .map_err(|e| crate::views::export::export_failure(&e)),
                Err(refusal) => Err(Self::export_refusal(&refusal)),
            };
            let config = if result.is_ok() {
                crate::views::export::remember_export_dir(data_dir, library, path.clone(), executor)
                    .await
            } else {
                None
            };
            this.update(cx, |this, cx| {
                if let Some(config) = config {
                    this.state
                        .update(cx, |state, cx| state.set_config(config, cx));
                }
                this.finish_export(
                    &id,
                    Some(match result {
                        Ok(_) => ResultFeedback::Exported(path),
                        Err(line) => ResultFeedback::Failed(line),
                    }),
                    cx,
                );
            })
            .ok();
        })
        .detach();
    }

    fn export_refusal(cause: &str) -> String {
        failure_text(
            "Couldn't export.",
            &sentence(cause),
            "Choose another folder, then try again.",
        )
    }

    fn finish_export(&mut self, id: &str, result: Option<ResultFeedback>, cx: &mut Context<Self>) {
        if let Some(actions) = self.result_actions.get_mut(id) {
            actions.exporting = false;
            // A Copy after the export began is newer feedback for this row.
            if matches!(actions.feedback, Some(ResultFeedback::Exporting)) {
                actions.feedback = result;
            }
            cx.notify();
        }
    }

    fn render_row(
        &self,
        ix: usize,
        row: &TemplateRow,
        can_run: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let open = self.is_open(&row.id);
        let deleted = matches!(self.definitions, Some(Ok(_))) && !row.defined;
        let id = row.id.clone();
        let panel = self.panels.get(&row.id).map(|p| &p.entity);
        let phase = panel.map(|p| p.read(cx).phase());
        let (tone, status, spoken) = match phase {
            Some(Phase::Queued(_)) => (BadgeTone::Neutral, "Queued".into(), "Queued".into()),
            Some(Phase::Submitting | Phase::Running(_)) => (
                BadgeTone::Neutral,
                "Generating…".into(),
                "Generating".into(),
            ),
            _ if row.stale => (
                BadgeTone::Attention,
                "Out of date".into(),
                "Out of date".into(),
            ),
            _ => match &row.result {
                Some(result) => (
                    BadgeTone::Neutral,
                    format!("Ready · {}", local_date(&result.created_at)),
                    format!("Ready, {}", spoken_local_date(&result.created_at)),
                ),
                None => (BadgeTone::Outline, "Not run".into(), "Not run".into()),
            },
        };
        let automatic = row.defined && self.auto.contains(&row.id);
        let mut disclosure_name = format!("{}, {spoken}", row.name);
        if automatic {
            disclosure_name.push_str(", runs automatically");
        }
        if deleted {
            disclosure_name.push_str(", template deleted");
        }

        let trigger = if deleted {
            Some(
                kit::disable_with_reason(
                    Button::new(SharedString::from(format!("template-run-{}", row.id)))
                        .label("Run…")
                        .accessibility_label(format!("Run {}…", row.name))
                        .outline()
                        .small(),
                    true,
                    DELETED_RUN_REASON,
                    cx,
                )
                .into_any_element(),
            )
        } else if can_run && row.defined {
            let width = self.scroll.bounds().size.width;
            panel.map(|panel| {
                panel.update(cx, |p, cx| {
                    p.set_options_width(width, cx);
                    p.template_run_trigger(window, cx)
                })
            })
        } else {
            None
        };

        let status_detail = panel.and_then(|panel| panel.update(cx, |p, cx| p.render_status(cx)));
        let restore = panel.and_then(|panel| panel.update(cx, |p, cx| p.restore_control(cx)));
        let theme = cx.theme();
        let mut el = div()
            .id(SharedString::from(format!("template-row-{}", row.id)))
            .role(Role::Group)
            .aria_label(SharedString::from(format!("{} template", row.name)))
            .flex()
            .flex_col()
            .flex_none()
            .gap_2()
            .p_3()
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(
                        kit::disclosure(
                            ("template-toggle", ix),
                            row.name.clone(),
                            open,
                            cx,
                            move |this: &mut Self, _, cx| this.toggle(&id, cx),
                        )
                        .flex_1()
                        .min_w_0()
                        .accessibility_label(disclosure_name)
                        .child(kit::badge(tone, status, cx))
                        .when(automatic, |button| {
                            button.child(kit::badge(BadgeTone::Neutral, "Runs automatically", cx))
                        })
                        .when(deleted, |button| {
                            button.child(kit::badge(BadgeTone::Outline, "Template deleted", cx))
                        })
                        .child(div().flex_1()),
                    )
                    .children(restore)
                    .children(trigger),
            )
            .children(status_detail);
        if !row.description.is_empty() {
            el = el.child(
                text_line(
                    ("template-description", ix),
                    Role::Label,
                    row.description.clone(),
                )
                .text_role(TextRole::Caption)
                .text_color(theme.muted_foreground),
            );
        }
        if !open {
            return el;
        }
        let Some(result) = &row.result else {
            return el.child(
                text_line(
                    ("template-not-run", ix),
                    Role::Label,
                    "Not run for this meeting yet.",
                )
                .text_role(TextRole::Caption)
                .text_color(theme.muted_foreground),
            );
        };
        if deleted {
            el = el.child(
                text_line(
                    ("template-deleted", ix),
                    Role::Label,
                    "The template was deleted. Its result stays with this meeting.",
                )
                .text_role(TextRole::Caption)
                .text_color(theme.muted_foreground),
            );
        }
        let provider = self
            .state
            .read(cx)
            .provider_name(&result.provider)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("unknown provider ({})", result.provider));
        let (credit, credit_aria) = template_credit(result, &provider);
        el = el
            .child(
                div()
                    .id(("template-credit", ix))
                    .spoken(Role::Label, credit_aria)
                    .text_role(TextRole::Caption)
                    .text_color(theme.muted_foreground)
                    .child(credit),
            )
            .child(
                text_line(
                    ("template-ai-note", ix),
                    Role::Label,
                    crate::views::generate::AI_RESULT_NOTE,
                )
                .text_role(TextRole::Caption)
                .text_color(theme.muted_foreground),
            );
        if row.stale {
            el = el.child(kit::status_line(
                ("template-stale", ix),
                StatusKind::Attention,
                "The transcript or its names changed after this result was written.",
                cx,
            ));
        }
        let blocks = self
            .parsed
            .get(&row.id)
            .map(|(_, blocks)| blocks.clone())
            .unwrap_or_default();
        let tab = cx.entity().downgrade();
        let open: OpenLink = Rc::new(move |url, _, cx| {
            tab.update(cx, |tab, cx| tab.open_link(url, cx)).ok();
        });
        // Each block is its own named node; the article's name is text no
        // block carries (INS-59).
        el.child(
            div()
                .id(("template-result", ix))
                .role(Role::Article)
                .aria_label(SharedString::from(format!("{} result", row.name)))
                .flex()
                .flex_col()
                .gap_2()
                .children(result_document(
                    "template", ix, &row.name, &blocks, open, cx,
                )),
        )
        .child(self.render_result_actions(ix, row, cx))
    }

    fn render_result_actions(
        &self,
        ix: usize,
        row: &TemplateRow,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let copy_id = row.id.clone();
        let export_id = row.id.clone();
        let actions = self.result_actions.get(&row.id);
        let feedback = actions
            .and_then(|actions| actions.feedback.as_ref())
            .map(|feedback| {
                let (kind, words) = match feedback {
                    ResultFeedback::Copied => (StatusKind::Done, "Copied.".to_owned()),
                    ResultFeedback::Exporting => (StatusKind::Pending, "Exporting…".to_owned()),
                    ResultFeedback::Exported(path) => (
                        StatusKind::Done,
                        format!(
                            "Exported {}",
                            path.file_name()
                                .unwrap_or(path.as_os_str())
                                .to_string_lossy()
                        ),
                    ),
                    ResultFeedback::Failed(words) => (StatusKind::Failed, words.clone()),
                };
                kit::status_line(
                    format!("template-result-status-{}", row.id),
                    kind,
                    words,
                    cx,
                )
                .when_some(
                    match feedback {
                        ResultFeedback::Exported(path) => {
                            Some(format!("Exported {}", path.display()))
                        }
                        _ => None,
                    },
                    |line, label| line.aria_label(label),
                )
            });
        div()
            .id(("template-result-actions", ix))
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(
                button(
                    ("template-copy", ix),
                    "Copy",
                    true,
                    cx,
                    move |this, _, cx| this.copy(&copy_id, cx),
                )
                .ghost()
                .small()
                .icon(IconName::Copy)
                .accessibility_label(format!("Copy {}", row.name)),
            )
            .child(
                button(
                    ("template-export", ix),
                    "Export…",
                    !actions.is_some_and(|actions| actions.exporting),
                    cx,
                    move |this, _, cx| this.export(&export_id, cx),
                )
                .outline()
                .small()
                .accessibility_label(format!("Export {}", row.name)),
            )
            .children(feedback)
    }
}

/// The credit line of a template result (like the summary's).
pub fn template_credit(result: &TemplateResult, provider: &str) -> (String, String) {
    credit_text(
        "AI result",
        provider,
        result.model.as_deref(),
        result.requested_model.as_deref(),
        result.model_reported,
        (
            &local_date_time(&result.created_at),
            &spoken_date_time(&result.created_at),
        ),
    )
}

/// One block of a template result, as it is drawn and read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResultBlock {
    /// `#` to `######`, or an underlined (setext) title.
    Heading { level: usize, text: Inline },
    /// A paragraph, its soft line breaks read as spaces (a block quote's
    /// paragraphs and raw HTML, kept as written but for its comments, are
    /// paragraphs too).
    Paragraph(Inline),
    /// A fenced or indented code block, its lines exactly as written; drawn
    /// in the monospace font.
    Code(String),
    /// A bullet list (`start` 1) or a numbered one counting from `start`.
    /// An item can hold blocks of its own: a nested list, more paragraphs,
    /// code.
    List {
        ordered: bool,
        start: u64,
        items: Vec<ResultItem>,
    },
    /// A table: its header row, then its rows, each as wide as the header
    /// (a short row ends in empty cells).
    Table {
        header: Vec<Inline>,
        rows: Vec<Vec<Inline>>,
    },
}

/// One item of a [`ResultBlock::List`]: the text of its first paragraph
/// (empty when it opens with something else) and the blocks under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultItem {
    pub text: Inline,
    pub blocks: Vec<ResultBlock>,
}

/// The text of a block as shown, with the marks it keeps (byte ranges of
/// `text`, in order): struck-out text, code spans and the links that open,
/// which its name also says ([`Inline::spoken`]), and emphasis and strong
/// emphasis, which are only drawn (italic and bold, as `TextView` drew
/// them) and read as their words.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inline {
    pub text: String,
    /// `~~struck~~` text, drawn struck through.
    pub struck: Vec<Range<usize>>,
    /// `**strong**` text, drawn bold.
    pub strong: Vec<Range<usize>>,
    /// `*emphasised*` text, drawn italic.
    pub emphasis: Vec<Range<usize>>,
    /// Code spans, drawn in the monospace font.
    pub code: Vec<Range<usize>>,
    /// Links to an http or https address
    /// (`broscribe_platform::system::check_web_url`), which open in the
    /// browser. A link anywhere else (a relative path such as `notes.md`,
    /// `mailto:`) is only its words.
    pub links: Vec<(Range<usize>, String)>,
}

impl Inline {
    /// Text without marks.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ..Self::default()
        }
    }

    /// What assistive technology reads for the block that holds this text:
    /// the text, with "(deleted)" after struck-out text and "(link)" after
    /// the words of a link that opens (that link is also a node of its own,
    /// named by its words), so what the marks show is also said, and the
    /// block never carries its only link's name.
    pub fn spoken(&self) -> String {
        let mut cues: Vec<(usize, u8, &str)> = self
            .links
            .iter()
            .map(|(range, _)| (range.end, 0, " (link)"))
            .chain(self.struck.iter().map(|range| (range.end, 1, " (deleted)")))
            .collect();
        cues.sort_unstable();
        let mut out = String::with_capacity(self.text.len() + cues.len() * 10);
        let mut at = 0;
        for (end, _, cue) in cues {
            out.push_str(&self.text[at..end]);
            out.push_str(cue);
            at = end;
        }
        out.push_str(&self.text[at..]);
        out
    }

    /// `prefix` followed by this text, its marks moved with it.
    fn after(self, prefix: &str) -> Self {
        let n = prefix.len();
        let shift = |range: Range<usize>| range.start + n..range.end + n;
        Self {
            text: format!("{prefix}{}", self.text),
            struck: self.struck.into_iter().map(shift).collect(),
            strong: self.strong.into_iter().map(shift).collect(),
            emphasis: self.emphasis.into_iter().map(shift).collect(),
            code: self.code.into_iter().map(shift).collect(),
            links: self
                .links
                .into_iter()
                .map(|(range, url)| (shift(range), url))
                .collect(),
        }
    }
}

/// Link reference definitions by their normalized label.
type Definitions = HashMap<String, String>;

#[cfg(test)]
thread_local! {
    /// How many times [`markdown_blocks`] parsed on this thread.
    static PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many times [`markdown_blocks`] has parsed on this thread.
#[cfg(test)]
pub(crate) fn parse_count() -> usize {
    PARSES.with(std::cell::Cell::get)
}

/// A template result's Markdown as the blocks [`TemplatesTab`] draws, one
/// named node each (INS-59). The `markdown` crate parses it
/// (`markdown::to_mdast` with `ParseOptions::gfm()`: CommonMark with
/// GitHub's tables, strikethrough, autolinks, task lists and footnotes;
/// math stays off, so `$5 and $10` is text and a line opening with `$$` is
/// prose), so lists nest, fences close as CommonMark says and code keeps
/// every character. This only maps the syntax tree to blocks, each with
/// its inline marks ([`Inline`]). Rules and link definitions draw nothing,
/// and raw HTML comments are hidden. gpui-base's `parse_markdown` (the
/// block tree `TextView` draws) was not used: it always parses math, drops
/// inline tags it does not know and leaves reference links without their
/// address (docs/ux-catalog/7-questions.md D32).
pub fn markdown_blocks(source: &str) -> Vec<ResultBlock> {
    #[cfg(test)]
    PARSES.with(|n| n.set(n.get() + 1));
    match markdown::to_mdast(source, &markdown::ParseOptions::gfm()) {
        Ok(root) => {
            let mut definitions = Definitions::new();
            collect_definitions(&root, &mut definitions);
            let mut blocks = Vec::new();
            push_blocks(
                root.children().map_or(&[][..], Vec::as_slice),
                &definitions,
                &mut blocks,
            );
            blocks
        }
        // Only MDX, which these options leave off, fails to parse; the
        // source is then shown as it is.
        Err(_) if source.trim().is_empty() => Vec::new(),
        Err(_) => vec![ResultBlock::Paragraph(Inline::plain(source))],
    }
}

/// Every link reference definition under `node`; the first one of a label
/// counts, as in CommonMark.
fn collect_definitions(node: &mdast::Node, out: &mut Definitions) {
    if let mdast::Node::Definition(definition) = node {
        out.entry(definition.identifier.clone())
            .or_insert_with(|| definition.url.clone());
    }
    for child in node.children().map_or(&[][..], Vec::as_slice) {
        collect_definitions(child, out);
    }
}

/// Raw HTML without its comments (`<!-- … -->`), which `TextView` never
/// drew either; `None` when it holds none. Other tags stay as written.
fn without_comments(html: &str) -> Option<String> {
    let mut rest = html;
    let mut out = String::new();
    let mut found = false;
    while let Some(start) = rest.find("<!--") {
        let Some(len) = rest[start + 4..].find("-->") else {
            break;
        };
        out.push_str(&rest[..start]);
        rest = &rest[start + 4 + len + 3..];
        found = true;
    }
    found.then(|| out + rest)
}

/// Appends the blocks `nodes` draw to `out`.
fn push_blocks(nodes: &[mdast::Node], definitions: &Definitions, out: &mut Vec<ResultBlock>) {
    use mdast::Node;

    let push_text = |text: Inline, out: &mut Vec<ResultBlock>| {
        if !text.text.trim().is_empty() {
            out.push(ResultBlock::Paragraph(text));
        }
    };
    for node in nodes {
        match node {
            Node::Heading(heading) => {
                let text = inline_text(&heading.children, definitions);
                if !text.text.trim().is_empty() {
                    out.push(ResultBlock::Heading {
                        level: heading.depth.into(),
                        text,
                    });
                }
            }
            Node::Paragraph(paragraph) => {
                push_text(inline_text(&paragraph.children, definitions), out);
            }
            Node::Html(html) => push_text(
                Inline::plain(match without_comments(&html.value) {
                    Some(shown) => shown.trim().to_owned(),
                    None => html.value.clone(),
                }),
                out,
            ),
            Node::Code(code) => {
                if !code.value.trim().is_empty() {
                    out.push(ResultBlock::Code(code.value.clone()));
                }
            }
            Node::List(list) => out.push(ResultBlock::List {
                ordered: list.ordered,
                start: list.start.map_or(1, u64::from),
                items: list
                    .children
                    .iter()
                    .filter_map(|item| match item {
                        Node::ListItem(item) => Some(list_item(item, definitions)),
                        _ => None,
                    })
                    .collect(),
            }),
            Node::Table(table) => out.extend(table_block(table, definitions)),
            Node::Blockquote(quote) => push_blocks(&quote.children, definitions, out),
            Node::FootnoteDefinition(note) => {
                let mark = format!("[^{}]:", note.label.as_deref().unwrap_or(&note.identifier));
                let first = out.len();
                push_blocks(&note.children, definitions, out);
                match out.get_mut(first) {
                    Some(ResultBlock::Paragraph(text)) => {
                        *text = std::mem::take(text).after(&format!("{mark} "));
                    }
                    _ => out.insert(first, ResultBlock::Paragraph(Inline::plain(mark))),
                }
            }
            // Rules, link definitions and front matter draw nothing.
            _ => {}
        }
    }
}

/// A table's header row and rows, each row cut or filled with empty cells
/// to the header's width (GFM); `None` for a table with no row.
fn table_block(table: &mdast::Table, definitions: &Definitions) -> Option<ResultBlock> {
    let mut rows = table.children.iter().filter_map(|row| match row {
        mdast::Node::TableRow(row) => Some(
            row.children
                .iter()
                .map(|cell| match cell {
                    mdast::Node::TableCell(cell) => inline_text(&cell.children, definitions),
                    _ => Inline::default(),
                })
                .collect::<Vec<_>>(),
        ),
        _ => None,
    });
    let header = rows.next()?;
    let width = header.len();
    let rows = rows
        .map(|mut row| {
            row.resize(width, Inline::default());
            row
        })
        .collect();
    Some(ResultBlock::Table { header, rows })
}

/// A list item: the text of its first paragraph (a task's box written
/// before it, `[x]` or `[ ]`) and the blocks after it.
fn list_item(item: &mdast::ListItem, definitions: &Definitions) -> ResultItem {
    let mut blocks = Vec::new();
    push_blocks(&item.children, definitions, &mut blocks);
    let mut text = match blocks.first() {
        Some(ResultBlock::Paragraph(_)) => match blocks.remove(0) {
            ResultBlock::Paragraph(text) => text,
            _ => Inline::default(),
        },
        _ => Inline::default(),
    };
    if let Some(done) = item.checked {
        text = text.after(&format!("[{}] ", if done { 'x' } else { ' ' }));
    }
    ResultItem { text, blocks }
}

/// Inline Markdown as the words shown ([`Inline`]): text (its character
/// references decoded, a soft line break read as a space), code spans
/// exactly as written, the text of emphasis, strong, struck-out text and
/// links (each marked but for links that don't go to an http or https
/// address),
/// an image's alt text, a hard line break as a line break, footnote
/// references and raw HTML as written, raw HTML comments left out.
fn inline_text(nodes: &[mdast::Node], definitions: &Definitions) -> Inline {
    let mut walk = InlineWalk {
        definitions,
        out: Inline::default(),
        after_comment: false,
    };
    walk.push(nodes);
    let mut out = walk.out;
    if walk.after_comment {
        // Nor does a comment at the end leave the space before it.
        let marked = out
            .struck
            .iter()
            .chain(&out.strong)
            .chain(&out.emphasis)
            .chain(&out.code)
            .chain(out.links.iter().map(|(range, _)| range))
            .map(|range| range.end)
            .max()
            .unwrap_or(0);
        let len = out.text.trim_end_matches(' ').len().max(marked);
        out.text.truncate(len);
    }
    out
}

struct InlineWalk<'a> {
    definitions: &'a Definitions,
    out: Inline,
    /// A raw HTML comment was just left out: the space after it goes when
    /// one comes before it (or nothing does), so "a <!-- x --> b" reads
    /// "a b".
    after_comment: bool,
}

impl InlineWalk<'_> {
    fn push_str(&mut self, text: &str) {
        self.after_comment = false;
        self.out.text.push_str(text);
    }

    /// Pushes `children` and returns the range of text they added, if any.
    fn marked(&mut self, children: &[mdast::Node]) -> Option<Range<usize>> {
        let start = self.out.text.len();
        self.push(children);
        let end = self.out.text.len();
        (end > start).then_some(start..end)
    }

    fn link(&mut self, url: &str, children: &[mdast::Node]) {
        let start = self.out.text.len();
        self.push(children);
        let end = self.out.text.len();
        if end > start && check_web_url(url).is_ok() {
            self.out.links.push((start..end, url.to_owned()));
        }
    }

    fn push(&mut self, nodes: &[mdast::Node]) {
        use mdast::Node;
        for node in nodes {
            match node {
                Node::Text(text) => {
                    let text: String = text
                        .value
                        .replace("\r\n", " ")
                        .chars()
                        .map(|c| match c {
                            '\n' | '\r' => ' ',
                            c => c,
                        })
                        .collect();
                    let text = if self.after_comment
                        && (self.out.text.is_empty() || self.out.text.ends_with(' '))
                    {
                        text.trim_start_matches(' ')
                    } else {
                        &text
                    };
                    self.push_str(text);
                }
                Node::InlineCode(mdast::InlineCode { value, .. }) => {
                    let start = self.out.text.len();
                    self.push_str(value);
                    if !value.is_empty() {
                        self.out.code.push(start..self.out.text.len());
                    }
                }
                Node::Html(mdast::Html { value, .. }) => match without_comments(value) {
                    Some(shown) if shown.trim().is_empty() => self.after_comment = true,
                    Some(shown) => self.push_str(&shown),
                    None => self.push_str(value),
                },
                Node::Image(mdast::Image { alt, .. })
                | Node::ImageReference(mdast::ImageReference { alt, .. }) => self.push_str(alt),
                Node::Break(_) => self.push_str("\n"),
                Node::FootnoteReference(note) => {
                    let label = note.label.as_deref().unwrap_or(&note.identifier);
                    self.push_str(&format!("[^{label}]"));
                }
                Node::Delete(delete) => {
                    let range = self.marked(&delete.children);
                    self.out.struck.extend(range);
                }
                Node::Strong(strong) => {
                    let range = self.marked(&strong.children);
                    self.out.strong.extend(range);
                }
                Node::Emphasis(emphasis) => {
                    let range = self.marked(&emphasis.children);
                    self.out.emphasis.extend(range);
                }
                Node::Link(link) => self.link(&link.url, &link.children),
                Node::LinkReference(link) => {
                    let definitions = self.definitions;
                    match definitions.get(&link.identifier) {
                        Some(url) => self.link(url, &link.children),
                        None => self.push(&link.children),
                    }
                }
                other => self.push(other.children().map_or(&[][..], Vec::as_slice)),
            }
        }
    }
}

/// Styled ranges of a text, sorted and without overlap.
type Highlights = Vec<(Range<usize>, HighlightStyle)>;
/// Ranges of a text drawn in another font family.
type FontRanges = Vec<(Range<usize>, SharedString)>;

/// The highlight of each part of `inline`'s text that carries a mark
/// (struck through; bold; italic; a link's colour and underline; a code
/// span's muted background) and the monospace ranges of its code spans.
fn inline_styles(inline: &Inline, colors: &ResultColors) -> (Highlights, FontRanges) {
    let mut cuts: Vec<usize> = inline
        .struck
        .iter()
        .chain(&inline.strong)
        .chain(&inline.emphasis)
        .chain(&inline.code)
        .chain(inline.links.iter().map(|(range, _)| range))
        .flat_map(|range| [range.start, range.end])
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    let within = |part: &Range<usize>, range: &Range<usize>| {
        range.start <= part.start && part.end <= range.end
    };
    let mut highlights = Vec::new();
    for pair in cuts.windows(2) {
        let part = pair[0]..pair[1];
        let struck = inline.struck.iter().any(|range| within(&part, range));
        let strong = inline.strong.iter().any(|range| within(&part, range));
        let emphasis = inline.emphasis.iter().any(|range| within(&part, range));
        let code = inline.code.iter().any(|range| within(&part, range));
        let linked = inline.links.iter().any(|(range, _)| within(&part, range));
        if !(struck || strong || emphasis || code || linked) {
            continue;
        }
        highlights.push((
            part,
            HighlightStyle {
                font_weight: strong.then_some(FontWeight::SEMIBOLD),
                font_style: emphasis.then_some(FontStyle::Italic),
                color: linked.then_some(colors.link),
                background_color: code.then_some(colors.code_background),
                underline: linked.then_some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(colors.link),
                    wavy: false,
                }),
                strikethrough: struck.then_some(StrikethroughStyle {
                    thickness: px(1.),
                    color: None,
                }),
                ..HighlightStyle::default()
            },
        ));
    }
    let mono = inline
        .code
        .iter()
        .map(|range| (range.clone(), colors.mono.clone()))
        .collect();
    (highlights, mono)
}

/// The theme's colours and font a result's marks are drawn with.
struct ResultColors {
    link: Hsla,
    ring: Hsla,
    code_background: Hsla,
    mono: SharedString,
}

/// Numbers the nodes of template result `ix` (of the template `name`) as
/// it is drawn: its blocks in reading order, nested ones included, so
/// every ID is unique; its lists and tables, named "<name> list N" and
/// "<name> table N"; and its selectable runs, so a selection across blocks
/// takes them, and Copy gives them, in reading order (runs without an
/// order would tie and copy in any order). Its links open through `open`.
struct ResultCx<'a> {
    prefix: &'static str,
    ix: usize,
    name: &'a str,
    blocks: usize,
    lists: usize,
    tables: usize,
    runs: u64,
    colors: ResultColors,
    open: OpenLink,
}

impl<'a> ResultCx<'a> {
    fn new(ix: usize, name: &'a str, open: OpenLink, cx: &App) -> Self {
        let theme = cx.theme();
        Self {
            prefix: "template",
            ix,
            name,
            blocks: 0,
            lists: 0,
            tables: 0,
            runs: 0,
            colors: ResultColors {
                link: theme.link,
                ring: theme.ring,
                code_background: theme.muted,
                mono: theme.mono_font_family.clone(),
            },
            open,
        }
    }

    /// The next block's number.
    fn block(&mut self) -> usize {
        self.blocks += 1;
        self.blocks - 1
    }

    /// The next run's place among the window's selectable runs.
    fn order(&mut self) -> u64 {
        self.runs += 1;
        ((self.ix as u64 + 1) << 32) | (self.runs - 1)
    }

    /// `template-<part>-<ix>`, the ID stem of a kind of node.
    fn id(&self, part: &str) -> SharedString {
        SharedString::from(format!("{}-{part}-{}", self.prefix, self.ix))
    }

    /// `inline` as the next selectable run, `id`, drawn with its marks; its
    /// links are nodes `template-a-<ix>-<run>-<i>`, named by their words.
    fn text(&mut self, id: impl Into<ElementId>, inline: &Inline) -> RichText {
        let order = self.order();
        let (highlights, mono) = inline_styles(inline, &self.colors);
        let links = inline
            .links
            .iter()
            .map(|(range, url)| RichLink {
                range: range.clone(),
                name: inline.text[range.clone()].to_owned().into(),
                url: url.clone().into(),
            })
            .collect();
        RichText::new(id, inline.text.clone(), order)
            .highlights(highlights)
            .font_families(mono)
            .links(
                SharedString::from(format!("{}-a-{}-{}", self.prefix, self.ix, self.runs - 1)),
                links,
                self.colors.ring,
                self.open.clone(),
            )
    }
}

pub(crate) fn result_document(
    prefix: &'static str,
    ix: usize,
    name: &str,
    blocks: &[ResultBlock],
    open: OpenLink,
    cx: &App,
) -> Vec<AnyElement> {
    let mut rcx = ResultCx::new(ix, name, open, cx);
    rcx.prefix = prefix;
    blocks
        .iter()
        .map(|block| result_block(&mut rcx, block, cx))
        .collect()
}

/// One block of a template result as named nodes: a heading, a selectable
/// paragraph, a code block in the monospace font, a list or a table. Every
/// text is selectable, in reading order, and named by its spoken form
/// ([`Inline::spoken`]).
fn result_block(rcx: &mut ResultCx, block: &ResultBlock, cx: &App) -> AnyElement {
    let j = rcx.block();
    match block {
        ResultBlock::Heading { level, text } => {
            let text_id = (rcx.id("h-text"), j);
            div()
                .id((rcx.id("h"), j))
                .spoken(Role::Heading, text.spoken())
                .text_role(TextRole::Section)
                .aria_level(level + 1)
                .child(rcx.text(text_id, text))
                .into_any_element()
        }
        ResultBlock::Paragraph(text) => {
            let text_id = (rcx.id("p-text"), j);
            div()
                .id((rcx.id("p"), j))
                .spoken(Role::Label, text.spoken())
                .text_role(TextRole::Reading)
                .child(rcx.text(text_id, text))
                .into_any_element()
        }
        ResultBlock::Code(text) => code_block(
            (rcx.id("code"), j),
            (rcx.id("code-text"), j),
            text.clone(),
            rcx.order(),
            cx,
        )
        .into_any_element(),
        ResultBlock::List {
            ordered,
            start,
            items,
        } => result_list(rcx, j, *ordered, *start, items, cx),
        ResultBlock::Table { header, rows } => result_table(rcx, j, header, rows, cx),
    }
}

/// List `j` of a result: a `List` named "<name> list N" whose items are
/// `ListItem`s named by their text (or "Item K" when they open with no
/// text), each with its place in the list and a decorative marker. The
/// blocks under an item (a nested list, more paragraphs) are drawn inside
/// it, under its text, so they are indented past its marker and a nested
/// list is its item's own.
fn result_list(
    rcx: &mut ResultCx,
    j: usize,
    ordered: bool,
    start: u64,
    items: &[ResultItem],
    cx: &App,
) -> AnyElement {
    rcx.lists += 1;
    let count = items.len();
    let muted = cx.theme().muted_foreground;
    let ix = rcx.ix;
    let item_id = SharedString::from(format!("{}-li-{ix}-{j}", rcx.prefix));
    let text_id = SharedString::from(format!("{}-li-text-{ix}-{j}", rcx.prefix));
    let mut list = div()
        .id((rcx.id("list"), j))
        .spoken(Role::List, format!("{} list {}", rcx.name, rcx.lists))
        .flex()
        .flex_col()
        .gap_1();
    for (k, item) in items.iter().enumerate() {
        let marker = if ordered {
            format!("{}.", start + k as u64)
        } else {
            "•".to_owned()
        };
        let has_text = !item.text.text.trim().is_empty();
        let name = if has_text {
            item.text.spoken()
        } else {
            format!("Item {}", k + 1)
        };
        // Named before what it holds, so names are given in reading order.
        let el = div()
            .id((item_id.clone(), k))
            .debug_selector({
                let prefix = rcx.prefix;
                move || format!("{prefix}-li-{ix}-{j}-{k}")
            })
            .spoken(Role::ListItem, name)
            .aria_position_in_set(k + 1)
            .aria_size_of_set(count)
            .flex()
            .gap_2()
            .child(div().flex_none().text_color(muted).child(marker));
        let mut body = div().flex_1().min_w_0().flex().flex_col().gap_1();
        if has_text {
            body = body.child(rcx.text((text_id.clone(), k), &item.text));
        }
        for block in &item.blocks {
            body = body.child(result_block(rcx, block, cx));
        }
        list = list.child(el.child(body));
    }
    list.into_any_element()
}

/// Table `j` of a result: a `Table` named "<name> table N" whose rows are
/// named by their place ("Column headers", "Row N"), never by a cell's
/// text; its column headers and cells are named by their text, an empty
/// cell reading a muted "None" (as in the Actions tab) and an empty column
/// header "Column N". Cell text is selectable, in reading order.
fn result_table(
    rcx: &mut ResultCx,
    j: usize,
    header: &[Inline],
    rows: &[Vec<Inline>],
    cx: &App,
) -> AnyElement {
    rcx.tables += 1;
    let theme = cx.theme();
    let (border, muted) = (theme.border, theme.muted_foreground);
    let ix = rcx.ix;
    let columns = header.len();
    let row_id = SharedString::from(format!("{}-tr-{ix}-{j}", rcx.prefix));
    let row = |r: usize, name: String| {
        div()
            .id((row_id.clone(), r))
            .role(Role::Row)
            .aria_label(name)
            .aria_row_index(r + 1)
            .flex()
            .gap_2()
            .py_1()
            .border_b_1()
            .border_color(border)
    };
    let mut table = div()
        .id((rcx.id("table"), j))
        .role(Role::Table)
        .aria_label(format!("{} table {}", rcx.name, rcx.tables))
        .aria_row_count(rows.len() + 1)
        .aria_column_count(columns)
        .flex()
        .flex_col()
        .text_role(TextRole::Body);
    let header_id = SharedString::from(format!("{}-th-{ix}-{j}", rcx.prefix));
    let header_text_id = SharedString::from(format!("{}-th-text-{ix}-{j}", rcx.prefix));
    let mut header_row = row(0, "Column headers".to_owned()).font_weight(FontWeight::SEMIBOLD);
    for (c, text) in header.iter().enumerate() {
        let empty = text.text.trim().is_empty();
        let mut cell = div()
            .id((header_id.clone(), c))
            .spoken(
                Role::ColumnHeader,
                if empty {
                    format!("Column {}", c + 1)
                } else {
                    text.spoken()
                },
            )
            .aria_column_index(c + 1)
            .flex_1()
            .min_w_0()
            .px_1();
        if !empty {
            cell = cell.child(rcx.text((header_text_id.clone(), c), text));
        }
        header_row = header_row.child(cell);
    }
    table = table.child(header_row);
    for (r, cells) in rows.iter().enumerate() {
        let cell_id = SharedString::from(format!("{}-td-{ix}-{j}-{r}", rcx.prefix));
        let text_id = SharedString::from(format!("{}-td-text-{ix}-{j}-{r}", rcx.prefix));
        let mut el = row(r + 1, format!("Row {}", r + 1));
        for (c, text) in cells.iter().enumerate() {
            let empty = text.text.trim().is_empty();
            let empty_label = "None";
            let cell = div()
                .id((cell_id.clone(), c))
                .spoken(
                    Role::Cell,
                    if empty {
                        empty_label.to_owned()
                    } else {
                        text.spoken()
                    },
                )
                .aria_column_index(c + 1)
                .flex_1()
                .min_w_0()
                .px_1();
            el = el.child(if empty {
                cell.text_color(muted).child(empty_label)
            } else {
                cell.child(rcx.text((text_id.clone(), c), text))
            });
        }
        table = table.child(el);
    }
    table.into_any_element()
}

/// A code block: one named node whose lines, exactly as written, can be
/// selected (at `order` among the window's runs), in the theme's monospace
/// font.
fn code_block(
    id: impl Into<ElementId>,
    text_id: impl Into<ElementId>,
    text: String,
    order: u64,
    cx: &App,
) -> gpui::Stateful<gpui::Div> {
    let theme = cx.theme();
    div()
        .id(id)
        .spoken(Role::Label, text.clone())
        .px_2()
        .py_1()
        .rounded(theme.radius)
        .bg(theme.muted)
        .text_role(TextRole::Mono)
        .font_family(theme.mono_font_family.clone())
        .child(SelectableText::new(text_id, text).document_order(order))
}

impl Render for TemplatesTab {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.window.is_none() {
            self.window = Some(window.window_handle());
            self.subscriptions = vec![
                cx.observe(&self.state, |this, state, cx| {
                    let config = state.read(cx).config();
                    let automatic = &config.templates.auto;
                    let providers_changed =
                        this.catalog.as_ref().is_some_and(|(fingerprint, _)| {
                            fingerprint != &providers_fingerprint(config)
                        });
                    if &this.auto != automatic || providers_changed {
                        this.auto.clone_from(automatic);
                        cx.notify();
                    }
                }),
                cx.observe_window_activation(window, |this, window, cx| {
                    if this.visible && window.is_window_active() {
                        this.reload(cx);
                    }
                }),
            ];
            self.auto
                .clone_from(&self.state.read(cx).config().templates.auto);
        }
        self.load_catalog(cx);
        let theme = cx.theme();
        let mut el = div()
            .id("templates-content")
            .flex()
            .flex_col()
            // Bounded by the tab panel so long results scroll instead of
            // being cut off below the window.
            .flex_1()
            .min_h_0()
            .p_4()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .gap_2()
                    .items_start()
                    .child(
                        text_line("templates-intro", Role::Label, TEMPLATES_INTRO)
                            .w_full()
                            .whitespace_normal()
                            .text_role(TextRole::Caption)
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        button(
                            "templates-edit",
                            "Edit templates",
                            true,
                            cx,
                            |this, _, cx| this.open_settings(SettingsPage::Templates, cx),
                        )
                        .link(),
                    ),
            );
        if let Some((text, error)) = &self.notice {
            el = el.child(kit::status_line(
                "templates-notice",
                if *error {
                    StatusKind::Failed
                } else {
                    StatusKind::Done
                },
                text.clone(),
                cx,
            ));
        }
        match &self.definitions {
            None => {
                return el.child(kit::status_line(
                    "templates-loading",
                    StatusKind::Pending,
                    "Reading the templates…",
                    cx,
                ));
            }
            Some(Err(e)) => {
                el = el.child(kit::retry_line(
                    "templates-error",
                    e.clone(),
                    cx,
                    |this, _, cx| this.reload(cx),
                ));
            }
            Some(Ok(_)) => {}
        }
        let can_run = match self.catalog.as_ref().and_then(|(_, lookup)| lookup.peek()) {
            Some(Ok(rows)) if rows.iter().any(is_usable_provider) => true,
            Some(Ok(_)) => {
                el = el.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_2()
                        .items_center()
                        .child(text_line(
                            "templates-no-provider",
                            Role::Status,
                            NO_TEMPLATE_PROVIDER,
                        ))
                        .child(
                            button(
                                "templates-set-up-provider",
                                "Set up AI providers",
                                true,
                                cx,
                                |this, _, cx| this.open_settings(SettingsPage::AiProviders, cx),
                            )
                            .link(),
                        ),
                );
                false
            }
            Some(Err(error)) => {
                el = el.child(kit::retry_line(
                    "templates-provider-error",
                    error.clone(),
                    cx,
                    |this, _, cx| {
                        this.state
                            .update(cx, |state, _| state.retry_generation_providers());
                        this.catalog = None;
                        cx.notify();
                    },
                ));
                false
            }
            None => {
                el = el.child(kit::status_line(
                    "templates-provider-loading",
                    StatusKind::Pending,
                    "Listing AI providers…",
                    cx,
                ));
                false
            }
        };
        let rows = self.rows();
        self.mount_rows(&rows, window, cx);
        self.refresh_parsed(&rows);
        for (ix, row) in rows.iter().enumerate() {
            if let Some(panel) = self
                .panels
                .get(&row.id)
                .filter(|_| !can_run || !row.defined)
            {
                panel
                    .entity
                    .update(cx, |p, cx| p.dismiss_template_options(self.window, cx));
            }
            el = el.child(self.render_row(ix, row, can_run, window, cx));
        }
        el
    }
}

// ---- Settings → Templates ----

/// The template being written.
struct Editor {
    /// The template edited, or `None` for a new one (also a duplicate).
    existing: Option<String>,
    /// Which editor this is: a save started by an editor that was cancelled
    /// or replaced never closes, fills or rebinds a later one.
    generation: u64,
    /// The definition as it was when the editor opened (`None` for a new
    /// template): a save is refused with [`Editor::conflict`] when the
    /// definition on disk no longer matches it.
    baseline: Option<Template>,
    id: Entity<InputState>,
    name: Entity<InputState>,
    description: Entity<InputState>,
    body: Entity<TextareaState>,
    error: Option<String>,
    conflict: Option<Conflict>,
    initial: Draft,
    advanced: bool,
    _changes: Vec<Subscription>,
}

impl Editor {
    /// The fields as typed: what a save sends, and how its reply tells
    /// whether the user typed on meanwhile.
    fn draft(&self, cx: &gpui::App) -> Draft {
        Draft {
            id: self.id.read(cx).value().to_string(),
            name: self.name.read(cx).value().to_string(),
            description: self.description.read(cx).value().to_string(),
            body: self.body.read(cx).value().to_string(),
        }
    }
}

/// An [`Editor`]'s fields as typed (untrimmed).
#[derive(Clone, PartialEq)]
struct Draft {
    id: String,
    name: String,
    description: String,
    body: String,
}

/// The definition changed outside this editor since it opened; the draft is
/// kept until the user chooses.
#[derive(Clone, Debug, PartialEq)]
pub enum Conflict {
    /// It was edited, reset or created elsewhere: what is defined now.
    Changed(Template),
    /// It was deleted elsewhere.
    Deleted,
}

/// What a save found and did.
/// "Couldn't save {template}. {Cause}." and, while the editor still holds
/// the draft (`kept`), "Your text is kept." (WRT-08, SET-64).
pub fn save_failure(name: &str, cause: &str, kept: bool) -> String {
    failure_text(
        &format!("Couldn't save {name}."),
        cause,
        if kept { "Your text is kept." } else { "" },
    )
}

/// Why a save refused as changed shows no conflict: the template as it is
/// now could not be read.
const UNREADABLE_AFTER_CONFLICT: &str =
    "It was changed elsewhere, and BroScribe couldn't read the new version.";

enum SaveOutcome {
    Saved(std::result::Result<serde_json::Value, crate::jobs::CliError>),
    Conflict(Conflict),
    /// Refused as changed, and what is defined now could not be read.
    Unreadable,
}

pub struct TemplatesSettings {
    state: Entity<AppState>,
    templates: Option<std::result::Result<Vec<Template>, String>>,
    /// Bumped by every list; an older list's late answer is dropped.
    load_generation: u64,
    editor: Option<Editor>,
    /// Bumped by every editor opened.
    editor_generation: u64,
    menu_focus: HashMap<String, gpui::FocusHandle>,
    edit_focus: HashMap<String, gpui::FocusHandle>,
    new_focus: gpui::FocusHandle,
    row_status: HashMap<String, (StatusKind, String)>,
    editor_modal: Option<gpui::AnyWindowHandle>,
    discard_prompt: bool,
    providers: Option<Entity<super::providers::ProvidersView>>,
    providers_changed: Option<Subscription>,
    busy: bool,
    notice: Option<(String, bool)>,
}

impl TemplatesSettings {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            state,
            templates: None,
            load_generation: 0,
            editor: None,
            editor_generation: 0,
            menu_focus: HashMap::new(),
            edit_focus: HashMap::new(),
            new_focus: cx.focus_handle(),
            row_status: HashMap::new(),
            editor_modal: None,
            discard_prompt: false,
            providers: None,
            providers_changed: None,
            busy: false,
            notice: None,
        };
        this.load(cx);
        this
    }

    pub(crate) fn use_summary_providers(
        &mut self,
        providers: Entity<super::providers::ProvidersView>,
        cx: &mut Context<Self>,
    ) {
        self.providers_changed = Some(cx.observe(&providers, |_, _, cx| cx.notify()));
        self.providers = Some(providers);
    }

    #[cfg(test)]
    pub(crate) fn templates(&self) -> Option<&std::result::Result<Vec<Template>, String>> {
        self.templates.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn notice(&self) -> Option<&(String, bool)> {
        self.notice.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn editing(&self) -> Option<Option<&str>> {
        self.editor.as_ref().map(|e| e.existing.as_deref())
    }

    #[cfg(test)]
    pub(crate) fn editor_error(&self) -> Option<&str> {
        self.editor.as_ref().and_then(|e| e.error.as_deref())
    }

    /// The outside change a save of the open editor ran into, if any.
    #[cfg(test)]
    pub(crate) fn editor_conflict(&self) -> Option<&Conflict> {
        self.editor.as_ref().and_then(|e| e.conflict.as_ref())
    }

    /// Whether `id` runs after each meeting (`[templates] auto`).
    pub fn is_auto(&self, id: &str, cx: &gpui::App) -> bool {
        self.state
            .read(cx)
            .config()
            .templates
            .auto
            .iter()
            .any(|a| a == id)
    }

    pub fn load(&mut self, cx: &mut Context<Self>) {
        self.load_generation += 1;
        let generation = self.load_generation;
        if matches!(&self.templates, Some(Err(_))) {
            self.templates = None;
            cx.notify();
        }
        let data_dir = self.state.read(cx).paths().data_dir.clone();
        let task = self
            .state
            .read(cx)
            .cli()
            .spawn(cx.background_executor(), async move {
                list_templates(&data_dir)
            });
        cx.spawn(async move |this, cx| {
            let list = task.await;
            this.update(cx, |this, cx| {
                if this.load_generation != generation {
                    return;
                }
                this.templates = Some(list);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn template(&self, id: &str) -> Option<&Template> {
        self.templates
            .as_ref()?
            .as_ref()
            .ok()?
            .iter()
            .find(|t| t.id == id)
    }

    fn open_editor(
        &mut self,
        existing: Option<String>,
        baseline: Option<Template>,
        values: (String, String, String, String),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (id, name, description, body) = values;
        let id = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("id: lowercase letters, digits and -")
                .default_value(id)
        });
        let name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Name")
                .default_value(name)
        });
        let description = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Short description (optional)")
                .default_value(description)
        });
        let body = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("Instructions: what to write from the transcript, in Markdown.")
                .default_value(body)
        });
        let initial = Draft {
            id: id.read(cx).value().to_string(),
            name: name.read(cx).value().to_string(),
            description: description.read(cx).value().to_string(),
            body: body.read(cx).value().to_string(),
        };
        let mut changes = [&name, &description]
            .into_iter()
            .map(|field| cx.observe(field, |_, _, cx| cx.refresh_windows()))
            .collect::<Vec<_>>();
        changes.push(cx.observe(&body, |_, _, cx| cx.refresh_windows()));
        self.editor_generation += 1;
        self.editor = Some(Editor {
            existing,
            generation: self.editor_generation,
            baseline,
            id,
            name,
            description,
            body,
            error: None,
            conflict: None,
            initial,
            advanced: false,
            _changes: changes,
        });
        self.notice = None;
        cx.notify();
    }

    fn editor_id(&self, editor: &Editor, draft: &Draft) -> String {
        if let Some(id) = &editor.existing {
            return id.clone();
        }
        if !draft.id.trim().is_empty() {
            return draft.id.trim().to_owned();
        }
        let stem = broscribe_core::paths::slugify(&draft.name);
        let stem = stem.trim_matches('-');
        let stem = if stem.is_empty() { "template" } else { stem };
        let id = stem
            .chars()
            .take(templates::ID_MAX_CHARS)
            .collect::<String>();
        if self.template(&id).is_none() {
            return id;
        }
        let taken = self
            .templates
            .iter()
            .flatten()
            .flatten()
            .map(|template| template.id.clone())
            .collect::<Vec<_>>();
        duplicate_id(&id, &taken)
    }

    fn mount_editor(
        &mut self,
        opener: gpui::FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = &self.editor else {
            return;
        };
        if self.editor_modal.is_some() || window.root::<gpui::component::Root>().flatten().is_none()
        {
            return;
        }
        let title = editor.baseline.as_ref().map_or_else(
            || "New template".to_owned(),
            |template| format!("Edit “{}”", template.name),
        );
        let name = editor.name.clone();
        let initial_focus = std::cell::Cell::new(true);
        let view = cx.weak_entity();
        self.editor_modal = Some(window.window_handle());
        opener.focus(window, cx);
        window.open_dialog(cx, move |dialog, window, cx| {
            // BroScribe's dialogs stand on the popover ground, not the desk.
            let dialog = dialog.bg(cx.theme().popover);
            if initial_focus.replace(false) {
                name.focus_handle(cx).focus(window, cx);
            }
            let content = view
                .update(cx, |this, cx| {
                    this.editor
                        .as_ref()
                        .map(|editor| this.render_editor(editor, cx).into_any_element())
                })
                .ok()
                .flatten();
            let cancel = view.clone();
            dialog
                .title(title.clone())
                .width(px(640.))
                .entrance(gpui::component::dialog::DialogEntrance::None)
                .overlay_closable(false)
                .close_button(false)
                .on_ok(|_, _, _| false)
                .on_cancel(move |_, window, cx| {
                    cancel
                        .update(cx, |this, cx| this.request_editor_close(window, cx))
                        .unwrap_or(true)
                })
                .on_close({
                    let opener = opener.clone();
                    move |_, window, cx| opener.focus(window, cx)
                })
                .children(content)
        });
    }

    /// Settings closes the editor first so its unsaved changes still ask.
    pub(crate) fn handle_window_close(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.editor_modal.is_none() {
            return false;
        }
        if self.request_editor_close(window, cx) {
            window.close_dialog(cx);
        }
        true
    }

    fn request_editor_close(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.discard_prompt {
            return false;
        }
        let Some(editor) = &self.editor else {
            return true;
        };
        if editor.draft(cx) == editor.initial {
            self.cancel_edit(cx);
            self.editor_modal = None;
            return true;
        }
        let name = editor
            .baseline
            .as_ref()
            .map_or("this template", |template| template.name.as_str());
        let title = if editor.baseline.is_some() {
            format!("Discard changes to “{name}”?")
        } else {
            "Discard changes to this template?".into()
        };
        let opener = window
            .focused(cx)
            .unwrap_or_else(|| editor.name.focus_handle(cx));
        let dialog = window.window_handle();
        let view = cx.weak_entity();
        let cancel = view.clone();
        self.discard_prompt = true;
        kit::confirm_dialog(
            opener,
            window,
            cx,
            move |_| {
                let mut spec = kit::ConfirmSpec::new(
                    kit::confirm_line("template-discard-title", title.clone()),
                    "template-discard",
                    "template-discard-actions",
                    "template-discard-cancel",
                    kit::confirm_line("template-discard-confirm", "Discard"),
                );
                spec.goes.push(kit::confirm_line(
                    "template-discard-changes",
                    "Your unsaved changes to the template.",
                ));
                Some(spec)
            },
            move |_, cx| {
                let _ = cancel.update(cx, |this, _| this.discard_prompt = false);
                true
            },
            move |_, cx| {
                let close = view.clone();
                cx.defer(move |cx| {
                    let _ = dialog.update(cx, |_, window, cx| {
                        let _ = close.update(cx, |this, cx| {
                            this.cancel_edit(cx);
                            this.editor_modal = None;
                            this.discard_prompt = false;
                        });
                        window.close_dialog(cx);
                    });
                });
                true
            },
        );
        false
    }

    pub fn new_template(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_editor(None, None, Default::default(), window, cx);
    }

    /// Opens the editor on `id` as listed; that definition is the baseline
    /// a save checks the file against.
    pub fn edit(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = self.template(id).cloned() else {
            return;
        };
        self.open_on(t, window, cx);
    }

    fn open_on(&mut self, t: Template, window: &mut Window, cx: &mut Context<Self>) {
        let values = (
            t.id.clone(),
            t.name.clone(),
            t.description.clone(),
            t.body.clone(),
        );
        self.open_editor(Some(t.id.clone()), Some(t), values, window, cx);
    }

    pub fn duplicate(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = self.template(id).cloned() else {
            return;
        };
        let taken: Vec<String> = self
            .templates
            .iter()
            .flatten()
            .flatten()
            .map(|t| t.id.clone())
            .collect();
        self.open_editor(
            None,
            None,
            (
                duplicate_id(&t.id, &taken),
                format!("{} (copy)", t.name),
                t.description,
                t.body,
            ),
            window,
            cx,
        );
    }

    /// Closes the editor and drops its draft. A save still running ends on
    /// its own and reports in the notice; it never touches a later editor.
    pub fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.editor = None;
        cx.notify();
    }

    /// Writes the editor's template through the CLI (stdin JSON). An edit
    /// sends the definition the editor opened on as `baseline` and a new
    /// template is create-only, so the CLI writes nothing (under the library
    /// write lock) once the definition changed elsewhere; the editor then
    /// shows the conflict with what is defined now, keeping the draft. The
    /// editor closes on success only while its fields still hold what was
    /// sent; text typed during the save stays, checked from then on against
    /// the version saved.
    pub fn save(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        if self.busy {
            return;
        }
        let draft = editor.draft(cx);
        let id = self.editor_id(editor, &draft);
        let name = draft.name.trim().to_owned();
        let description = draft.description.trim();
        let body = draft.body.as_str();
        let error = if let Err(e) = templates::validate_id(&id) {
            Some(failure_text(
                "This ID can't be used.",
                &core_user_text(&e),
                "",
            ))
        } else if name.is_empty() {
            Some("Give the template a name.".to_owned())
        } else if body.trim().is_empty() {
            Some("Write the template's instructions.".to_owned())
        } else if body.chars().count() > templates::BODY_MAX_CHARS {
            Some("Use 8,000 characters or fewer for instructions.".into())
        } else {
            None
        };
        let generation = editor.generation;
        let baseline = editor.baseline.clone();
        let command = save_template_args(&id, editor.existing.is_some());
        if let Some(editor) = &mut self.editor {
            editor.error = error.clone();
            editor.conflict = None;
        }
        if error.is_some() {
            cx.notify();
            return;
        }
        let stdin = template_stdin(&name, description, body, baseline.as_ref());
        let (cli, data_dir) = {
            let s = self.state.read(cx);
            (s.cli().clone(), s.paths().data_dir.clone())
        };
        let task = cx.background_executor().spawn(async move {
            match cli.run_blocking_stdin(&command, ChildKind::Command, stdin) {
                // Refused as changed: show what is defined now.
                Err(e) if CONFLICT_CODES.contains(&e.code.as_str()) => {
                    match templates::get(&data_dir, &id) {
                        Ok(current) => SaveOutcome::Conflict(Conflict::Changed(current)),
                        Err(Error::NotFound(_)) => SaveOutcome::Conflict(Conflict::Deleted),
                        Err(e) => {
                            tracing::warn!(code = e.code(), "reading a template after a conflict");
                            SaveOutcome::Unreadable
                        }
                    }
                }
                result => SaveOutcome::Saved(result),
            }
        });
        self.busy = true;
        self.notice = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let close = this
                .update(cx, |this, cx| {
                    let mut close = None;
                    this.busy = false;
                    let now = this
                        .editor
                        .as_ref()
                        .filter(|e| e.generation == generation)
                        .map(|e| e.draft(cx));
                    let unchanged = now.as_ref() == Some(&draft);
                    let editor = this.editor.as_mut().filter(|e| e.generation == generation);
                    match (outcome, editor) {
                        (SaveOutcome::Saved(Ok(reply)), Some(editor)) if !unchanged => {
                            // Typed on during the save: the draft stays, now an
                            // edit of the version saved (a new template given
                            // another ID meanwhile is still a new one).
                            let same_id = now
                                .as_ref()
                                .is_some_and(|now| now.id == draft.id && now.name == draft.name);
                            if (editor.existing.is_some() || same_id)
                                && let Some(saved) = saved_template(&reply)
                            {
                                editor.existing = Some(saved.id.clone());
                                editor.baseline = Some(saved);
                            }
                            this.notice = Some((
                                format!("Saved {name}. Changes typed since are not saved yet."),
                                false,
                            ));
                        }
                        (SaveOutcome::Saved(Ok(_)), editor) => {
                            if editor.is_some() {
                                this.editor = None;
                                close = this.editor_modal.take();
                            }
                            this.notice = Some((format!("Saved {name}."), false));
                        }
                        (SaveOutcome::Saved(Err(e)), Some(editor)) => {
                            editor.error = Some(save_failure(&name, &e.user_text(), true));
                        }
                        (SaveOutcome::Saved(Err(e)), None) => {
                            this.notice = Some((save_failure(&name, &e.user_text(), false), true));
                        }
                        (SaveOutcome::Conflict(conflict), Some(editor)) => {
                            editor.conflict = Some(conflict);
                        }
                        (SaveOutcome::Conflict(_), None) => {
                            this.notice = Some((
                                save_failure(&name, "It was changed elsewhere.", false),
                                true,
                            ));
                        }
                        (SaveOutcome::Unreadable, Some(editor)) => {
                            editor.error =
                                Some(save_failure(&name, UNREADABLE_AFTER_CONFLICT, true));
                        }
                        (SaveOutcome::Unreadable, None) => {
                            this.notice =
                                Some((save_failure(&name, UNREADABLE_AFTER_CONFLICT, false), true));
                        }
                    }
                    // The list shows what is defined now (a save or an outside change).
                    this.load(cx);
                    cx.refresh_windows();
                    cx.notify();
                    close
                })
                .ok()
                .flatten();
            if let Some(close) = close {
                let _ = cx.update_window(close, |_, window, cx| window.close_dialog(cx));
            }
        })
        .detach();
    }

    /// After a conflict: saves the draft over what is defined now (a
    /// deleted template is created again), checked against that version.
    pub fn keep_mine(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        let Some(conflict) = editor.conflict.take() else {
            return;
        };
        match conflict {
            Conflict::Changed(current) => {
                editor.existing = Some(current.id.clone());
                editor.baseline = Some(current);
            }
            Conflict::Deleted => {
                editor.existing = None;
                editor.baseline = None;
            }
        }
        self.save(cx);
    }

    /// After a conflict: drops the draft for what is defined now (closes
    /// the editor when the template was deleted).
    pub fn take_theirs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conflict) = self.editor.as_mut().and_then(|e| e.conflict.take()) else {
            return;
        };
        match conflict {
            Conflict::Changed(current) => self.open_on(current, window, cx),
            Conflict::Deleted => {
                self.editor = None;
                if self.editor_modal.take().is_some() {
                    window.close_dialog(cx);
                }
                cx.notify();
            }
        }
    }

    /// Runs a write without stdin, then reloads the list and the config.
    /// `failed` is the failure's first sentence ("Couldn't delete Meeting
    /// notes.").
    fn row_busy(&self, id: &str) -> bool {
        self.row_status
            .get(id)
            .is_some_and(|(kind, _)| *kind == StatusKind::Pending)
    }

    fn run_write(
        &mut self,
        id: &str,
        command: Vec<OsString>,
        done: String,
        failed: String,
        cx: &mut Context<Self>,
    ) {
        if self.row_busy(id) {
            return;
        }
        let task = self.state.read(cx).run_cli(command, ChildKind::Command, cx);
        let id = id.to_owned();
        self.row_status
            .insert(id.clone(), (StatusKind::Pending, "Saving…".into()));
        cx.notify();
        let state = self.state.clone();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            cx.update(|cx| reload_config(&state, cx)).await;
            let _ = this.update(cx, |this, cx| {
                this.row_status.insert(
                    id,
                    match result {
                        Ok(_) => (StatusKind::Done, done),
                        Err(e) => (
                            StatusKind::Failed,
                            failure_text(&failed, &e.user_text(), ""),
                        ),
                    },
                );
                this.load(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn confirm_template_change(
        &mut self,
        id: &str,
        reset: bool,
        opener: gpui::FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(template) = self
            .template(id)
            .filter(|t| !self.row_busy(id) && if reset { t.overridden } else { !t.builtin })
            .cloned()
        else {
            return;
        };
        let title = if reset {
            format!("Reset “{}” to the built-in text?", template.name)
        } else {
            format!("Delete “{}”?", template.name)
        };
        let edit_id = if reset {
            Some(id)
        } else {
            let list = self.templates.as_ref().and_then(|list| list.as_ref().ok());
            list.and_then(|list| {
                let index = list.iter().position(|row| row.id == id)?;
                list.get(index + 1).map(|row| row.id.as_str())
            })
        };
        let success = edit_id
            .and_then(|id| self.edit_focus.get(id))
            .unwrap_or(&self.new_focus)
            .clone();
        let view = cx.weak_entity();
        kit::confirm_dialog(
            opener,
            window,
            cx,
            move |_| {
                use kit::confirm_line as line;
                let mut spec = kit::ConfirmSpec::new(
                    line("template-delete-title", title.clone()),
                    "template-delete-body",
                    "template-delete-footer",
                    "template-delete-cancel",
                    line(
                        "template-delete-confirm",
                        if reset {
                            "Reset template"
                        } else {
                            "Delete template"
                        },
                    ),
                );
                spec.success_focus = Some(success.clone());
                spec.goes.push(line(
                    "template-delete-goes",
                    if reset {
                        "Your edits to the template’s name, description and instructions."
                    } else {
                        "The template, from every meeting's Templates tab"
                    },
                ));
                if reset {
                    spec.changes.push(line(
                        "template-reset-changes",
                        "The original built-in template returns.",
                    ));
                }
                spec.stays.push(line(
                    "template-delete-stays",
                    "Results it already produced, which you can still read, copy and export",
                ));
                Some(spec)
            },
            |_, _| true,
            move |_, cx| {
                let _ = view.update(cx, |this, cx| {
                    if this.template(&template.id) == Some(&template) {
                        if reset {
                            this.reset(&template.id, cx);
                        } else {
                            this.delete(&template.id, cx);
                        }
                    }
                });
                true
            },
        );
    }

    fn delete(&mut self, id: &str, cx: &mut Context<Self>) {
        let name = self.template(id).map_or(id, |t| t.name.as_str()).to_owned();
        self.run_write(
            id,
            args(["templates", "delete", id]),
            format!("Deleted {name}. Results it produced stay with their meetings."),
            format!("Couldn't delete {name}."),
            cx,
        );
    }

    pub fn reset(&mut self, id: &str, cx: &mut Context<Self>) {
        let name = self.template(id).map_or(id, |t| t.name.as_str()).to_owned();
        self.run_write(
            id,
            args(["templates", "reset", id]),
            format!("{name} is back to its built-in instructions."),
            format!("Couldn't reset {name}."),
            cx,
        );
    }

    pub fn set_auto(&mut self, id: &str, on: bool, cx: &mut Context<Self>) {
        let name = self.template(id).map_or(id, |t| t.name.as_str()).to_owned();
        self.run_write(
            id,
            auto_args(id, on),
            if on {
                format!("{name} is set to run after automatic summaries.")
            } else {
                format!("{name} runs only on request.")
            },
            format!("Couldn't change when {name} runs."),
            cx,
        );
    }

    fn render_editor(&self, editor: &Editor, cx: &Context<Self>) -> impl IntoElement {
        let draft = editor.draft(cx);
        let count = draft.body.chars().count();
        let over_limit = count > templates::BODY_MAX_CHARS;
        let id = self.editor_id(editor, &draft);
        let count = format!("{count} / 8,000");
        let mut el = kit::stack().w_full().id("template-editor")
            .role(Role::Group).aria_label("Template editor")
            .child(kit::field("template-name-label", "Name", None,
                |label| Input::new(&editor.name).aria_label(label), cx))
            .child(kit::field("template-description-label", "Description",
                Some(("template-description-optional".into(), "Optional".into())),
                |label| Input::new(&editor.description).aria_label(label), cx))
            .child(kit::field("template-instructions-label", "Instructions",
                Some(("template-editor-note".into(), "The transcript is sent with these instructions to the AI provider you choose when you run the template.".into())),
                |label| Textarea::new(&editor.body).h(gpui::rems(12.5)).aria_label(label), cx))
            .child(div().w_full().flex().justify_end().child(if over_limit {
                kit::status_line("template-instructions-count", StatusKind::Failed, count, cx)
            } else { muted("template-instructions-count", count, cx) }))
            .child(gpui::component::button::Button::new("template-editor-advanced").label("Advanced").ghost()
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(editor) = &mut this.editor { editor.advanced = !editor.advanced; }
                    cx.refresh_windows();
                })))
            .when(editor.advanced, |el| el.child(muted("template-id-label", format!("ID: {id}"), cx)))
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "enter" && event.keystroke.modifiers.secondary(cx)
                    && this.editor.as_ref().is_some_and(|editor| editor.conflict.is_none()) {
                    cx.stop_propagation();
                    this.save(cx);
                }
            }));
        if let Some(error) = &editor.error {
            el = el.child(kit::status_line(
                "template-editor-error",
                StatusKind::Failed,
                error.clone(),
                cx,
            ));
        }
        if let Some(conflict) = &editor.conflict {
            let (text, keep, take) = match conflict {
                Conflict::Changed(current) => (
                    format!(
                        "Not saved: \"{}\" was changed elsewhere after you opened it. Your text \
                         is kept here.",
                        current.name
                    ),
                    "Save mine over it",
                    "Use the other version",
                ),
                Conflict::Deleted => (
                    "Not saved: this template was deleted elsewhere after you opened it. Your \
                     text is kept here."
                        .to_owned(),
                    "Recreate with your text",
                    "Discard your changes",
                ),
            };
            el = el.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(kit::status_line(
                        "template-editor-conflict",
                        StatusKind::Failed,
                        text,
                        cx,
                    ))
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(button(
                                "template-keep-mine",
                                keep,
                                !self.busy,
                                cx,
                                |this: &mut Self, _, cx| this.keep_mine(cx),
                            ))
                            .child(button(
                                "template-take-theirs",
                                take,
                                !self.busy,
                                cx,
                                |this: &mut Self, window, cx| this.take_theirs(window, cx),
                            )),
                    ),
            );
        }
        let can_save = !self.busy
            && editor.conflict.is_none()
            && !over_limit
            && !draft.name.trim().is_empty()
            && !draft.body.trim().is_empty();
        el.child(kit::confirm_actions(
            button(
                "template-cancel",
                "Cancel",
                true,
                cx,
                |this: &mut Self, window, cx| {
                    let mounted = this.editor_modal.is_some();
                    if this.request_editor_close(window, cx) && mounted {
                        window.close_dialog(cx);
                    }
                },
            )
            .outline(),
            button(
                "template-save",
                "Save",
                can_save,
                cx,
                |this: &mut Self, _, cx| this.save(cx),
            )
            .map(|button| kit::primary(button, !can_save, cx))
            .accessibility_label("Save template"),
            cx,
        ))
    }
}

impl Render for TemplatesSettings {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let idle = !self.busy;
        let create_focus = self.new_focus.clone();
        let new_opener = create_focus.clone();
        let mut el = div()
            .id("settings-templates")
            .flex()
            .flex_col()
            .gap_2()
            .child(muted(
                "settings-templates-note",
                "Run a template from a meeting's Templates tab. Built-in templates can be \
                 edited; Reset brings back their original text.",
                cx,
            ))
            .child(
                button(
                    "template-new",
                    "New template…",
                    idle && self.editor.is_none(),
                    cx,
                    move |this: &mut Self, window, cx| {
                        this.new_template(window, cx);
                        this.mount_editor(new_opener.clone(), window, cx);
                    },
                )
                .track_focus(&create_focus)
                .small()
                .self_end()
                .icon(crate::ui::icons::ADD)
                .accessibility_label("New template"),
            );
        if let Some((text, error)) = &self.notice {
            el = el.child(kit::status_line(
                "templates-settings-notice",
                if *error {
                    StatusKind::Failed
                } else {
                    StatusKind::Done
                },
                text.clone(),
                cx,
            ));
        }
        if self.editor_modal.is_none()
            && let Some(editor) = &self.editor
        {
            el = el.child(self.render_editor(editor, cx));
        }
        let list = match &self.templates {
            None => {
                return el.child(kit::status_line(
                    "templates-settings-loading",
                    StatusKind::Pending,
                    "Reading the templates…",
                    cx,
                ));
            }
            Some(Err(e)) => {
                return el.child(kit::retry_line(
                    "templates-settings-error",
                    e.clone(),
                    cx,
                    |this, _, cx| this.load(cx),
                ));
            }
            Some(Ok(list)) => list.clone(),
        };
        for (ix, t) in list.iter().enumerate() {
            let auto = self.is_auto(&t.id, cx);
            let idle = !self.row_busy(&t.id)
                && (!self.busy
                    || self
                        .editor
                        .as_ref()
                        .is_none_or(|editor| editor.existing.as_deref() != Some(&t.id)));
            let kind = match (t.builtin, t.overridden) {
                (true, true) => "Built-in · edited",
                (true, false) => "Built-in",
                _ => "Yours",
            };
            let edit_id = t.id.clone();
            let edit_focus = self
                .edit_focus
                .entry(t.id.clone())
                .or_insert_with(|| cx.focus_handle())
                .clone();
            let edit_opener = edit_focus.clone();
            let more_focus = self
                .menu_focus
                .entry(t.id.clone())
                .or_insert_with(|| cx.focus_handle())
                .clone();
            let more_opener = more_focus.clone();
            let menu_view = cx.weak_entity();
            let definition = t.clone();
            let auto_id = t.id.clone();
            let actions = div()
                .flex()
                .flex_none()
                .justify_end()
                .gap_2()
                .child(
                    button(
                        ("template-edit", ix),
                        "Edit",
                        idle && self.editor.is_none(),
                        cx,
                        move |this: &mut Self, window, cx| {
                            this.edit(&edit_id, window, cx);
                            this.mount_editor(edit_opener.clone(), window, cx);
                        },
                    )
                    .track_focus(&edit_focus)
                    .accessibility_label(format!("Edit {}", t.name))
                    .small(),
                )
                .child(
                    gpui::component::button::Button::new(("template-more", ix))
                        .icon(gpui::component::Icon::empty().path("icons/ellipsis.svg"))
                        .small()
                        .ghost()
                        .track_focus(&more_focus)
                        .disabled(!idle)
                        .accessibility_label(format!("More actions for {}", t.name))
                        .dropdown_menu(move |menu, _, _| {
                            let id = definition.id.clone();
                            let trigger = more_opener.clone();
                            let view = menu_view.clone();
                            let duplicate =
                                PopupMenuItem::new("Duplicate").on_click(move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.duplicate(&id, window, cx);
                                        this.mount_editor(trigger.clone(), window, cx);
                                    });
                                });
                            let menu = menu.item(duplicate);
                            if !definition.builtin || definition.overridden {
                                let reset = definition.builtin;
                                let id = definition.id.clone();
                                let trigger = more_opener.clone();
                                let view = menu_view.clone();
                                menu.item(
                                    PopupMenuItem::new(if reset {
                                        "Reset to built-in"
                                    } else {
                                        "Delete"
                                    })
                                    .on_click(
                                        move |_, window, cx| {
                                            let _ = view.update(cx, |this, cx| {
                                                this.confirm_template_change(
                                                    &id,
                                                    reset,
                                                    trigger.clone(),
                                                    window,
                                                    cx,
                                                )
                                            });
                                        },
                                    ),
                                )
                            } else {
                                menu
                            }
                        }),
                );
            let automatic = Checkbox::new(("template-auto", ix))
                .label("Run after automatic summaries")
                .accessibility_label(format!("Run {} after automatic summaries", t.name))
                .checked(auto)
                .disabled(!idle)
                .on_change(
                    cx.listener(move |this, checked, _, cx| this.set_auto(&auto_id, *checked, cx)),
                );
            // The row's group is named apart from its name line, so no node
            // nests another with the same text.
            el = el.child(
                kit::list_row(("settings-template", ix), false, false, cx)
                    .tooltip(kit::title_tooltip(t.name.clone()))
                    .role(Role::Group)
                    .aria_label(SharedString::from(format!("Template: {}", t.name)))
                    .flex()
                    .flex_col()
                    .gap_1()
                    .py_1()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .child(
                                                text_line(
                                                    format!("template-{ix}-name"),
                                                    Role::Label,
                                                    t.name.clone(),
                                                )
                                                .truncate()
                                                .flex_1()
                                                .min_w_0()
                                                .font_weight(FontWeight::SEMIBOLD),
                                            )
                                            .child(
                                                gpui::component::tag::Tag::secondary()
                                                    .small()
                                                    .child(
                                                        muted(
                                                            format!("template-{ix}-kind"),
                                                            kind,
                                                            cx,
                                                        )
                                                        .truncate(),
                                                    ),
                                            ),
                                    )
                                    .children((!t.description.is_empty()).then(|| {
                                        muted(
                                            format!("template-{ix}-description"),
                                            t.description.clone(),
                                            cx,
                                        )
                                        .truncate()
                                    })),
                            )
                            .child(actions),
                    )
                    .child(automatic)
                    .children(self.row_status.get(&t.id).map(|(kind, message)| {
                        kit::status_line(
                            format!("template-row-result-{}", t.id),
                            *kind,
                            message.clone(),
                            cx,
                        )
                    })),
            );
        }
        let config = self.state.read(cx).config();
        let reason =
            if config.templates.auto.is_empty() {
                None
            } else if config.auto_enrich == broscribe_core::config::AutoEnrich::Off {
                Some((
                    "Automatic summaries are off.",
                    crate::state::SettingsPage::Summaries,
                ))
            } else if !self.providers.as_ref().is_some_and(|providers| {
                providers.read(cx).summary_rows(config).iter().any(|row| {
                    config.profile.llm.is_empty() || config.profile.llm.contains(&row.id)
                })
            }) {
                Some((
                    "No AI provider is turned on for automatic summaries.",
                    crate::state::SettingsPage::AiProviders,
                ))
            } else {
                None
            };
        let finished_deletions = self.row_status.iter().filter(|(id, (kind, _))| {
            *kind == StatusKind::Done && !list.iter().any(|template| &template.id == *id)
        });
        let el = el.children(finished_deletions.map(|(id, (kind, words))| {
            kit::status_line(
                format!("template-row-result-{id}"),
                *kind,
                words.clone(),
                cx,
            )
        }));
        el.when_some(reason, |el, (reason, page)| {
            el.child(kit::status_line(
                "settings-templates-auto-note",
                StatusKind::Attention,
                format!("Templates set to run after automatic summaries won’t run. {reason}"),
                cx,
            ))
            .child(
                gpui::component::button::Button::new("templates-open-automatic-settings")
                    .label(format!("Open {}", page.title()))
                    .ghost()
                    .self_start()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.state.update(cx, |_, cx| {
                            cx.emit(crate::state::AppEvent::OpenSettings(Some(page)))
                        })
                    })),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use broscribe_core::Library;
    use gpui::TestAppContext;
    use serde_json::json;

    use super::*;
    use crate::views::person_picker::fixtures;

    /// A synthetic `broscribe`: argv to `args.log`, stdin of `--stdin`
    /// commands to `stdin.log` and of `--spec-stdin` to `spec.json`;
    /// `providers list` lists one usable provider; `templates edit` answers
    /// `template_changed` (writing nothing, like the CLI's baseline check)
    /// while `template-changed` exists, else `edit-reply.json` when present.
    fn synthetic_cli(dir: &Path) -> PathBuf {
        use crate::test_support::{ps_path, ps_str, write_cli};
        let d = dir.display();
        let file = |name: &str| ps_path(&dir.join(name));
        let ps = format!(
            "fx_log_args {args_log};\
             if($all -clike '*--stdin*' -or $all -clike '*--instructions-stdin*'){{fx_append_stdin {stdin_log};fx_append {stdin_log} ([string][char]10)}}\
             elseif($all -clike '*--spec-stdin*'){{fx_save_stdin {spec}}};\
             $w=fx_words 4,5;\
             if($w -ceq 'providers list'){{if(fx_exists {provider_reply}){{fx_cat {provider_reply}}}else{{fx_print {providers}}}}}\
             elseif($w -ceq 'providers models'){{fx_print {models}}}\
             elseif($w -clike 'export *' -and (fx_exists {export_fail})){{fx_print {export_error};exit 1}}\
             elseif($w -ceq 'templates edit'){{\
               if(fx_exists {changed}){{fx_print {changed_error};exit 1}};\
               if(fx_exists {edit_reply}){{fx_cat {edit_reply}}}else{{fx_print '{{}}'}}}}\
             else{{fx_print '{{}}'}}",
            args_log = file("args.log"),
            stdin_log = file("stdin.log"),
            spec = file("spec.json"),
            providers = ps_str(
                r#"{"providers":[{"id":"agent-a","name":"Synthetic Agent","purpose":"llm","runtime":"agent-cli","enabled":true,"consented":true}]}"#
            ),
            models = ps_str(r#"{"provider":"agent-a","source":"none","models":[]}"#),
            provider_reply = file("providers.json"),
            export_fail = file("export-failure"),
            export_error = ps_str(r#"{"error":{"code":"library_busy","message":"library busy"}}"#),
            changed = file("template-changed"),
            changed_error = ps_str(r#"{"error":{"code":"template_changed","message":"changed"}}"#),
            edit_reply = file("edit-reply.json"),
        );
        write_cli(
            &dir.join("synthetic-cli"),
            &format!(
                "printf '%s\\n' \"$*\" >> '{d}/args.log'\n\
                 case \"$*\" in *--stdin*|*--instructions-stdin*) cat >> '{d}/stdin.log'; printf '\\n' >> '{d}/stdin.log' ;; *--spec-stdin*) cat > '{d}/spec.json' ;; esac\n\
                 case \"$4 $5\" in\n\
                 'providers list') if [ -f '{d}/providers.json' ]; then cat '{d}/providers.json'; else printf '{{\"providers\":[{{\"id\":\"agent-a\",\"name\":\"Synthetic Agent\",\"purpose\":\"llm\",\"runtime\":\"agent-cli\",\"enabled\":true,\"consented\":true}}]}}'; fi ;;\n\
                 'providers models') printf '{{\"provider\":\"agent-a\",\"source\":\"none\",\"models\":[]}}' ;;\n\
                 'export '*) if [ -f '{d}/export-failure' ]; then printf '{{\"error\":{{\"code\":\"library_busy\",\"message\":\"library busy\"}}}}'; exit 1; fi; printf '{{}}' ;;\n\
                 'templates edit') if [ -f '{d}/template-changed' ]; then printf '{{\"error\":{{\"code\":\"template_changed\",\"message\":\"changed\"}}}}'; exit 1; fi; if [ -f '{d}/edit-reply.json' ]; then cat '{d}/edit-reply.json'; else printf '{{}}'; fi ;;\n\
                 *) printf '{{}}' ;;\n\
                 esac\n"
            ),
            &ps,
        )
    }

    fn log(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(name)).unwrap_or_default()
    }

    fn result(id: &str, name: &str, markdown: &str) -> TemplateResult {
        serde_json::from_value(json!({
            "template_id": id, "template_name": name, "instructions_sha256": "00",
            "provider": "agent-a", "created_at": "2026-11-15T00:00:00Z", "markdown": markdown
        }))
        .unwrap()
    }

    fn h(level: usize, text: &str) -> ResultBlock {
        ResultBlock::Heading {
            level,
            text: Inline::plain(text),
        }
    }

    fn p(text: &str) -> ResultBlock {
        ResultBlock::Paragraph(Inline::plain(text))
    }

    fn code(text: &str) -> ResultBlock {
        ResultBlock::Code(text.into())
    }

    /// An item with blocks under it.
    fn item(text: &str, blocks: Vec<ResultBlock>) -> ResultItem {
        ResultItem {
            text: Inline::plain(text),
            blocks,
        }
    }

    /// A list of items with only text.
    fn list(ordered: bool, start: u64, items: &[&str]) -> ResultBlock {
        ResultBlock::List {
            ordered,
            start,
            items: items.iter().map(|text| item(text, Vec::new())).collect(),
        }
    }

    fn table(header: &[&str], rows: &[&[&str]]) -> ResultBlock {
        let row = |cells: &[&str]| cells.iter().map(|c| Inline::plain(*c)).collect();
        ResultBlock::Table {
            header: row(header),
            rows: rows.iter().map(|r| row(r)).collect(),
        }
    }

    /// `source`'s blocks with their text only: the marks
    /// ([`Inline`]'s ranges) are left out, for tests about the words.
    fn parsed(source: &str) -> Vec<ResultBlock> {
        fn words(block: ResultBlock) -> ResultBlock {
            let text = |inline: Inline| Inline::plain(inline.text);
            match block {
                ResultBlock::Heading { level, text: t } => ResultBlock::Heading {
                    level,
                    text: text(t),
                },
                ResultBlock::Paragraph(t) => ResultBlock::Paragraph(text(t)),
                ResultBlock::Code(code) => ResultBlock::Code(code),
                ResultBlock::List {
                    ordered,
                    start,
                    items,
                } => ResultBlock::List {
                    ordered,
                    start,
                    items: items
                        .into_iter()
                        .map(|item| ResultItem {
                            text: text(item.text),
                            blocks: item.blocks.into_iter().map(words).collect(),
                        })
                        .collect(),
                },
                ResultBlock::Table { header, rows } => ResultBlock::Table {
                    header: header.into_iter().map(text).collect(),
                    rows: rows
                        .into_iter()
                        .map(|row| row.into_iter().map(text).collect())
                        .collect(),
                },
            }
        }
        markdown_blocks(source).into_iter().map(words).collect()
    }

    /// INS-59: what each block of a result is drawn and read as. As in
    /// CommonMark (and the `TextView` results were drawn with before), a
    /// new bullet character or number delimiter starts a new list.
    #[test]
    fn markdown_results_split_into_headings_paragraphs_and_lists() {
        assert_eq!(
            parsed(
                "# Next steps\n\nAna Example will draft\nthe **plan**.\n\n- Send the agenda\n- Book \
                 the [room](https://example.com/room)\n"
            ),
            [
                h(1, "Next steps"),
                p("Ana Example will draft the plan."),
                list(false, 1, &["Send the agenda", "Book the room"]),
            ]
        );
        assert_eq!(
            parsed(
                "Summary\n=======\n\n## Owners ##\n1. Bia\n   continues here\n\n2) Carla\n\n\
                 Notes\n---\n***\n* a\n+ b\n- `c`\n\n> Quoted \\*text\\*\n\n\
                 ```\ncode line\n```\n\n| Who | What |\n|---|---|\n| Ana | Plan |\n"
            ),
            [
                h(1, "Summary"),
                h(2, "Owners"),
                list(true, 1, &["Bia continues here"]),
                list(true, 2, &["Carla"]),
                h(2, "Notes"),
                list(false, 1, &["a"]),
                list(false, 1, &["b"]),
                list(false, 1, &["c"]),
                p("Quoted *text*"),
                code("code line"),
                table(&["Who", "What"], &[&["Ana", "Plan"]]),
            ]
        );
        // A list after a paragraph, a paragraph after a list, and nothing
        // for blank input or a lone rule.
        assert_eq!(
            parsed("Intro\n- one\n\nAfter"),
            [p("Intro"), list(false, 1, &["one"]), p("After")]
        );
        assert!(parsed("\n  \n---\n").is_empty());
        assert_eq!(parsed("#hashtag"), [p("#hashtag")]);
        // A soft line break is one space, whatever spaces were around it.
        assert_eq!(parsed("one \n  two\r\nthree"), [p("one two three")]);
        // Dollar signs are text, not math.
        assert_eq!(parsed("Costs $5 and $10."), [p("Costs $5 and $10.")]);
    }

    /// INS-59: the result never changes what a meeting's text says. A
    /// closing `#` run needs a space before it; a backslash goes only where
    /// it escapes punctuation; code spans and code blocks are kept as
    /// written (blocks in the monospace font); emphasis marks go but
    /// snake_case stays; a split ordered list keeps counting.
    #[test]
    fn markdown_results_keep_the_text_as_written() {
        // Headings: only a closing run after a space closes.
        assert_eq!(parsed("# C#"), [h(1, "C#")]);
        assert_eq!(parsed("## F# and C# ##"), [h(2, "F# and C#")]);
        assert_eq!(parsed("### foo \\###"), [h(3, "foo ###")]);
        assert_eq!(parsed("# Title #"), [h(1, "Title")]);
        assert!(parsed("# #").is_empty());
        // Backslashes: kept before a letter, dropped before punctuation.
        assert_eq!(
            parsed("Saved to C:\\temp\\notes and \\*not\\* emphasis \\\\ done"),
            [p("Saved to C:\\temp\\notes and *not* emphasis \\ done")]
        );
        // Code spans keep everything inside them.
        assert_eq!(
            parsed("Call `__init__` with `*args` and `` a`b `` or `C:\\temp\\x`."),
            [p("Call __init__ with *args and a`b or C:\\temp\\x.")]
        );
        assert_eq!(parsed("An `unclosed span"), [p("An `unclosed span")]);
        // Emphasis: single and double marks go; words and arithmetic stay.
        assert_eq!(
            parsed(
                "*emphasis*, _x_, **strong**, __strong__ and ***both*** but snake_case_name, \
                 my_var_, 2 * 3 * 4 and a_b stay; foo*bar*baz"
            ),
            [p(
                "emphasis, x, strong, strong and both but snake_case_name, my_var_, 2 * 3 * 4 \
                 and a_b stay; foobarbaz"
            )]
        );
        assert_eq!(
            parsed("- **Ana**: owns `deploy_v2`\n- an *unclosed mark"),
            [list(
                false,
                1,
                &["Ana: owns deploy_v2", "an *unclosed mark"]
            )]
        );
        assert_eq!(
            parsed("See ![the chart](c.png) and [**the plan**](p.md)! [not a link]"),
            [p("See the chart and the plan! [not a link]")]
        );
        // A paragraph between items: the second list counts on from 2; a
        // new delimiter starts a list of its own, from its own number.
        assert_eq!(
            parsed("1. First\n\nA note between.\n\n2. Second\n3. Third\n\n7) Seventh"),
            [
                list(true, 1, &["First"]),
                p("A note between."),
                list(true, 2, &["Second", "Third"]),
                list(true, 7, &["Seventh"]),
            ]
        );
        // Code blocks: every character and indent kept, and their own kind.
        assert_eq!(
            parsed("```rust\nfn __init__(a_b: &str) {\n    *x = \\n; // `C:\\temp`\n}\n```"),
            [code(
                "fn __init__(a_b: &str) {\n    *x = \\n; // `C:\\temp`\n}"
            )]
        );
    }

    /// INS-59: what the hand-written splitter got wrong. Points nested
    /// under a topic stay that topic's own list (the Meeting outline asks
    /// for exactly that); a bracketed time before a link stays text; a
    /// fence closes only on a plain fence at least as long as its own, and
    /// code keeps its trailing whitespace; tables are tables; character
    /// references, autolinks and struck-out text read as their text.
    #[test]
    fn markdown_results_nest_lists_and_keep_code_and_tables() {
        let outline = "- [00:00:05] Kickoff\n  - Goals agreed\n  - Dates set\n\
                       - [00:10:00] Budget\n  - Over by 5%\n";
        assert_eq!(
            parsed(outline),
            [ResultBlock::List {
                ordered: false,
                start: 1,
                items: vec![
                    item(
                        "[00:00:05] Kickoff",
                        vec![list(false, 1, &["Goals agreed", "Dates set"])]
                    ),
                    item("[00:10:00] Budget", vec![list(false, 1, &["Over by 5%"])]),
                ],
            }]
        );
        // A numbered topic with bullets under it is one numbered list.
        assert_eq!(
            parsed("1. Kickoff\n   - Goals agreed\n   - Dates set\n2. Budget\n   - Over"),
            [ResultBlock::List {
                ordered: true,
                start: 1,
                items: vec![
                    item(
                        "Kickoff",
                        vec![list(false, 1, &["Goals agreed", "Dates set"])]
                    ),
                    item("Budget", vec![list(false, 1, &["Over"])]),
                ],
            }]
        );
        // An item's second paragraph stays in it; tasks keep their box.
        assert_eq!(
            parsed("- First\n\n  More about it\n- [x] Sent\n- [ ] Book"),
            [ResultBlock::List {
                ordered: false,
                start: 1,
                items: vec![
                    item("First", vec![p("More about it")]),
                    item("[x] Sent", Vec::new()),
                    item("[ ] Book", Vec::new()),
                ],
            }]
        );
        assert_eq!(
            parsed("[00:01:00] Review [plan](notes.md)"),
            [p("[00:01:00] Review plan")]
        );
        // Fences: a four-backtick fence holds a three-backtick one, a line
        // with an info string does not close, trailing whitespace stays.
        assert_eq!(
            parsed("````\n```\ninner\n```\n````"),
            [code("```\ninner\n```")]
        );
        assert_eq!(
            parsed("```\nfirst\n```python\nsecond\n```"),
            [code("first\n```python\nsecond")]
        );
        assert_eq!(
            parsed("```\nlet a = 1;   \n\tb  \n```"),
            [code("let a = 1;   \n\tb  ")]
        );
        assert_eq!(parsed("~~~\nwhole\n```\n~~~"), [code("whole\n```")]);
        // A table: a short row ends in empty cells; an escaped pipe is text.
        assert_eq!(
            parsed(
                "| Who | What | Due |\n|:--|---|--:|\n| Ana | Plan \\| draft | Fri |\n\
                 | Bia | **Budget** |\n"
            ),
            [table(
                &["Who", "What", "Due"],
                &[&["Ana", "Plan | draft", "Fri"], &["Bia", "Budget", ""]]
            )]
        );
        assert_eq!(
            parsed("Tom &amp; Ana: <https://example.com/a> ~~old~~ new &lt;b&gt;\\\nnext line"),
            [p("Tom & Ana: https://example.com/a old new <b>\nnext line")]
        );
        assert_eq!(
            parsed("Seen in the notes[^1].\n\n[^1]: The shared folder."),
            [p("Seen in the notes[^1]."), p("[^1]: The shared folder.")]
        );
    }

    /// INS-59: struck-out text, code spans and links to web addresses keep
    /// their marks, and the name of the block that holds them says what
    /// the marks show: "(deleted)" after struck-out text, "(link)" after a
    /// link that opens. A link elsewhere (relative, email, another scheme)
    /// is only its words.
    #[test]
    fn results_keep_struck_text_code_spans_and_web_links() {
        let marked = |source: &str| match markdown_blocks(source).as_slice() {
            [ResultBlock::Paragraph(inline), ..] => inline.clone(),
            other => panic!("{other:?}"),
        };
        // The words under each range.
        let words = |inline: &Inline, ranges: &[Range<usize>]| {
            ranges
                .iter()
                .map(|range| inline.text[range.clone()].to_owned())
                .collect::<Vec<_>>()
        };
        let ship = marked("Ship ~~Friday~~ Monday");
        assert_eq!(ship.text, "Ship Friday Monday");
        assert_eq!(words(&ship, &ship.struck), ["Friday"]);
        assert!(ship.code.is_empty() && ship.links.is_empty());
        assert_eq!(ship.spoken(), "Ship Friday (deleted) Monday");

        let plan = marked(
            "Read the [plan](https://example.com/plan), run `make test`, see [notes](notes.md).",
        );
        assert_eq!(plan.text, "Read the plan, run make test, see notes.");
        assert_eq!(plan.links, [(9..13, "https://example.com/plan".to_owned())]);
        assert_eq!(&plan.text[plan.code[0].clone()], "make test");
        assert!(plan.struck.is_empty());
        assert_eq!(
            plan.spoken(),
            "Read the plan (link), run make test, see notes."
        );

        // Autolinks, bare web addresses and reference links open; email,
        // relative and other schemes stay text.
        let all = marked(
            "<https://example.com/a>, www.example.com, [doc][d], <ana@example.com>, \
             [x](ftp://example.com/x), [y](javascript:alert), [z](/plan)\n\n\
             [d]: http://example.com/doc",
        );
        let opened: Vec<(&str, &str)> = all
            .links
            .iter()
            .map(|(range, url)| (&all.text[range.clone()], url.as_str()))
            .collect();
        assert_eq!(
            opened,
            [
                ("https://example.com/a", "https://example.com/a"),
                ("www.example.com", "http://www.example.com"),
                ("doc", "http://example.com/doc"),
            ]
        );

        // A struck link carries both marks and both cues.
        let both = marked("~~[old plan](https://example.com/old)~~ kept");
        assert_eq!(words(&both, &both.struck), ["old plan"]);
        assert_eq!(both.links, [(0..8, "https://example.com/old".to_owned())]);
        assert_eq!(both.spoken(), "old plan (link) (deleted) kept");

        // Marks in headings, after a task's box and in table cells.
        let blocks = markdown_blocks(
            "# ~~Draft~~ Plan\n\n- [x] Send the [agenda](https://example.com/a)\n\n\
             | Doc |\n|---|\n| <https://example.com/d> |\n",
        );
        let [
            ResultBlock::Heading { text: heading, .. },
            ResultBlock::List { items, .. },
            ResultBlock::Table { rows, .. },
        ] = blocks.as_slice()
        else {
            panic!("{blocks:?}");
        };
        assert_eq!(heading.spoken(), "Draft (deleted) Plan");
        assert_eq!(items[0].text.text, "[x] Send the agenda");
        assert_eq!(
            items[0].text.links,
            [(13..19, "https://example.com/a".to_owned())]
        );
        assert_eq!(items[0].text.spoken(), "[x] Send the agenda (link)");
        assert_eq!(rows[0][0].spoken(), "https://example.com/d (link)");
    }

    /// INS-59: raw HTML comments are hidden, as `TextView` hid them, and
    /// the space around an inline one is one space; other raw HTML, and a
    /// comment never closed, stay as written.
    #[test]
    fn raw_html_comments_are_hidden() {
        assert_eq!(
            parsed(
                "<!-- generated by a template -->\n\n# Notes\n\n\
                 Text <!-- inline --> more <b>bold</b>\n\n<!-- a -->\n<div>kept</div>\n\n\
                 <!-- unclosed"
            ),
            [
                h(1, "Notes"),
                p("Text more <b>bold</b>"),
                p("<div>kept</div>"),
                p("<!-- unclosed"),
            ]
        );
        assert_eq!(
            parsed("- <!-- hidden --> First\n- Second <!-- hidden -->"),
            [list(false, 1, &["First", "Second"])]
        );
    }

    /// INS-59: how the marks are drawn: struck-out text struck through, a
    /// link in the link colour and underlined, a code span on the muted
    /// background in the monospace font, strong text bold and emphasised
    /// text italic, as `TextView` drew them; a part with two marks has
    /// both. Bold and italic are only drawn: the block reads the words.
    #[test]
    fn marks_are_drawn_struck_through_underlined_and_in_monospace() {
        let colors = ResultColors {
            link: gpui::blue(),
            ring: gpui::red(),
            code_background: gpui::green(),
            mono: "Synthetic Mono".into(),
        };
        let blocks = markdown_blocks(
            "Ship ~~Friday~~ Monday, read the [plan](https://example.com/plan), run `make test`, \
             ~~[old](https://example.com/old)~~.",
        );
        let [ResultBlock::Paragraph(inline)] = blocks.as_slice() else {
            panic!("one paragraph: {blocks:?}");
        };
        let (highlights, mono) = inline_styles(inline, &colors);
        let parts: Vec<(&str, bool, bool, Option<gpui::Hsla>)> = highlights
            .iter()
            .map(|(range, style)| {
                assert_eq!(style.underline.is_some(), style.color.is_some());
                (
                    &inline.text[range.clone()],
                    style.strikethrough.is_some(),
                    style.underline.is_some(),
                    style.background_color,
                )
            })
            .collect();
        assert_eq!(
            parts,
            [
                ("Friday", true, false, None),
                ("plan", false, true, None),
                ("make test", false, false, Some(gpui::green())),
                ("old", true, true, None),
            ]
        );
        let link = &highlights[1].1;
        assert_eq!(link.color, Some(gpui::blue()));
        assert_eq!(
            link.underline.map(|u| (u.color, u.wavy)),
            Some((Some(gpui::blue()), false))
        );
        assert_eq!(
            mono.iter()
                .map(|(range, family)| (&inline.text[range.clone()], family.as_ref()))
                .collect::<Vec<_>>(),
            [("make test", "Synthetic Mono")]
        );
        assert!(
            highlights
                .iter()
                .all(|(_, style)| style.font_weight.is_none() && style.font_style.is_none())
        );

        let blocks = markdown_blocks(
            "A **bold** and *slanted* ***both*** [**link**](https://example.com/l) __word__",
        );
        let [ResultBlock::Paragraph(inline)] = blocks.as_slice() else {
            panic!("one paragraph: {blocks:?}");
        };
        assert_eq!(inline.text, "A bold and slanted both link word");
        assert_eq!(inline.spoken(), "A bold and slanted both link (link) word");
        let (highlights, _) = inline_styles(inline, &colors);
        let bold = Some(FontWeight::SEMIBOLD);
        let italic = Some(FontStyle::Italic);
        assert_eq!(
            highlights
                .iter()
                .map(|(range, style)| (
                    &inline.text[range.clone()],
                    style.font_weight,
                    style.font_style,
                    style.underline.is_some()
                ))
                .collect::<Vec<_>>(),
            [
                ("bold", bold, None, false),
                ("slanted", None, italic, false),
                ("both", bold, italic, false),
                ("link", bold, None, true),
                ("word", bold, None, false),
            ]
        );
    }

    /// INS-59: a code block is drawn in the theme's monospace font, as one
    /// named node.
    #[gpui::test]
    fn code_blocks_keep_the_monospace_font(cx: &mut TestAppContext) {
        cx.update(gpui::init);
        cx.update(|cx| {
            let mono = cx.theme().mono_font_family.clone();
            let mut block = code_block("code", "code-text", "let a_b = 1;".to_owned(), 1, cx);
            assert_eq!(gpui::Styled::style(&mut block).text.font_family, Some(mono));
            assert_eq!(
                crate::ui::a11y_name(&block),
                Some(("code".to_owned(), Role::Label, "let a_b = 1;".to_owned()))
            );
        });
    }

    /// Drags across a result of `source` drawn 400 px wide, from the left
    /// edge of its first block to the right edge of its last, in a window
    /// built like BroScribe's (gpui-component's `Root`, which draws the
    /// selection layer and answers Edit › Copy), then copies through the
    /// menu's Copy. Returns the selected text and what the clipboard holds.
    fn select_and_copy(cx: &mut TestAppContext, source: &'static str) -> (String, Option<String>) {
        use gpui::base::TextSelection;
        use gpui::component::input::Copy;
        use gpui::{Modifiers, MouseButton, point};

        /// A result under the control that opened it (in the app, the
        /// row's Show button, which a click focuses).
        struct Result(gpui::FocusHandle, &'static str);
        impl Render for Result {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let mut rcx = ResultCx::new(0, "Notes", Rc::new(|_, _, _| {}), cx);
                div()
                    .id("show")
                    .track_focus(&self.0)
                    .w(px(400.))
                    .flex()
                    .flex_col()
                    .children(
                        markdown_blocks(self.1)
                            .iter()
                            .enumerate()
                            .map(|(j, block)| {
                                div()
                                    .debug_selector(move || format!("block-{j}"))
                                    .child(result_block(&mut rcx, block, cx))
                            })
                            .collect::<Vec<_>>(),
                    )
            }
        }

        cx.update(gpui::init);
        let last = markdown_blocks(source).len() - 1;
        let focus = cx.update(|cx| cx.focus_handle());
        let (_, cx) = cx.add_window_view({
            let focus = focus.clone();
            move |window, cx| {
                let view = cx.new(|_| Result(focus, source));
                gpui::component::Root::new(view, window, cx)
            }
        });
        cx.update(|window, cx| {
            window.focus(&focus, cx);
            window.draw(cx).clear(cx);
        });
        let start = cx
            .debug_bounds("block-0")
            .expect("the first block is drawn");
        let last: &'static str = Box::leak(format!("block-{last}").into_boxed_str());
        let end = cx.debug_bounds(last).expect("the last block is drawn");
        cx.simulate_mouse_down(
            point(start.left() + px(1.), start.top() + px(2.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            point(end.right() - px(1.), end.bottom() - px(2.)),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            point(end.right() - px(1.), end.bottom() - px(2.)),
            MouseButton::Left,
            Modifiers::default(),
        );
        let selected = cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            TextSelection::selected_text(window, cx)
        });
        // The menu's Copy, as the menu bar dispatches it.
        cx.dispatch_action(Copy);
        cx.run_until_parked();
        (
            selected,
            cx.read_from_clipboard().and_then(|item| item.text()),
        )
    }

    /// INS-59: a selection that starts in a result's heading runs on into
    /// the paragraph under it, and Edit › Copy copies both.
    #[gpui::test]
    fn a_selection_runs_from_a_heading_into_its_paragraph(cx: &mut TestAppContext) {
        let both = "Next steps\nAna Example will draft the plan.";
        assert_eq!(
            select_and_copy(cx, "# Next steps\n\nAna Example will draft the plan."),
            (both.to_owned(), Some(both.to_owned()))
        );
    }

    /// INS-59: links and struck-out text do not break a selection: one
    /// that starts before a paragraph's link and ends after it, past
    /// struck-out text and a code span, takes the whole paragraph's words
    /// as shown, and Edit › Copy copies them.
    #[gpui::test]
    fn a_selection_runs_across_links_and_struck_text(cx: &mut TestAppContext) {
        let shown = "Ship Friday Monday: read the plan, run make test.\nNext";
        assert_eq!(
            select_and_copy(
                cx,
                "Ship ~~Friday~~ Monday: read the [plan](https://example.com/plan), run \
                 `make test`.\n\nNext"
            ),
            (shown.to_owned(), Some(shown.to_owned()))
        );
    }

    /// INS-59: the Meeting outline's topics hold their points as lists of
    /// their own, drawn inside the topic and indented under its text, and
    /// read so: the nested list is named and sits in its topic's item, its
    /// points in it, and the outer list holds only the topics. A table is a
    /// table of rows and cells named by their text. Copy still gives the
    /// Markdown source.
    #[gpui::test]
    fn outline_topics_hold_their_points_and_tables_read_as_tables(cx: &mut TestAppContext) {
        use crate::ui::{a11y_tree, spoken_log};

        const OUTLINE: &str = "- [00:00:05] Kickoff\n  - Goals agreed\n  - Dates set\n\
                               - [00:10:00] Budget\n  - Over by 5%\n";
        let dir = tempfile::tempdir().unwrap();
        let (app, lib, s) = fixtures::state_with_cli(cx, dir.path(), synthetic_cli(dir.path()));
        let data_dir = app.read_with(cx, |st, _| st.paths().data_dir.clone());
        let mut meeting = seed(&lib, &s.meeting, &data_dir);
        meeting
            .template_results
            .push(result("meeting-outline", "Meeting outline", OUTLINE));
        meeting.template_results.push(result(
            "meeting-notes",
            "Meeting notes",
            "| Who | What |\n|---|---|\n| Ana Example | Plan |\n| Bia Example | |\n",
        ));
        lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
        let window = cx.add_window({
            let app = app.clone();
            let id = s.meeting.clone();
            move |_, _| TemplatesTab::new(app, id)
        });
        window
            .update(cx, |tab, _, cx| {
                tab.set_meeting(meeting, PeopleLookup::default(), cx)
            })
            .unwrap();
        cx.run_until_parked();
        let rows = window.read_with(cx, |tab, _| tab.rows()).unwrap();
        let outline = rows.iter().position(|r| r.id == "meeting-outline").unwrap();
        let notes = rows.iter().position(|r| r.id == "meeting-notes").unwrap();

        // What is named, in reading order.
        let named = window
            .update(cx, |tab, window, cx| {
                spoken_log::while_building(|| Render::render(tab, window, cx)).1
            })
            .unwrap();
        let node = |id: String, role: Role, text: &str| (id, role, text.to_owned());
        let of = |prefixes: &[String]| {
            named
                .iter()
                .filter(|(id, ..)| prefixes.iter().any(|p| id.starts_with(p.as_str())))
                .cloned()
                .collect::<Vec<_>>()
        };
        let o = outline;
        assert_eq!(
            of(&[format!("template-list-{o}-"), format!("template-li-{o}-")]),
            [
                node(
                    format!("template-list-{o}-0"),
                    Role::List,
                    "Meeting outline list 1"
                ),
                node(
                    format!("template-li-{o}-0-0"),
                    Role::ListItem,
                    "[00:00:05] Kickoff"
                ),
                node(
                    format!("template-list-{o}-1"),
                    Role::List,
                    "Meeting outline list 2"
                ),
                node(
                    format!("template-li-{o}-1-0"),
                    Role::ListItem,
                    "Goals agreed"
                ),
                node(format!("template-li-{o}-1-1"), Role::ListItem, "Dates set"),
                node(
                    format!("template-li-{o}-0-1"),
                    Role::ListItem,
                    "[00:10:00] Budget"
                ),
                node(
                    format!("template-list-{o}-2"),
                    Role::List,
                    "Meeting outline list 3"
                ),
                node(format!("template-li-{o}-2-0"), Role::ListItem, "Over by 5%"),
            ]
        );
        let n = notes;
        assert_eq!(
            of(&[format!("template-th-{n}-"), format!("template-td-{n}-")]),
            [
                node(format!("template-th-{n}-0-0"), Role::ColumnHeader, "Who"),
                node(format!("template-th-{n}-0-1"), Role::ColumnHeader, "What"),
                node(format!("template-td-{n}-0-0-0"), Role::Cell, "Ana Example"),
                node(format!("template-td-{n}-0-0-1"), Role::Cell, "Plan"),
                node(format!("template-td-{n}-0-1-0"), Role::Cell, "Bia Example"),
                node(format!("template-td-{n}-0-1-1"), Role::Cell, "None"),
            ]
        );

        // The accessibility tree GPUI builds from them.
        let tree = cx
            .update_window(window.into(), |_, window, cx| {
                a11y_tree::draw_and_read(window, cx)
            })
            .unwrap();
        let find = |id: String| tree.find(&id);
        let topics = find(format!("template-list-{o}-0"));
        let kickoff = find(format!("template-li-{o}-0-0"));
        let budget = find(format!("template-li-{o}-0-1"));
        let points = find(format!("template-list-{o}-1"));
        assert_eq!(tree.read(topics), ("List", "Meeting outline list 1"));
        assert_eq!(tree.nodes[topics].children, [kickoff, budget], "{tree:?}");
        assert_eq!(tree.read(points), ("List", "Meeting outline list 2"));
        assert_eq!(tree.parent(points), Some(kickoff), "{tree:?}");
        assert_eq!(
            tree.nodes[points].children,
            [
                find(format!("template-li-{o}-1-0")),
                find(format!("template-li-{o}-1-1"))
            ]
        );
        assert_eq!(
            tree.parent(find(format!("template-list-{o}-2"))),
            Some(budget)
        );
        let table = find(format!("template-table-{n}-0"));
        assert_eq!(tree.read(table), ("Table", "Meeting notes table 1"));
        let children = |ix: usize| {
            tree.nodes[ix]
                .children
                .iter()
                .map(|&c| tree.read(c))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            children(table),
            [
                ("Row", "Column headers"),
                ("Row", "Row 1"),
                ("Row", "Row 2")
            ]
        );
        let row = |r: usize| children(tree.nodes[table].children[r]);
        assert_eq!(row(0), [("ColumnHeader", "Who"), ("ColumnHeader", "What")]);
        assert_eq!(row(1), [("Cell", "Ana Example"), ("Cell", "Plan")]);
        assert_eq!(row(2), [("Cell", "Bia Example"), ("Cell", "None")]);
        for result in [outline, notes] {
            let article = find(format!("template-result-{result}"));
            assert!(tree.unnamed_under(article).is_empty(), "{tree:?}");
            assert!(
                tree.names_repeated_from_ancestors(article).is_empty(),
                "{tree:?}"
            );
        }

        // Copy gives the Markdown as written, nesting and all (the stored
        // result's final line break aside).
        window
            .update(cx, |tab, _, cx| tab.copy("meeting-outline", cx))
            .unwrap();
        assert_eq!(
            cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
            Some(OUTLINE.trim_end())
        );

        // Drawn: each point under its topic's text, indented past its
        // marker, before the next topic.
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        let mut bounds = |id: String| {
            // `debug_bounds` takes a static selector.
            let id: &'static str = Box::leak(id.into_boxed_str());
            cx.debug_bounds(id)
                .unwrap_or_else(|| panic!("{id} is not drawn"))
        };
        let kickoff = bounds(format!("template-li-{o}-0-0"));
        let goals = bounds(format!("template-li-{o}-1-0"));
        let dates = bounds(format!("template-li-{o}-1-1"));
        let budget = bounds(format!("template-li-{o}-0-1"));
        assert!(goals.left() > kickoff.left(), "{goals:?} {kickoff:?}");
        assert_eq!(dates.left(), goals.left());
        assert!(goals.top() > kickoff.top() && dates.bottom() <= kickoff.bottom());
        assert!(budget.top() >= dates.bottom());
        assert_eq!(budget.left(), kickoff.left());
    }

    /// INS-59: a result's link to a web address is a `Link` node named by
    /// its words, inside its paragraph or list item, which reads "(link)"
    /// after them (and "(deleted)" after struck-out text). A click, or
    /// Enter once the link has focus, opens it through the platform's
    /// browser launcher, once per press of Enter (key down and key up, as
    /// the platform sends them); Space opens nothing; Tab moves on to the
    /// next link. A relative link is only text, with no node.
    #[gpui::test]
    fn web_links_in_results_open_through_the_browser_launcher(cx: &mut TestAppContext) {
        use broscribe_platform::fakes::FakeBrowserLauncher;

        use crate::ui::a11y_tree;

        const NOTES: &str = "Ship ~~Friday~~ Monday: read the [plan](https://example.com/plan) \
                             and [notes](notes.md).\n\n- See the [agenda](https://example.com/agenda)\n";
        let dir = tempfile::tempdir().unwrap();
        let (app, lib, s) = fixtures::state_with_cli(cx, dir.path(), synthetic_cli(dir.path()));
        let browser = Arc::new(FakeBrowserLauncher::default());
        app.update(cx, |s, _| s.set_browser(browser.clone()));
        let data_dir = app.read_with(cx, |st, _| st.paths().data_dir.clone());
        let mut meeting = seed(&lib, &s.meeting, &data_dir);
        meeting
            .template_results
            .push(result("meeting-notes", "Meeting notes", NOTES));
        lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
        let window = cx.add_window({
            let app = app.clone();
            let id = s.meeting.clone();
            move |_, _| TemplatesTab::new(app, id)
        });
        window
            .update(cx, |tab, _, cx| {
                tab.set_meeting(meeting, PeopleLookup::default(), cx)
            })
            .unwrap();
        cx.run_until_parked();
        let rows = window.read_with(cx, |tab, _| tab.rows()).unwrap();
        let n = rows.iter().position(|r| r.id == "meeting-notes").unwrap();

        let tree = cx
            .update_window(window.into(), |_, window, cx| {
                a11y_tree::draw_and_read(window, cx)
            })
            .unwrap();
        let article = tree.find(&format!("template-result-{n}"));
        let paragraph = tree.find(&format!("template-p-{n}-0"));
        let item = tree.find(&format!("template-li-{n}-1-0"));
        assert_eq!(
            tree.read(paragraph),
            (
                "Label",
                "Ship Friday (deleted) Monday: read the plan (link) and notes."
            )
        );
        assert_eq!(tree.read(item), ("ListItem", "See the agenda (link)"));
        let links: Vec<(&str, Option<usize>, Option<&str>)> = tree
            .subtree(article)
            .into_iter()
            .filter(|&ix| tree.nodes[ix].role == "Link")
            .map(|ix| {
                (
                    tree.read(ix).1,
                    tree.parent(ix),
                    tree.nodes[ix].element_id.as_deref(),
                )
            })
            .collect();
        let (plan_id, agenda_id) = (format!("template-a-{n}-0-0"), format!("template-a-{n}-1-0"));
        assert_eq!(
            links,
            [
                ("plan", Some(paragraph), Some(plan_id.as_str())),
                ("agenda", Some(item), Some(agenda_id.as_str())),
            ],
            "{tree:?}"
        );
        assert!(tree.unnamed_under(article).is_empty(), "{tree:?}");
        assert!(
            tree.names_repeated_from_ancestors(article).is_empty(),
            "{tree:?}"
        );

        let opened = || browser.opened.lock().unwrap().clone();
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        // `debug_bounds` takes a static selector.
        let plan: &'static str = Box::leak(plan_id.into_boxed_str());
        let bounds = cx.debug_bounds(plan).expect("the link is drawn");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(opened(), ["https://example.com/plan"]);
        // The click focused the link: pressing and releasing Enter opens it
        // again, once (GPUI's keyboard click on the release opens
        // nothing); a held Enter's repeats open nothing more; Space opens
        // nothing; Tab moves on.
        press(&mut cx, "enter");
        assert_eq!(opened(), ["https://example.com/plan"; 2]);
        key(&mut cx, "enter", Key::Down);
        key(&mut cx, "enter", Key::Repeat);
        key(&mut cx, "enter", Key::Repeat);
        key(&mut cx, "enter", Key::Up);
        assert_eq!(opened(), ["https://example.com/plan"; 3]);
        press(&mut cx, "space");
        assert_eq!(opened(), ["https://example.com/plan"; 3]);
        cx.update(|window, cx| window.focus_next(cx));
        press(&mut cx, "enter");
        assert_eq!(
            opened(),
            [
                "https://example.com/plan",
                "https://example.com/plan",
                "https://example.com/plan",
                "https://example.com/agenda"
            ]
        );
        let notice = cx.update(|window, cx| {
            window
                .root::<TemplatesTab>()
                .flatten()
                .and_then(|tab| tab.read(cx).notice().cloned())
        });
        assert_eq!(notice, None, "nothing failed");
    }

    /// A link that wraps can be clicked on every row it spans, while it
    /// stays one `Link` node and one tab stop, on its first row.
    #[gpui::test]
    fn a_wrapped_link_opens_from_each_of_its_rows(cx: &mut TestAppContext) {
        use std::cell::RefCell;

        use crate::ui::a11y_tree;

        const SOURCE: &str = "Before [the synthetic planning document for the next quarter]\
                              (https://example.com/plan) after.";
        type Opened = Rc<RefCell<Vec<String>>>;
        struct Narrow(Opened);
        impl Render for Narrow {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let opened = self.0.clone();
                let open: OpenLink = Rc::new(move |url, _, _| opened.borrow_mut().push(url.into()));
                let mut rcx = ResultCx::new(0, "Notes", open, cx);
                div().id("narrow").w(px(160.)).children(
                    markdown_blocks(SOURCE)
                        .iter()
                        .map(|block| result_block(&mut rcx, block, cx))
                        .collect::<Vec<_>>(),
                )
            }
        }

        cx.update(gpui::init);
        let opened = Opened::default();
        let (_, cx) = cx.add_window_view({
            let opened = opened.clone();
            move |_, _| Narrow(opened)
        });
        let tree = cx.update(a11y_tree::draw_and_read);
        let links: Vec<usize> = (0..tree.nodes.len())
            .filter(|&ix| tree.nodes[ix].role == "Link")
            .collect();
        assert_eq!(links.len(), 1, "{tree:?}");
        assert_eq!(
            tree.read(links[0]),
            (
                "Link",
                "the synthetic planning document for the next quarter"
            )
        );
        assert_eq!(tree.parent(links[0]), Some(tree.find("template-p-0-0")));
        let first = cx.debug_bounds("template-a-0-0-0").expect("the first row");
        let second = cx
            .debug_bounds("template-a-0-0-0-row-1")
            .expect("the link wraps onto a second row");
        assert!(second.top() >= first.bottom(), "{first:?} {second:?}");
        cx.simulate_click(second.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        cx.simulate_click(first.center(), gpui::Modifiers::none());
        cx.run_until_parked();
        assert_eq!(*opened.borrow(), ["https://example.com/plan"; 2]);
    }

    /// A key event as the platform sends it.
    #[derive(Clone, Copy)]
    enum Key {
        Down,
        /// A key-down repeated while the key is held.
        Repeat,
        Up,
    }

    /// Sends `key` ("enter", "space") through the window's event dispatch
    /// as the platform does: key down and key up are separate events,
    /// which `simulate_keystrokes` (key down only) does not send.
    fn key(cx: &mut gpui::VisualTestContext, key: &'static str, event: Key) {
        use gpui::InputEvent as _;

        let keystroke = gpui::Keystroke::parse(key).unwrap();
        let input = match event {
            Key::Down | Key::Repeat => gpui::KeyDownEvent {
                keystroke,
                is_held: matches!(event, Key::Repeat),
                prefer_character_input: false,
            }
            .to_platform_input(),
            Key::Up => gpui::KeyUpEvent { keystroke }.to_platform_input(),
        };
        cx.update(|window, cx| {
            window.dispatch_event(input, cx);
        });
        cx.run_until_parked();
    }

    /// Presses and releases `key`.
    fn press(cx: &mut gpui::VisualTestContext, name: &'static str) {
        key(cx, name, Key::Down);
        key(cx, name, Key::Up);
    }

    /// A window built like BroScribe's (gpui-component's `Root`, which
    /// draws the window's text selection layer and answers Edit › Copy)
    /// holding the Templates tab of a meeting whose "Meeting notes" result
    /// is `notes`, opened, with `browser` as the platform's browser
    /// launcher. Returns the window, the tab and the result's row.
    fn notes_in_a_window(
        cx: &mut TestAppContext,
        dir: &Path,
        notes: &str,
        browser: Arc<dyn broscribe_platform::BrowserLauncher>,
    ) -> (gpui::VisualTestContext, Entity<TemplatesTab>, usize) {
        let (app, lib, s) = fixtures::state_with_cli(cx, dir, synthetic_cli(dir));
        app.update(cx, |s, _| s.set_browser(browser));
        let data_dir = app.read_with(cx, |st, _| st.paths().data_dir.clone());
        let mut meeting = seed(&lib, &s.meeting, &data_dir);
        meeting
            .template_results
            .push(result("meeting-notes", "Meeting notes", notes));
        lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
        let tab = cx.new(|_| TemplatesTab::new(app.clone(), s.meeting.clone()));
        let window = cx.add_window({
            let tab = tab.clone();
            move |window, cx| gpui::component::Root::new(tab, window, cx)
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        tab.update(&mut cx, |tab, cx| {
            tab.set_meeting(meeting, PeopleLookup::default(), cx)
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        let n = tab
            .read_with(&cx, |tab, _| tab.rows())
            .iter()
            .position(|r| r.id == "meeting-notes")
            .unwrap();
        (cx, tab, n)
    }

    /// INS-59: dragging across words inside a link selects them and opens
    /// nothing: Edit › Copy copies the words, and the browser launcher was
    /// never called. A plain click still opens the link once. A double
    /// click opens it on its first click only; its second selects a word.
    #[gpui::test]
    fn selecting_a_links_words_does_not_open_it(cx: &mut TestAppContext) {
        use broscribe_platform::fakes::FakeBrowserLauncher;
        use gpui::base::TextSelection;
        use gpui::component::input::Copy;
        use gpui::{Modifiers, MouseButton, MouseDownEvent, MouseUpEvent, point};

        let dir = tempfile::tempdir().unwrap();
        let browser = Arc::new(FakeBrowserLauncher::default());
        let (mut cx, _, n) = notes_in_a_window(
            cx,
            dir.path(),
            "Read [the synthetic plan](https://example.com/plan) today.",
            browser.clone(),
        );
        let opened = || browser.opened.lock().unwrap().clone();
        let id: &'static str = Box::leak(format!("template-a-{n}-0-0").into_boxed_str());
        let link = cx.debug_bounds(id).expect("the link is drawn");
        let y = link.center().y;

        cx.simulate_mouse_down(
            point(link.left() + px(1.), y),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.simulate_mouse_move(
            point(link.right() - px(1.), y),
            Some(MouseButton::Left),
            Modifiers::default(),
        );
        cx.simulate_mouse_up(
            point(link.right() - px(1.), y),
            MouseButton::Left,
            Modifiers::default(),
        );
        cx.run_until_parked();
        cx.dispatch_action(Copy);
        cx.run_until_parked();
        assert_eq!(
            cx.read_from_clipboard().and_then(|item| item.text()),
            Some("the synthetic plan".to_owned())
        );
        assert!(opened().is_empty(), "{:?}", opened());

        cx.simulate_click(link.center(), Modifiers::none());
        cx.run_until_parked();
        assert_eq!(opened(), ["https://example.com/plan"]);

        let at = link.center();
        for click_count in [1, 2] {
            cx.simulate_event(MouseDownEvent {
                button: MouseButton::Left,
                position: at,
                modifiers: Modifiers::default(),
                click_count,
                first_mouse: false,
            });
            cx.simulate_event(MouseUpEvent {
                button: MouseButton::Left,
                position: at,
                modifiers: Modifiers::default(),
                click_count,
            });
        }
        cx.run_until_parked();
        assert_eq!(opened(), ["https://example.com/plan"; 2]);
        let word = cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            TextSelection::selected_text(window, cx)
        });
        assert!(
            ["the", "synthetic", "plan"].contains(&word.as_str()),
            "{word:?}"
        );
    }

    /// What `tracing` wrote on this thread while `run` ran.
    fn logged(run: impl FnOnce()) -> String {
        use std::sync::Mutex;

        #[derive(Clone, Default)]
        struct Lines(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Lines {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let lines = Lines::default();
        let sink = lines.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || sink.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        String::from_utf8(lines.0.lock().unwrap().clone()).unwrap()
    }

    /// INS-59: a link that does not open shows the fixed alert "Couldn't
    /// open this link in your browser.", with no address. The launcher's
    /// error quotes the address, as macOS's does, and the address carries
    /// a synthetic private query string behind the words "plan"; neither
    /// the alert, any node of the window nor the log holds it. The log
    /// says a link failed and gives the error's code.
    #[gpui::test]
    fn a_link_that_fails_to_open_shows_no_address(cx: &mut TestAppContext) {
        use crate::ui::a11y_tree;

        const SECRET: &str = "synthetic-private-query";
        struct FailingLauncher;
        impl broscribe_platform::BrowserLauncher for FailingLauncher {
            fn open(&self, url: &str) -> broscribe_platform::Result<()> {
                Err(Error::Internal(format!("the system could not open {url}")))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let (mut cx, tab, n) = notes_in_a_window(
            cx,
            dir.path(),
            "Read the [plan](https://example.com/plan?token=synthetic-private-query).",
            Arc::new(FailingLauncher),
        );
        let id: &'static str = Box::leak(format!("template-a-{n}-0-0").into_boxed_str());
        let link = cx.debug_bounds(id).expect("the link is drawn");
        let log = logged(|| {
            cx.simulate_click(link.center(), gpui::Modifiers::none());
            cx.run_until_parked();
        });
        assert!(log.contains("a template link could not be opened"), "{log}");
        assert!(log.contains("internal"), "{log}");
        assert!(
            !log.contains(SECRET) && !log.contains("example.com"),
            "{log}"
        );

        let notice = tab.read_with(&cx, |tab, _| tab.notice().cloned());
        assert_eq!(notice, Some((LINK_FAILED.to_owned(), true)));
        let tree = cx.update(a11y_tree::draw_and_read);
        assert_eq!(
            tree.read(tree.find("templates-notice")),
            ("Alert", "Couldn't open this link in your browser.")
        );
        assert!(
            tree.nodes.iter().all(|node| !node.name.contains(SECRET)),
            "{tree:?}"
        );
    }

    /// Redrawing an open result (a playback tick, any notify) does not
    /// parse its Markdown again; new Markdown is parsed once, and a closed
    /// result is not parsed at all.
    #[gpui::test]
    fn a_result_is_parsed_again_only_when_its_markdown_changes(cx: &mut TestAppContext) {
        use crate::ui::spoken_log;

        let dir = tempfile::tempdir().unwrap();
        let (app, lib, s) = fixtures::state_with_cli(cx, dir.path(), synthetic_cli(dir.path()));
        let data_dir = app.read_with(cx, |st, _| st.paths().data_dir.clone());
        let mut meeting = seed(&lib, &s.meeting, &data_dir);
        meeting.template_results.push(result(
            "meeting-notes",
            "Meeting notes",
            "# Plan\n\nShip ~~Friday~~ Monday.",
        ));
        lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
        let window = cx.add_window({
            let app = app.clone();
            let id = s.meeting.clone();
            move |_, _| TemplatesTab::new(app, id)
        });
        window
            .update(cx, |tab, _, cx| {
                tab.set_meeting(meeting.clone(), PeopleLookup::default(), cx)
            })
            .unwrap();
        cx.run_until_parked();
        let draw = |cx: &mut TestAppContext| {
            window.update(cx, |_, _, cx| cx.notify()).unwrap();
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
        };
        window
            .update(cx, |tab, _, cx| tab.toggle("meeting-notes", cx))
            .unwrap();
        draw(cx);
        let closed = parse_count();
        draw(cx);
        assert_eq!(parse_count(), closed, "a closed result is not parsed");

        window
            .update(cx, |tab, _, cx| tab.toggle("meeting-notes", cx))
            .unwrap();
        cx.run_until_parked();
        draw(cx);
        let opened = parse_count();
        assert_eq!(opened, closed + 1);
        for _ in 0..4 {
            draw(cx);
        }
        assert_eq!(parse_count(), opened, "redraws parse nothing");

        meeting.template_results.last_mut().unwrap().markdown = "# Plan\n\nShip Tuesday.".into();
        window
            .update(cx, |tab, _, cx| {
                tab.set_meeting(meeting.clone(), PeopleLookup::default(), cx)
            })
            .unwrap();
        cx.run_until_parked();
        draw(cx);
        draw(cx);
        assert_eq!(parse_count(), opened + 1, "new Markdown is parsed once");
        let named = window
            .update(cx, |tab, window, cx| {
                spoken_log::while_building(|| Render::render(tab, window, cx)).1
            })
            .unwrap();
        assert!(
            named
                .iter()
                .any(|(_, role, name)| *role == Role::Label && name == "Ship Tuesday."),
            "{named:?}"
        );

        // Closed, the result is forgotten; opened again, parsed again.
        window
            .update(cx, |tab, _, cx| {
                tab.toggle("meeting-notes", cx);
            })
            .unwrap();
        draw(cx);
        window
            .update(cx, |tab, _, cx| {
                tab.toggle("meeting-notes", cx);
            })
            .unwrap();
        draw(cx);
        assert_eq!(parse_count(), opened + 2);
    }

    /// INS-01 and INS-59: every text of the Templates tab is a named node,
    /// and a result with a heading, a paragraph and two list items is four
    /// named nodes, the list items placed in their list.
    #[gpui::test]
    fn templates_tab_texts_and_result_blocks_are_named_nodes(cx: &mut TestAppContext) {
        use crate::ui::spoken_log::{self, assert_distinct, node};

        let dir = tempfile::tempdir().unwrap();
        let (app, lib, s) = fixtures::state_with_cli(cx, dir.path(), synthetic_cli(dir.path()));
        let data_dir = app.read_with(cx, |st, _| st.paths().data_dir.clone());
        let mut meeting = seed(&lib, &s.meeting, &data_dir);
        meeting.template_results.push(result(
            "meeting-notes",
            "Meeting notes",
            "# Next steps\n\nAna Example will draft the plan.\n\n- Send the agenda\n- Book the room",
        ));
        lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
        let window = cx.add_window({
            let app = app.clone();
            let id = s.meeting.clone();
            move |_, _| TemplatesTab::new(app, id)
        });
        let render = |cx: &mut TestAppContext| {
            window
                .update(cx, |tab, window, cx| {
                    spoken_log::while_building(|| Render::render(tab, window, cx)).1
                })
                .unwrap()
        };
        window
            .update(cx, |tab, _, cx| {
                tab.set_meeting(meeting, PeopleLookup::default(), cx)
            })
            .unwrap();
        let reading = render(cx);
        assert!(
            reading.contains(&(
                "templates-loading".to_owned(),
                Role::Status,
                "Reading the templates…".to_owned()
            )),
            "{reading:?}"
        );
        cx.run_until_parked();
        window
            .update(cx, |tab, _, cx| tab.toggle("meeting-outline", cx))
            .unwrap();
        cx.run_until_parked();
        let named = render(cx);
        // Every node is named, under its own ID, and no two nodes carry the
        // same text, except for credits and AI captions in separate rows.
        assert_distinct(&named, &["template-credit-*", "template-ai-note-*"]);
        assert_eq!(node(&named, "templates-intro").0, Role::Label);
        let rows = window.read_with(cx, |tab, _| tab.rows()).unwrap();
        for (ix, row) in rows.iter().enumerate() {
            if !row.description.is_empty() {
                assert_eq!(
                    node(&named, &format!("template-description-{ix}")),
                    (Role::Label, row.description.as_str())
                );
            }
        }
        let notes = rows.iter().position(|r| r.id == "meeting-notes").unwrap();
        let outline = rows.iter().position(|r| r.id == "meeting-outline").unwrap();
        assert_eq!(
            node(&named, &format!("template-not-run-{outline}")).1,
            "Not run for this meeting yet."
        );
        assert_eq!(
            node(&named, &format!("template-credit-{notes}")).0,
            Role::Label
        );
        let blocks: Vec<(String, Role, String)> = named
            .iter()
            .filter(|(id, ..)| {
                ["h", "p", "li"]
                    .iter()
                    .any(|part| id.starts_with(&format!("template-{part}-{notes}-")))
            })
            .cloned()
            .collect();
        let block = |id: &str, role: Role, text: &str| (id.to_owned(), role, text.to_owned());
        assert_eq!(
            blocks,
            [
                block(
                    &format!("template-h-{notes}-0"),
                    Role::Heading,
                    "Next steps"
                ),
                block(
                    &format!("template-p-{notes}-1"),
                    Role::Label,
                    "Ana Example will draft the plan."
                ),
                block(
                    &format!("template-li-{notes}-2-0"),
                    Role::ListItem,
                    "Send the agenda"
                ),
                block(
                    &format!("template-li-{notes}-2-1"),
                    Role::ListItem,
                    "Book the room"
                ),
            ]
        );
        // The list is named apart from its items and from the result.
        assert_eq!(
            node(&named, &format!("template-list-{notes}-2")),
            (Role::List, "Meeting notes list 1")
        );
        // The result is drawn: selectable paragraphs and items lay out.
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
    }

    #[test]
    fn helpers_build_stdin_args_and_free_ids() {
        let stdin: serde_json::Value =
            serde_json::from_slice(&template_stdin("Synthetic", "Short", "Write it.", None))
                .unwrap();
        assert_eq!(
            stdin,
            json!({"name": "Synthetic", "description": "Short", "body": "Write it."})
        );
        assert_eq!(
            save_template_args("synthetic", false),
            args(["templates", "new", "synthetic", "--stdin"])
        );
        assert_eq!(
            save_template_args("bant", true),
            args(["templates", "edit", "bant", "--stdin"])
        );
        assert_eq!(
            auto_args("bant", true),
            args(["templates", "auto", "add", "bant"])
        );
        assert_eq!(
            auto_args("bant", false),
            args(["templates", "auto", "remove", "bant"])
        );
        let taken = vec!["bant-copy".to_owned()];
        assert_eq!(duplicate_id("bant", &taken), "bant-copy-2");
        assert_eq!(duplicate_id("meddic", &taken), "meddic-copy");
        assert!(duplicate_id(&"x".repeat(40), &[]).len() <= 40);
    }

    fn seed(lib: &Library, meeting: &MeetingId, data_dir: &Path) -> Meeting {
        templates::save_user(
            data_dir,
            "synthetic-retro",
            "Synthetic retro",
            "What went well",
            "List what went well.",
        )
        .unwrap();
        let mut m = lib.load_meeting(meeting).unwrap();
        m.template_results = vec![
            result("synthetic-retro", "Synthetic retro", "# Retro\n\nAll good."),
            result("synthetic-gone", "Synthetic gone", "# Gone\n\nStill here."),
        ];
        lib.write(|tx| tx.save_meeting(&m)).unwrap();
        m
    }

    #[gpui::test]
    fn tab_lists_definitions_and_results_of_deleted_templates(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (app, lib, s) = fixtures::state_with_cli(cx, dir.path(), synthetic_cli(dir.path()));
        let data_dir = app.read_with(cx, |st, _| st.paths().data_dir.clone());
        let meeting = seed(&lib, &s.meeting, &data_dir);
        let people = PeopleLookup::default();
        let window = cx.add_window({
            let app = app.clone();
            let id = s.meeting.clone();
            move |_, _| TemplatesTab::new(app, id)
        });
        window
            .update(cx, |tab, _, cx| tab.set_meeting(meeting, people, cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |tab, _, _| {
                let rows = tab.rows();
                let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
                assert_eq!(
                    ids,
                    [
                        "meeting-notes",
                        "meeting-outline",
                        "follow-up-email",
                        "bant",
                        "meddic",
                        "synthetic-retro",
                        "synthetic-gone"
                    ]
                );
                let gone = rows.last().unwrap();
                assert!(!gone.defined);
                assert_eq!(gone.name, "Synthetic gone");
                assert!(rows[5].defined && rows[5].result.is_some());
                assert!(rows[0].result.is_none());
                // Its result stays readable; Run is disabled.
                assert_eq!(
                    tab.result_markdown("synthetic-gone").as_deref(),
                    Some("# Gone\n\nStill here.")
                );
            })
            .unwrap();
        window
            .update(cx, |tab, _, cx| {
                assert!(tab.panel("synthetic-gone").is_none());
                tab.copy("synthetic-gone", cx);
                // …and exportable.
                tab.export("synthetic-gone", cx);
            })
            .unwrap();
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        assert_eq!(
            cx.read_from_clipboard().and_then(|c| c.text()).as_deref(),
            Some("# Gone\n\nStill here.")
        );
        let out = dir.path().join("gone.md");
        let chosen = out.clone();
        cx.simulate_new_path_selection(move |_| Some(chosen));
        cx.run_until_parked();
        let args_log = log(dir.path(), "args.log");
        assert!(
            args_log.contains(&format!(
                "export {} --include template:synthetic-gone --format md --spec-stdin\n",
                s.meeting,
            )),
            "{args_log}"
        );
        assert!(
            !args_log.contains("gone.md"),
            "the export path leaked into argv"
        );
        let spec: serde_json::Value = serde_json::from_str(&log(dir.path(), "spec.json")).unwrap();
        assert_eq!(spec, json!({ "out": out.to_str().unwrap() }));

        // Generation history belongs to the saved result, even after the
        // definition is deleted and the meeting is opened again.
        let mut meeting = lib.load_meeting(&s.meeting).unwrap();
        let mut current = meeting.template_result("synthetic-gone").unwrap().clone();
        current.created_at += chrono::Duration::seconds(1);
        meeting.publish_template_result(current);
        window
            .update(cx, |tab, _, cx| {
                tab.set_meeting(meeting, PeopleLookup::default(), cx)
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            let restore = tree.find("generate-template-synthetic-gone-restore");
            assert_eq!(tree.read(restore).1, "Restore previous result");
        })
        .unwrap();
    }

    #[gpui::test]
    fn a_pending_template_ticks_while_the_tab_is_shown_even_if_collapsed(cx: &mut TestAppContext) {
        use broscribe_core::queue::{Queue, TEMPLATE_KIND, template_job_key};
        let dir = tempfile::tempdir().unwrap();
        let (app, lib, s) = fixtures::state_with_cli(cx, dir.path(), synthetic_cli(dir.path()));
        let data_dir = app.read_with(cx, |st, _| st.paths().data_dir.clone());
        let meeting = seed(&lib, &s.meeting, &data_dir);
        // A run of the template was queued elsewhere (the CLI).
        let job = lib
            .write(|tx| {
                Queue::open(tx.root())?.enqueue(
                    tx,
                    TEMPLATE_KIND,
                    &template_job_key(&s.meeting, "synthetic-retro"),
                    json!({"meeting_id": s.meeting, "template_id": "synthetic-retro"}),
                )
            })
            .unwrap();
        let window = cx.add_window({
            let app = app.clone();
            let id = s.meeting.clone();
            move |_, _| TemplatesTab::new(app, id)
        });
        window
            .update(cx, |tab, _, cx| {
                tab.set_meeting(meeting, PeopleLookup::default(), cx);
                tab.set_visible(true, cx);
            })
            .unwrap();
        cx.run_until_parked();
        let ticking = |cx: &mut TestAppContext| {
            window
                .update(cx, |tab, _, cx| {
                    let p = tab.panel("synthetic-retro").unwrap().read(cx);
                    assert!(p.phase().in_flight(), "{:?}", p.phase());
                    p.ticking()
                })
                .unwrap()
        };
        let toggle = |cx: &mut TestAppContext| {
            window
                .update(cx, |tab, _, cx| tab.toggle("synthetic-retro", cx))
                .unwrap();
            cx.run_until_parked();
        };
        assert!(ticking(cx), "expanded and shown");
        toggle(cx);
        assert!(ticking(cx), "collapsed and shown");
        let row_label = |cx: &mut TestAppContext| {
            cx.update_window(window.into(), |_, window, cx| {
                let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
                tree.read(tree.find("template-toggle-5")).1.to_owned()
            })
            .unwrap()
        };
        assert_eq!(row_label(cx), "Synthetic retro, Queued");
        let _lease = Queue::open(lib.root())
            .unwrap()
            .claim(
                &lib,
                &job,
                &broscribe_platform::fakes::FakeProcessProbe::new(4242),
            )
            .unwrap()
            .unwrap();
        app.update(cx, |app, cx| {
            use crate::jobs::{ProgressEvent, ProgressEventKind, WorkerBatch, WorkerMsg};
            let mut batch = WorkerBatch::default();
            batch.push(WorkerMsg::Progress(ProgressEvent {
                event: ProgressEventKind::JobStarted,
                job_id: Some(job.to_string()),
                kind: Some(TEMPLATE_KIND.into()),
                meeting_id: Some(s.meeting.clone()),
                fraction: None,
                message: None,
            }));
            app.apply_worker_batch(batch, cx);
        });
        cx.run_until_parked();
        assert_eq!(row_label(cx), "Synthetic retro, Generating");
        window
            .update(cx, |tab, _, cx| tab.set_visible(false, cx))
            .unwrap();
        assert!(!ticking(cx), "another tab shown");
    }

    fn u44_window(
        cx: &mut TestAppContext,
        dir: &Path,
        setup: impl FnOnce(&mut Meeting),
    ) -> (
        gpui::VisualTestContext,
        Entity<TemplatesTab>,
        Entity<AppState>,
        MeetingId,
    ) {
        let (app, lib, s) = fixtures::state_with_cli(cx, dir, synthetic_cli(dir));
        let mut meeting = lib.load_meeting(&s.meeting).unwrap();
        setup(&mut meeting);
        lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
        let tab = cx.new(|_| TemplatesTab::new(app.clone(), s.meeting.clone()));
        let root = cx.add_window({
            let tab = tab.clone();
            move |window, cx| {
                gpui::init(cx);
                let keys = crate::ui::keymap::key_bindings(cx);
                cx.bind_keys(keys);
                gpui::component::Root::new(tab, window, cx)
            }
        });
        let mut visual = gpui::VisualTestContext::from_window(root.into(), cx);
        visual.simulate_resize(gpui::size(px(1000.), px(1000.)));
        visual.update(|window, _| window.activate_window());
        tab.update(&mut visual, |tab, cx| {
            tab.set_meeting(meeting, PeopleLookup::default(), cx);
            tab.set_visible(true, cx);
        });
        visual.run_until_parked();
        kit::dialog_gate::draw(&mut visual);
        visual.run_until_parked();
        kit::dialog_gate::draw(&mut visual);
        (visual, tab, app, s.meeting)
    }

    fn control(
        visual: &mut gpui::VisualTestContext,
        id: &str,
    ) -> gpui::base::test_support::ElementSnapshot {
        visual.update(|window, _| {
            gpui::base::test_support::snapshots(window)
                .into_iter()
                .find(|node| {
                    node.path()
                        .last()
                        .is_some_and(|part| part.to_string() == id)
                })
                .unwrap_or_else(|| panic!("missing control {id}"))
        })
    }

    #[gpui::test]
    fn template_rows_keep_actions_and_named_results_in_their_own_disclosures(
        cx: &mut TestAppContext,
    ) {
        use crate::ui::{
            a11y_tree,
            kit::dialog_gate::{click, draw},
        };

        let dir = tempfile::tempdir().unwrap();
        let (mut visual, _, app, _) = u44_window(cx, dir.path(), |meeting| {
            meeting.template_results.push(result(
                "meeting-notes",
                "Meeting notes",
                "# Plan\n\nSend the agenda.",
            ));
        });
        app.update(&mut visual, |app, cx| {
            let mut config = app.config().clone();
            config.templates.auto.push("meeting-outline".into());
            app.set_config(config, cx);
        });
        visual.run_until_parked();
        draw(&mut visual);
        let row = control(&mut visual, "template-toggle-0");
        assert_eq!(row.expanded(), Some(true));
        assert!(
            row.label().unwrap().starts_with("Meeting notes, Ready"),
            "{row:?}"
        );
        let empty = control(&mut visual, "template-toggle-1");
        assert_eq!(empty.expanded(), Some(false));
        assert_eq!(
            empty.label(),
            Some("Meeting outline, Not run, runs automatically")
        );
        let tree = visual.update(a11y_tree::draw_and_read);
        assert_eq!(tree.read(tree.find("template-h-0-0")), ("Heading", "Plan"));
        assert_eq!(
            tree.read(tree.find("template-p-0-1")),
            ("Label", "Send the agenda.")
        );
        assert_eq!(
            tree.read(tree.find("template-run-meeting-notes")),
            ("Button", "Run Meeting notes again…")
        );
        assert_eq!(
            tree.read(tree.find("template-run-meeting-outline")),
            ("Button", "Run Meeting outline…")
        );
        assert!(tree.all("generate-provider").is_empty());
        click(&mut visual, "template-toggle-0", Role::Button);
        assert_eq!(
            control(&mut visual, "template-toggle-0").expanded(),
            Some(false)
        );
        assert!(
            visual
                .update(a11y_tree::draw_and_read)
                .all("template-result-0")
                .is_empty()
        );
        for _ in 0..8 {
            if control(&mut visual, "template-toggle-0").focused() == Some(true) {
                break;
            }
            visual.update(|window, cx| window.focus_next(cx));
            draw(&mut visual);
        }
        assert_eq!(
            control(&mut visual, "template-toggle-0").focused(),
            Some(true)
        );
        let keystroke = gpui::Keystroke::parse("enter").unwrap();
        visual.simulate_event(gpui::KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        visual.simulate_event(gpui::KeyUpEvent { keystroke });
        draw(&mut visual);
        assert_eq!(
            control(&mut visual, "template-toggle-0").expanded(),
            Some(true)
        );
        assert!(
            visual
                .update(a11y_tree::draw_and_read)
                .all("template-result-0")
                .len()
                == 1
        );
    }

    #[gpui::test]
    fn template_run_options_disclose_the_destination_and_keep_the_draft_bound_to_the_row(
        cx: &mut TestAppContext,
    ) {
        use crate::ui::{
            a11y_tree,
            kit::dialog_gate::{click, draw},
        };

        let dir = tempfile::tempdir().unwrap();
        let (mut visual, tab, _, meeting_id) = u44_window(cx, dir.path(), |meeting| {
            let mut stale = result(
                "meeting-notes",
                "Meeting notes",
                "# Plan\n\nPrepare the draft.",
            );
            stale.stale = true;
            meeting.template_results.push(stale);
        });
        let stale_only_once = |visual: &mut gpui::VisualTestContext| {
            let tree = visual.update(a11y_tree::draw_and_read);
            let markers: Vec<_> = tree
                .nodes
                .iter()
                .filter(|node| node.name.to_lowercase().contains("out of date"))
                .map(|node| node.name.as_str())
                .collect();
            assert_eq!(markers, ["Meeting notes, Out of date"]);
            assert_eq!(
                tree.read(tree.find("template-stale-0")),
                (
                    "Status",
                    "The transcript or its names changed after this result was written."
                )
            );
        };
        stale_only_once(&mut visual);
        click(&mut visual, "template-run-meeting-notes", Role::Button);
        assert_eq!(
            control(&mut visual, "generate-provider").focused(),
            Some(true)
        );
        assert_eq!(
            control(&mut visual, "generate-model").value(),
            Some("Provider default")
        );
        let tree = visual.update(a11y_tree::draw_and_read);
        let (role, destination) = tree.read(tree.find("generate-destination"));
        assert_eq!(role, "Label");
        assert!(destination.contains(
            "Sends the transcript text, your notes and instructions to Synthetic Agent."
        ));
        assert!(
            destination.ends_with("Speaker names, audio and voice data stay on this computer.")
        );
        stale_only_once(&mut visual);
        assert_eq!(
            control(&mut visual, "instructions-toggle").expanded(),
            Some(false)
        );
        click(&mut visual, "instructions-toggle", Role::Button);
        assert_eq!(
            control(&mut visual, "instructions-toggle").expanded(),
            Some(true)
        );
        let count = |visual: &mut gpui::VisualTestContext, role: &str, words: &str| {
            let tree = visual.update(a11y_tree::draw_and_read);
            assert_eq!(tree.read(tree.find("instructions-count")), (role, words));
        };
        count(&mut visual, "Label", "0 of 4,000 characters");
        for ix in 0..3 {
            click(
                &mut visual,
                &format!("instructions-example-{ix}"),
                Role::Button,
            );
        }
        let panel = tab.read_with(&visual, |tab, _| {
            tab.panel("meeting-notes").unwrap().clone()
        });
        let draft = "Keep it short\nFocus on decisions\nList open questions";
        assert_eq!(
            panel
                .read_with(&visual, |p, cx| p.instructions_override(cx))
                .as_deref(),
            Some(draft)
        );
        count(
            &mut visual,
            "Label",
            &format!("{} of 4,000 characters", draft.chars().count()),
        );
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.instructions_input().update(cx, |input, cx| {
                    input.set_value("é".repeat(3600), window, cx);
                });
            });
        });
        draw(&mut visual);
        count(&mut visual, "Status", "3600 of 4,000 characters");
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.instructions_input().update(cx, |input, cx| {
                    input.set_value("é".repeat(4001), window, cx);
                });
            });
        });
        draw(&mut visual);
        assert_eq!(control(&mut visual, "generate").disabled(), Some(true));
        visual.update(|window, cx| {
            panel.update(cx, |panel, cx| {
                panel.instructions_input().update(cx, |input, cx| {
                    input.set_value(draft, window, cx);
                });
            });
        });
        click(&mut visual, "instructions-toggle", Role::Button);
        assert_eq!(
            control(&mut visual, "instructions-toggle").label(),
            Some("Extra instructions · changed for this run")
        );
        visual.simulate_keystrokes("escape");
        draw(&mut visual);
        assert_eq!(
            control(&mut visual, "template-run-meeting-notes").focused(),
            Some(true)
        );
        click(&mut visual, "template-run-meeting-outline", Role::Button);
        assert_eq!(
            control(&mut visual, "instructions-toggle").label(),
            Some("Extra instructions")
        );
        click(&mut visual, "generation-cancel", Role::Button);
        assert!(!log(dir.path(), "args.log").contains("templates run"));
        click(&mut visual, "template-run-meeting-notes", Role::Button);
        click(&mut visual, "generate", Role::Button);
        let argv = log(dir.path(), "args.log");
        assert!(argv.contains(&format!("templates run {meeting_id} meeting-notes --queue --provider agent-a --instructions-stdin")), "{argv}");
        assert!(!argv.contains("Keep it short"));
        assert_eq!(log(dir.path(), "stdin.log").trim_end(), draft);
    }

    #[gpui::test]
    fn narrow_template_options_close_when_leaving_the_tab(cx: &mut TestAppContext) {
        use crate::ui::{
            a11y_tree,
            kit::dialog_gate::{click, draw},
        };

        let dir = tempfile::tempdir().unwrap();
        let (mut visual, tab, _, _) = u44_window(cx, dir.path(), |_| {});
        visual.simulate_resize(gpui::size(px(420.), px(720.)));
        draw(&mut visual);
        click(&mut visual, "template-run-meeting-notes", Role::Button);
        assert_eq!(
            control(&mut visual, "generate-provider").focused(),
            Some(true)
        );
        let tree = visual.update(a11y_tree::draw_and_read);
        let title = tree.find("generate-title");
        assert!(
            tree.nodes
                .iter()
                .enumerate()
                .any(|(ix, node)| node.role == "Dialog" && tree.subtree(ix).contains(&title))
        );
        tab.update(&mut visual, |tab, cx| tab.set_visible(false, cx));
        visual.run_until_parked();
        draw(&mut visual);
        assert!(
            visual
                .update(a11y_tree::draw_and_read)
                .all("generate-title")
                .is_empty()
        );
        assert!(!log(dir.path(), "args.log").contains("templates run"));
    }

    #[gpui::test]
    fn templates_link_to_settings_and_keep_deleted_results_available_without_a_provider(
        cx: &mut TestAppContext,
    ) {
        use crate::ui::{
            a11y_tree,
            kit::dialog_gate::{click, draw},
        };

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("providers.json"), r#"{"providers":[{"id":"unconsented","purpose":"llm","runtime":"agent-cli","enabled":true,"consented":false}]}"#).unwrap();
        let (mut visual, tab, app, _) = u44_window(cx, dir.path(), |meeting| {
            meeting
                .template_results
                .push(result("meeting-notes", "Meeting notes", "# Kept notes"));
            meeting.template_results.push(result(
                "synthetic-gone",
                "Invented recap",
                "# Kept recap",
            ));
        });
        let opened = Rc::new(std::cell::RefCell::new(Vec::new()));
        let _sub = tab.update(&mut visual, |_, cx| {
            let opened = opened.clone();
            cx.subscribe(&app, move |_, _, event, _| {
                if let AppEvent::OpenSettings(Some(page)) = event {
                    opened.borrow_mut().push(*page);
                }
            })
        });
        let tree = visual.update(a11y_tree::draw_and_read);
        assert_eq!(
            tree.read(tree.find("templates-intro")),
            (
                "Label",
                "Templates use an AI provider you turned on to write notes, an outline, a follow-up email and more from the transcript."
            )
        );
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|node| node.name
                    == "Templates are written by an AI provider. Turn one on to run them.")
                .count(),
            1
        );
        assert_eq!(tree.read(tree.find("templates-no-provider")).0, "Status");
        assert!(tree.all("template-run-meeting-notes").is_empty());
        assert!(tree.all("template-run-meeting-outline").is_empty());
        assert!(tree.all("generate-provider").is_empty());
        assert_eq!(
            tree.read(tree.find("template-result-0")),
            ("Article", "Meeting notes result")
        );
        let gone = tab.read_with(&visual, |tab, _| {
            tab.rows()
                .iter()
                .position(|row| row.id == "synthetic-gone")
                .unwrap()
        });
        let deleted_run = control(&mut visual, "template-run-synthetic-gone");
        assert_eq!(deleted_run.disabled(), Some(true));
        assert_eq!(
            deleted_run.description(),
            Some("The template was deleted, so it can’t run again.")
        );
        assert_eq!(
            tree.read(tree.find(&format!("template-deleted-{gone}"))),
            (
                "Label",
                "The template was deleted. Its result stays with this meeting."
            )
        );
        assert_eq!(
            tree.read(tree.find(&format!("template-copy-{gone}"))),
            ("Button", "Copy Invented recap")
        );
        assert_eq!(
            tree.read(tree.find(&format!("template-export-{gone}"))),
            ("Button", "Export Invented recap")
        );
        click(&mut visual, "templates-edit", Role::Link);
        click(&mut visual, "templates-set-up-provider", Role::Link);
        assert_eq!(
            *opened.borrow(),
            [SettingsPage::Templates, SettingsPage::AiProviders]
        );

        // Changing consent refreshes the tab catalog and restores Run for definitions.
        std::fs::remove_file(dir.path().join("providers.json")).unwrap();
        app.update(&mut visual, |app, cx| {
            let mut config = app.config().clone();
            config.providers.entry("agent-a".into()).or_default().model = Some("synthetic".into());
            app.set_config(config, cx);
        });
        draw(&mut visual);
        visual.run_until_parked();
        draw(&mut visual);
        let tree = visual.update(a11y_tree::draw_and_read);
        assert!(tree.all("templates-no-provider").is_empty());
        assert_eq!(
            tree.read(tree.find("template-run-meeting-notes")).0,
            "Button"
        );
        assert_eq!(
            control(&mut visual, "template-run-synthetic-gone").disabled(),
            Some(true)
        );
    }

    #[gpui::test]
    fn template_copy_and_export_report_on_the_originating_row(cx: &mut TestAppContext) {
        use crate::ui::{
            a11y_tree,
            kit::dialog_gate::{click, draw},
        };

        let dir = tempfile::tempdir().unwrap();
        let (mut visual, tab, _, meeting_id) = u44_window(cx, dir.path(), |meeting| {
            meeting
                .template_results
                .push(result("meeting-notes", "Meeting notes", "# Notes"));
            meeting.template_results.push(result(
                "meeting-outline",
                "Meeting outline",
                "# Outline",
            ));
        });
        let feedback = |visual: &mut gpui::VisualTestContext, id: &str, role: &str, words: &str| {
            let tree = visual.update(a11y_tree::draw_and_read);
            let status = tree.find(&format!("template-result-status-{id}"));
            assert!(
                tree.subtree(tree.find(&format!("template-row-{id}")))
                    .contains(&status)
            );
            assert_eq!(tree.read(status), (role, words));
        };
        click(&mut visual, "template-copy-1", Role::Button);
        feedback(&mut visual, "meeting-outline", "Status", "Copied.");
        click(&mut visual, "template-export-0", Role::Button);
        feedback(&mut visual, "meeting-notes", "Status", "Exporting…");
        assert_eq!(
            control(&mut visual, "template-export-0").disabled(),
            Some(true)
        );
        assert_ne!(
            control(&mut visual, "template-export-1").disabled(),
            Some(true)
        );
        feedback(&mut visual, "meeting-outline", "Status", "Copied.");
        let out = dir.path().join("Meeting notes.md");
        let chosen = out.clone();
        visual.simulate_new_path_selection(move |_| Some(chosen));
        visual.run_until_parked();
        draw(&mut visual);
        feedback(
            &mut visual,
            "meeting-notes",
            "Status",
            &format!("Exported {}", out.display()),
        );
        let argv = log(dir.path(), "args.log");
        assert!(argv.contains(&format!(
            "export {meeting_id} --include template:meeting-notes --format md --spec-stdin"
        )));
        assert!(!argv.contains("Meeting notes.md"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&log(dir.path(), "spec.json")).unwrap(),
            json!({ "out": out })
        );

        std::fs::write(dir.path().join("export-failure"), "").unwrap();
        click(&mut visual, "template-export-1", Role::Button);
        let chosen = dir.path().join("Outline.md");
        visual.simulate_new_path_selection(move |_| Some(chosen));
        visual.run_until_parked();
        draw(&mut visual);
        let tree = visual.update(a11y_tree::draw_and_read);
        let (role, failure) = tree.read(tree.find("template-result-status-meeting-outline"));
        assert_eq!(role, "Alert");
        assert!(
            failure.starts_with("Couldn't export.") && failure.contains("Choose another folder"),
            "{failure}"
        );
        feedback(
            &mut visual,
            "meeting-notes",
            "Status",
            &format!("Exported {}", out.display()),
        );
        click(&mut visual, "template-export-1", Role::Button);
        // A newer Copy on that row wins over its older export's async answer.
        tab.update(&mut visual, |tab, cx| tab.copy("meeting-outline", cx));
        visual.simulate_new_path_selection(|_| None);
        visual.run_until_parked();
        draw(&mut visual);
        feedback(&mut visual, "meeting-outline", "Status", "Copied.");
    }

    #[gpui::test]
    fn template_delete_dialog_cancel_returns_to_the_same_template(cx: &mut TestAppContext) {
        use crate::ui::kit::dialog_gate::{Expected, cancel_both_ways, click, draw};

        let dir = tempfile::tempdir().unwrap();
        let (state, _, _) = fixtures::state_with_cli(cx, dir.path(), synthetic_cli(dir.path()));
        let data_dir = state.read_with(cx, |state, _| state.paths().data_dir.clone());
        templates::save_user(
            &data_dir,
            "synthetic-recap",
            "Synthetic recap",
            "",
            "List it.",
        )
        .unwrap();
        cx.update(crate::ui::theme::init);
        let mut entity = None;
        let window = cx.add_window(|window, cx| {
            let view = cx.new(|cx| TemplatesSettings::new(state, cx));
            entity = Some(view.clone());
            gpui::component::Root::new(view, window, cx)
        });
        let view = entity.unwrap();
        cx.run_until_parked();
        let vcx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        draw(vcx);
        let index = view.read_with(vcx, |view, _| {
            view.templates()
                .unwrap()
                .as_ref()
                .unwrap()
                .iter()
                .position(|template| template.id == "synthetic-recap")
                .unwrap()
        });
        let opener_id = format!("template-more-{index}");
        let open = |vcx: &mut gpui::VisualTestContext| {
            click(vcx, &opener_id, Role::Button);
            let point = vcx.update(|window, _| {
                gpui::base::test_support::snapshots(window)
                    .into_iter()
                    .find(|node| {
                        node.role() == Some(Role::MenuItem) && node.label() == Some("Delete")
                    })
                    .expect("Delete in More menu")
                    .bounds()
                    .center()
            });
            vcx.simulate_click(point, Default::default());
            vcx.run_until_parked();
            draw(vcx);
        };
        cancel_both_ways(
            vcx,
            &opener_id,
            Expected {
                title: "Delete “Synthetic recap”?",
                cancel_id: "template-delete-cancel",
                action_id: "template-delete-confirm",
                action: "Delete template",
                lines: &[
                    (
                        "template-delete-goes",
                        "The template, from every meeting's Templates tab",
                    ),
                    (
                        "template-delete-stays",
                        "Results it already produced, which you can still read, copy and export",
                    ),
                ],
                headings: &["What goes", "What stays"],
            },
            open,
        );
        open(vcx);
        click(vcx, "template-delete-confirm", Role::Button);
        assert!(log(dir.path(), "args.log").contains(" templates delete synthetic-recap\n"));
    }

    #[gpui::test]
    fn resetting_an_edited_template_confirms_the_original_returns_and_results_stay(
        cx: &mut TestAppContext,
    ) {
        use crate::ui::kit::dialog_gate::{click, draw};
        let dir = tempfile::tempdir().unwrap();
        let (state, _, _) = fixtures::state_with_cli(cx, dir.path(), synthetic_cli(dir.path()));
        let data_dir = state.read_with(cx, |state, _| state.paths().data_dir.clone());
        templates::save_user(&data_dir, "bant", "Acorn recap", "", "List the risks.").unwrap();
        cx.update(crate::ui::theme::init);
        let mut entity = None;
        let window = cx.add_window(|window, cx| {
            let view = cx.new(|cx| TemplatesSettings::new(state, cx));
            entity = Some(view.clone());
            gpui::component::Root::new(view, window, cx)
        });
        let view = entity.unwrap();
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let open = |visual: &mut gpui::VisualTestContext| {
            visual.update(|window, cx| {
                view.update(cx, |this, cx| {
                    this.confirm_template_change("bant", true, cx.focus_handle(), window, cx);
                })
            })
        };
        open(&mut visual);
        draw(&mut visual);
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert!(
                tree.nodes
                    .iter()
                    .any(|node| node.name == "Reset “Acorn recap” to the built-in text?")
            );
            tree.find("template-reset-changes");
            assert!(
                tree.read(tree.find("template-delete-stays"))
                    .1
                    .contains("Results it already produced")
            );
        });
        visual.simulate_keystrokes("escape");
        visual.run_until_parked();
        assert!(!log(dir.path(), "args.log").contains("templates reset bant"));
        open(&mut visual);
        click(&mut visual, "template-delete-confirm", Role::Button);
        assert!(log(dir.path(), "args.log").contains("templates reset bant"));
    }

    #[gpui::test]
    fn settings_editors_write_through_the_cli_with_stdin(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (app, _lib, _s) = fixtures::state_with_cli(cx, dir.path(), synthetic_cli(dir.path()));
        let data_dir = app.read_with(cx, |st, _| st.paths().data_dir.clone());
        templates::save_user(
            &data_dir,
            "synthetic-retro",
            "Synthetic retro",
            "",
            "List it.",
        )
        .unwrap();
        templates::save_user(&data_dir, "bant", "BANT, mine", "", "My own BANT.").unwrap();
        let window = cx.add_window({
            let app = app.clone();
            move |_, cx| TemplatesSettings::new(app, cx)
        });
        cx.run_until_parked();
        window
            .update(cx, |t, window, cx| {
                let list = t.templates().unwrap().as_ref().unwrap();
                let bant = list.iter().find(|x| x.id == "bant").unwrap();
                assert!(bant.builtin && bant.overridden);
                // New: invalid input is refused before anything runs.
                t.new_template(window, cx);
                t.save(cx);
                assert!(t.editor_error().is_some());
                let editor = t.editor.as_ref().unwrap();
                let (id, name, body) =
                    (editor.id.clone(), editor.name.clone(), editor.body.clone());
                id.update(cx, |i, cx| i.set_value("synthetic-new", window, cx));
                name.update(cx, |i, cx| i.set_value("Synthetic new", window, cx));
                body.update(cx, |i, cx| {
                    i.set_value("Summarise the synthetic risks.", window, cx)
                });
                t.save(cx);
            })
            .unwrap();
        cx.run_until_parked();
        let args_log = log(dir.path(), "args.log");
        assert!(
            args_log.contains(" templates new synthetic-new --stdin\n"),
            "{args_log}"
        );
        assert!(
            !args_log.contains("synthetic risks"),
            "instructions leaked into argv"
        );
        let stdin: serde_json::Value =
            serde_json::from_str(log(dir.path(), "stdin.log").lines().next().unwrap()).unwrap();
        assert_eq!(
            stdin,
            json!({"name": "Synthetic new", "description": "", "body": "Summarise the synthetic risks."})
        );
        window
            .update(cx, |t, window, cx| {
                assert!(t.editing().is_none());
                // Edit keeps the ID and sends `edit`.
                t.edit("synthetic-retro", window, cx);
                assert_eq!(t.editing(), Some(Some("synthetic-retro")));
                t.save(cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert!(log(dir.path(), "args.log").contains(" templates edit synthetic-retro --stdin\n"));
        window
            .update(cx, |t, window, cx| {
                // Duplicate opens a new one with a free ID.
                t.duplicate("synthetic-retro", window, cx);
                assert_eq!(t.editing(), Some(None));
                let id = t.editor.as_ref().unwrap().id.read(cx).value().to_string();
                assert_eq!(id, "synthetic-retro-copy");
                t.cancel_edit(cx);
                // Exercise the accepted write after the dialog-specific gate.
                t.delete("synthetic-retro", cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |t, _, cx| {
                t.reset("bant", cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |t, _, cx| t.set_auto("bant", true, cx))
            .unwrap();
        cx.run_until_parked();
        let commands: Vec<String> = log(dir.path(), "args.log")
            .lines()
            .filter_map(|l| l.split(" --json ").nth(1).map(str::to_owned))
            .collect();
        assert_eq!(
            commands
                .iter()
                .filter(|c| *c == "templates delete synthetic-retro")
                .count(),
            1
        );
        assert!(commands.contains(&"templates reset bant".to_owned()));
        assert!(commands.contains(&"templates auto add bant".to_owned()));
    }

    /// SET-05: every text of Settings → Templates is its own named node with a
    /// stable ID (`template-<ix>-name`); the wait for the list is a status,
    /// failures (list, save, conflict) are alerts, and no node nests another
    /// with the same text.
    #[gpui::test]
    fn template_editor_dialog_generates_the_id_and_counts_unicode_instructions(
        cx: &mut TestAppContext,
    ) {
        use crate::ui::kit::dialog_gate::draw;
        let dir = tempfile::tempdir().unwrap();
        let (state, _, _) = fixtures::state_with_cli(cx, dir.path(), synthetic_cli(dir.path()));
        cx.update(crate::ui::theme::init);
        let mut view = None;
        let window = cx.add_window(|window, cx| {
            let settings = cx.new(|cx| TemplatesSettings::new(state, cx));
            view = Some(settings.clone());
            gpui::component::Root::new(settings, window, cx)
        });
        let view = view.unwrap();
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.update(|window, cx| {
            view.update(cx, |this, cx| {
                this.new_template(window, cx);
                this.mount_editor(cx.focus_handle(), window, cx);
            })
        });
        draw(&mut visual);
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert!(tree.nodes.iter().any(|node| node.role == "Dialog"));
            for id in [
                "template-name-label",
                "template-description-label",
                "template-instructions-label",
            ] {
                tree.find(id);
            }
            assert!(tree.nodes.iter().all(|node| !node.name.starts_with("ID:")));
            view.update(cx, |this, cx| {
                let editor = this.editor.as_mut().unwrap();
                assert!(editor.name.focus_handle(cx).is_focused(window));
                editor
                    .name
                    .update(cx, |input, cx| input.set_value("Réunion Notes", window, cx));
                editor
                    .body
                    .update(cx, |input, cx| input.set_value("Aç", window, cx));
                editor.advanced = true;
            });
        });
        draw(&mut visual);
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("template-id-label")).1,
                "ID: reunion-notes"
            );
            assert_eq!(
                tree.read(tree.find("template-instructions-count")).1,
                "2 / 8,000"
            );
        });
        view.update(&mut visual, |this, cx| this.save(cx));
        visual.run_until_parked();
        assert!(log(dir.path(), "args.log").contains("templates new reunion-notes --stdin"));
        assert!(view.read_with(&visual, |this, _| this.editor.is_none()));
    }

    #[gpui::test]
    fn template_automatic_checkbox_explains_missing_summary_prerequisites(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (state, _, _) = fixtures::state_with_cli(cx, dir.path(), synthetic_cli(dir.path()));
        let window = cx.add_window({
            let state = state.clone();
            move |_, cx| TemplatesSettings::new(state, cx)
        });
        state.update(cx, |state, cx| {
            let mut config = state.config().clone();
            config.auto_enrich = broscribe_core::config::AutoEnrich::Off;
            config.templates.auto.push("bant".into());
            state.set_config(config, cx);
        });
        cx.run_until_parked();
        let read = |cx: &mut TestAppContext, reason: &str| {
            cx.update_window(window.into(), |_, window, cx| {
                let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
                assert!(
                    tree.read(tree.find("settings-templates-auto-note"))
                        .1
                        .ends_with(reason)
                );
                assert!(tree.nodes.iter().any(|node| node.role == "CheckBox"
                    && node.name.ends_with(" after automatic summaries")));
            })
            .unwrap();
        };
        read(cx, "Automatic summaries are off.");
        state.update(cx, |state, cx| {
            let mut config = state.config().clone();
            config.auto_enrich = broscribe_core::config::AutoEnrich::New;
            state.set_config(config, cx);
        });
        read(cx, "No AI provider is turned on for automatic summaries.");
    }

    #[gpui::test]
    fn settings_template_texts_are_named_nodes(cx: &mut TestAppContext) {
        use crate::ui::spoken_log::{self, assert_distinct, node};

        let dir = tempfile::tempdir().unwrap();
        let (app, _lib, _s) = fixtures::state_with_cli(cx, dir.path(), synthetic_cli(dir.path()));
        let data_dir = app.read_with(cx, |st, _| st.paths().data_dir.clone());
        templates::save_user(
            &data_dir,
            "synthetic-retro",
            "Synthetic retro",
            "Invented retro notes.",
            "List it.",
        )
        .unwrap();
        let window = cx.add_window({
            let app = app.clone();
            move |_, cx| TemplatesSettings::new(app, cx)
        });
        cx.run_until_parked();
        let named = |cx: &mut TestAppContext, change: &dyn Fn(&mut TemplatesSettings)| {
            window
                .update(cx, |t, window, cx| {
                    change(t);
                    spoken_log::while_building(|| Render::render(t, window, cx)).1
                })
                .unwrap()
        };

        let loaded = named(cx, &|_| {});
        assert_distinct(&loaded, &["template-*-kind"]);
        assert_eq!(node(&loaded, "settings-templates-note").0, Role::Label);
        assert!(
            !loaded
                .iter()
                .any(|(id, _, _)| id == "settings-templates-auto-note")
        );
        let (name_id, ..) = loaded
            .iter()
            .find(|(id, role, name)| {
                id.starts_with("template-")
                    && id.ends_with("-name")
                    && *role == Role::Label
                    && name == "Synthetic retro"
            })
            .unwrap_or_else(|| panic!("no name line for the user template: {loaded:?}"));
        let row = name_id.trim_end_matches("-name");
        assert_eq!(
            node(&loaded, &format!("{row}-kind")),
            (Role::Label, "Yours")
        );
        assert_eq!(
            node(&loaded, &format!("{row}-description")),
            (Role::Label, "Invented retro notes.")
        );

        let done = named(cx, &|t| {
            t.notice = Some(("Synthetic retro runs only on request.".into(), false))
        });
        assert_eq!(
            node(&done, "templates-settings-notice"),
            (Role::Status, "Synthetic retro runs only on request.")
        );
        let failed = named(cx, &|t| t.notice = Some(("synthetic failure".into(), true)));
        assert_eq!(
            node(&failed, "templates-settings-notice"),
            (Role::Alert, "synthetic failure")
        );

        window
            .update(cx, |t, window, cx| {
                t.notice = None;
                t.edit("synthetic-retro", window, cx);
            })
            .unwrap();
        let editing = named(cx, &|_| {});
        assert_distinct(&editing, &["template-*-kind"]);
        assert_eq!(node(&editing, "template-name-label"), (Role::Label, "Name"));
        assert_eq!(node(&editing, "template-editor-note").0, Role::Label);
        let refused = named(cx, &|t| {
            t.editor.as_mut().unwrap().error = Some("Give the template a name.".into())
        });
        assert_eq!(
            node(&refused, "template-editor-error"),
            (Role::Alert, "Give the template a name.")
        );
        let conflicted = named(cx, &|t| {
            let editor = t.editor.as_mut().unwrap();
            editor.error = None;
            editor.conflict = Some(Conflict::Deleted);
        });
        let (role, text) = node(&conflicted, "template-editor-conflict");
        assert_eq!(role, Role::Alert);
        assert!(
            text.starts_with("Not saved: this template was deleted"),
            "{text}"
        );
        assert_eq!(
            conflicted
                .iter()
                .filter(|(_, _, name)| name == text)
                .count(),
            1,
            "the conflict's words are one node: {conflicted:?}"
        );

        // Waits and read failures of the list.
        let waiting = named(cx, &|t| {
            t.editor = None;
            t.templates = None;
        });
        assert_eq!(
            node(&waiting, "templates-settings-loading"),
            (Role::Status, "Reading the templates…")
        );
        let unreadable = named(cx, &|t| {
            t.templates = Some(Err("synthetic read failure".into()))
        });
        assert_eq!(
            node(&unreadable, "templates-settings-error"),
            (Role::Alert, "synthetic read failure")
        );
    }

    /// The body typed into the open editor.
    fn type_body(t: &TemplatesSettings, text: &str, window: &mut Window, cx: &mut gpui::App) {
        let body = t.editor.as_ref().unwrap().body.clone();
        body.update(cx, |i, cx| i.set_value(text.to_owned(), window, cx));
    }

    fn editor_body(t: &TemplatesSettings, cx: &gpui::App) -> String {
        t.editor.as_ref().unwrap().body.read(cx).value().to_string()
    }

    #[gpui::test]
    fn an_outside_change_is_a_conflict_that_keeps_the_draft(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (app, _lib, _s) = fixtures::state_with_cli(cx, dir.path(), synthetic_cli(dir.path()));
        let data_dir = app.read_with(cx, |st, _| st.paths().data_dir.clone());
        templates::save_user(
            &data_dir,
            "synthetic-retro",
            "Synthetic retro",
            "",
            "List it.",
        )
        .unwrap();
        let window = cx.add_window({
            let app = app.clone();
            move |_, cx| TemplatesSettings::new(app, cx)
        });
        cx.run_until_parked();
        window
            .update(cx, |t, window, cx| {
                t.edit("synthetic-retro", window, cx);
                type_body(t, "My synthetic draft.", window, cx);
            })
            .unwrap();
        // Another surface (the CLI) edits the template meanwhile; the CLI's
        // baseline check refuses the stale save.
        templates::save_user(
            &data_dir,
            "synthetic-retro",
            "Synthetic retro",
            "",
            "Their synthetic text.",
        )
        .unwrap();
        std::fs::write(dir.path().join("template-changed"), "").unwrap();
        window.update(cx, |t, _, cx| t.save(cx)).unwrap();
        cx.run_until_parked();
        // The edit carried the definition the editor opened on.
        let sent: serde_json::Value =
            serde_json::from_str(log(dir.path(), "stdin.log").lines().next().unwrap()).unwrap();
        assert_eq!(sent["body"], "My synthetic draft.");
        assert_eq!(sent["baseline"]["body"], "List it.");
        assert_eq!(sent["baseline"]["id"], "synthetic-retro");
        std::fs::remove_file(dir.path().join("template-changed")).unwrap();
        window
            .update(cx, |t, window, cx| {
                match t.editor_conflict() {
                    Some(Conflict::Changed(current)) => {
                        assert_eq!(current.body, "Their synthetic text.")
                    }
                    other => panic!("expected a conflict, got {other:?}"),
                }
                assert_eq!(editor_body(t, cx), "My synthetic draft.");
                // Taking theirs replaces the draft with the version on disk.
                t.take_theirs(window, cx);
                assert!(t.editor_conflict().is_none());
                assert_eq!(editor_body(t, cx), "Their synthetic text.");
                type_body(t, "My synthetic draft, again.", window, cx);
                t.save(cx);
            })
            .unwrap();
        cx.run_until_parked();
        // Against the version it now knows, the save goes through.
        assert!(log(dir.path(), "args.log").contains(" templates edit synthetic-retro --stdin\n"));
        let resent: serde_json::Value =
            serde_json::from_str(log(dir.path(), "stdin.log").lines().nth(1).unwrap()).unwrap();
        assert_eq!(resent["body"], "My synthetic draft, again.");
        assert_eq!(resent["baseline"]["body"], "Their synthetic text.");
        window
            .update(cx, |t, _, _| assert!(t.editing().is_none()))
            .unwrap();
    }

    #[gpui::test]
    fn a_late_save_never_closes_or_fills_a_newer_editor(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (app, _lib, _s) = fixtures::state_with_cli(cx, dir.path(), synthetic_cli(dir.path()));
        let data_dir = app.read_with(cx, |st, _| st.paths().data_dir.clone());
        templates::save_user(
            &data_dir,
            "synthetic-retro",
            "Synthetic retro",
            "",
            "List it.",
        )
        .unwrap();
        let window = cx.add_window({
            let app = app.clone();
            move |_, cx| TemplatesSettings::new(app, cx)
        });
        cx.run_until_parked();
        window
            .update(cx, |t, window, cx| {
                t.edit("synthetic-retro", window, cx);
                t.save(cx);
                // Cancelled while the save runs; a new draft is started.
                t.cancel_edit(cx);
                t.new_template(window, cx);
                type_body(t, "A newer synthetic draft.", window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |t, _, cx| {
                assert_eq!(t.editing(), Some(None), "the newer editor stays open");
                assert_eq!(editor_body(t, cx), "A newer synthetic draft.");
                assert!(t.editor_error().is_none() && t.editor_conflict().is_none());
                let (text, error) = t.notice().unwrap();
                assert!(!error && text.starts_with("Saved "), "{text}");
            })
            .unwrap();
    }

    /// Text typed into the editor while its own save runs stays: the editor
    /// closes only while its fields still hold what was sent, and the kept
    /// draft is from then on an edit of the version that save wrote.
    #[gpui::test]
    fn text_typed_during_its_save_stays_against_the_saved_version(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (app, _lib, _s) = fixtures::state_with_cli(cx, dir.path(), synthetic_cli(dir.path()));
        let data_dir = app.read_with(cx, |st, _| st.paths().data_dir.clone());
        templates::save_user(
            &data_dir,
            "synthetic-retro",
            "Synthetic retro",
            "",
            "List it.",
        )
        .unwrap();
        // The CLI's answer to the first save: the definition it wrote.
        let written = json!({"template": {
            "id": "synthetic-retro", "name": "Synthetic retro", "description": "",
            "body": "First synthetic draft.", "builtin": false, "overridden": false, "auto": false
        }});
        std::fs::write(dir.path().join("edit-reply.json"), written.to_string()).unwrap();
        let window = cx.add_window({
            let app = app.clone();
            move |_, cx| TemplatesSettings::new(app, cx)
        });
        cx.run_until_parked();
        window
            .update(cx, |t, window, cx| {
                t.edit("synthetic-retro", window, cx);
                type_body(t, "First synthetic draft.", window, cx);
                t.save(cx);
                // Typed on before the save answers.
                type_body(t, "Second synthetic draft.", window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |t, _, cx| {
                assert_eq!(
                    t.editing(),
                    Some(Some("synthetic-retro")),
                    "the draft stays"
                );
                assert_eq!(editor_body(t, cx), "Second synthetic draft.");
                assert!(t.editor_error().is_none() && t.editor_conflict().is_none());
                let (text, error) = t.notice().unwrap();
                assert!(!error && text.starts_with("Saved "), "{text}");
                t.save(cx);
            })
            .unwrap();
        cx.run_until_parked();
        let sent: Vec<serde_json::Value> = log(dir.path(), "stdin.log")
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0]["body"], "First synthetic draft.");
        assert_eq!(sent[0]["baseline"]["body"], "List it.");
        assert_eq!(sent[1]["body"], "Second synthetic draft.");
        assert_eq!(sent[1]["baseline"]["body"], "First synthetic draft.");
        window
            .update(cx, |t, _, _| {
                assert!(t.editing().is_none(), "unchanged since its own save");
            })
            .unwrap();
    }
}
