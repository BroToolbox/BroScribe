//! The library view (design §4): meetings grouped by period or match in a
//! virtualized list, a search box with person / tag / app / source / date filters,
//! import progress rows and, for an empty library, a drop zone.
//!
//! Searches go through [`AppState::search_meetings`] ([`pages`]: index
//! keyset pages on the background executor, superseded reads cancelled);
//! typing is debounced. The list grows as the reader approaches the end
//! or presses Down on the last row. A refresh after library changes
//! reads as far as listed and keeps the selected meeting and the typed
//! filters. The app list for the app filter is read from the index on the
//! background executor and reloaded when meetings change. Rows are sized in
//! rems, so they follow the reading size. Dropping files anywhere on the
//! window is handled by the shell, which routes to
//! the import dialog.
//!
//! Mounting: create once with [`LibraryView::new`] and render it for
//! [`crate::state::Route::Library`]; subscribe to [`LibraryEvent`] to open a
//! meeting (`LibraryEvent::SelectMeeting`).

pub mod pages;

use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use crate::ui::format::{
    count, date_heading, format_clock, length, local_date_time, local_time, spoken_date_time,
    spoken_length, spoken_position,
};
use crate::ui::select::{OptionItem, Picker};
use crate::ui::type_scale::{TextRole, TextRoleExt as _, tabular};
use broscribe_core::index::{Index, MeetingHit, MeetingOrder, MetadataMatch, SearchQuery};
use broscribe_core::schema::{ProcessingStatus, Source};
use broscribe_core::{Error, MeetingId, Result};
use chrono::{DateTime, Datelike as _, Days, FixedOffset, Local, NaiveDate, TimeZone as _};
use gpui::component::button::{Button, ButtonVariants as _, DropdownButton};
use gpui::component::date_picker::{DatePicker, DatePickerEvent, DatePickerState, DateRangePreset};
use gpui::component::input::{Input, InputEvent, InputState};
use gpui::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, VirtualListScrollHandle, v_virtual_list,
};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Div, Entity, EventEmitter, FocusHandle, Focusable,
    FontWeight, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _,
    PathPromptOptions, Pixels, Point, Render, Role, ScrollStrategy, SharedString, Size, Stateful,
    StatefulInteractiveElement as _, Styled as _, StyledText, Subscription, Task, Window, actions,
    anchored, deferred, div, point, px, rems, size,
};

use crate::jobs::{ChildKind, failure_text, sentence};
use crate::state::{AppState, Route};
use crate::ui::{kit, text_line, title};
use crate::views::import_sheet::{ImportRunStatus, ImportRuns, file_label, pick_import_paths};
use crate::views::library::pages::{Load, SearchError, TranscriptError};

actions!(
    library,
    [
        /// Opens a files-only picker to import audio or video recordings.
        ImportRecordings,
        /// Opens a folder picker to import recordings from one or more folders.
        ImportFolder,
    ]
);

/// `PathPromptOptions` for choosing recording files ("Import recordings…").
pub fn import_recordings_options() -> PathPromptOptions {
    PathPromptOptions {
        files: true,
        directories: false,
        multiple: true,
        prompt: Some("Import".into()),
    }
}

/// `PathPromptOptions` for choosing folders ("Import folder…").
pub fn import_folder_options() -> PathPromptOptions {
    PathPromptOptions {
        files: false,
        directories: true,
        multiple: true,
        prompt: Some("Import".into()),
    }
}

/// `PathPromptOptions` for the main "Import…" button: mixed files and folders
/// when the platform supports it (macOS), files-only otherwise (Windows, Linux).
pub fn import_main_options(can_select_mixed: bool) -> PathPromptOptions {
    PathPromptOptions {
        files: true,
        directories: can_select_mixed,
        multiple: true,
        prompt: Some("Import".into()),
    }
}

/// The Attention line beside meetings listed without (all of) their
/// transcript matches (LIB-61), in the pattern "Couldn't <do X>. <Cause>.
/// <Next step>." (WRT-04): `cause` is [`crate::jobs::CliError::user_text`],
/// never a code; a blank one reads "The search tool didn't answer.".
/// `retry_more`: it failed during Load more, so the meetings read before
/// stay listed; otherwise only title, people and tag matches are.
pub fn transcript_error_message(cause: &str, retry_more: bool) -> String {
    let mut cause = sentence(cause);
    if cause.is_empty() {
        cause = "The search tool didn't answer.".to_owned();
    }
    if retry_more {
        failure_text("Couldn't load more transcript matches.", &cause, "")
    } else {
        failure_text(
            "Couldn't search transcripts.",
            &cause,
            "Showing matches in titles, people and tags.",
        )
    }
}

/// What the list says when nothing is listed for the filters. While the
/// transcripts couldn't be searched, only the title, people and tag
/// matches are known to be none.
pub fn no_match_message(transcripts_unknown: bool) -> &'static str {
    if transcripts_unknown {
        "No titles, people or tags match."
    } else {
        "No meetings match these filters."
    }
}

/// Delay between the last keystroke and the search.
pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(250);
const HEADER_REMS: f32 = 2.;
const ROW_REMS: f32 = 3.5;
const MATCH_REMS: f32 = 4.75;
const INITIAL_LOAD_DELAY: Duration = Duration::from_millis(300);
const PAGE_THRESHOLD: usize = 20;

/// Emitted for the shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LibraryEvent {
    SelectMeeting {
        id: MeetingId,
        enter: bool,
        preview: bool,
        segment: Option<u32>,
        action: Option<RowCommand>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowCommand {
    Rename,
    AddTag,
    Export,
    Delete,
}

/// One row of the virtualized list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LibraryRow {
    /// A day header ("Today", "Yesterday", "Friday, 13 November 2026").
    Day(SharedString),
    /// Index into the search results.
    Meeting(usize),
    Now,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NowRow {
    session: String,
    title: String,
    app: Option<String>,
    phase: &'static str,
    stop_reason: Option<&'static str>,
}

impl NowRow {
    fn from_panel(panel: &crate::views::panel::PanelModel) -> Option<Self> {
        use broscribe_ipc::{
            RecordingStatus,
            recording_words::{interrupted_stop, meeting_title, stop_reason_text},
        };
        if let Some(active) = &panel.recording {
            return Some(Self {
                session: active.session_id.clone(),
                title: meeting_title(active.title.as_deref()).to_owned(),
                app: active.app.clone(),
                phase: match active.status {
                    RecordingStatus::Recording => "Recording",
                    RecordingStatus::Paused => "Paused",
                },
                stop_reason: None,
            });
        }
        panel.stopped.iter().rev().find_map(|stopped| {
            let reason = interrupted_stop(stopped.stop_reason.as_deref())?;
            (stopped.state.is_pending_save() && reason != "start_failed").then(|| Self {
                session: stopped.session_id.clone(),
                title: meeting_title(stopped.title.as_deref()).to_owned(),
                app: None,
                phase: "Stopped early",
                stop_reason: Some(stop_reason_text(reason)),
            })
        })
    }
}

struct OpenRowMenu {
    menu: Entity<PopupMenu>,
    position: Point<Pixels>,
    _dismiss: Subscription,
}

/// The filters next to the search box.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LibraryFilters {
    pub text: String,
    pub person: String,
    pub tag: String,
    pub app: Option<String>,
    pub source: Option<Source>,
    pub date: Option<DateFilter>,
    pub order: MeetingOrder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateFilter {
    Today,
    Week,
    Month,
    Year,
    Custom(NaiveDate, NaiveDate),
}

impl DateFilter {
    const PRESETS: [(Self, &str, &str); 4] = [
        (Self::Today, "today", "Today"),
        (Self::Week, "week", "Last 7 days"),
        (Self::Month, "month", "Last 30 days"),
        (Self::Year, "year", "This year"),
    ];

    fn dates(self, today: NaiveDate) -> (NaiveDate, NaiveDate) {
        let start = match self {
            Self::Today => today,
            Self::Week => today - Days::new(6),
            Self::Month => today - Days::new(29),
            Self::Year => NaiveDate::from_ymd_opt(today.year(), 1, 1).unwrap(),
            Self::Custom(start, end) => return (start, end),
        };
        (start, today)
    }

    fn display(self) -> String {
        match self {
            Self::Custom(start, end) => format!(
                "{} – {}",
                start.format("%-d %b %Y"),
                end.format("%-d %b %Y")
            ),
            _ => Self::PRESETS
                .iter()
                .find(|(value, _, _)| *value == self)
                .unwrap()
                .2
                .to_owned(),
        }
    }

    fn bounds(
        self,
        today: NaiveDate,
    ) -> (Option<DateTime<FixedOffset>>, Option<DateTime<FixedOffset>>) {
        // Local midnight can be skipped by a time-zone transition. Use the
        // first representable minute on that date (or the following one).
        fn start_of_day(day: NaiveDate) -> Option<DateTime<FixedOffset>> {
            let midnight = day.and_hms_opt(0, 0, 0)?;
            (0..=1440).find_map(|minute| {
                let at = midnight.checked_add_signed(chrono::TimeDelta::minutes(minute))?;
                Local
                    .from_local_datetime(&at)
                    .earliest()
                    .map(|at| at.fixed_offset())
            })
        }
        let (start, end) = self.dates(today);
        (start_of_day(start), end.succ_opt().and_then(start_of_day))
    }
}

impl LibraryFilters {
    /// The index query for these filters: blank fields are no filter.
    pub fn query(&self) -> SearchQuery {
        let (since, until) = self
            .date
            .map(|date| date.bounds(Local::now().date_naive()))
            .unwrap_or_default();
        SearchQuery {
            text: non_blank(&self.text),
            person: non_blank(&self.person),
            tag: non_blank(&self.tag),
            app: self.app.as_deref().and_then(non_blank),
            source: self.source,
            meeting_order: self.order,
            since,
            until,
            ..SearchQuery::default()
        }
    }

    /// Whether any filter or text is set (an empty result then means "no
    /// match" rather than "empty library").
    pub fn is_filtering(&self) -> bool {
        let q = self.query();
        q.text.is_some()
            || q.person.is_some()
            || q.tag.is_some()
            || q.app.is_some()
            || q.source.is_some()
            || self.date.is_some()
    }
}

fn non_blank(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_owned())
}

/// Groups chronologically ordered hits by local calendar period.
pub fn group_by_day(hits: &[MeetingHit], today: NaiveDate) -> Vec<LibraryRow> {
    let mut rows = Vec::with_capacity(hits.len() + hits.len().min(64));
    let mut current: Option<String> = None;
    for (i, hit) in hits.iter().enumerate() {
        let day = hit.started_at.with_timezone(&Local).date_naive();
        let age = (today - day).num_days();
        let heading = match age {
            0..=1 => date_heading(day, today),
            2..=7 => "Previous 7 days".into(),
            8..=30 => "Previous 30 days".into(),
            _ if day.year() == today.year() => day.format("%B").to_string(),
            _ => day.format("%B %Y").to_string(),
        };
        if current.as_deref() != Some(&heading) {
            rows.push(LibraryRow::Day(heading.clone().into()));
            current = Some(heading);
        }
        rows.push(LibraryRow::Meeting(i));
    }
    rows
}

/// A snippet without the index's `**` match markers.
pub fn plain_snippet(snippet: &str) -> String {
    snippet.replace("**", "")
}

fn highlighted_snippet(prefix: &str, snippet: &str, cx: &App) -> StyledText {
    let hit = crate::ui::rich_text::find_hit_style(cx);
    let mut text = prefix.to_owned();
    let mut highlights = Vec::new();
    let mut remainder = snippet;
    while let Some((before, after)) = remainder.split_once("**") {
        text.push_str(before);
        let Some((matched, rest)) = after.split_once("**") else {
            text.push_str("**");
            text.push_str(after);
            remainder = "";
            break;
        };
        let start = text.len();
        text.push_str(matched);
        highlights.push((start..text.len(), hit));
        remainder = rest;
    }
    text.push_str(remainder);
    StyledText::new(text).with_highlights(highlights)
}

fn project_rows(
    search: &pages::MeetingSearch,
    now: Option<&NowRow>,
    day: NaiveDate,
) -> Vec<LibraryRow> {
    let results = search.results();
    let mut rows = if search.query().text.is_some() {
        let mut rows = Vec::new();
        let metadata = search.metadata_count();
        for (label, range) in [
            ("Title, people or tags", 0..metadata),
            ("In transcripts", metadata..results.len()),
        ] {
            if !range.is_empty() {
                rows.push(LibraryRow::Day(label.into()));
                rows.extend(range.map(LibraryRow::Meeting));
            }
        }
        rows
    } else {
        group_by_day(results, day)
    };
    if let Some(now) = now {
        rows.retain(|row| match row {
            LibraryRow::Meeting(i) => {
                search.recording_session(&results[*i].meeting_id) != Some(&now.session)
            }
            _ => true,
        });
        // A draft replaced by the Now row may have been its section's only row.
        for i in (0..rows.len()).rev() {
            if matches!(rows[i], LibraryRow::Day(_))
                && !matches!(rows.get(i + 1), Some(LibraryRow::Meeting(_)))
            {
                rows.remove(i);
            }
        }
        rows.splice(0..0, [LibraryRow::Day("Now".into()), LibraryRow::Now]);
    }
    rows
}

/// Next selectable (meeting) row from `from` in direction `down`; `None`
/// when there is none.
pub fn next_meeting_row(rows: &[LibraryRow], from: Option<usize>, down: bool) -> Option<usize> {
    let is_meeting =
        |i: &usize| matches!(rows.get(*i), Some(LibraryRow::Meeting(_) | LibraryRow::Now));
    match (from, down) {
        (None, true) => (0..rows.len()).find(is_meeting),
        (None, false) => (0..rows.len()).rev().find(is_meeting),
        (Some(i), true) => (i + 1..rows.len()).find(is_meeting),
        (Some(i), false) => (0..i).rev().find(is_meeting),
    }
}

/// The row showing meeting `id`, if listed.
pub fn meeting_row(rows: &[LibraryRow], hits: &[MeetingHit], id: &MeetingId) -> Option<usize> {
    rows.iter().position(|row| {
        matches!(row, LibraryRow::Meeting(i) if hits.get(*i).is_some_and(|h| h.meeting_id == *id))
    })
}

/// Search failure wording shared by the alert and its tests.
pub fn search_failure(cause: &str) -> String {
    failure_text("Couldn't search the library.", cause, "")
}

/// The status line above the list: how many meetings are listed, whether
/// more match, and what is being read.
pub fn summary(count: usize, loading: Option<Load>, more: bool) -> String {
    if loading == Some(Load::Search) {
        return "Searching…".into();
    }
    if more {
        format!("{count}+ matches")
    } else {
        crate::ui::format::count(count as u64, "match", "matches")
    }
}

/// What the view last showed of the search, so state notifications that
/// change none of it (job progress) repaint nothing.
#[derive(Debug)]
struct Shown {
    revision: u64,
    loading: Option<Load>,
    error: Option<SearchError>,
    index_missing: bool,
    transcript_error: Option<TranscriptError>,
    more: bool,
}

#[derive(Default, PartialEq)]
struct FilterChoices {
    apps: Vec<String>,
    app_names: BTreeMap<String, String>,
    people: Vec<OptionItem>,
    tags: Vec<OptionItem>,
    naming: BTreeMap<MeetingId, usize>,
}

impl FilterChoices {
    fn read(lib: &broscribe_core::Library, data_dir: &Path) -> Result<Self> {
        let app_names = broscribe_core::detected_apps::names(data_dir);
        let index = match Index::open_readonly(lib.root()) {
            Ok(index) => index,
            Err(Error::NotFound(_)) if !Index::path(lib.root()).exists() => {
                return Ok(Self {
                    app_names,
                    ..Self::default()
                });
            }
            Err(e) => return Err(e),
        };
        let people = index
            .list_people()?
            .into_iter()
            .map(|person| {
                OptionItem::new(person.id.to_string(), person.name)
                    .detail(person.emails.first().cloned())
                    .search_terms(person.aliases.into_iter().chain(person.emails))
            })
            .collect();
        let tags = index
            .meeting_tags()?
            .into_iter()
            .map(|(tag, n)| {
                OptionItem::new(
                    tag.clone(),
                    format!("{tag}, {}", count(n as u64, "meeting", "meetings")),
                )
            })
            .collect();
        Ok(Self {
            apps: index.meeting_apps()?,
            app_names,
            people,
            tags,
            naming: broscribe_core::ops::review_naming_counts(lib)?,
        })
    }

    fn app_name(&self, id: &str) -> String {
        self.app_names
            .get(&id.to_ascii_lowercase())
            .cloned()
            .unwrap_or_else(|| {
                let mut chars = id.chars();
                chars
                    .next()
                    .map(|first| format!("{}{}", first.to_uppercase(), chars.as_str()))
                    .unwrap_or_default()
            })
    }

    fn person_name(&self, id: &str) -> String {
        self.people
            .iter()
            .find(|person| person.value == id)
            .map_or_else(|| id.to_owned(), |person| person.label.to_string())
    }
}

pub struct LibraryView {
    state: Entity<AppState>,
    search_input: Entity<InputState>,
    filters: LibraryFilters,
    choices: FilterChoices,
    choices_generation: u64,
    date_picker: Entity<DatePickerState>,
    custom_date_open: bool,
    rows: Vec<LibraryRow>,
    positions: Vec<usize>,
    set_size: usize,
    now: Option<NowRow>,
    daemon_connected: bool,
    row_menu: Option<OpenRowMenu>,
    index_repair: Option<(kit::StatusKind, String)>,
    shown: Option<Shown>,
    seen_revisions: Option<(u64, u64)>,
    /// The selected row and the meeting it shows (kept across refreshes).
    selected: Option<usize>,
    compact: bool,
    embedded: bool,
    filters_open: bool,
    focus_first_filter: bool,
    selected_id: Option<MeetingId>,
    reveal_pending: Option<MeetingId>,
    filters_cleared: bool,
    scroll: VirtualListScrollHandle,
    sizes: Rc<Vec<Size<Pixels>>>,
    measured: Option<(Pixels, bool)>,
    /// Focus handle on the library root: the node the import actions are
    /// dispatched on from the empty state's button, even while focus is
    /// outside the library. A click on the root itself never takes focus.
    view_focus: FocusHandle,
    focus: FocusHandle,
    pending_search: Option<Task<()>>,
    focus_results: bool,
    initial_load: Option<Task<()>>,
    show_loading: bool,
    pending_selection: Option<Task<()>>,
    runs: Entity<ImportRuns>,
    import_details: bool,
    import_focus: FocusHandle,
    transcription_block: Option<String>,
    model_check_generation: u64,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<LibraryEvent> for LibraryView {}

#[derive(IntoElement)]
pub(crate) struct LibraryToolbar {
    pub library: Entity<LibraryView>,
    pub hide_import: bool,
    pub hide_search: bool,
}

impl gpui::RenderOnce for LibraryToolbar {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let controls = self.library.update(cx, |library, cx| {
            let empty = library.empty_library(cx);
            library.toolbar_controls(self.hide_import, !self.hide_search, empty, cx)
        });
        div().flex_1().min_w_0().child(controls)
    }
}

impl Focusable for LibraryView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl LibraryView {
    pub fn new(state: Entity<AppState>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let current = state.read(cx).search().query().clone();
        let search_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Search meetings")
                .default_value(current.text.clone().unwrap_or_default())
        });
        let date_picker = cx.new(|cx| DatePickerState::range(window, cx));
        let date = current.since.zip(current.until).and_then(|(since, until)| {
            Some(DateFilter::Custom(
                since.with_timezone(&Local).date_naive(),
                until.with_timezone(&Local).date_naive().pred_opt()?,
            ))
        });
        let runs = ImportRuns::global(cx);
        let mut subscriptions = vec![
            cx.observe_in(&state, window, |this, _, window, cx| {
                let waiting_for_list = !this.has_meetings() && this.view_focus.is_focused(window);
                this.sync(cx);
                this.continue_reveal(window, cx);
                if waiting_for_list && this.has_meetings() && this.view_focus.is_focused(window) {
                    this.focus_list(window, cx);
                }
                if this.focus_results && this.state.read(cx).search().loading().is_none() {
                    this.focus_results = false;
                    if this.search_has_focus(window, cx) {
                        this.focus_first_result(window, cx);
                    }
                }
            }),
            cx.observe(&runs, |_, _, cx| cx.notify()),
            cx.subscribe(&search_input, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.schedule_search(cx);
                }
            }),
            cx.subscribe(&date_picker, |this, _, event: &DatePickerEvent, cx| {
                let DatePickerEvent::Change(value) = event;
                if let Some((start, end)) = value.start().zip(value.end()) {
                    this.filters.date = Some(DateFilter::Custom(start.date(), end.date()));
                    this.search_now(cx);
                }
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.check_transcription_models(cx);
                }
            }),
        ];
        if let Some(daemon) = state.read(cx).daemon().cloned() {
            subscriptions.push(cx.observe_in(&daemon, window, |this, _, window, cx| {
                this.sync(cx);
                this.continue_reveal(window, cx);
            }));
        }
        let mut view = Self {
            state,
            search_input,
            filters: LibraryFilters {
                person: current.person.unwrap_or_default(),
                tag: current.tag.unwrap_or_default(),
                app: current.app,
                source: current.source,
                date,
                order: current.meeting_order,
                ..LibraryFilters::default()
            },
            choices: FilterChoices::default(),
            choices_generation: 0,
            date_picker,
            custom_date_open: false,
            rows: Vec::new(),
            positions: Vec::new(),
            set_size: 0,
            now: None,
            daemon_connected: false,
            row_menu: None,
            index_repair: None,
            shown: None,
            seen_revisions: None,
            selected: None,
            compact: false,
            embedded: false,
            filters_open: false,
            focus_first_filter: false,
            selected_id: None,
            reveal_pending: None,
            filters_cleared: false,
            scroll: VirtualListScrollHandle::new(),
            sizes: Rc::default(),
            measured: None,
            view_focus: cx.focus_handle(),
            focus: cx.focus_handle(),
            pending_search: None,
            focus_results: false,
            initial_load: None,
            show_loading: false,
            pending_selection: None,
            runs,
            import_details: false,
            import_focus: cx.focus_handle(),
            transcription_block: None,
            model_check_generation: 0,
            _subscriptions: subscriptions,
        };
        // First show: nothing searched yet.
        if view.state.read(cx).search().generation() == 0 {
            view.search_now(cx);
        }
        view.sync(cx);
        view.check_transcription_models(cx);
        view
    }

    fn check_transcription_models(&mut self, cx: &mut Context<Self>) {
        use super::settings::{parse_models_list, transcription_block};
        self.model_check_generation += 1;
        let generation = self.model_check_generation;
        let task = self.state.read(cx).run_cli(
            crate::views::settings::args(["models", "list"]),
            ChildKind::Command,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.model_check_generation == generation {
                    this.transcription_block = result.ok().and_then(|v| {
                        transcription_block(&parse_models_list(&v).missing_profile_models())
                    });
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The filters as currently entered.
    pub fn filters(&self, cx: &App) -> LibraryFilters {
        LibraryFilters {
            text: self.search_input.read(cx).value().to_string(),
            ..self.filters.clone()
        }
    }

    pub fn rows(&self) -> &[LibraryRow] {
        &self.rows
    }

    /// The selected meeting, kept while it stays listed.
    pub fn selected_meeting(&self) -> Option<&MeetingId> {
        self.selected_id.as_ref()
    }

    pub(crate) fn reveal_meeting(
        &mut self,
        id: &MeetingId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pending_selection = None;
        self.selected_id = Some(id.clone());
        self.selected = None;
        self.reveal_pending = Some(id.clone());
        self.continue_reveal(window, cx);
        cx.notify();
    }

    fn continue_reveal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = &self.reveal_pending else {
            return;
        };
        let search = self.state.read(cx).search();
        if self.pending_search.is_some() || search.loading().is_some() || search.error().is_some() {
            return;
        }
        if let Some(row) = meeting_row(&self.rows, search.results(), id) {
            self.reveal_pending = None;
            self.select(Some(row), cx);
        } else if search.has_more() && search.transcript_error().is_none() {
            self.load_more(cx);
        } else if self.filters(cx).is_filtering() && search.transcript_error().is_none() {
            self.reset_filters(window, cx);
            self.search_input
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.filters_cleared = true;
            self.search_now(cx);
        }
    }

    pub(crate) fn has_meetings(&self) -> bool {
        !self.rows.is_empty()
    }

    pub(crate) fn select_after_deleting(
        &mut self,
        id: &MeetingId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<MeetingId> {
        let row =
            meeting_row(&self.rows, self.state.read(cx).search().results(), id).or(self.selected);
        let next = next_meeting_row(&self.rows, row, true)
            .or_else(|| row.and_then(|row| next_meeting_row(&self.rows, Some(row), false)));
        self.pending_selection = None;
        self.reveal_pending = None;
        self.selected = None;
        self.selected_id = None;
        self.select(next, cx);
        if next.is_some() {
            self.focus_list(window, cx);
        } else {
            self.focus_search(window, cx);
        }
        cx.notify();
        self.selected_id.clone().filter(|selected| selected != id)
    }

    pub(crate) fn cancel_pending_selection(&mut self) {
        self.pending_selection = None;
        self.reveal_pending = None;
    }

    pub(crate) fn focus_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(row) = self.selected {
            self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
        }
        let focus = if self.has_meetings() {
            &self.focus
        } else {
            &self.view_focus
        };
        focus.focus(window, cx);
    }

    pub fn apps(&self) -> &[String] {
        &self.choices.apps
    }

    pub(crate) fn show_person_meetings(
        &mut self,
        person: &broscribe_core::PersonId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.reset_filters(window, cx);
        self.cancel_pending_selection();
        self.selected = None;
        self.selected_id = None;
        self.search_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.filters.person = person.to_string();
        self.search_now(cx);
    }

    pub fn set_app_filter(&mut self, app: Option<String>, cx: &mut Context<Self>) {
        if self.filters.app != app {
            self.filters.app = app;
            self.search_now(cx);
        }
    }

    pub fn set_source_filter(&mut self, source: Option<Source>, cx: &mut Context<Self>) {
        if self.filters.source != source {
            self.filters.source = source;
            self.search_now(cx);
        }
    }

    /// Searches after [`SEARCH_DEBOUNCE`] unless more typing arrives
    /// (replacing the task cancels the previous timer).
    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        if non_blank(&self.search_input.read(cx).value())
            == self.state.read(cx).search().query().text
        {
            self.pending_search = None;
            return;
        }
        self.pending_search = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            this.update(cx, |this, cx| this.search_now(cx)).ok();
        }));
    }

    /// Runs the search for the current filters immediately.
    pub fn search_now(&mut self, cx: &mut Context<Self>) {
        self.pending_search = None;
        self.pending_selection = None;
        let query = self.filters(cx).query();
        self.state.update(cx, |state, cx| {
            if query.text != state.search().query().text
                && !matches!(state.route(), Route::Library { .. })
            {
                state.navigate(Route::library(), cx);
            }
            state.search_meetings(query, cx);
        });
    }

    fn retry_search(&mut self, cx: &mut Context<Self>) {
        if self.filters(cx).query() == *self.state.read(cx).search().query() {
            self.pending_search = None;
            self.state.update(cx, |state, cx| state.rerun_search(cx));
        } else {
            self.search_now(cx);
        }
    }

    fn empty_library(&self, cx: &App) -> bool {
        let search = self.state.read(cx).search();
        search.results().is_empty()
            && search.loading() != Some(Load::Search)
            && search.error().is_none()
            && search.transcript_error().is_none()
            && !search.index_missing()
            && self.now.is_none()
            && search.query().text.is_none()
            && !self.filters.is_filtering()
    }

    /// Lists the next page of the search, if there is one.
    pub fn load_more(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.load_more_meetings(cx);
        });
    }

    /// "Try again" beside a transcript failure: the failed page through
    /// Load more when it failed there and is still next, otherwise the
    /// search from its first page.
    fn retry_transcripts(&mut self, cx: &mut Context<Self>) {
        let search = self.state.read(cx).search();
        let more = search.has_more() && search.transcript_error().is_some_and(|e| e.retry_more);
        if more {
            self.load_more(cx);
        } else {
            self.search_now(cx);
        }
    }

    /// Follows the state: rebuilds rows when the results changed (keeping
    /// the selected meeting where it is still listed), reloads the app list
    /// when meetings changed, and repaints only when what it shows changed.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let revisions = (state.meetings_revision(), state.people_revision());
        let search = state.search();
        let daemon = state.daemon().map(|daemon| daemon.read(cx));
        let connected = daemon.is_some_and(|daemon| daemon.connected);
        let now = daemon
            .filter(|daemon| daemon.connected && !daemon.agent_library().elsewhere())
            .and_then(|daemon| NowRow::from_panel(&daemon.panel));
        let now_changed = now != self.now || connected != self.daemon_connected;
        let (revision, loading, error, transcript_error, more) = (
            search.revision(),
            search.loading(),
            search.failure(),
            search.transcript_error(),
            search.has_more(),
        );
        // A refresh presents the same settled UI until any result changes.
        let visible_loading = loading.filter(|load| *load != Load::Refresh);
        let unchanged = self.shown.as_ref().is_some_and(|shown| {
            !now_changed
                && shown.revision == revision
                && shown.loading == visible_loading
                && shown.error.as_ref() == error
                && shown.index_missing == search.index_missing()
                && shown.transcript_error.as_ref() == transcript_error
                && shown.more == more
        });
        let rebuild =
            self.now != now || self.shown.as_ref().map(|shown| shown.revision) != Some(revision);
        let shown = (!unchanged).then(|| Shown {
            revision,
            loading: visible_loading,
            error: error.cloned(),
            index_missing: search.index_missing(),
            transcript_error: transcript_error.cloned(),
            more,
        });
        let now_selected =
            self.selected.and_then(|row| self.rows.get(row)) == Some(&LibraryRow::Now);
        let same_session = self
            .now
            .as_ref()
            .zip(now.as_ref())
            .is_some_and(|(old, new)| old.session == new.session);
        self.now = now;
        self.daemon_connected = connected;
        if rebuild {
            let today = Local::now().date_naive();
            let hits = search.results();
            self.rows = project_rows(search, self.now.as_ref(), today);
            self.measured = None;
            self.set_size = 0;
            self.positions = self
                .rows
                .iter()
                .map(|row| {
                    if !matches!(row, LibraryRow::Day(_)) {
                        self.set_size += 1;
                    }
                    self.set_size
                })
                .collect();
            self.selected = self
                .selected_id
                .as_ref()
                .and_then(|id| meeting_row(&self.rows, hits, id))
                .or_else(|| (now_selected && same_session).then_some(1));
        }
        if loading == Some(Load::Search) && search.results().is_empty() {
            if self.initial_load.is_none() && !self.show_loading {
                self.initial_load = Some(cx.spawn(async move |view, cx| {
                    cx.background_executor().timer(INITIAL_LOAD_DELAY).await;
                    let _ = view.update(cx, |view, cx| {
                        view.initial_load = None;
                        if view.state.read(cx).search().loading() == Some(Load::Search)
                            && view.state.read(cx).search().results().is_empty()
                        {
                            view.show_loading = true;
                            cx.notify();
                        }
                    });
                }));
            }
        } else {
            self.initial_load = None;
            self.show_loading = false;
        }
        if self.seen_revisions != Some(revisions) {
            self.seen_revisions = Some(revisions);
            self.reload_choices(cx);
        }
        if let Some(shown) = shown {
            self.shown = Some(shown);
            cx.notify();
        }
    }

    fn reload_choices(&mut self, cx: &mut Context<Self>) {
        self.choices_generation += 1;
        let generation = self.choices_generation;
        let library = self.state.read(cx).library().clone();
        let data_dir = self.state.read(cx).paths().data_dir.clone();
        let task = self
            .state
            .read(cx)
            .cli()
            .spawn(cx.background_executor(), async move {
                FilterChoices::read(&library, &data_dir)
            });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                if generation != this.choices_generation {
                    return;
                }
                match result {
                    Ok(choices) => {
                        if this.choices != choices {
                            this.choices = choices;
                            cx.notify();
                        }
                    }
                    Err(e) => tracing::warn!(code = e.code(), "reading library filter choices"),
                }
            })
            .ok();
        })
        .detach();
    }

    /// The meeting shown by `row`, if it shows one.
    fn row_meeting(&self, row: usize, cx: &App) -> Option<MeetingId> {
        let Some(LibraryRow::Meeting(i)) = self.rows.get(row) else {
            return None;
        };
        self.state
            .read(cx)
            .search()
            .results()
            .get(*i)
            .map(|h| h.meeting_id.clone())
    }

    fn open_row(&mut self, row: usize, enter: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_selection = None;
        self.reveal_pending = None;
        if self.rows.get(row) == Some(&LibraryRow::Now) {
            let present = self.state.read(cx).daemon().and_then(|daemon| {
                let daemon = daemon.read(cx);
                daemon
                    .connected
                    .then(|| NowRow::from_panel(&daemon.panel))
                    .flatten()
            });
            if self
                .now
                .as_ref()
                .zip(present.as_ref())
                .is_some_and(|(shown, current)| shown.session == current.session)
            {
                self.select(Some(row), cx);
                window.dispatch_action(Box::new(crate::menu::ShowRecordingControls), cx);
            }
            return;
        }
        if let Some(id) = self.row_meeting(row, cx) {
            let segment = self
                .state
                .read(cx)
                .search()
                .transcript_match(&id)
                .and_then(|hit| hit.segment);
            self.select(Some(row), cx);
            cx.emit(LibraryEvent::SelectMeeting {
                id,
                enter,
                preview: false,
                segment,
                action: None,
            });
        }
    }

    fn run_row_command(
        &mut self,
        id: &MeetingId,
        action: RowCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // An open menu is bound to the row it was opened on. If a refresh
        // removed it, do not apply a command to the new selection.
        let Some(row) = meeting_row(&self.rows, self.state.read(cx).search().results(), id) else {
            return;
        };
        self.pending_selection = None;
        self.reveal_pending = None;
        self.select(Some(row), cx);
        self.focus.focus(window, cx);
        cx.emit(LibraryEvent::SelectMeeting {
            id: id.clone(),
            enter: false,
            preview: false,
            segment: None,
            action: Some(action),
        });
    }

    fn row_menu_items(menu: PopupMenu, view: gpui::WeakEntity<Self>, id: MeetingId) -> PopupMenu {
        let command = |label, action| {
            let view = view.clone();
            let id = id.clone();
            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                let _ = view.update(cx, |view, cx| view.run_row_command(&id, action, window, cx));
            })
        };
        let reveal = broscribe_platform_native::NativePlatform::host_platform().reveal_label();
        let delete = command("Delete meeting", RowCommand::Delete);
        menu.item(command("Rename", RowCommand::Rename))
            .item(command("Add tag…", RowCommand::AddTag))
            .item(command("Export…", RowCommand::Export))
            .item(PopupMenuItem::new(reveal).on_click(move |_, _, cx| {
                if let Some(view) = view.upgrade() {
                    let library = view.read(cx).state.read(cx).library().clone();
                    let id = id.clone();
                    let task = cx
                        .background_executor()
                        .spawn(async move { library.meeting_dir(&id) });
                    cx.spawn(async move |cx| {
                        if let Ok(path) = task.await {
                            cx.update(|cx| cx.reveal_path(&path));
                        }
                    })
                    .detach();
                }
            }))
            .separator()
            .item(delete)
    }

    fn open_row_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((row, id)) = self.selected.zip(self.selected_id.clone()) else {
            return;
        };
        let scroll = self.scroll.base_handle();
        let viewport = scroll.bounds();
        // The virtual list lays out only mounted rows. Its base handle has
        // no child bounds indexed by logical row.
        let bottom = self
            .sizes
            .iter()
            .take(row + 1)
            .fold(viewport.top() + scroll.offset().y, |y, size| {
                y + size.height
            });
        let position = point(
            viewport.left() + px(12.),
            bottom.max(viewport.top()).min(viewport.bottom()),
        );
        let this = cx.weak_entity();
        let menu = PopupMenu::build(window, cx, |menu, _, _| {
            Self::row_menu_items(menu.action_context(self.focus.clone()), this, id)
        });
        let dismiss = cx.subscribe_in(
            &menu,
            window,
            |this, _, _: &gpui::DismissEvent, window, cx| {
                this.close_row_menu(window, cx);
            },
        );
        menu.focus_handle(cx).focus(window, cx);
        self.row_menu = Some(OpenRowMenu {
            menu,
            position,
            _dismiss: dismiss,
        });
        cx.notify();
    }

    fn close_row_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(open) = self.row_menu.take() {
            if open.menu.focus_handle(cx).contains_focused(window, cx) {
                self.focus.focus(window, cx);
            }
            cx.notify();
        }
    }

    fn select(&mut self, row: Option<usize>, cx: &mut Context<Self>) {
        if let Some(row) = row {
            if self.selected != Some(row) {
                self.pending_selection = None;
            }
            self.selected = Some(row);
            self.selected_id = self.row_meeting(row, cx);
            self.scroll.scroll_to_item(row, ScrollStrategy::Nearest);
            cx.notify();
        }
    }

    fn on_list_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if ev.keystroke.modifiers.modified() {
            return;
        }
        let previous = self.selected;
        match ev.keystroke.key.as_str() {
            "down" => {
                match next_meeting_row(&self.rows, self.selected, true) {
                    Some(next) => self.select(Some(next), cx),
                    // Past the last row: the next page, if any.
                    None => self.load_more(cx),
                }
            }
            "up" => {
                let prev = next_meeting_row(&self.rows, self.selected, false);
                self.select(prev, cx);
            }
            "home" => {
                let first = next_meeting_row(&self.rows, None, true);
                self.select(first, cx);
            }
            "end" => {
                let last = next_meeting_row(&self.rows, None, false);
                self.select(last, cx);
            }
            "pageup" | "pagedown" => {
                let down = ev.keystroke.key == "pagedown";
                let mut row = self.selected;
                let scroll = self.scroll.base_handle();
                let viewport = scroll.bounds().size.height;
                let top = -scroll.offset().y;
                let mut origin = px(0.);
                let mut fully_visible = 0;
                for (index, size) in self.sizes.iter().enumerate() {
                    let end = origin + size.height;
                    if end > top + viewport {
                        break;
                    }
                    if origin >= top && !matches!(self.rows[index], LibraryRow::Day(_)) {
                        fully_visible += 1;
                    }
                    origin = end;
                }
                for _ in 0..fully_visible.max(1) {
                    let Some(next) = next_meeting_row(&self.rows, row, down) else {
                        break;
                    };
                    row = Some(next);
                }
                self.select(row, cx);
            }
            "escape" => self.focus_search(window, cx),
            "enter" | "space" | "tab" => {
                if let Some(row) = self.selected {
                    if ev.keystroke.key == "tab"
                        && (!self.embedded || self.rows.get(row) == Some(&LibraryRow::Now))
                    {
                        return;
                    }
                    self.open_row(row, true, window, cx);
                } else if ev.keystroke.key == "tab" {
                    return;
                }
            }
            _ => return,
        }
        if self.embedded
            && previous != self.selected
            && let Some(id) = self.selected_id.clone()
        {
            self.reveal_pending = None;
            self.pending_selection = Some(cx.spawn(async move |view, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(150))
                    .await;
                let _ = view.update(cx, |_, cx| {
                    cx.emit(LibraryEvent::SelectMeeting {
                        id,
                        enter: false,
                        preview: true,
                        segment: None,
                        action: None,
                    })
                });
            }));
        }
        cx.stop_propagation();
    }

    fn on_search_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.modifiers.modified() {
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" if !self.search_input.read(cx).value().is_empty() => {
                self.focus_results = false;
                self.search_input
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.search_now(cx);
            }
            "escape" | "down" | "enter" => {
                if self.pending_search.is_some() {
                    self.search_now(cx);
                }
                if self.state.read(cx).search().loading() == Some(Load::Search) {
                    self.focus_results = true;
                } else {
                    self.focus_first_result(window, cx);
                }
            }
            _ => return,
        }
        cx.stop_propagation();
        window.prevent_default();
    }

    fn focus_first_result(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let first = next_meeting_row(&self.rows, None, true);
        self.select(first, cx);
        self.focus_list(window, cx);
    }

    pub(crate) fn search_has_focus(&self, window: &Window, cx: &App) -> bool {
        self.search_input
            .read(cx)
            .focus_handle(cx)
            .contains_focused(window, cx)
    }

    /// Opens a native path picker with `options` and routes chosen paths to
    /// the import sheet.
    fn open_import_picker(&mut self, options: PathPromptOptions, cx: &mut Context<Self>) {
        let picked = pick_import_paths(options, cx);
        cx.spawn(async move |_, cx| match picked.await {
            Ok(Some(paths)) if !paths.is_empty() => {
                cx.update(|cx| ImportRuns::request_dialog(paths, cx))
            }
            Err(e) => tracing::warn!("open panel: {e}"),
            _ => {}
        })
        .detach();
    }

    /// Main "Import…" button: mixed picker when the platform supports it
    /// (macOS), files-only otherwise (Windows, Linux).
    fn choose_import(&mut self, cx: &mut Context<Self>) {
        let options = import_main_options(cx.can_select_mixed_files_and_dirs());
        self.open_import_picker(options, cx);
    }

    /// Files-only picker ([`ImportRecordings`]).
    pub(crate) fn choose_recordings(&mut self, cx: &mut Context<Self>) {
        self.open_import_picker(import_recordings_options(), cx);
    }

    /// Folders-only picker (`ImportFolder`).
    pub(crate) fn choose_folder(&mut self, cx: &mut Context<Self>) {
        self.open_import_picker(import_folder_options(), cx);
    }

    // ---- rendering ----

    fn toolbar_controls(
        &self,
        hide_import: bool,
        show_search: bool,
        search_disabled: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .when(show_search, |el| {
                el.child(
                    div()
                        .id("library-search-control")
                        .flex_1()
                        .min_w(rems(10.))
                        .max_w(rems(15.))
                        .when(search_disabled, |el| {
                            el.tooltip(kit::title_tooltip("No meetings to search yet"))
                        })
                        .capture_key_down(cx.listener(Self::on_search_key))
                        .child(
                            Input::new(&self.search_input)
                                .disabled(search_disabled)
                                .prefix(crate::ui::icons::icon(crate::ui::icons::SEARCH))
                                .aria_label("Search meetings")
                                .cleanable(true),
                        ),
                )
            })
            .when(!hide_import, |el| {
                el.child(
                    div().debug_selector(|| "library-import".into()).child(
                        DropdownButton::new("library-import")
                            .button(
                                Button::new("library-import-button")
                                    .debug_selector(|| "library-import-button".into())
                                    .label("Import…")
                                    .accessibility_label("Import recordings")
                                    .tooltip_with_action(
                                        "Import recordings",
                                        &ImportRecordings,
                                        None,
                                    )
                                    .on_click(cx.listener(|this, _, _, cx| this.choose_import(cx))),
                            )
                            .menu_accessibility_label("More import options")
                            .ghost()
                            // The items' actions bubble from the menu through
                            // the library root, which handles them; focus goes
                            // back where it was.
                            .dropdown_menu(|menu, _, _| {
                                menu.menu("Import recordings…", Box::new(ImportRecordings))
                                    .menu("Import folder…", Box::new(ImportFolder))
                            }),
                    ),
                )
            })
    }

    pub(crate) fn set_compact(&mut self, compact: bool, cx: &mut Context<Self>) {
        if self.compact != compact {
            self.compact = compact;
            cx.notify();
        }
    }

    pub(crate) fn embed(&mut self) {
        self.embedded = true;
    }

    pub(crate) fn focus_search(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.empty_library(cx) {
            return;
        }
        self.search_input.update(cx, |input, cx| {
            input.focus_handle(cx).focus(window, cx);
            input.select_all(window, cx);
        });
    }

    fn filters_button(&mut self, cx: &Context<Self>) -> impl IntoElement {
        let library = cx.weak_entity();
        let active = self.filter_values().into_iter().flatten().count();
        let focus_first = std::mem::take(&mut self.focus_first_filter);
        gpui::component::popover::Popover::new("library-filters-popover")
            .trigger(
                Button::new("library-open-filters")
                    .debug_selector(|| "library-open-filters".into())
                    .label("Filters")
                    .accessibility_label(if active == 0 {
                        "Filters".to_owned()
                    } else {
                        format!("Filters, {active} active")
                    })
                    .dropdown_caret(true)
                    .ghost(),
            )
            .open(self.filters_open)
            .on_open_change(cx.listener(|this, open, _, cx| {
                this.filters_open = *open;
                this.focus_first_filter = *open;
                cx.notify();
            }))
            .content(move |_, _, cx| {
                library
                    .update(cx, |this, cx| this.filter_fields(focus_first, cx))
                    .unwrap_or_else(|_| div())
            })
    }

    fn filter_fields(&self, focus_first: bool, cx: &Context<Self>) -> Div {
        let date_options =
            DateFilter::PRESETS.map(|(_, value, label)| OptionItem::new(value, label));
        let selected_date = match self.filters.date {
            _ if self.custom_date_open => "custom",
            Some(DateFilter::Custom(..)) => "custom",
            Some(date) => {
                DateFilter::PRESETS
                    .iter()
                    .find(|(value, _, _)| *value == date)
                    .unwrap()
                    .1
            }
            None => "",
        };
        let field = |id, label, picker| {
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(text_line(id, Role::Label, label).text_role(TextRole::Label))
                .child(picker)
        };
        let today = Local::now().date_naive();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .w(rems(18.))
            .p_2()
            .child(field(
                "filter-person-label",
                "Person",
                Picker::new(
                    "filter-person",
                    "Person",
                    self.choices.people.clone(),
                    &self.filters.person,
                    cx,
                    |this, value, _, cx| {
                        this.filters.person = value.to_owned();
                        this.search_now(cx);
                    },
                )
                .clearable("Anyone")
                .focus_on_mount(focus_first),
            ))
            .child(field(
                "filter-tag-label",
                "Tag",
                Picker::new(
                    "filter-tag",
                    "Tag",
                    self.choices.tags.clone(),
                    &self.filters.tag,
                    cx,
                    |this, value, _, cx| {
                        this.filters.tag = value.to_owned();
                        this.search_now(cx);
                    },
                )
                .clearable("Any tag"),
            ))
            .child(field(
                "filter-app-label",
                "App",
                Picker::new(
                    "filter-app",
                    "App",
                    self.apps()
                        .iter()
                        .map(|app| OptionItem::new(app.clone(), self.choices.app_name(app)))
                        .collect(),
                    self.filters.app.as_deref().unwrap_or_default(),
                    cx,
                    |this, value, _, cx| this.set_app_filter(non_blank(value), cx),
                )
                .clearable("Any app"),
            ))
            .child(field(
                "filter-source-label",
                "Source",
                Picker::new(
                    "filter-source",
                    "Source",
                    vec![
                        OptionItem::new("recorded", "Recorded"),
                        OptionItem::new("imported", "Imported"),
                    ],
                    match self.filters.source {
                        Some(Source::Recording) => "recorded",
                        Some(Source::Import) => "imported",
                        None => "",
                    },
                    cx,
                    |this, value, _, cx| {
                        this.set_source_filter(
                            match value {
                                "recorded" => Some(Source::Recording),
                                "imported" => Some(Source::Import),
                                _ => None,
                            },
                            cx,
                        )
                    },
                )
                .clearable("Recorded and imported"),
            ))
            .child(field(
                "filter-sort-label",
                "Sort",
                Picker::new(
                    "filter-sort",
                    "Sort",
                    vec![
                        OptionItem::new("newest", "Newest first"),
                        OptionItem::new("oldest", "Oldest first"),
                    ],
                    match self.filters.order {
                        MeetingOrder::Newest => "newest",
                        MeetingOrder::Oldest => "oldest",
                    },
                    cx,
                    |this, value, _, cx| {
                        this.filters.order = if value == "oldest" {
                            MeetingOrder::Oldest
                        } else {
                            MeetingOrder::Newest
                        };
                        this.search_now(cx);
                    },
                )
                .disabled(!self.filters(cx).text.trim().is_empty()),
            ))
            .child(field(
                "filter-date-label",
                "Date",
                Picker::new(
                    "filter-date",
                    "Date",
                    date_options
                        .into_iter()
                        .chain([OptionItem::new("custom", "Custom range…")])
                        .collect(),
                    selected_date,
                    cx,
                    |this, value, _, cx| {
                        this.custom_date_open = value == "custom";
                        if this.custom_date_open {
                            cx.notify();
                        } else {
                            this.filters.date = DateFilter::PRESETS
                                .iter()
                                .find(|(_, key, _)| *key == value)
                                .map(|(date, _, _)| *date);
                            this.search_now(cx);
                        }
                    },
                )
                .clearable("Any time"),
            ))
            .when(
                self.custom_date_open || matches!(self.filters.date, Some(DateFilter::Custom(..))),
                |el| {
                    el.child(text_line(
                        "filter-range-label",
                        Role::Label,
                        "Custom date range",
                    ))
                    .child(
                        DatePicker::new(&self.date_picker)
                            .placeholder("Choose dates")
                            .presets(
                                DateFilter::PRESETS
                                    .iter()
                                    .map(|(date, _, label)| {
                                        let (start, end) = date.dates(today);
                                        DateRangePreset::range(*label, start, end)
                                    })
                                    .collect(),
                            ),
                    )
                },
            )
            .child(self.clear_filters_button("popover-clear-filters", cx))
    }

    fn filter_values(&self) -> [Option<String>; 5] {
        [
            non_blank(&self.filters.person)
                .map(|person| format!("Person: {}", self.choices.person_name(&person))),
            non_blank(&self.filters.tag).map(|tag| format!("Tag: {tag}")),
            self.filters
                .app
                .as_deref()
                .map(|app| format!("App: {}", self.choices.app_name(app))),
            self.filters.source.map(|source| match source {
                Source::Recording => "Source: Recorded".into(),
                Source::Import => "Source: Imported".into(),
            }),
            self.filters
                .date
                .map(|date| format!("Date: {}", date.display())),
        ]
    }

    fn clear_filters_button(&self, id: &'static str, cx: &Context<Self>) -> Button {
        Button::new(id)
            .debug_selector(move || id.into())
            .label("Clear filters")
            .small()
            .ghost()
            .disabled(self.filter_values().iter().all(Option::is_none))
            .on_click(cx.listener(|this, _, window, cx| {
                this.reset_filters(window, cx);
                this.search_now(cx);
            }))
    }

    fn reset_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.filters = LibraryFilters {
            order: self.filters.order,
            ..LibraryFilters::default()
        };
        self.clear_custom_date(window, cx);
    }

    fn clear_custom_date(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.custom_date_open = false;
        self.date_picker.update(cx, |picker, cx| {
            picker.set_date(
                gpui::component::date_picker::DateTime::Range(None, None).date(),
                window,
                cx,
            );
        });
    }

    fn filter_tokens(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
        let values = self.filter_values();
        if values.iter().all(Option::is_none) {
            return None;
        }
        let row = div()
            .id("library-filter-tokens")
            .role(Role::Group)
            .aria_label("Active filters")
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1();
        Some(
            row.children(values.into_iter().enumerate().filter_map(|(index, value)| {
                let value = value?;
                Some(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .pl_2()
                        .pr_0p5()
                        .h(rems(1.5))
                        .rounded_full()
                        .border_1()
                        .border_color(cx.theme().input)
                        .child(
                            text_line(("library-filter-value", index), Role::Label, value.clone())
                                .text_role(TextRole::Caption),
                        )
                        .child(
                            Button::new(("library-filter-token", index))
                                .debug_selector(move || format!("library-filter-token-{index}"))
                                .icon(crate::ui::icons::DISMISS)
                                .accessibility_label(format!("Remove filter {value}"))
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    match index {
                                        0 => this.filters.person.clear(),
                                        1 => this.filters.tag.clear(),
                                        2 => this.filters.app = None,
                                        3 => this.filters.source = None,
                                        _ => {
                                            this.filters.date = None;
                                            this.clear_custom_date(window, cx);
                                        }
                                    }
                                    this.search_now(cx);
                                })),
                        ),
                )
            }))
            .child(self.clear_filters_button("clear-library-filters", cx)),
        )
    }

    pub(crate) fn focus_imports(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.import_details = true;
        self.import_focus.focus(window, cx);
        cx.notify();
    }

    pub(crate) fn import_rows(&self, cx: &Context<Self>) -> Option<Stateful<Div>> {
        let runs = self.runs.read(cx);
        let total = runs.runs().len();
        if total == 0 {
            return None;
        }
        let waiting = runs
            .runs()
            .iter()
            .filter(|r| r.status == ImportRunStatus::Waiting)
            .count();
        let failed = runs.runs().iter().filter(|r| r.failed()).count();
        let finished = runs
            .runs()
            .iter()
            .filter(|r| r.status.is_finished())
            .count();
        let active = runs.progress();
        let (kind, words) = if let Some((position, total, run)) = active {
            (
                kit::StatusKind::Pending,
                format!(
                    "Importing {position} of {total} · {}",
                    file_label(&run.path)
                ),
            )
        } else if waiting > 0 {
            (
                kit::StatusKind::Pending,
                format!("{waiting} waiting to import"),
            )
        } else if failed > 0 {
            (
                kit::StatusKind::Failed,
                format!("Imported {} of {total} · {failed} failed", total - failed),
            )
        } else {
            (
                kit::StatusKind::Done,
                format!("Imported {total} of {total}"),
            )
        };
        let row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(kit::status_line("library-import-summary", kind, words, cx))
            .when(runs.is_busy(), |row| {
                row.child(
                    kit::progress(
                        "library-import-progress",
                        format!("Imports: {finished} of {total} finished"),
                        finished as f32 / total as f32,
                    )
                    .small(),
                )
            })
            .child(kit::disclosure(
                "library-import-details",
                "Details",
                self.import_details,
                cx,
                |this, _, cx| {
                    this.import_details = !this.import_details;
                    cx.notify();
                },
            ))
            .when(waiting >= 2, |row| {
                row.child(
                    Button::new("library-import-cancel-remaining")
                        .label("Cancel remaining")
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.runs
                                .update(cx, |runs, cx| runs.cancel_waiting(None, cx));
                        })),
                )
            })
            .when(!runs.is_busy(), |row| {
                row.child(
                    Button::new("library-import-dismiss")
                        .label("Dismiss")
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.runs.update(cx, |runs, cx| runs.dismiss_finished(cx));
                            this.import_details = false;
                            cx.notify();
                        })),
                )
            });
        Some(div().id("library-imports").role(Role::Group).aria_label("Imports")
            .track_focus(&self.import_focus).flex().flex_col().gap_1().child(row)
            .when(active.is_some(), |list| list.child(text_line(
                "library-import-wait", Role::Label,
                "Long recordings can take several minutes. You can keep working. The meeting appears in the list when it’s ready.",
            ).text_role(TextRole::Caption)))
            .when(self.import_details, |list| list.child(
                div().id("library-import-run-list").flex().flex_col().gap_1()
                    .max_h(rems(10.)).overflow_y_scroll()
                    .children(runs.runs().iter().map(|run| self.import_run_row(run, cx))),
            )))
    }

    fn import_run_row(
        &self,
        run: &crate::views::import_sheet::ImportRun,
        cx: &Context<Self>,
    ) -> Div {
        let id = run.id;
        let name = file_label(&run.path);
        let kind = match &run.status {
            ImportRunStatus::Waiting | ImportRunStatus::Running => kit::StatusKind::Pending,
            ImportRunStatus::Done(_) => kit::StatusKind::Done,
            ImportRunStatus::Failed(_) => kit::StatusKind::Failed,
        };
        let open = match &run.status {
            ImportRunStatus::Done(outcome) => {
                match outcome.meetings().collect::<Vec<_>>().as_slice() {
                    [] => None,
                    [(meeting, _)] => {
                        let meeting = meeting.clone();
                        Some(
                            Button::new(("import-open", id))
                                .debug_selector(|| "import-open-meeting".into())
                                .label("Open meeting")
                                .ghost()
                                .small()
                                .accessibility_label(format!("Open meeting imported from {name}"))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(LibraryEvent::SelectMeeting {
                                        id: meeting.clone(),
                                        enter: true,
                                        preview: false,
                                        segment: None,
                                        action: None,
                                    });
                                }))
                                .into_any_element(),
                        )
                    }
                    meetings => {
                        let view = cx.weak_entity();
                        let meetings = meetings.to_vec();
                        Some(
                            Button::new(("import-open-many", id))
                                .label("Open meeting…")
                                .ghost()
                                .small()
                                .accessibility_label(format!("Open a meeting imported from {name}"))
                                .dropdown_menu(move |menu, _, _| {
                                    meetings.iter().fold(menu, |menu, (id, title)| {
                                        let view = view.clone();
                                        let id = id.clone();
                                        menu.item(PopupMenuItem::new(title.clone()).on_click(
                                            move |_, _, cx| {
                                                let _ = view.update(cx, |_, cx| {
                                                    cx.emit(LibraryEvent::SelectMeeting {
                                                        id: id.clone(),
                                                        enter: true,
                                                        preview: false,
                                                        segment: None,
                                                        action: None,
                                                    })
                                                });
                                            },
                                        ))
                                    })
                                })
                                .into_any_element(),
                        )
                    }
                }
            }
            _ => None,
        };
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(kit::status_line(("library-import", id), kind, run.words(), cx).flex_1())
            .when(run.status == ImportRunStatus::Waiting, |row| {
                row.child(
                    Button::new(("import-cancel-waiting", id))
                        .label("Cancel")
                        .ghost()
                        .small()
                        .accessibility_label(format!("Cancel import of {name}"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.runs
                                .update(cx, |runs, cx| runs.cancel_waiting(Some(id), cx));
                        })),
                )
            })
            .when(run.failed(), |row| {
                row.child(
                    Button::new(("import-retry", id))
                        .debug_selector(|| "import-retry-run".into())
                        .label("Try again")
                        .ghost()
                        .small()
                        .accessibility_label(format!("Try importing {name} again"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(request) = this.runs.read(cx).retry_request(id) {
                                this.runs.update(cx, |runs, cx| runs.dismiss(id, cx));
                                this.state.update(cx, |state, cx| {
                                    state.start_imports(vec![request], &this.runs, cx)
                                });
                            }
                        })),
                )
            })
            .children(open)
    }

    /// `transcripts_unknown`: the transcript search failed, so only the
    /// title, people and tag matches are known.
    fn empty_state(
        &self,
        filtering: bool,
        transcripts_unknown: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        if filtering || transcripts_unknown {
            let query = self.state.read(cx).search().query().text.clone();
            let filters: Vec<_> = self.filter_values().into_iter().flatten().collect();
            let heading = query.as_ref().map_or_else(
                || "No meetings match these filters".to_owned(),
                |query| format!("No results for “{query}”"),
            );
            let description = div()
                .id("library-no-match-description")
                .flex()
                .flex_col()
                .gap_1()
                .when(transcripts_unknown, |el| {
                    el.child(text_line(
                        "library-no-match",
                        Role::Status,
                        no_match_message(true),
                    ))
                })
                .when(!filters.is_empty(), |el| {
                    el.child(text_line(
                        "library-no-match-filters",
                        Role::Label,
                        format!("Filtered by {}.", filters.join(" and ")),
                    ))
                });
            let clear_search = query.is_some().then(|| {
                Button::new("library-clear-search")
                    .label("Clear search")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.search_input
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        this.search_now(cx);
                        this.focus_search(window, cx);
                    }))
            });
            let clear_filters = (!filters.is_empty())
                .then(|| self.clear_filters_button("library-clear-no-match-filters", cx));
            let (primary, secondary) = if clear_search.is_some() {
                (clear_search, clear_filters)
            } else {
                (clear_filters, None)
            };
            return kit::empty_state(
                "library-no-match-title",
                heading,
                description,
                false,
                primary,
                secondary,
                cx,
            )
            .into_any_element();
        }
        let ready = self.daemon_connected;
        // Always the primary action here (it is only offered when the agent
        // is connected), so its record dot is in the label colour.
        let words = "Start recording";
        let start = Button::new("library-start-recording")
            .accessibility_label(words)
            .child(kit::record_dot(true, false, cx))
            .child(words)
            .on_click(|_, window, cx| {
                window.dispatch_action(Box::new(crate::menu::BeginRecording), cx);
            });
        let choose = Button::new("library-choose")
            .debug_selector(|| "library-choose".into())
            .label("Import recordings…")
            .on_click(cx.listener(|this, _, _, cx| this.choose_recordings(cx)));
        let (primary, secondary) = if ready {
            (start, Some(choose))
        } else {
            (choose, None)
        };
        div()
            .id("library-drop-zone")
            .role(Role::Group)
            .aria_label("No meetings yet")
            .flex()
            .flex_col()
            .flex_grow_1()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .gap_3()
            .m_4()
            .p_8()
            .rounded(theme.radius_lg)
            .border_2()
            .border_dashed()
            .border_color(theme.input)
            .text_color(theme.muted_foreground)
            .child(kit::empty_state(
                "library-empty-title",
                "No meetings yet",
                text_line(
                    "library-empty-drop-hint",
                    Role::Label,
                    "Record a call or import recordings you already have. You can also drop files or folders here.",
                ),
                true,
                Some(primary),
                secondary,
                cx,
            ))
            .when_some(self.transcription_block.clone(), |el, words| {
                el.child(kit::status_line(
                    "library-transcription-blocked",
                    kit::StatusKind::Attention,
                    words,
                    cx,
                ))
                .child(
                    Button::new("library-download-settings")
                        .label("Open settings")
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.state.update(cx, |_, cx| {
                                cx.emit(crate::state::AppEvent::OpenSettings(Some(
                                    crate::state::SettingsPage::Transcription,
                                )));
                            })
                        })),
                )
            })
            .into_any_element()
    }

    fn repair_index(&mut self, cx: &mut Context<Self>) {
        if self
            .index_repair
            .as_ref()
            .is_some_and(|(kind, _)| *kind == kit::StatusKind::Pending)
        {
            return;
        }
        self.index_repair = Some((kit::StatusKind::Pending, "Rebuilding the index…".into()));
        cx.notify();
        let task = self.state.read(cx).run_cli(
            ["library", "repair", "--only", "index", "--yes"]
                .into_iter()
                .map(Into::into)
                .collect(),
            ChildKind::Command,
            cx,
        );
        cx.spawn(async move |view, cx| {
            let result = task.await;
            let _ = view.update(cx, |view, cx| {
                view.index_repair = Some(match result {
                    Ok(_) => {
                        view.state.update(cx, |state, cx| state.refresh_all(cx));
                        (kit::StatusKind::Done, "Index rebuilt".into())
                    }
                    Err(error) => (
                        kit::StatusKind::Failed,
                        failure_text("Couldn't rebuild the index.", &error.user_text(), ""),
                    ),
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn index_attention(&self, missing: bool, cx: &Context<Self>) -> Div {
        let (label, words) = if missing {
            (
                "Rebuild index",
                "The library index is missing. Your meetings are safe in their folders.",
            )
        } else {
            (
                "Repair index",
                "The library index needs repair. Your meetings are safe in their folders.",
            )
        };
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_2()
            .child(kit::status_line(
                "library-index-attention",
                kit::StatusKind::Attention,
                words,
                cx,
            ))
            .child(
                kit::button(
                    "library-index-repair",
                    label,
                    !self
                        .index_repair
                        .as_ref()
                        .is_some_and(|(kind, _)| *kind == kit::StatusKind::Pending),
                    cx,
                    |this, _, cx| this.repair_index(cx),
                )
                .ghost(),
            )
    }

    fn render_row(
        &self,
        row: usize,
        hits: &[MeetingHit],
        window: &Window,
        cx: &Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let searching = self.state.read(cx).search().query().text.is_some();
        match self.rows.get(row) {
            Some(LibraryRow::Day(label)) => crate::ui::heading(("library-day", row), label.clone())
                .h(rems(HEADER_REMS))
                .w_full()
                .flex()
                .items_end()
                .px_2()
                .pb_1()
                .text_role(TextRole::Caption)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.muted_foreground)
                .into_any_element(),
            Some(LibraryRow::Meeting(i)) if *i < hits.len() => {
                let hit = &hits[*i];
                let selected = self.selected == Some(row);
                let title =
                    broscribe_ipc::recording_words::meeting_title(hit.title.as_deref()).to_owned();
                let menu_view = cx.weak_entity();
                let menu_id = hit.meeting_id.clone();
                let open_id = hit.meeting_id.clone();
                let age = (Local::now().date_naive()
                    - hit.started_at.with_timezone(&Local).date_naive())
                .num_days();
                let time = if !searching && (0..=1).contains(&age) {
                    local_time(&hit.started_at)
                } else {
                    local_date_time(&hit.started_at)
                };
                let people = (hit.participant_count > 0)
                    .then(|| count(hit.participant_count as u64, "person", "people"));
                let search = self.state.read(cx).search();
                let transcript = search.transcript_match(&hit.meeting_id);
                let metadata = hit
                    .snippet
                    .as_deref()
                    .filter(|_| transcript.is_none())
                    .map(plain_snippet);
                let person = search
                    .query()
                    .person
                    .as_deref()
                    .map(|id| self.choices.person_name(id))
                    .or_else(|| hit.matched_person.clone())
                    .map(|name| format!("with {name}"));
                let app = match hit.source {
                    Source::Import => Some("Imported file".to_owned()),
                    Source::Recording => hit.app.as_deref().map(|app| self.choices.app_name(app)),
                };
                let facts = [
                    Some(time),
                    Some(length(hit.duration_s)),
                    person.clone(),
                    people.clone(),
                    app.clone(),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
                let flag = self.row_flag(hit);
                let metadata_label = match (hit.metadata_match, metadata) {
                    (Some(MetadataMatch::Title), _) => Some("Matched in title".to_owned()),
                    (Some(MetadataMatch::Tags), Some(text)) => {
                        Some(format!("Matched in tags: {text}"))
                    }
                    (Some(MetadataMatch::People), text) => hit
                        .matched_person
                        .clone()
                        .or(text)
                        .map(|name| format!("Matched in people: {name}")),
                    _ => None,
                };
                let match_label = transcript
                    .map(|hit| {
                        format!(
                            "match in transcript: {} at {}, {}",
                            hit.speaker,
                            spoken_position(hit.start),
                            plain_snippet(&hit.snippet)
                        )
                    })
                    .or_else(|| metadata_label.clone());
                let mut label = format!(
                    "{title}, {}, {}",
                    spoken_date_time(&hit.started_at),
                    spoken_length(hit.duration_s)
                );
                for fact in [
                    person,
                    people,
                    app,
                    flag.as_ref().map(|(words, _)| words.clone()),
                    match_label,
                ]
                .into_iter()
                .flatten()
                {
                    label.push_str(", ");
                    label.push_str(&fact);
                }
                let caption = kit::row_metadata_colour(selected, cx);
                kit::list_row(
                    ("library-meeting", row),
                    selected,
                    self.focus.contains_focused(window, cx),
                    cx,
                )
                .role(Role::ListItem)
                .aria_label(label)
                .aria_position_in_set(self.positions[row])
                .aria_size_of_set(self.set_size)
                .when(selected && self.focus.contains_focused(window, cx), |row| {
                    row.aria_active_descendant()
                })
                .h(rems(if searching { MATCH_REMS } else { ROW_REMS }))
                .flex_col()
                .justify_center()
                .gap_0p5()
                .px_3()
                .tooltip(kit::title_tooltip(title.clone()))
                .on_click(
                    cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        if this.row_meeting(row, cx).as_ref() == Some(&open_id) {
                            this.focus.focus(window, cx);
                            this.open_row(row, event.click_count() > 1, window, cx);
                        }
                    }),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .min_w_0()
                        .child(kit::row_body(title, None::<String>, selected, cx))
                        .when_some(flag, |line, (words, severity)| {
                            let color = match severity {
                                kit::StatusKind::Failed => theme.danger,
                                kit::StatusKind::Attention => theme.warning,
                                _ => caption,
                            };
                            line.child(kit::row_trailing(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .max_w(rems(11.))
                                    .text_role(TextRole::Caption)
                                    .text_color(if selected {
                                        kit::row_metadata_colour(true, cx)
                                    } else {
                                        color
                                    })
                                    .child(kit::glyph(severity.glyph_path(), color))
                                    .child(div().truncate().child(words)),
                            ))
                        }),
                )
                .child(
                    tabular(div())
                        .truncate()
                        .text_role(TextRole::Caption)
                        .text_color(caption)
                        .child(facts),
                )
                .when(searching, |row| {
                    let line = tabular(div())
                        .truncate()
                        .text_role(TextRole::Caption)
                        .text_color(caption);
                    row.child(if let Some(hit) = transcript {
                        line.child(highlighted_snippet(
                            &format!("{} · {} · “", hit.speaker, format_clock(hit.start)),
                            &format!("{}”", hit.snippet),
                            cx,
                        ))
                    } else {
                        line.children(metadata_label)
                    })
                })
                .when(
                    matches!(self.rows.get(row + 1), Some(LibraryRow::Meeting(_))),
                    |row| {
                        row.child(
                            div()
                                .absolute()
                                .bottom_0()
                                .left_3()
                                .right_3()
                                .border_b_1()
                                .border_color(theme.border),
                        )
                    },
                )
                .context_menu(move |menu, _, _| {
                    Self::row_menu_items(menu, menu_view.clone(), menu_id.clone())
                })
                .into_any_element()
            }
            Some(LibraryRow::Now) => self.render_now(row, window, cx).into_any_element(),
            _ => div().h(rems(ROW_REMS)).into_any_element(),
        }
    }

    fn render_now(&self, row: usize, window: &Window, cx: &Context<Self>) -> Stateful<Div> {
        let now = self
            .now
            .as_ref()
            .expect("Now is projected from this recording");
        let session = now.session.clone();
        let selected = self.selected == Some(row);
        let focused = self.focus.contains_focused(window, cx);
        let app = now.app.as_deref().map(|app| self.choices.app_name(app));
        let label = [
            Some(now.title.as_str()),
            Some(now.phase),
            app.as_deref(),
            now.stop_reason,
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        let color = kit::row_metadata_colour(selected, cx);
        let glyph = match now.phase {
            "Paused" => crate::ui::icons::icon(crate::ui::icons::PAUSE).into_any_element(),
            "Stopped early" => {
                kit::glyph(kit::StatusKind::Attention.glyph_path(), cx.theme().warning)
                    .into_any_element()
            }
            _ => div()
                .size(rems(0.5))
                .rounded_full()
                .bg(cx.theme().danger)
                .into_any_element(),
        };
        kit::list_row("library-now", selected, focused, cx)
            .role(Role::ListItem)
            .aria_label(label)
            .aria_position_in_set(1)
            .aria_size_of_set(self.set_size)
            .when(selected && focused, |row| row.aria_active_descendant())
            .h(rems(ROW_REMS))
            .flex_col()
            .justify_center()
            .gap_0p5()
            .px_3()
            .child(div().min_w_0().child(kit::row_body(
                now.title.clone(),
                None::<String>,
                selected,
                cx,
            )))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_role(TextRole::Caption)
                    .text_color(color)
                    .child(glyph)
                    .child(now.phase)
                    .children(app.map(|name| div().truncate().child(format!("· {name}")))),
            )
            .when_some(now.stop_reason, |row, reason| {
                row.tooltip(kit::title_tooltip(reason))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                if this.now.as_ref().is_some_and(|now| now.session == session) {
                    this.focus.focus(window, cx);
                    this.open_row(row, true, window, cx);
                }
            }))
    }

    fn row_flag(&self, hit: &MeetingHit) -> Option<(String, kit::StatusKind)> {
        use kit::StatusKind::{Attention, Failed, Pending};
        match hit.processing_status {
            ProcessingStatus::Queued => Some(("Queued".into(), Pending)),
            ProcessingStatus::Analyzed => Some(("Identifying speakers…".into(), Pending)),
            ProcessingStatus::Provisional => Some(("Draft transcript".into(), Pending)),
            ProcessingStatus::Failed => Some(("Couldn't transcribe".into(), Failed)),
            ProcessingStatus::Done => self.choices.naming.get(&hit.meeting_id).map(|n| {
                (
                    count(*n as u64, "speaker needs a name", "speakers need a name"),
                    Attention,
                )
            }),
        }
    }

    fn measure_rows(&mut self, window: &Window, cx: &App) {
        let search = self.state.read(cx).search();
        let key = (window.rem_size(), search.query().text.is_some());
        if self.measured == Some(key) {
            return;
        }
        self.measured = Some(key);
        self.sizes = Rc::new(
            self.rows
                .iter()
                .map(|row| {
                    let height = match row {
                        LibraryRow::Day(_) => HEADER_REMS,
                        LibraryRow::Meeting(_) if key.1 => MATCH_REMS,
                        LibraryRow::Meeting(_) | LibraryRow::Now => ROW_REMS,
                    };
                    size(px(0.), key.0 * height)
                })
                .collect(),
        );
    }

    fn meeting_list(&self, cx: &mut Context<Self>) -> Stateful<Div> {
        let list = v_virtual_list(
            cx.entity(),
            "library-meetings-list",
            self.sizes.clone(),
            |this, range, window, cx| {
                let search = this.state.read(cx).search();
                if range.end >= this.rows.len().saturating_sub(PAGE_THRESHOLD)
                    && search.has_more()
                    && search.loading().is_none()
                    && search.error().is_none()
                    && search.transcript_error().is_none()
                {
                    let view = cx.weak_entity();
                    let generation = search.generation();
                    cx.defer(move |cx| {
                        let _ = view.update(cx, |view, cx| {
                            if view.state.read(cx).search().generation() == generation {
                                view.load_more(cx);
                            }
                        });
                    });
                }
                let hits = this.state.read(cx).search().results();
                range
                    .map(|row| this.render_row(row, hits, window, cx))
                    .collect()
            },
        )
        .track_scroll(&self.scroll)
        .size_full();
        div()
            .id("library-meetings")
            .role(Role::List)
            .aria_label("Meetings")
            .track_focus(&self.focus.clone().tab_index(0).tab_stop(true))
            .key_context("LibraryList")
            .flex_1()
            .min_h_0()
            .on_key_down(cx.listener(Self::on_list_key))
            .on_action(cx.listener(|this, _: &kit::OpenRowMenu, window, cx| {
                this.open_row_menu(window, cx);
                cx.stop_propagation();
            }))
            .on_action(
                cx.listener(|this, _: &crate::menu::RenameMeeting, window, cx| {
                    if let Some(id) = this.selected_id.clone() {
                        this.run_row_command(&id, RowCommand::Rename, window, cx);
                    }
                    cx.stop_propagation();
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::menu::DeleteMeeting, window, cx| {
                    if let Some(id) = this.selected_id.clone() {
                        this.run_row_command(&id, RowCommand::Delete, window, cx);
                    }
                    cx.stop_propagation();
                }),
            )
            .child(list)
    }

    fn initial_placeholders(&self, cx: &App) -> impl IntoElement {
        let muted = cx.theme().muted;
        div()
            .id("library-loading-placeholders")
            .debug_selector(|| "library-loading-placeholders".into())
            .flex()
            .flex_col()
            .min_h_0()
            .overflow_hidden()
            .children((0..6).map(|_| {
                div()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap_2()
                    .w_full()
                    .h(rems(ROW_REMS))
                    .px_3()
                    .child(
                        div()
                            .w(gpui::relative(0.4))
                            .h(rems(0.625))
                            .rounded_full()
                            .bg(muted),
                    )
                    .child(
                        div()
                            .w(gpui::relative(0.65))
                            .h(rems(0.5))
                            .rounded_full()
                            .bg(muted),
                    )
            }))
    }
}

impl Render for LibraryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.measure_rows(window, cx);
        let muted_foreground = cx.theme().muted_foreground;
        let (
            count,
            error,
            transcript_error,
            empty,
            loading,
            more,
            page_failed,
            index_missing,
            searching_transcripts,
        ) = {
            let search = self.state.read(cx).search();
            let count = search.results().len();
            (
                count,
                search.failure().cloned(),
                search.transcript_error().cloned(),
                count == 0 && matches!(search.loading(), None | Some(Load::Refresh)),
                search.loading(),
                search.has_more(),
                search.page_failed(),
                search.index_missing(),
                search.searching_transcripts(),
            )
        };
        // The input entity changes on each key. The list only depends on the
        // query submitted by the debounce and on immediate filter choices.
        let filtering =
            self.state.read(cx).search().query().text.is_some() || self.filters.is_filtering();
        let empty_library = self.empty_library(cx);
        let toolbar =
            (!self.embedded).then(|| self.toolbar_controls(self.compact, true, empty_library, cx));
        let imports = self.import_rows(cx);
        let needs_repair = error
            .as_ref()
            .is_some_and(|error| error.code == "index_repair");
        let body = if error.is_none() && empty && self.now.is_none() && !index_missing {
            div()
                .id("library-empty-scroll")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(self.empty_state(filtering, transcript_error.is_some(), cx))
                .into_any_element()
        } else if self.show_loading && self.now.is_none() {
            self.initial_placeholders(cx).into_any_element()
        } else {
            self.meeting_list(cx).into_any_element()
        };
        let summary_words = if filtering && (self.show_loading || count > 0) {
            let mut words = summary(count, loading, more);
            if searching_transcripts && loading != Some(Load::Search) {
                words.push_str(" · searching transcripts…");
            }
            Some(words)
        } else if self.show_loading {
            Some("Loading meetings…".into())
        } else {
            None
        };
        let summary_node = summary_words.map(|words| {
            text_line("library-summary", Role::Status, words)
                .text_role(TextRole::Caption)
                .text_color(muted_foreground)
        });
        let filters_button = (!empty_library).then(|| self.filters_button(cx));
        let header = filters_button.map(|button| {
            div()
                .id("library-list-header")
                .flex()
                .items_center()
                .justify_between()
                .min_h(rems(2.75))
                .px_3()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(div().flex_1().children(summary_node))
                .child(button)
        });
        let row_menu = self.row_menu.as_ref().map(|open| {
            let menu = open.menu.clone();
            deferred(
                anchored()
                    .position(open.position)
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        div()
                            .id("library-keyboard-row-menu")
                            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                                this.close_row_menu(window, cx);
                            }))
                            .child(menu),
                    ),
            )
            .with_priority(gpui::base::POPUP_PRIORITY)
        });
        div()
            .id("library")
            .role(Role::Main)
            .aria_label("Library")
            .track_focus(&self.view_focus)
            // A click on blank space keeps focus where it is (the list or
            // a field) instead of moving it to the root; children that take
            // focus on a click have done so before this runs.
            .on_any_mouse_down(|_, window, _| window.prevent_default())
            .on_action(cx.listener(|this, _: &ImportRecordings, _, cx| this.choose_recordings(cx)))
            .on_action(cx.listener(|this, _: &ImportFolder, _, cx| this.choose_folder(cx)))
            .flex()
            .flex_col()
            .flex_1()
            .size_full()
            .gap_2()
            .p_4()
            .when(!self.embedded, |el| {
                el.child(title("library-title", "Library"))
            })
            .children(toolbar)
            .children(header)
            .children(
                self.state
                    .read(cx)
                    .meeting_deletion()
                    .map(|(text, attention)| {
                        kit::status_line(
                            "library-deleted-meeting",
                            if attention {
                                kit::StatusKind::Attention
                            } else {
                                kit::StatusKind::Done
                            },
                            text.to_owned(),
                            cx,
                        )
                    }),
            )
            .children(self.filter_tokens(cx))
            .when(self.filters_cleared, |view| {
                view.child(crate::ui::kit::status_line(
                    "library-filters-cleared",
                    crate::ui::kit::StatusKind::Info,
                    "Filters cleared to show this meeting.",
                    cx,
                ))
            })
            .children(imports)
            .when(index_missing || needs_repair, |el| {
                el.child(self.index_attention(index_missing, cx))
            })
            .when_some(self.index_repair.as_ref(), |el, (kind, text)| {
                el.child(kit::status_line(
                    "library-index-status",
                    *kind,
                    text.clone(),
                    cx,
                ))
            })
            .when_some(
                error.as_ref().filter(|_| !page_failed && !needs_repair),
                |el, error| {
                    el.child(kit::retry_line_with_id(
                        "library-search-error",
                        "library-search-retry",
                        search_failure(&sentence(error.user_text())),
                        cx,
                        |this, window, cx| {
                            this.retry_search(cx);
                            this.focus_search(window, cx);
                        },
                    ))
                },
            )
            .when_some(transcript_error, |el, transcript_error| {
                // Metadata results are usable even if the transcript page failed.
                el.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(kit::CONTROL_GAP)
                        .child(kit::status_line(
                            "library-transcript-error",
                            kit::StatusKind::Attention,
                            transcript_error_message(
                                &transcript_error.message,
                                transcript_error.retry_more,
                            ),
                            cx,
                        ))
                        .child(
                            kit::button(
                                "library-transcript-retry",
                                "Try again",
                                true,
                                cx,
                                |this: &mut Self, _, cx| this.retry_transcripts(cx),
                            )
                            .debug_selector(|| "library-transcript-retry".into())
                            .small()
                            .ghost()
                            .accessibility_label("Try again"),
                        ),
                )
            })
            .child(body)
            .when_some(
                error.filter(|_| page_failed && !needs_repair && loading != Some(Load::More)),
                |el, error| {
                    el.child(kit::retry_line_with_id(
                        "library-page-error",
                        "library-page-retry",
                        failure_text("Couldn't load more meetings.", error.user_text(), ""),
                        cx,
                        |this, window, cx| {
                            this.load_more(cx);
                            this.focus_list(window, cx);
                        },
                    ))
                },
            )
            .when(loading == Some(Load::More), |el| {
                el.child(
                    text_line(
                        "library-page-loading",
                        Role::Status,
                        "Loading more meetings…",
                    )
                    .text_role(TextRole::Caption),
                )
            })
            .children(row_menu)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, FixedOffset};
    use gpui::px;

    use super::*;

    fn hit(id: &str, at: DateTime<FixedOffset>) -> MeetingHit {
        MeetingHit {
            meeting_id: MeetingId::parse(id).unwrap(),
            title: Some(id.into()),
            started_at: at,
            duration_s: 60.0,
            tags: Vec::new(),
            snippet: None,
            processing_status: ProcessingStatus::Done,
            participant_count: 0,
            metadata_match: None,
            matched_person: None,
            source: Source::Import,
            app: None,
        }
    }

    fn local(y: i32, m: u32, d: u32, h: u32) -> DateTime<FixedOffset> {
        Local
            .with_ymd_and_hms(y, m, d, h, 0, 0)
            .earliest()
            .unwrap()
            .fixed_offset()
    }

    #[test]
    fn groups_meetings_by_local_day() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 14).unwrap();
        let hits = [
            hit("m-00000a", local(2026, 11, 14, 9)),
            hit("m-00000b", local(2026, 11, 14, 8)),
            hit("m-00000c", local(2026, 11, 13, 15)),
            hit("m-00000d", local(2026, 11, 10, 10)),
            hit("m-00000e", local(2026, 10, 28, 10)),
            hit("m-00000f", local(2026, 10, 8, 10)),
            hit("m-000007", local(2025, 11, 8, 10)),
        ];
        let rows = group_by_day(&hits, today);
        assert_eq!(
            rows,
            vec![
                LibraryRow::Day("Today".into()),
                LibraryRow::Meeting(0),
                LibraryRow::Meeting(1),
                LibraryRow::Day("Yesterday".into()),
                LibraryRow::Meeting(2),
                LibraryRow::Day("Previous 7 days".into()),
                LibraryRow::Meeting(3),
                LibraryRow::Day("Previous 30 days".into()),
                LibraryRow::Meeting(4),
                LibraryRow::Day("October".into()),
                LibraryRow::Meeting(5),
                LibraryRow::Day("November 2025".into()),
                LibraryRow::Meeting(6),
            ]
        );
        assert!(group_by_day(&[], today).is_empty());
    }

    #[test]
    fn keyboard_navigation_skips_day_headers() {
        let rows = vec![
            LibraryRow::Day("Today".into()),
            LibraryRow::Meeting(0),
            LibraryRow::Day("Yesterday".into()),
            LibraryRow::Meeting(1),
        ];
        assert_eq!(next_meeting_row(&rows, None, true), Some(1));
        assert_eq!(next_meeting_row(&rows, Some(1), true), Some(3));
        assert_eq!(next_meeting_row(&rows, Some(3), true), None);
        assert_eq!(next_meeting_row(&rows, Some(3), false), Some(1));
        assert_eq!(next_meeting_row(&rows, Some(1), false), None);
        assert_eq!(next_meeting_row(&rows, None, false), Some(3));
        assert_eq!(next_meeting_row(&[], None, true), None);
    }

    #[test]
    fn blank_filters_are_no_filter() {
        let filters = LibraryFilters {
            text: "  ".into(),
            person: " Ana ".into(),
            tag: String::new(),
            app: Some(" ".into()),
            source: None,
            date: None,
            order: MeetingOrder::Newest,
        };
        let q = filters.query();
        assert_eq!(q.text, None);
        assert_eq!(q.person.as_deref(), Some("Ana"));
        assert_eq!(q.tag, None);
        assert_eq!(q.app, None);
        assert!(filters.is_filtering());
        assert!(!LibraryFilters::default().is_filtering());
        let by_source = LibraryFilters {
            source: Some(Source::Recording),
            ..LibraryFilters::default()
        };
        assert_eq!(by_source.query().source, Some(Source::Recording));
        assert!(by_source.is_filtering());
    }

    #[test]
    fn date_filters_bound_whole_local_days() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 23).unwrap();
        assert_eq!(DateFilter::Week.dates(today).0.day(), 17);
        assert_eq!(DateFilter::Month.dates(today).0.day(), 25);
        assert_eq!(DateFilter::Year.dates(today).0.ordinal(), 1);
        let (since, until) = DateFilter::Custom(today, today).bounds(today);
        assert_eq!(since.unwrap().with_timezone(&Local).date_naive(), today);
        assert_eq!(
            until.unwrap().with_timezone(&Local).date_naive(),
            today.succ_opt().unwrap()
        );
    }

    #[gpui::test]
    fn library_filters_use_index_choices_and_remove_tokens(cx: &mut gpui::TestAppContext) {
        use broscribe_core::index::PersonSummary;
        use gpui::component::searchable_list::SearchableListItem as _;

        let (_dir, lib, state, window, view, ids) = setup_library_test(cx, &["Synthetic sync"]);
        let id = &ids[0];
        let person_id = broscribe_core::PersonId::random();
        let mut meeting = lib.load_meeting(id).unwrap();
        meeting.app = Some("teams".into());
        meeting.tags = vec!["synthetic".into()];
        meeting.processing.status = ProcessingStatus::Failed;
        meeting
            .participants
            .push(broscribe_core::schema::Participant {
                person_id: Some(person_id.clone()),
                name: Some("Évora Sample".into()),
                ..Default::default()
            });
        broscribe_core::ops::with_index(&lib, |tx, index| {
            let mut person = broscribe_core::schema::Person::new(person_id.clone(), "Évora Sample");
            person.aliases.push("Tavi".into());
            person.emails.push("tavi@example.invalid".into());
            tx.save_person(&person)?;
            index.upsert_person(tx, &person)?;
            tx.save_meeting(&meeting)?;
            index.upsert_meeting(tx, &meeting)
        })
        .unwrap();
        state.update(cx, |state, cx| state.refresh_all(cx));
        cx.run_until_parked();
        window
            .update(cx, |view, _, cx| {
                let index = Index::open_readonly(lib.root()).unwrap();
                let people: Vec<PersonSummary> = index.list_people().unwrap();
                assert_eq!(people[0].id, person_id);
                assert_eq!(index.meeting_tags().unwrap(), [("synthetic".into(), 1)]);
                assert_eq!(view.choices.app_name("teams"), "Microsoft Teams");
                assert_eq!(view.choices.app_name("quartz"), "Quartz");
                let person = &view.choices.people[0];
                assert!(person.matches("evora tavi"));
                assert!(person.matches("example.invalid"));
                assert_eq!(person.value, person_id.as_str());
                assert_eq!(view.choices.tags[0].label.as_ref(), "synthetic, 1 meeting");
                view.filters.person = person_id.to_string();
                view.filters.tag = "synthetic".into();
                view.filters.date = Some(DateFilter::Today);
                view.filters.app = Some("teams".into());
                view.filters.source = Some(Source::Import);
                view.search_now(cx);
            })
            .unwrap();
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.update(|window, cx| {
            let search = state.read(cx).search();
            assert_eq!(search.query().person.as_deref(), Some(person_id.as_str()));
            assert_eq!(search.query().app.as_deref(), Some("teams"));
            assert_eq!(search.query().tag.as_deref(), Some("synthetic"));
            assert_eq!(search.query().source, Some(Source::Import));
            assert!(search.query().since.is_some() && search.query().until.is_some());
            assert_eq!(search.results()[0].participant_count, 1);
            assert_eq!(
                search.results()[0].processing_status,
                ProcessingStatus::Failed
            );
            let participant_hit = Index::open_readonly(lib.root())
                .unwrap()
                .search_meetings(&SearchQuery {
                    text: Some("evora".into()),
                    ..Default::default()
                })
                .unwrap();
            assert_eq!(
                participant_hit[0].matched_person.as_deref(),
                Some("Évora Sample")
            );
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            tree.find("library-import-button");
            tree.find("library-filter-tokens");
            assert_eq!(tree.read(tree.find("library-summary")).1, "1 match");
            assert_eq!(
                tree.read(tree.find("library-filter-token-0")).1,
                "Remove filter Person: Évora Sample"
            );
            let (_, row) = tree.read(tree.find("library-meeting-1"));
            assert!(
                row.contains("with Évora Sample, 1 person, Imported file, Couldn't transcribe"),
                "{row}"
            );
        });
        let trigger = visual.debug_bounds("library-open-filters").unwrap();
        visual.simulate_click(trigger.center(), gpui::Modifiers::none());
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            for (id, title) in [
                ("person", "Person"),
                ("tag", "Tag"),
                ("app", "App"),
                ("source", "Source"),
                ("date", "Date"),
            ] {
                assert_eq!(tree.read(tree.find(&format!("filter-{id}-label"))).1, title);
            }
        });
        visual.simulate_keystrokes("escape");
        visual.refresh().unwrap();
        let remove = visual.debug_bounds("library-filter-token-0").unwrap();
        visual.simulate_click(remove.center(), gpui::Modifiers::none());
        visual.run_until_parked();
        assert!(state.read_with(&visual, |state, _| state.search().query().person.is_none()));
        visual.refresh().unwrap();
        let clear = visual.debug_bounds("clear-library-filters").unwrap();
        visual.simulate_click(clear.center(), gpui::Modifiers::none());
        visual.run_until_parked();
        assert!(!view.read_with(&visual, |view, cx| view.filters(cx).is_filtering()));
    }

    #[gpui::test]
    fn library_empty_refresh_keeps_its_content_and_toolbar(cx: &mut gpui::TestAppContext) {
        let (_dir, _lib, state, window, view, _) = setup_library_test(cx, &[]);
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.update(|window, cx| {
            state.update(cx, |state, cx| state.rerun_search(cx));
            assert_eq!(state.read(cx).search().loading(), Some(Load::Refresh));
            assert!(view.read(cx).empty_library(cx));
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            tree.find("library-empty-title");
            tree.find("library-import-button");
            assert!(tree.all("library-open-filters").is_empty());
            assert!(tree.all("library-summary").is_empty());
            assert!(!view.read(cx).show_loading);
        });
        visual.run_until_parked();
        assert!(view.read_with(&visual, |view, cx| view.empty_library(cx)));
        state.update(&mut visual, |state, cx| {
            let watcher = broscribe_platform::fakes::FakeLibraryWatcher::default();
            state.attach_watcher(&watcher, cx).unwrap();
            let generation = state.search().generation();
            state.window_activated(cx);
            assert_eq!(state.search().generation(), generation);
        });
    }

    #[gpui::test]
    fn first_load_shows_static_placeholders_only_after_three_hundred_ms(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_dir, lib, state, window, view, _) = setup_library_test(cx, &[]);
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let job = state.update(&mut visual, |state, cx| state.hold_search_for_test(cx));
        visual.run_until_parked();
        let executor = visual.update(|_, cx| cx.background_executor().clone());
        executor.advance_clock(Duration::from_millis(299));
        visual.refresh().unwrap();
        assert!(
            visual
                .debug_bounds("library-loading-placeholders")
                .is_none()
        );
        executor.advance_clock(Duration::from_millis(1));
        visual.run_until_parked();
        visual.refresh().unwrap();
        assert!(
            visual
                .debug_bounds("library-loading-placeholders")
                .is_some()
        );
        assert!(named_nodes(&mut visual, window, &view).contains(&(
            "library-summary".into(),
            Role::Status,
            "Loading meetings…".into(),
        )));
        let generation = job.generation();
        let outcome = state.read_with(&visual, |state, _| job.run(lib.root(), state.cli()));
        state.update(&mut visual, |state, cx| {
            state.apply_search(generation, outcome, cx)
        });
        visual.run_until_parked();
        visual.refresh().unwrap();
        assert!(
            visual
                .debug_bounds("library-loading-placeholders")
                .is_none()
        );
    }

    #[gpui::test]
    fn library_typing_waits_for_the_debounce_before_replacing_rows(cx: &mut gpui::TestAppContext) {
        let (_dir, _lib, state, window, view, ids) =
            setup_library_test(cx, &["Zephyr review", "Synthetic orbit"]);
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let generation = state.read_with(&visual, |state, _| state.search().generation());
        visual.update(|window, cx| view.update(cx, |view, cx| view.focus_search(window, cx)));
        visual.refresh().unwrap();
        visual.simulate_input("zep");
        visual.run_until_parked();
        let executor = visual.update(|_, cx| cx.background_executor().clone());
        executor.advance_clock(Duration::from_millis(100));
        visual.simulate_input("hyr");
        visual.run_until_parked();
        executor.advance_clock(SEARCH_DEBOUNCE - Duration::from_millis(1));
        visual.run_until_parked();
        state.read_with(&visual, |state, _| {
            assert_eq!(state.search().generation(), generation);
            assert_eq!(state.search().results().len(), 2);
        });
        executor.advance_clock(Duration::from_millis(1));
        visual.run_until_parked();
        state.read_with(&visual, |state, _| {
            assert_eq!(state.search().generation(), generation + 1);
            assert_eq!(state.search().query().text.as_deref(), Some("zephyr"));
            assert_eq!(state.search().results()[0].meeting_id, ids[0]);
        });
    }

    #[gpui::test]
    fn library_keyboard_uses_one_list_stop_and_variable_sizes(cx: &mut gpui::TestAppContext) {
        let titles = vec!["Synthetic session"; 120];
        let (_dir, _lib, state, window, view, _) = setup_library_test(cx, &titles);
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_resize(size(px(600.), px(560.)));
        visual.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.measure_rows(window, cx);
                let old = view.sizes[1].height;
                window.set_rem_size(window.rem_size() * 1.5);
                view.measure_rows(window, cx);
                assert_eq!(view.sizes[1].height, old * 1.5);
                assert!(view.sizes[0].height < view.sizes[1].height);
                view.focus_search(window, cx);
            });
        });
        visual.refresh().unwrap();
        visual.simulate_keystrokes("down");
        assert_eq!(view.read_with(&visual, |view, _| view.selected), Some(1));
        let selection = |window: &mut Window, cx: &mut App| {
            crate::ui::a11y_tree::draw_and_read(window, cx);
            let tree: serde_json::Value =
                serde_json::from_str(&window.debug_a11y_tree_json().unwrap()).unwrap();
            let active = tree["active_descendant_focus"].as_str().unwrap();
            (
                tree["nodes"][active]["aria"]["position_in_set"]
                    .as_u64()
                    .unwrap(),
                tree["nodes"][active]["aria"]["size_of_set"]
                    .as_u64()
                    .unwrap(),
            )
        };
        assert_eq!(visual.update(selection), (1, 100));
        visual.simulate_keystrokes("down");
        assert_eq!(visual.update(selection), (2, 100));
        visual.simulate_keystrokes("up");
        assert_eq!(visual.update(selection), (1, 100));
        visual.simulate_keystrokes("pagedown");
        assert!(view.read_with(&visual, |view, _| view.selected.unwrap()) > 2);
        visual.simulate_keystrokes("pageup home down up");
        assert_eq!(view.read_with(&visual, |view, _| view.selected), Some(1));
        visual.update(|window, cx| {
            assert!(view.focus_handle(cx).is_focused(window));
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            tree.find("library-meetings");
            assert!(
                tree.nodes
                    .iter()
                    .filter(|node| node.role == "ListItem")
                    .count()
                    < 30
            );
        });
        visual.simulate_keystrokes("escape");
        visual.update(|window, cx| assert!(view.read(cx).search_has_focus(window, cx)));
        visual.simulate_keystrokes("enter end");
        visual.run_until_parked();
        visual.refresh().unwrap();
        visual.run_until_parked();
        assert_eq!(
            state.read_with(&visual, |state, _| state.search().results().len()),
            120
        );
    }

    #[test]
    fn snippet_markers_are_removed() {
        assert_eq!(plain_snippet("the **budget** review"), "the budget review");
    }

    #[test]
    fn rows_are_found_by_meeting_and_the_summary_names_paging() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 14).unwrap();
        let hits = [
            hit("m-00000a", local(2026, 11, 14, 9)),
            hit("m-00000b", local(2026, 11, 13, 8)),
        ];
        let rows = group_by_day(&hits, today);
        let id = |s: &str| MeetingId::parse(s).unwrap();
        assert_eq!(meeting_row(&rows, &hits, &id("m-00000a")), Some(1));
        assert_eq!(meeting_row(&rows, &hits, &id("m-00000b")), Some(3));
        assert_eq!(meeting_row(&rows, &hits, &id("m-00000c")), None);

        assert_eq!(summary(0, Some(Load::Search), false), "Searching…");
        assert_eq!(summary(0, None, false), "0 matches");
        assert_eq!(summary(1, None, false), "1 match");
        assert_eq!(summary(100, None, true), "100+ matches");
        assert_eq!(summary(100, Some(Load::More), true), "100+ matches");
        assert_eq!(summary(3, Some(Load::Search), false), "Searching…");
        assert_eq!(summary(3, Some(Load::Refresh), false), "3 matches");
    }

    /// The imports box is a plain group; each import is its own live line,
    /// an Alert once it failed (§4.2: every Status is live).
    #[gpui::test]
    fn import_lines_are_live_inside_a_plain_group(cx: &mut gpui::TestAppContext) {
        use broscribe_core::Library;
        use broscribe_core::config::Config;
        use broscribe_core::schema::Handling;
        use broscribe_platform::fakes::FakePaths;
        use broscribe_platform::resolve_paths;

        use crate::ui::spoken_log;
        use crate::views::import_sheet::ImportRequest;

        let dir = tempfile::tempdir().unwrap();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Config::default(),
                    Library::open(dir.path().join("lib")).unwrap(),
                    dir.path().join("no-such-cli"),
                ),
                cx,
            )
        });
        cx.update(gpui::init);
        let runs = cx.update(ImportRuns::global);
        state.update(cx, |s, cx| {
            let request = ImportRequest {
                path: "/synthetic/standup.m4a".into(),
                handling: Handling::Copy,
                language: None,
                title: None,
                date: None,
                participants: Vec::new(),
                manifest: false,
            };
            s.start_imports(vec![request], &runs, cx);
        });
        let window = cx.add_window(|window, cx| LibraryView::new(state.clone(), window, cx));
        // The CLI does not exist: the import fails.
        cx.run_until_parked();
        let id = runs.read_with(cx, |r, _| r.runs()[0].id);
        window
            .update(cx, |view, _, cx| {
                view.import_details = true;
                let (imports, named) = spoken_log::while_building(|| view.import_rows(cx));
                let imports = imports.expect("the imports box");
                assert_eq!(gpui::Element::a11y_role(&imports), Some(Role::Group));
                let (line, role, text) = named
                    .iter()
                    .find(|(line, _, _)| line == &format!("library-import-{id}"))
                    .unwrap();
                assert_eq!(
                    (line.as_str(), *role),
                    (format!("library-import-{id}").as_str(), Role::Alert)
                );
                assert_eq!(
                    text,
                    "Couldn't import standup.m4a. BroScribe's command-line tool couldn't start."
                );
            })
            .unwrap();
    }

    /// WRT-04, WRT-08: a failed import is its own Failed line (an Alert) in
    /// the imports box, beside its Dismiss, never a row that only changes
    /// colour; a refused source inside the library and a folder's partial
    /// failure are said in words, never with a path or a code.
    #[cfg(unix)]
    #[gpui::test]
    fn import_failures_are_their_own_alerts_without_a_path_or_a_code(
        cx: &mut gpui::TestAppContext,
    ) {
        use broscribe_core::Library;
        use broscribe_core::config::Config;
        use broscribe_core::schema::Handling;
        use broscribe_platform::fakes::FakePaths;
        use broscribe_platform::resolve_paths;

        use crate::ui::spoken_log;
        use crate::views::import_sheet::ImportRequest;

        const PRIVATE: &str = "/Users/synthetic-private/Library/BroScribe";
        const CODE: &str = "zz_synthetic_code_4q1";
        let dir = tempfile::tempdir().unwrap();
        let answer = dir.path().join("answer.json");
        let status = dir.path().join("status");
        // `import` prints the answer file and exits with the status file's
        // code; anything else answers nothing.
        let cli = crate::test_support::write_cli(
            &dir.path().join("import-cli"),
            &format!(
                "cat > /dev/null\ncase \"$*\" in *import*) cat '{}'; exit \"$(cat '{}')\";; \
                 esac\nexit 0\n",
                answer.display(),
                status.display()
            ),
            "",
        );
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Config::default(),
                    Library::open(dir.path().join("lib")).unwrap(),
                    cli,
                ),
                cx,
            )
        });
        cx.update(gpui::init);
        let runs = cx.update(ImportRuns::global);
        let window = cx.add_window(|window, cx| LibraryView::new(state.clone(), window, cx));
        let refused = format!(
            "invalid input: {}",
            broscribe_pipeline::import::LIBRARY_SOURCE_REFUSED
        );
        let refused_with_paths = format!(
            "invalid input: {PRIVATE}/.queue/j-1/audio.wav is inside the library folder \
             {PRIVATE}, in a folder the library manages; move it out of there before importing it"
        );
        let partial = serde_json::json!({
            "imported": [{}],
            "failed": [{
                "path": format!("{PRIVATE}/inbox/b.wav"),
                "code": CODE,
                "message": format!("invalid input: {PRIVATE}/inbox/b.wav is not a recording"),
            }],
        });
        for (answer_json, code, line, kind) in [
            (
                serde_json::json!({"error": {"code": "invalid_input", "message": refused}}),
                1,
                "Couldn't import audio.wav. It is inside the library folder, in a folder the \
                 library manages; move it out of the library folder before importing it.",
                Role::Alert,
            ),
            (
                serde_json::json!({"error": {"code": "invalid_input", "message": refused_with_paths}}),
                1,
                "Couldn't import audio.wav. audio.wav is inside the library folder BroScribe, in \
                 a folder the library manages; move it out of there before importing it.",
                Role::Alert,
            ),
            (
                partial,
                0,
                "From inbox: 1 imported, 1 couldn’t be read. b.wav is not a recording.",
                Role::Status,
            ),
        ] {
            std::fs::write(&answer, answer_json.to_string()).unwrap();
            std::fs::write(&status, code.to_string()).unwrap();
            let path = if code == 0 {
                format!("{PRIVATE}/inbox")
            } else {
                format!("{PRIVATE}/.queue/j-1/audio.wav")
            };
            state.update(cx, |s, cx| {
                let request = ImportRequest {
                    path: path.into(),
                    handling: Handling::Copy,
                    language: None,
                    title: None,
                    date: None,
                    participants: Vec::new(),
                    manifest: false,
                };
                s.start_imports(vec![request], &runs, cx);
            });
            cx.run_until_parked();
            let id = runs.read_with(cx, |r, _| r.runs().last().unwrap().id);
            window
                .update(cx, |view, _, cx| {
                    view.import_details = true;
                    let (imports, named) = spoken_log::while_building(|| view.import_rows(cx));
                    assert_eq!(
                        gpui::Element::a11y_role(&imports.expect("the imports box")),
                        Some(Role::Group)
                    );
                    let own = format!("library-import-{id}");
                    let (_, role, text) = named
                        .iter()
                        .find(|(node, ..)| *node == own)
                        .expect("the import's own line");
                    assert_eq!((*role, text.as_str()), (kind, line));
                    for (_, _, text) in &named {
                        for raw in ["/Users", "synthetic-private", CODE, "invalid input"] {
                            assert!(!text.contains(raw), "{raw} rendered: {text}");
                        }
                    }
                })
                .unwrap();
        }
    }

    #[gpui::test]
    fn a_refresh_keeps_the_selection_and_unrelated_progress_repaints_nothing(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::cell::Cell;
        use std::rc::Rc;

        use broscribe_core::Library;
        use broscribe_core::config::Config;
        use broscribe_core::schema::Meeting;
        use broscribe_platform::fakes::FakePaths;
        use broscribe_platform::resolve_paths;

        let index_meeting = |lib: &Library, title: &str, minutes_ago: i64| {
            let started = broscribe_core::schema::now() - chrono::TimeDelta::minutes(minutes_ago);
            let mut meeting = Meeting::new(MeetingId::random(), Source::Import, started);
            meeting.title = Some(title.into());
            broscribe_core::ops::with_index(lib, |tx, index| {
                tx.create_meeting_dir(&meeting)?;
                tx.save_meeting(&meeting)?;
                index.upsert_meeting(tx, &meeting)?;
                Ok(())
            })
            .unwrap();
            meeting.id
        };
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        index_meeting(&lib, "Synthetic one", 30);
        let picked = index_meeting(&lib, "Synthetic two", 20);
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Config::default(),
                    Library::open(dir.path().join("lib")).unwrap(),
                    dir.path().join("no-such-cli"),
                ),
                cx,
            )
        });
        cx.update(gpui::init);
        let window = cx.add_window(|window, cx| LibraryView::new(state.clone(), window, cx));
        let view = window.update(cx, |_, _, cx| cx.entity()).unwrap();
        cx.run_until_parked();
        let row = view.read_with(cx, |view, cx| {
            meeting_row(view.rows(), view.state.read(cx).search().results(), &picked).unwrap()
        });
        view.update(cx, |view, cx| view.select(Some(row), cx));
        // The user is typing a search that has not settled yet.
        window
            .update(cx, |view, window, cx| {
                view.search_input
                    .update(cx, |input, cx| input.set_value("draft", window, cx));
            })
            .unwrap();
        let repaints = Rc::new(Cell::new(0));
        let _watch = cx.update(|cx| {
            let repaints = repaints.clone();
            cx.observe(&view, move |_, _| repaints.set(repaints.get() + 1))
        });

        // Progress that changes nothing the library shows.
        state.update(cx, |_, cx| cx.notify());
        cx.run_until_parked();
        assert_eq!(repaints.get(), 0);

        // A newer meeting arrives and the list is read again: the selected
        // meeting stays selected on its new row; the typed filter stays.
        index_meeting(&lib, "Synthetic newest", 0);
        state.update(cx, |s, cx| s.rerun_search(cx));
        cx.run_until_parked();
        assert!(repaints.get() > 0);
        view.read_with(cx, |view, cx| {
            let hits = view.state.read(cx).search().results();
            assert_eq!(hits.len(), 3);
            assert_eq!(view.selected_meeting(), Some(&picked));
            assert_eq!(view.selected, meeting_row(view.rows(), hits, &picked));
            assert_ne!(view.selected, Some(row), "the row moved");
            assert_eq!(view.search_input.read(cx).value().to_string(), "draft");
        });
    }

    #[test]
    fn import_picker_options_split_files_and_folders_by_platform_capability() {
        let mixed = import_main_options(true);
        assert!(mixed.files && mixed.directories && mixed.multiple);
        assert_eq!(mixed.prompt.as_deref(), Some("Import"));

        let split = import_main_options(false);
        assert!(split.files && !split.directories && split.multiple);
        assert_eq!(split.prompt.as_deref(), Some("Import"));

        let files = import_recordings_options();
        assert!(files.files && !files.directories && files.multiple);
        assert_eq!(files.prompt.as_deref(), Some("Import"));

        let folders = import_folder_options();
        assert!(!folders.files && folders.directories && folders.multiple);
        assert_eq!(folders.prompt.as_deref(), Some("Import"));
    }

    #[test]
    fn transcript_error_lines_capitalise_the_cause_and_never_show_a_code() {
        assert_eq!(
            transcript_error_message("index is busy.", false),
            "Couldn't search transcripts. Index is busy. Showing matches in titles, people and tags."
        );
        assert_eq!(
            transcript_error_message("  ", false),
            "Couldn't search transcripts. The search tool didn't answer. Showing matches in titles, people and tags."
        );
        assert_eq!(
            transcript_error_message("index is busy", true),
            "Couldn't load more transcript matches. Index is busy."
        );
        assert_eq!(
            transcript_error_message("", true),
            "Couldn't load more transcript matches. The search tool didn't answer."
        );
        assert_eq!(no_match_message(false), "No meetings match these filters.");
        assert_eq!(no_match_message(true), "No titles, people or tags match.");
    }

    fn setup_library_test(
        cx: &mut gpui::TestAppContext,
        titles: &[&str],
    ) -> (
        tempfile::TempDir,
        broscribe_core::Library,
        Entity<AppState>,
        gpui::WindowHandle<LibraryView>,
        Entity<LibraryView>,
        Vec<MeetingId>,
    ) {
        setup_library_test_with_cli(cx, titles, "no-such-cli")
    }

    /// A library of `titles` (newest first) in a window, with the CLI at
    /// `cli` in the test folder (nothing is written there).
    fn setup_library_test_with_cli(
        cx: &mut gpui::TestAppContext,
        titles: &[&str],
        cli: &str,
    ) -> (
        tempfile::TempDir,
        broscribe_core::Library,
        Entity<AppState>,
        gpui::WindowHandle<LibraryView>,
        Entity<LibraryView>,
        Vec<MeetingId>,
    ) {
        use broscribe_core::Library;
        use broscribe_core::config::Config;
        use broscribe_core::schema::Meeting;
        use broscribe_platform::fakes::FakePaths;
        use broscribe_platform::resolve_paths;

        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        let mut ids = Vec::new();
        for (i, title) in titles.iter().enumerate() {
            let started =
                broscribe_core::schema::now() - chrono::TimeDelta::minutes((i as i64) + 1);
            let mut meeting = Meeting::new(MeetingId::random(), Source::Import, started);
            meeting.title = Some((*title).into());
            broscribe_core::ops::with_index(&lib, |tx, index| {
                tx.create_meeting_dir(&meeting)?;
                tx.save_meeting(&meeting)?;
                index.upsert_meeting(tx, &meeting)?;
                Ok(())
            })
            .unwrap();
            ids.push(meeting.id);
        }
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Config::default(),
                    Library::open(dir.path().join("lib")).unwrap(),
                    dir.path().join(cli),
                ),
                cx,
            )
        });
        cx.update(crate::ui::theme::init);
        let window = cx.add_window(|window, cx| LibraryView::new(state.clone(), window, cx));
        let view = window.update(cx, |_, _, cx| cx.entity()).unwrap();
        cx.run_until_parked();
        (dir, lib, state, window, view, ids)
    }

    fn take_prompt_options(
        vcx: &mut gpui::VisualTestContext,
        response: Option<Vec<std::path::PathBuf>>,
    ) -> (bool, bool, bool, Option<String>) {
        use std::cell::RefCell;
        use std::rc::Rc;

        assert!(vcx.did_prompt_for_paths(), "expected a path prompt");
        let captured = Rc::new(RefCell::new(None));
        let out = captured.clone();
        vcx.simulate_path_prompt_response(move |opts| {
            *out.borrow_mut() = Some((
                opts.files,
                opts.directories,
                opts.multiple,
                opts.prompt.as_ref().map(ToString::to_string),
            ));
            response
        });
        vcx.run_until_parked();
        captured.borrow_mut().take().unwrap()
    }

    #[gpui::test]
    fn import_picker_options_with_mixed_platform_support(cx: &mut gpui::TestAppContext) {
        cx.set_can_select_mixed_files_and_dirs(true);
        let (dir, _lib, state, window, view, _ids) = setup_library_test(cx, &[]);
        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.refresh().unwrap();

        // Main "Import…" button keeps mixed files+folders when supported (macOS)
        // and routes selected paths to the import sheet.
        let main_btn = vcx.debug_bounds("library-import-button").unwrap();
        vcx.simulate_click(main_btn.center(), gpui::Modifiers::none());
        let chosen = dir.path().join("recording.m4a");
        assert_eq!(
            take_prompt_options(&mut vcx, Some(vec![chosen.clone()])),
            (true, true, true, Some("Import".into()))
        );
        assert_eq!(
            state.read_with(&vcx, |s, _| s.route().clone()),
            Route::library()
        );
        vcx.update(|_, cx| {
            assert_eq!(
                ImportRuns::global(cx).update(cx, |runs, _| runs.take_dialog_request()),
                [chosen]
            );
        });

        // Empty-state "Choose files…" button uses files-only options.
        let choose_btn = vcx.debug_bounds("library-choose").unwrap();
        vcx.simulate_click(choose_btn.center(), gpui::Modifiers::none());
        assert_eq!(
            take_prompt_options(&mut vcx, None),
            (true, false, true, Some("Import".into()))
        );

        // ImportRecordings action uses files-only options.
        vcx.update_window(window.into(), |_, window, cx| {
            let focus = view.read(cx).view_focus.clone();
            focus.focus(window, cx);
            window.dispatch_action(Box::new(ImportRecordings), cx);
        })
        .unwrap();
        assert_eq!(
            take_prompt_options(&mut vcx, None),
            (true, false, true, Some("Import".into()))
        );

        // ImportFolder action uses folders-only options.
        vcx.update_window(window.into(), |_, window, cx| {
            let focus = view.read(cx).view_focus.clone();
            focus.focus(window, cx);
            window.dispatch_action(Box::new(ImportFolder), cx);
        })
        .unwrap();
        assert_eq!(
            take_prompt_options(&mut vcx, None),
            (false, true, true, Some("Import".into()))
        );

        // Dropdown menu items dispatch ImportRecordings and ImportFolder.
        let split = vcx.debug_bounds("library-import").unwrap();
        let caret = gpui::point(split.right() - px(6.), split.center().y);
        vcx.simulate_click(caret, gpui::Modifiers::none());
        vcx.refresh().unwrap();
        vcx.simulate_keystrokes("down enter");
        assert_eq!(
            take_prompt_options(&mut vcx, None),
            (true, false, true, Some("Import".into()))
        );

        vcx.simulate_click(caret, gpui::Modifiers::none());
        vcx.refresh().unwrap();
        vcx.simulate_keystrokes("down down enter");
        assert_eq!(
            take_prompt_options(&mut vcx, None),
            (false, true, true, Some("Import".into()))
        );
    }

    #[gpui::test]
    fn import_picker_options_without_mixed_platform_support(cx: &mut gpui::TestAppContext) {
        cx.set_can_select_mixed_files_and_dirs(false);
        let (_dir, _lib, _state, window, view, _ids) = setup_library_test(cx, &[]);
        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.refresh().unwrap();

        // Main "Import…" button switches to files-only on platforms where
        // mixed selection is unsupported (Windows, Linux).
        let main_btn = vcx.debug_bounds("library-import-button").unwrap();
        vcx.simulate_click(main_btn.center(), gpui::Modifiers::none());
        assert_eq!(
            take_prompt_options(&mut vcx, None),
            (true, false, true, Some("Import".into()))
        );

        // ImportRecordings opens files-only picker.
        vcx.update_window(window.into(), |_, window, cx| {
            let focus = view.read(cx).view_focus.clone();
            focus.focus(window, cx);
            window.dispatch_action(Box::new(ImportRecordings), cx);
        })
        .unwrap();
        assert_eq!(
            take_prompt_options(&mut vcx, None),
            (true, false, true, Some("Import".into()))
        );

        // ImportFolder opens folders-only picker.
        vcx.update_window(window.into(), |_, window, cx| {
            let focus = view.read(cx).view_focus.clone();
            focus.focus(window, cx);
            window.dispatch_action(Box::new(ImportFolder), cx);
        })
        .unwrap();
        assert_eq!(
            take_prompt_options(&mut vcx, None),
            (false, true, true, Some("Import".into()))
        );

        // Dropdown menu items keep files-only and folders-only pickers on
        // platforms without mixed selection too.
        let split = vcx.debug_bounds("library-import").unwrap();
        let caret = gpui::point(split.right() - px(6.), split.center().y);
        vcx.simulate_click(caret, gpui::Modifiers::none());
        vcx.refresh().unwrap();
        vcx.simulate_keystrokes("down enter");
        assert_eq!(
            take_prompt_options(&mut vcx, None),
            (true, false, true, Some("Import".into()))
        );

        vcx.simulate_click(caret, gpui::Modifiers::none());
        vcx.refresh().unwrap();
        vcx.simulate_keystrokes("down down enter");
        assert_eq!(
            take_prompt_options(&mut vcx, None),
            (false, true, true, Some("Import".into()))
        );
    }

    #[gpui::test]
    fn failed_transcript_search_shows_metadata_hits_attention_line_and_retry(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_dir, _lib, state, window, view, ids) =
            setup_library_test(cx, &["Alpha sync", "Budget review"]);
        let alpha = ids[0].clone();
        let budget = ids[1].clone();
        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.refresh().unwrap();

        // Query A (unfiltered) lists both Alpha sync and Budget review.
        view.read_with(&vcx, |view, cx| {
            let hits = view.state.read(cx).search().results();
            assert_eq!(hits.len(), 2);
            assert!(meeting_row(view.rows(), hits, &alpha).is_some());
            assert!(meeting_row(view.rows(), hits, &budget).is_some());
        });

        // Query B ("budget"): CLI binary is missing so transcript search fails,
        // while metadata search finds Budget review.
        vcx.update_window(window.into(), |_, window, cx| {
            view.update(cx, |view, cx| {
                view.search_input
                    .update(cx, |input, cx| input.set_value("budget", window, cx));
                view.search_now(cx);
            });
        })
        .unwrap();
        vcx.run_until_parked();
        vcx.refresh().unwrap();

        // Query A's Alpha sync is gone; only Query B's Budget review is listed.
        view.read_with(&vcx, |view, cx| {
            let search = view.state.read(cx).search();
            let hits = search.results();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].meeting_id, budget);
            assert!(meeting_row(view.rows(), hits, &alpha).is_none());
            assert!(meeting_row(view.rows(), hits, &budget).is_some());
            assert_eq!(search.error(), None);
            assert!(search.transcript_error().is_some());
        });

        // Renders StatusKind::Attention (Role::Status) with the exact wording
        // and no fatal Alert line.
        let named = vcx
            .update_window(window.into(), |_, window, cx| {
                view.update(cx, |view, cx| {
                    crate::ui::spoken_log::while_building(|| Render::render(view, window, cx)).1
                })
            })
            .unwrap();
        let (_, role, text) = named
            .iter()
            .find(|(id, ..)| id == "library-transcript-error")
            .expect("transcript error status line rendered");
        assert_eq!(*role, Role::Status);
        assert!(
            text.starts_with("Couldn't search transcripts. ")
                && text.ends_with(". Showing matches in titles, people and tags."),
            "{text}"
        );
        assert!(named.iter().all(|(id, ..)| id != "library-search-error"));

        // Clicking "Try again" triggers search_now (bumps search generation).
        let gen_before = state.read_with(&vcx, |s, _| s.search().generation());
        let retry_btn = vcx.debug_bounds("library-transcript-retry").unwrap();
        vcx.simulate_click(retry_btn.center(), gpui::Modifiers::none());
        vcx.run_until_parked();
        assert!(
            state.read_with(&vcx, |s, _| s.search().generation()) > gen_before,
            "Try again must re-run search_now"
        );
    }

    #[gpui::test]
    fn total_search_failure_clears_previous_rows_and_shows_alert(cx: &mut gpui::TestAppContext) {
        let (_dir, lib, _state, window, view, ids) = setup_library_test(cx, &["Alpha sync"]);
        let alpha = ids[0].clone();
        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.refresh().unwrap();

        view.read_with(&vcx, |view, cx| {
            let hits = view.state.read(cx).search().results();
            assert!(meeting_row(view.rows(), hits, &alpha).is_some());
        });

        // Corrupt the index so a new query fails completely.
        std::fs::write(Index::path(lib.root()), b"not a database").unwrap();
        vcx.update_window(window.into(), |_, window, cx| {
            view.update(cx, |view, cx| {
                view.search_input
                    .update(cx, |input, cx| input.set_value("beta", window, cx));
                view.search_now(cx);
            });
        })
        .unwrap();
        vcx.run_until_parked();
        vcx.refresh().unwrap();

        view.read_with(&vcx, |view, cx| {
            let search = view.state.read(cx).search();
            assert!(search.results().is_empty());
            assert!(view.rows().is_empty(), "old query rows must be cleared");
            assert!(search.error().is_some());
            assert_eq!(search.transcript_error(), None);
        });

        let named = vcx
            .update_window(window.into(), |_, window, cx| {
                view.update(cx, |view, cx| {
                    crate::ui::spoken_log::while_building(|| Render::render(view, window, cx)).1
                })
            })
            .unwrap();
        let (_, role, text) = named
            .iter()
            .find(|(id, ..)| id == "library-search-error")
            .expect("search failure alert rendered");
        assert_eq!(*role, Role::Alert);
        assert!(text.starts_with("Couldn't search the library. "), "{text}");
        assert!(!text.contains("Search failed"), "{text}");
        assert!(named.iter().all(|(id, ..)| id != "library-no-match"));
        assert!(
            named
                .iter()
                .all(|(id, ..)| id != "library-transcript-error")
        );
        std::fs::remove_file(Index::path(lib.root())).unwrap();
        lib.write(|tx| Index::rebuild(tx)).unwrap();
        let retry = vcx.debug_bounds("library-search-retry").unwrap();
        vcx.simulate_click(retry.center(), gpui::Modifiers::none());
        vcx.run_until_parked();
        vcx.update(|window, cx| {
            let view = view.read(cx);
            assert!(view.search_has_focus(window, cx));
            let search = view.state.read(cx).search();
            assert!(search.error().is_none());
            assert!(search.results().is_empty());
        });
    }

    #[gpui::test]
    fn missing_index_offers_rebuild_and_keeps_meeting_files(cx: &mut gpui::TestAppContext) {
        let (_dir, lib, state, window, view, ids) = setup_library_test(cx, &["Synthetic orbit"]);
        std::fs::remove_file(Index::path(lib.root())).unwrap();
        state.update(cx, |state, cx| state.rerun_search(cx));
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.update(|window, cx| {
            assert!(state.read(cx).search().index_missing());
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert!(tree.all("library-empty-title").is_empty());
            assert_eq!(
                tree.read(tree.find("library-index-repair")).1,
                "Rebuild index"
            );
            assert_eq!(
                tree.read(tree.find("library-index-attention")).1,
                "The library index is missing. Your meetings are safe in their folders."
            );
            assert!(lib.load_meeting(&ids[0]).is_ok());
            view.update(cx, |view, cx| view.repair_index(cx));
            assert_eq!(
                view.read(cx).index_repair.as_ref().unwrap().0,
                kit::StatusKind::Pending
            );
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert!(
                tree.read(tree.find("library-index-status"))
                    .1
                    .starts_with("Couldn't rebuild the index.")
            );
        });
    }

    #[cfg(unix)]
    #[gpui::test]
    fn grouped_search_keeps_speaker_position_and_exact_segment(cx: &mut gpui::TestAppContext) {
        use std::os::unix::fs::PermissionsExt as _;
        use std::{cell::RefCell, rc::Rc};

        let (dir, _lib, state, window, view, ids) =
            setup_library_test_with_cli(cx, &["Zephyr review", "Synthetic orbit"], "search-cli");
        let executable = dir.path().join("search-cli");
        let result = serde_json::json!({"results": [{
            "meeting_id": ids[1], "snippet": "The **zephyr** draft",
            "speaker_name": "Iva Sample", "start": 763, "segment_id": 12
        }]});
        std::fs::write(
            &executable,
            format!("#!/bin/sh\ncat > /dev/null\nprintf '%s' '{result}'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        type_search(&mut visual, window, &view, "zephyr");
        let events = Rc::new(RefCell::new(Vec::new()));
        let _sub = visual.update(|_, cx| {
            let events = events.clone();
            cx.subscribe(&view, move |_, event: &LibraryEvent, _| {
                events.borrow_mut().push(event.clone())
            })
        });
        visual.update(|window, cx| {
            let search = state.read(cx).search();
            assert_eq!(
                search.results()[0].metadata_match,
                Some(MetadataMatch::Title)
            );
            assert_eq!(search.transcript_match(&ids[1]).unwrap().segment, Some(12));
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("library-day-0")).1,
                "Title, people or tags"
            );
            assert_eq!(tree.read(tree.find("library-day-2")).1, "In transcripts");
            assert!(tree.read(tree.find("library-meeting-3")).1.contains(
                "match in transcript: Iva Sample at 12 minutes 43 seconds, The zephyr draft"
            ));
            view.update(cx, |view, cx| view.focus_list(window, cx));
        });
        visual.simulate_keystrokes("end space");
        assert!(
            matches!(events.borrow().last(), Some(LibraryEvent::SelectMeeting {
            id, segment: Some(12), enter: true, ..
        }) if id == &ids[1])
        );

        type_search(&mut visual, window, &view, "saturn");
        view.update(&mut visual, |view, cx| {
            view.set_app_filter(Some("zoom".into()), cx)
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("library-no-match-title")).1,
                "No results for “saturn”"
            );
            assert_eq!(
                tree.read(tree.find("library-no-match-filters")).1,
                "Filtered by App: Zoom."
            );
            tree.find("library-clear-search");
            tree.find("library-clear-no-match-filters");
            assert!(tree.all("library-summary").is_empty());
        });
    }

    #[gpui::test]
    fn library_row_menu_keyboard_targets_the_same_meeting_and_requests_confirmation(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::{cell::RefCell, rc::Rc};
        let (_dir, _lib, _state, window, view, ids) = setup_library_test(cx, &["Synthetic orbit"]);
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let events = Rc::new(RefCell::new(Vec::new()));
        let _sub = visual.update(|window, cx| {
            cx.bind_keys(crate::ui::keymap::key_bindings_for(
                broscribe_platform::HostPlatform::Windows,
                cx,
            ));
            view.update(cx, |view, cx| view.focus_list(window, cx));
            let events = events.clone();
            cx.subscribe(&view, move |_, event: &LibraryEvent, _| {
                events.borrow_mut().push(event.clone())
            })
        });
        visual.simulate_keystrokes("down shift-f10");
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            let commands: Vec<_> = tree
                .nodes
                .iter()
                .filter(|node| node.role == "MenuItem")
                .map(|node| node.name.as_str())
                .collect();
            let reveal = broscribe_platform_native::NativePlatform::host_platform().reveal_label();
            for label in ["Rename", "Add tag…", "Export…", reveal, "Delete meeting"] {
                assert!(commands.contains(&label), "{commands:?}");
            }
        });
        visual.simulate_keystrokes("escape");
        visual.update(|window, cx| {
            assert!(view.focus_handle(cx).is_focused(window));
            assert!(view.read(cx).row_menu.is_none());
        });
        visual.simulate_keystrokes("shift-f10 up enter");
        visual.run_until_parked();
        assert!(
            matches!(events.borrow().last(), Some(LibraryEvent::SelectMeeting {
            id, action: Some(RowCommand::Delete), ..
        }) if id == &ids[0])
        );
    }

    #[gpui::test]
    fn library_now_uses_the_journal_session_and_deduplicates_its_draft(
        cx: &mut gpui::TestAppContext,
    ) {
        use broscribe_ipc::{RecordingState, StoppedRecording, StoppedRecordingState};
        use std::sync::Arc;
        let (dir, lib, state, _, _, _) = setup_library_test(cx, &[]);
        let session = "synthetic-session";
        let mut draft = broscribe_core::schema::Meeting::new(
            MeetingId::random(),
            Source::Recording,
            broscribe_core::schema::now(),
        );
        draft.title = Some("Synthetic draft".into());
        draft.source_id = Some(format!("recording:{session}"));
        draft.processing.status = ProcessingStatus::Provisional;
        let daemon = cx.update(|cx| {
            crate::daemon::DaemonClient::detached(
                Arc::new(crate::daemon::SocketTransport::new(
                    dir.path().to_path_buf(),
                )),
                cx,
            )
        });
        state.update(cx, |state, cx| state.attach_daemon(daemon.clone(), cx));
        let window = cx.add_window(|window, cx| LibraryView::new(state.clone(), window, cx));
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        daemon.update(&mut visual, |daemon, cx| {
            daemon.connected = true;
            cx.notify();
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            let primary = kit::painted_primary_actions(window, cx);
            assert_eq!(primary, ["library-start-recording"]);
            assert_eq!(
                tree.read(tree.find("library-start-recording")).1,
                "Start recording"
            );
            assert_eq!(
                tree.read(tree.find("library-choose")).1,
                "Import recordings…"
            );
        });
        broscribe_core::ops::with_index(&lib, |tx, index| {
            tx.create_meeting_dir(&draft)?;
            tx.save_meeting(&draft)?;
            index.upsert_meeting(tx, &draft)
        })
        .unwrap();
        state.update(&mut visual, |state, cx| state.rerun_search(cx));
        daemon.update(&mut visual, |daemon, cx| {
            let active: RecordingState = serde_json::from_value(serde_json::json!({
                "session_id":session, "status":"recording", "started_at":draft.started_at,
                "elapsed_s":25.0, "app":"zoom", "title":"Synthetic draft", "health":{"tracks":[]}
            }))
            .unwrap();
            daemon.panel.recording = Some(active);
            cx.notify();
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(tree.read(tree.find("library-day-0")).1, "Now");
            assert_eq!(
                tree.read(tree.find("library-now")).1,
                "Synthetic draft, Recording, Zoom"
            );
            assert_eq!(
                tree.nodes
                    .iter()
                    .filter(|node| node.role == "ListItem")
                    .count(),
                1
            );
        });
        daemon.update(&mut visual, |daemon, cx| {
            daemon.panel.recording.as_mut().unwrap().status =
                broscribe_ipc::RecordingStatus::Paused;
            cx.notify();
        });
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("library-now")).1,
                "Synthetic draft, Paused, Zoom"
            );
        });
        daemon.update(&mut visual, |daemon, cx| {
            daemon.panel.recording = None;
            daemon.panel.stopped.push(StoppedRecording {
                session_id: session.into(),
                title: Some("Synthetic draft".into()),
                tags: vec![],
                stopped_at: chrono::Utc::now(),
                stop_reason: Some("disk_low".into()),
                state: StoppedRecordingState::Queued { ahead: 0 },
            });
            cx.notify();
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert!(
                tree.read(tree.find("library-now"))
                    .1
                    .starts_with("Synthetic draft, Stopped early, The disk is almost full")
            );
        });
        daemon.update(&mut visual, |daemon, cx| {
            daemon.panel.stopped.clear();
            cx.notify();
        });
        visual.run_until_parked();
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert!(tree.all("library-now").is_empty());
            tree.find("library-meeting-1");
        });
    }

    /// The nodes the view names when it renders now: `(id, role, text)`.
    fn named_nodes(
        vcx: &mut gpui::VisualTestContext,
        window: gpui::WindowHandle<LibraryView>,
        view: &Entity<LibraryView>,
    ) -> Vec<(String, Role, String)> {
        vcx.update_window(window.into(), |_, window, cx| {
            view.update(cx, |view, cx| {
                crate::ui::spoken_log::while_building(|| Render::render(view, window, cx)).1
            })
        })
        .unwrap()
    }

    fn type_search(
        vcx: &mut gpui::VisualTestContext,
        window: gpui::WindowHandle<LibraryView>,
        view: &Entity<LibraryView>,
        text: &str,
    ) {
        vcx.update_window(window.into(), |_, window, cx| {
            view.update(cx, |view, cx| {
                view.search_input
                    .update(cx, |input, cx| input.set_value(text, window, cx));
                view.search_now(cx);
            });
        })
        .unwrap();
        vcx.run_until_parked();
        vcx.refresh().unwrap();
    }

    #[gpui::test]
    fn no_title_match_with_a_failed_transcript_search_doesnt_claim_no_meeting_matches(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_dir, _lib, _state, window, view, _ids) = setup_library_test(cx, &["Alpha sync"]);
        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.refresh().unwrap();

        // No title, person or tag matches, and the transcript search fails
        // (the CLI is missing).
        type_search(&mut vcx, window, &view, "zephyr");
        view.read_with(&vcx, |view, cx| {
            let search = view.state.read(cx).search();
            assert!(search.results().is_empty());
            assert!(search.transcript_error().is_some());
        });

        let named = named_nodes(&mut vcx, window, &view);
        let text_of = |id: &str| {
            named
                .iter()
                .find(|(node, ..)| node == id)
                .map(|(_, role, text)| (*role, text.clone()))
        };
        let (role, text) = text_of("library-transcript-error").expect("Attention line");
        assert_eq!(role, Role::Status);
        assert!(text.starts_with("Couldn't search transcripts. "), "{text}");
        assert_eq!(
            text_of("library-no-match"),
            Some((Role::Status, "No titles, people or tags match.".to_owned()))
        );
        assert!(
            named
                .iter()
                .all(|(_, _, text)| text != "No meetings match these filters."),
            "{named:?}"
        );
        assert!(text_of("library-search-error").is_none());
        assert!(text_of("library-drop-zone").is_none());
    }

    #[gpui::test]
    fn an_empty_library_without_an_agent_makes_import_primary(cx: &mut gpui::TestAppContext) {
        let (_dir, _lib, _state, window, _view, _ids) = setup_library_test(cx, &[]);
        cx.update(crate::ui::theme::init);
        cx.run_until_parked();
        let actions = cx
            .update_window(window.into(), |_, window, cx| {
                kit::painted_primary_actions(window, cx)
            })
            .unwrap();
        assert_eq!(actions, ["library-choose"]);
    }

    #[gpui::test]
    fn library_draws_one_title_and_level_two_periods_with_spoken_durations(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_dir, _lib, _state, window, _view, _ids) = setup_library_test(cx, &["Synthetic sync"]);
        cx.run_until_parked();
        cx.update_window(window.into(), |_, window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            tree.assert_title("library-title");
            tree.assert_heading("library-day-0", 2);
            let (_, row) = tree.read(tree.find("library-meeting-1"));
            assert!(row.starts_with("Synthetic sync, "));
            assert!(row.contains(" at "));
            assert!(row.contains("seconds") || row.contains("minute"), "{row}");
            assert!(row.ends_with("Imported file, Queued"), "{row}");
        })
        .unwrap();
    }

    #[gpui::test]
    fn a_click_on_blank_library_space_keeps_focus_where_it_was(cx: &mut gpui::TestAppContext) {
        let (_dir, _lib, _state, window, view, _ids) = setup_library_test(cx, &["Alpha sync"]);
        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.refresh().unwrap();
        vcx.update_window(window.into(), |_, window, cx| {
            let focus = view.read(cx).focus.clone();
            focus.focus(window, cx);
        })
        .unwrap();
        vcx.run_until_parked();

        // The root's padding: blank space.
        vcx.simulate_click(gpui::point(px(4.), px(4.)), gpui::Modifiers::none());
        vcx.run_until_parked();
        vcx.update_window(window.into(), |_, window, cx| {
            let view = view.read(cx);
            assert!(view.focus.is_focused(window), "the list keeps focus");
            assert!(!view.view_focus.is_focused(window));
        })
        .unwrap();

        // The import actions still reach the library from the list.
        vcx.update_window(window.into(), |_, window, cx| {
            window.dispatch_action(Box::new(ImportFolder), cx);
        })
        .unwrap();
        assert_eq!(
            take_prompt_options(&mut vcx, None),
            (false, true, true, Some("Import".into()))
        );
    }

    /// WRT-04: what the view renders for failed transcript searches never
    /// holds the CLI's code, a private path from its message or a system
    /// error number, whatever the code (a refusal, the CLI's `io`).
    #[cfg(unix)]
    #[gpui::test]
    fn rendered_failures_never_show_a_code_or_a_path(cx: &mut gpui::TestAppContext) {
        use std::os::unix::fs::PermissionsExt as _;

        const CODE: &str = "zz_synthetic_code_7f3";
        let (dir, _lib, _state, window, view, _) =
            setup_library_test_with_cli(cx, &["Zephyr roadmap"], "search-cli");
        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        for (query, answer, cause) in [
            (
                "zephyr",
                format!(r#"{{"error":{{"code":"{CODE}","message":"the transcript index is busy"}}}}"#),
                "The transcript index is busy.",
            ),
            (
                "roadmap",
                r#"{"error":{"code":"io","message":"/synthetic/private/Zephyr title/index.db: Permission denied (os error 13)"}}"#
                    .to_owned(),
                "BroScribe couldn't read or write a file.",
            ),
        ] {
            let cli = dir.path().join("search-cli");
            std::fs::write(
                &cli,
                format!("#!/bin/sh\ncat > /dev/null\nprintf '%s' '{answer}'\nexit 1\n"),
            )
            .unwrap();
            std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
            type_search(&mut vcx, window, &view, query);
            vcx.run_until_parked();
            vcx.refresh().unwrap();
            let named = named_nodes(&mut vcx, window, &view);
            let (_, _, line) = named
                .iter()
                .find(|(id, ..)| id == "library-transcript-error")
                .expect("the failure is rendered");
            assert_eq!(
                line,
                &format!(
                    "Couldn't search transcripts. {cause} Showing matches in titles, people \
                     and tags."
                )
            );
            for (_, _, text) in &named {
                for raw in [CODE, "/synthetic", "os error", "Permission denied", "(io)"] {
                    assert!(!text.contains(raw), "{raw} rendered: {text}");
                }
            }
        }
    }

    /// A transcript page that fails during Load more: the meetings stay,
    /// the line says so, and Try again reads the failed page, not the first.
    #[cfg(unix)]
    #[gpui::test]
    fn try_again_after_a_failed_load_more_reads_the_failed_transcript_page(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::os::unix::fs::PermissionsExt as _;

        let (dir, _lib, state, window, view, ids) = setup_library_test_with_cli(
            cx,
            &["Zephyr roadmap", "Weekly sync", "Planning"],
            "search-cli",
        );
        let (weekly, planning) = (ids[1].to_string(), ids[2].to_string());
        // Page 1 finds the weekly sync, pages 2-5 nothing, so a load stops
        // before page 6; page 6 fails while the `fail` marker exists, and
        // then finds the planning meeting. Its message is empty.
        let page = |results: &str, next: Option<&str>| {
            let next = next
                .map(|c| format!(r#","next_cursor":"{c}""#))
                .unwrap_or_default();
            format!(r#"{{"results":[{results}],"revision":{{"instance":"x","revision":1}}{next}}}"#)
        };
        let hit = |id: &str| format!(r#"{{"meeting_id":"{id}","snippet":"**zephyr**"}}"#);
        let log = dir.path().join("search-args");
        let fail = dir.path().join("fail");
        let mut script = format!(
            "#!/bin/sh\ncat > /dev/null\nprintf '%s\\n' \"$*\" >> '{}'\ncase \"$*\" in\n",
            log.display()
        );
        script.push_str(&format!(
            "  *'--cursor c6'*) if [ -e '{}' ]; then printf '%s' '{}'; else printf '%s' '{}'; fi ;;\n",
            fail.display(),
            r#"{"error":{"code":"internal","message":""}}"#,
            page(&hit(&planning), None),
        ));
        for n in 2..=5 {
            let next = format!("c{}", n + 1);
            script.push_str(&format!(
                "  *'--cursor c{n}'*) printf '%s' '{}' ;;\n",
                page("", Some(&next))
            ));
        }
        script.push_str(&format!(
            "  *) printf '%s' '{}' ;;\nesac\n",
            page(&hit(&weekly), Some("c2"))
        ));
        let cli = dir.path().join("search-cli");
        std::fs::write(&cli, script).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&fail, b"").unwrap();

        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.refresh().unwrap();
        type_search(&mut vcx, window, &view, "zephyr");
        let listed = |vcx: &mut gpui::VisualTestContext| {
            state.read_with(vcx, |s, _| {
                s.search()
                    .results()
                    .iter()
                    .map(|h| h.meeting_id.to_string())
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(listed(&mut vcx), [ids[0].to_string(), weekly.clone()]);
        assert!(state.read_with(&vcx, |s, _| s.search().has_more()));

        view.update(&mut vcx, |view, cx| view.load_more(cx));
        vcx.run_until_parked();
        vcx.refresh().unwrap();
        assert_eq!(listed(&mut vcx), [ids[0].to_string(), weekly.clone()]);
        assert!(state.read_with(&vcx, |s, _| s.search().has_more()));
        let named = named_nodes(&mut vcx, window, &view);
        let (_, role, text) = named
            .iter()
            .find(|(id, ..)| id == "library-transcript-error")
            .expect("Attention line");
        assert_eq!(*role, Role::Status);
        assert_eq!(
            text,
            "Couldn't load more transcript matches. The search tool didn't answer."
        );

        std::fs::remove_file(&fail).unwrap();
        let calls = std::fs::read_to_string(&log).unwrap().lines().count();
        let retry = vcx.debug_bounds("library-transcript-retry").unwrap();
        vcx.simulate_click(retry.center(), gpui::Modifiers::none());
        vcx.run_until_parked();
        assert_eq!(listed(&mut vcx), [ids[0].to_string(), weekly, planning]);
        assert!(state.read_with(&vcx, |s, _| s.search().transcript_error().is_none()));
        let log = std::fs::read_to_string(&log).unwrap();
        let retried: Vec<&str> = log.lines().skip(calls).collect();
        assert_eq!(retried.len(), 1, "{retried:?}");
        assert!(retried[0].contains("--cursor c6"), "{retried:?}");
    }
}
