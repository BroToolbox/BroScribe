//! The import dialog inspects selected or dropped paths, makes optional
//! metadata explicit, and queues the resulting CLI requests in ImportRuns.
//! Paths, titles and participants travel on stdin.

use std::cell::RefCell;
use std::ffi::OsString;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use broscribe_core::MeetingId;
use broscribe_core::import_files::{MANIFEST_FORMAT, is_media};
use broscribe_core::schema::Handling;
use broscribe_platform::ui::{ImportFileDrop, ImportPathPicker, ImportPathRequest};
use chrono::{DateTime, Local, NaiveDate, NaiveDateTime};
use gpui::component::Sizable as _;
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::combobox::{Combobox, ComboboxState};
use gpui::component::date_picker::{DatePicker, DatePickerState};
use gpui::component::input::{Input, InputState};
use gpui::component::searchable_list::{SearchableListDelegate, SearchableListItem};
use gpui::component::time_field::{HourCycle, TimePrecision};
use gpui::component::{ActiveTheme as _, Disableable as _};
use gpui::component::{IndexPath, Root, WindowExt as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Div, Entity, FocusHandle, Focusable, Global,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, PathPromptOptions,
    Render, Role, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Task,
    Window, div, px,
};

use crate::jobs::{ChildKind, CliError, unsendable_path};
use crate::state::AppState;
use crate::ui::type_scale::{TextRole, TextRoleExt as _};
use crate::ui::{self, text_line};
use crate::views::person_picker::{PeopleIndex, load_people, search};

// ---- form model (pure, unit-tested) ----

/// What the app knows about a dropped path (read on the background executor).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathInfo {
    File {
        modified: Option<SystemTime>,
        bytes: u64,
    },
    Folder {
        recordings: Option<usize>,
    },
    Manifest {
        meetings: Option<usize>,
    },
    Missing,
    Unsupported,
}

/// A form field, for inline errors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Paths,
    Title,
    Date,
    Language,
    /// Participant row by position.
    Participant(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldError {
    pub field: Field,
    pub message: String,
}

impl FieldError {
    fn new(field: Field, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }
}

/// A participant as `import --participant` takes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParticipantSpec {
    pub name: Option<String>,
    pub email: Option<String>,
}

impl ParticipantSpec {
    /// `Name <email>`, `Name` or `<email>`.
    pub fn to_arg(&self) -> String {
        match (&self.name, &self.email) {
            (Some(n), Some(email)) => format!("{n} <{email}>"),
            (Some(n), None) => n.clone(),
            (None, Some(email)) => format!("<{email}>"),
            (None, None) => String::new(),
        }
    }
}

/// Parses `Name <email>`, `Name`, `<email>` or a bare email. `Ok(None)` for
/// a blank row.
pub fn parse_participant(text: &str) -> Result<Option<ParticipantSpec>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let (name, email) = if let Some(open) = text.find('<') {
        let rest = &text[open + 1..];
        let Some(close) = rest.find('>') else {
            return Err("missing “>” after the email".into());
        };
        if !rest[close + 1..].trim().is_empty() {
            return Err("nothing may follow “<email>”".into());
        }
        (text[..open].trim(), Some(rest[..close].trim()))
    } else if text.contains('>') {
        return Err("missing “<” before the email".into());
    } else if !text.contains(char::is_whitespace) && text.contains('@') {
        ("", Some(text))
    } else {
        (text, None)
    };
    let email = match email {
        Some(email) if !is_email(email) => {
            return Err(format!("“{email}” is not an email address"));
        }
        Some(email) => Some(email.to_owned()),
        None => None,
    };
    let name = (!name.is_empty()).then(|| name.to_owned());
    if name.is_none() && email.is_none() {
        return Err("give a name, an email or both".into());
    }
    Ok(Some(ParticipantSpec { name, email }))
}

pub(super) fn is_email(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !s.contains(|c: char| c.is_whitespace() || c == '<' || c == '>')
}

/// A BCP 47-shaped tag: a 2–3 letter language, then 1–8 character
/// alphanumeric subtags (`en`, `pt-BR`, `zh-Hant-TW`).
pub fn is_language_tag(tag: &str) -> bool {
    let mut parts = tag.split('-');
    let lang_ok = parts
        .next()
        .is_some_and(|l| (2..=3).contains(&l.len()) && l.chars().all(|c| c.is_ascii_alphabetic()));
    lang_ok
        && parts.all(|p| (1..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Accepts what `import --date` takes: `YYYY-MM-DD`, `YYYY-MM-DD HH:MM` or
/// RFC 3339.
pub fn is_import_date(s: &str) -> bool {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
        || NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").is_ok()
        || DateTime::parse_from_rfc3339(s).is_ok()
}

/// The default title: the file name without its extension.
pub fn default_title(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The default date: the file's modification time, local, `YYYY-MM-DD HH:MM`.
pub fn default_date(modified: SystemTime) -> String {
    DateTime::<Local>::from(modified)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

pub fn handling_arg(handling: Handling) -> &'static str {
    match handling {
        Handling::Copy => "copy",
        Handling::Reference => "reference",
        Handling::Move => "move",
    }
}

/// One `broscribe import` invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportRequest {
    pub path: PathBuf,
    pub handling: Handling,
    /// A selected language, or automatic detection for a direct import.
    pub language: Option<String>,
    pub title: Option<String>,
    pub date: Option<String>,
    pub participants: Vec<ParticipantSpec>,
    pub manifest: bool,
}

impl ImportRequest {
    /// Arguments after `broscribe --library <lib> --json`: flags only; the
    /// path, title and participants travel on stdin ([`Self::spec`]).
    pub fn args(&self) -> Vec<OsString> {
        if self.manifest {
            let mut args = super::setup::manifest_import_args();
            args.extend(["--handling".into(), handling_arg(self.handling).into()]);
            return args;
        }
        let mut args: Vec<OsString> = vec![
            "import".into(),
            "--handling".into(),
            handling_arg(self.handling).into(),
        ];
        if let Some(lang) = &self.language {
            args.extend(["--lang".into(), lang.into()]);
        } else {
            args.push("--auto-language".into());
        }
        if let Some(date) = &self.date {
            args.extend(["--date".into(), date.into()]);
        }
        args.push("--spec-stdin".into());
        args
    }

    /// The `import --spec-stdin` input: `{"path","title","participants"}`.
    /// `Err` for a path JSON cannot carry (not UTF-8).
    pub fn spec(&self) -> Result<Vec<u8>, String> {
        if self.manifest {
            return super::setup::manifest_import_spec(&self.path);
        }
        let path = self
            .path
            .to_str()
            .ok_or_else(|| unsendable_path(&self.path, "imported"))?;
        let participants: Vec<String> = self
            .participants
            .iter()
            .map(ParticipantSpec::to_arg)
            .collect();
        Ok(serde_json::to_vec(&serde_json::json!({
            "path": path,
            "title": self.title,
            "participants": participants,
        }))
        .expect("an import spec serializes"))
    }
}

/// The sheet's values, validated into one request per path.
#[derive(Clone, Debug, PartialEq)]
pub struct ImportForm {
    pub paths: Vec<PathBuf>,
    /// `None` while the paths are being inspected.
    pub info: Option<Vec<PathInfo>>,
    pub title: String,
    pub date: String,
    /// The values the sheet prefilled. A field still holding its prefill was
    /// not edited and is not sent: the importer then uses the container's
    /// creation time and a file-derived title it may later replace.
    pub prefilled_title: Option<String>,
    pub language: String,
    pub handling: Handling,
    pub participants: Vec<String>,
}

fn importable_items<'a>(
    paths: &'a [PathBuf],
    info: Option<&'a [PathInfo]>,
) -> Vec<(&'a PathBuf, &'a PathInfo)> {
    paths
        .iter()
        .zip(info.unwrap_or_default())
        .filter(|(_, kind)| !matches!(kind, PathInfo::Unsupported | PathInfo::Missing))
        .collect()
}

impl ImportForm {
    /// Title and date apply to a single file only.
    pub fn single_file(&self) -> bool {
        matches!(self.importable().as_slice(), [(_, PathInfo::File { .. })])
    }

    fn importable(&self) -> Vec<(&PathBuf, &PathInfo)> {
        importable_items(&self.paths, self.info.as_deref())
    }

    pub fn validate(&self) -> Result<Vec<ImportRequest>, Vec<FieldError>> {
        let items = self.importable();
        let mut errors = Vec::new();
        if items.is_empty() {
            return Err(vec![FieldError::new(
                Field::Paths,
                "None of these items is a recording, a folder or an import manifest.",
            )]);
        }
        for (path, _) in &items {
            if path.to_str().is_none() {
                errors.push(FieldError::new(
                    Field::Paths,
                    unsendable_path(path, "imported"),
                ));
            }
        }
        let single = self.single_file();
        let title = self.title.trim();
        let title = (single && !title.is_empty() && self.prefilled_title.as_deref() != Some(title))
            .then(|| title.to_owned());
        let date = self.date.trim();
        let date = (single && !date.is_empty()).then(|| date.to_owned());
        if date.as_deref().is_some_and(|d| !is_import_date(d)) {
            errors.push(FieldError::new(Field::Date, "Choose a date and time."));
        }
        let has_media = items
            .iter()
            .any(|(_, kind)| !matches!(kind, PathInfo::Manifest { .. }));
        let language = Some(self.language.trim())
            .filter(|l| has_media && !l.is_empty())
            .map(str::to_owned);
        if let Some(l) = &language
            && !is_language_tag(l)
        {
            errors.push(FieldError::new(
                Field::Language,
                "Use a language tag such as en or pt-BR, or leave empty for automatic",
            ));
        }
        let mut participants = Vec::new();
        for (i, row) in self.participants.iter().enumerate().filter(|_| has_media) {
            match parse_participant(row) {
                Ok(Some(p)) => participants.push(p),
                Ok(None) => {}
                Err(message) => errors.push(FieldError::new(Field::Participant(i), message)),
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        Ok(items
            .into_iter()
            .map(|(path, kind)| {
                let manifest = matches!(kind, PathInfo::Manifest { .. });
                ImportRequest {
                    path: path.clone(),
                    handling: if manifest && self.handling == Handling::Move {
                        Handling::Copy
                    } else {
                        self.handling
                    },
                    language: if manifest { None } else { language.clone() },
                    title: if manifest { None } else { title.clone() },
                    date: if manifest { None } else { date.clone() },
                    participants: if manifest {
                        Vec::new()
                    } else {
                        participants.clone()
                    },
                    manifest,
                }
            })
            .collect())
    }
}

/// Why importing a file failed, in words (WRT-04): a file whose audio can't
/// be decoded (`decode`) is said as such; anything else is
/// [`CliError::user_text`].
pub fn import_cause(error: &CliError) -> String {
    match error.code.as_str() {
        "decode" if error.from_cli => "BroScribe couldn't read the file's audio.".to_owned(),
        _ => error.user_text(),
    }
}

/// The child's outcome, kept with its run so actions always address that run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportOutcome(serde_json::Value);

impl ImportOutcome {
    fn failures(&self) -> usize {
        ["failed", "issues"]
            .into_iter()
            .map(|key| self.0[key].as_array().map_or(0, Vec::len))
            .sum()
    }

    pub fn meetings(&self) -> impl Iterator<Item = (MeetingId, String)> + '_ {
        std::iter::once(&self.0)
            .chain(self.0["imported"].as_array().into_iter().flatten())
            .filter_map(|value| {
                let meeting = MeetingId::parse(value["meeting_id"].as_str()?).ok()?;
                Some((meeting, value))
            })
            .enumerate()
            .map(|(index, (meeting, value))| {
                let label = value["title"]
                    .as_str()
                    .filter(|title| !title.is_empty())
                    .map(ToOwned::to_owned)
                    .or_else(|| {
                        value["source_path"]
                            .as_str()
                            .map(|path| file_label(Path::new(path)))
                    })
                    .unwrap_or_else(|| format!("Meeting {}", index + 1));
                (meeting, label)
            })
    }

    fn words(&self, name: &str) -> String {
        let value = &self.0;
        let failures = self.failures();
        if value["already_exists"].as_bool() == Some(true) {
            return format!("{name} is already in the library.");
        }
        if let Some(imported) = value["imported"].as_array() {
            let mut words = format!("From {name}: {} imported", imported.len());
            if failures > 0 {
                words.push_str(&format!(", {failures} couldn’t be read"));
                let first = &value["failed"][0];
                let cause = import_cause(&CliError::answered(
                    first["code"].as_str().unwrap_or_default(),
                    first["message"].as_str().unwrap_or_default(),
                ));
                return format!("{words}. {cause}");
            }
            return format!("{words}.");
        }
        if value["meeting_id"].is_string() {
            let title = value["title"]
                .as_str()
                .filter(|title| !title.is_empty())
                .unwrap_or(name);
            let mut words = format!("Imported “{title}”");
            if let Some(duration) = value["duration_s"].as_f64().filter(|seconds| *seconds > 0.) {
                words.push_str(&format!(
                    " · {}",
                    broscribe_core::display_units::length(duration)
                ));
            }
            let named = value["auto_named"].as_u64().unwrap_or_default();
            let unnamed = value["speakers"]
                .as_u64()
                .unwrap_or_default()
                .saturating_sub(named);
            let mut speakers = Vec::new();
            if named > 0 {
                speakers.push(format!(
                    "{named} {} named automatically",
                    if named == 1 { "speaker" } else { "speakers" }
                ));
            }
            if unnamed > 0 {
                speakers.push(format!(
                    "{unnamed} {} a name",
                    if unnamed == 1 { "needs" } else { "need" }
                ));
            }
            if !speakers.is_empty() {
                words.push_str(&format!(" · {}", speakers.join(", ")));
            }
            if value["notes"].as_array().is_some_and(|notes| {
                notes.iter().any(|note| {
                    note.as_str()
                        .is_some_and(|note| note.starts_with("--lang "))
                })
            }) {
                words.push_str(". The speech model detects the language automatically.");
            }
            return words;
        }
        if let Some(resolved) = value["resolved"].as_u64() {
            return format!("From {name}: {resolved} imported, {failures} couldn’t be read.");
        }
        if value["jobs"].is_array() || value["job_id"].is_string() {
            return format!("{name} is queued for transcription.");
        }
        format!("Finished importing {name}.")
    }
}

pub(crate) fn file_label(path: &Path) -> String {
    path.file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}

struct NativeImportPicker<'a>(&'a App);

impl ImportPathPicker for NativeImportPicker<'_> {
    fn pick(
        &self,
        request: ImportPathRequest,
    ) -> futures::future::LocalBoxFuture<'static, Result<Option<Vec<PathBuf>>, String>> {
        let picked = self.0.prompt_for_paths(PathPromptOptions {
            files: request.files,
            directories: request.directories,
            multiple: request.multiple,
            prompt: request.prompt.map(Into::into),
        });
        Box::pin(async move {
            match picked.await {
                Ok(Ok(paths)) => Ok(paths),
                Ok(Err(_)) => Err("Couldn’t open the file picker.".into()),
                Err(_) => Ok(None),
            }
        })
    }
}

pub(crate) fn pick_import_paths(
    options: PathPromptOptions,
    cx: &App,
) -> futures::future::LocalBoxFuture<'static, Result<Option<Vec<PathBuf>>, String>> {
    NativeImportPicker(cx).pick(ImportPathRequest {
        files: options.files,
        directories: options.directories,
        multiple: options.multiple,
        prompt: options.prompt.map(|prompt| prompt.to_string()),
    })
}

struct NativeImportDrop<'a>(&'a gpui::ExternalPaths);

impl ImportFileDrop for NativeImportDrop<'_> {
    fn paths(&self) -> &[PathBuf] {
        self.0.paths()
    }
}

fn unique_paths(source: &dyn ImportFileDrop) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    source
        .paths()
        .iter()
        .filter(|path| seen.insert(*path))
        .cloned()
        .collect()
}

pub(crate) fn dropped_paths(source: &gpui::ExternalPaths) -> Vec<PathBuf> {
    unique_paths(&NativeImportDrop(source))
}

fn is_json(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
}

/// A drag starts with a bounded check. The CLI validates the complete manifest.
pub(crate) fn accepts_drop(paths: &[PathBuf]) -> bool {
    paths.iter().any(|path| {
        is_media(path) || path.is_dir() || (is_json(path) && inspect_manifest(path).is_some())
    })
}

fn inspect_manifest(path: &Path) -> Option<PathInfo> {
    let mut prefix = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(4096)
        .read_to_end(&mut prefix)
        .ok()?;
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&prefix) {
        return (value["format"].as_str() == Some(MANIFEST_FORMAT)).then(|| PathInfo::Manifest {
            meetings: value["meetings"].as_array().map(Vec::len),
        });
    }
    // For larger manifests, the format marker is in the top-level header.
    // Full validation belongs to the importer.
    let prefix = String::from_utf8_lossy(&prefix);
    let (_, format) = prefix.split_once("\"format\"")?;
    let format = format.trim_start().strip_prefix(':')?.trim_start();
    format
        .strip_prefix('"')?
        .strip_prefix(MANIFEST_FORMAT)?
        .starts_with('"')
        .then_some(PathInfo::Manifest { meetings: None })
}

fn inspect_paths(paths: &[PathBuf]) -> Vec<PathInfo> {
    paths
        .iter()
        .map(|path| match std::fs::metadata(path) {
            Ok(m) if m.is_dir() => PathInfo::Folder {
                recordings: broscribe_core::import_files::folder_media(path)
                    .ok()
                    .map(|(media, _)| media.len()),
            },
            Ok(m) if m.is_file() && is_media(path) => PathInfo::File {
                modified: m.modified().ok(),
                bytes: m.len(),
            },
            Ok(m) if m.is_file() && is_json(path) => {
                inspect_manifest(path).unwrap_or(PathInfo::Unsupported)
            }
            Ok(_) => PathInfo::Unsupported,
            Err(_) => PathInfo::Missing,
        })
        .collect()
}

// ---- import runs (outlive the sheet) ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportRunStatus {
    Waiting,
    Running,
    /// Summary of the CLI output.
    Done(ImportOutcome),
    /// Why it failed, as a sentence in words ([`import_cause`]).
    Failed(String),
}

impl ImportRunStatus {
    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Done(_) | Self::Failed(_))
    }
}

#[derive(Clone, Debug)]
pub struct ImportRun {
    pub id: u64,
    pub path: PathBuf,
    pub status: ImportRunStatus,
    request: ImportRequest,
}

impl ImportRun {
    pub fn is_manifest(&self) -> bool {
        self.request.manifest
    }

    pub fn failed(&self) -> bool {
        match &self.status {
            ImportRunStatus::Failed(_) => true,
            ImportRunStatus::Done(outcome) => outcome.failures() > 0,
            _ => false,
        }
    }

    pub fn words(&self) -> String {
        let name = file_label(&self.path);
        match &self.status {
            ImportRunStatus::Waiting => format!("Waiting: {name}"),
            ImportRunStatus::Running => format!("Importing and transcribing {name}…"),
            ImportRunStatus::Done(outcome) => outcome.words(&name),
            ImportRunStatus::Failed(cause) => {
                crate::jobs::failure_text(&format!("Couldn't import {name}."), cause, "")
            }
        }
    }
}

/// Imports started from the sheet, run one at a time.
#[derive(Default)]
pub struct ImportRuns {
    runs: Vec<ImportRun>,
    next_id: u64,
    draining: bool,
    dialog_request: Vec<PathBuf>,
}

struct GlobalImportRuns(Entity<ImportRuns>);

impl Global for GlobalImportRuns {}

impl ImportRuns {
    /// The app-wide instance, created on first use.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(global) = cx.try_global::<GlobalImportRuns>() {
            return global.0.clone();
        }
        let runs = cx.new(|_| Self::default());
        cx.set_global(GlobalImportRuns(runs.clone()));
        runs
    }

    pub fn request_dialog(paths: Vec<PathBuf>, cx: &mut App) {
        if !paths.is_empty() {
            Self::global(cx).update(cx, |runs, cx| {
                runs.dialog_request.extend(paths);
                cx.notify();
            });
        }
    }

    pub(crate) fn has_dialog_request(&self) -> bool {
        !self.dialog_request.is_empty()
    }

    pub(crate) fn take_dialog_request(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.dialog_request)
    }

    pub fn progress(&self) -> Option<(usize, usize, &ImportRun)> {
        let active = self
            .runs
            .iter()
            .find(|run| run.status == ImportRunStatus::Running)?;
        let finished = self
            .runs
            .iter()
            .filter(|run| run.status.is_finished())
            .count();
        Some((finished + 1, self.runs.len(), active))
    }

    pub fn cancel_waiting(&mut self, id: Option<u64>, cx: &mut Context<Self>) {
        let before = self.runs.len();
        self.runs
            .retain(|r| r.status != ImportRunStatus::Waiting || id.is_some_and(|id| id != r.id));
        if self.runs.len() != before {
            cx.notify();
        }
    }

    pub fn dismiss_finished(&mut self, cx: &mut Context<Self>) {
        self.runs.retain(|r| !r.status.is_finished());
        cx.notify();
    }

    pub fn retry_request(&self, id: u64) -> Option<ImportRequest> {
        self.runs
            .iter()
            .find(|r| r.id == id && r.failed())
            .map(|r| r.request.clone())
    }

    pub fn runs(&self) -> &[ImportRun] {
        &self.runs
    }

    /// Some import is waiting or running.
    pub fn is_busy(&self) -> bool {
        self.runs.iter().any(|r| !r.status.is_finished())
    }

    fn add(&mut self, request: ImportRequest, cx: &mut Context<Self>) {
        self.next_id += 1;
        self.runs.push(ImportRun {
            id: self.next_id,
            path: request.path.clone(),
            status: ImportRunStatus::Waiting,
            request,
        });
        cx.notify();
    }

    /// Marks the next waiting run as running and hands out its request.
    fn start_next(&mut self, cx: &mut Context<Self>) -> Option<(u64, ImportRequest)> {
        let run = self
            .runs
            .iter_mut()
            .find(|r| r.status == ImportRunStatus::Waiting)?;
        run.status = ImportRunStatus::Running;
        let request = run.request.clone();
        cx.notify();
        Some((run.id, request))
    }

    fn finish(&mut self, id: u64, status: ImportRunStatus, cx: &mut Context<Self>) {
        if let Some(run) = self.runs.iter_mut().find(|r| r.id == id) {
            run.status = status;
            cx.notify();
        }
    }

    /// Removes a finished run from the list.
    pub fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        let before = self.runs.len();
        self.runs.retain(|r| r.id != id || !r.status.is_finished());
        if self.runs.len() != before {
            cx.notify();
        }
    }
}

impl AppState {
    /// Queues `requests` in `runs` and runs them one at a time as
    /// `broscribe import` children on the background executor; after each,
    /// kicks the queue worker and refreshes searches and the review count.
    pub fn start_imports(
        &mut self,
        requests: Vec<ImportRequest>,
        runs: &Entity<ImportRuns>,
        cx: &mut Context<Self>,
    ) {
        if requests.is_empty() {
            return;
        }
        let start_drain = runs.update(cx, |r, cx| {
            for request in requests {
                r.add(request, cx);
            }
            !std::mem::replace(&mut r.draining, true)
        });
        if !start_drain {
            return;
        }
        let runs = runs.clone();
        cx.spawn(async move |this, cx| {
            while let Some((id, request)) = runs.update(cx, |r, cx| r.start_next(cx)) {
                let task = match request.spec() {
                    Ok(spec) => this
                        .update(cx, |state, cx| {
                            state.cli().run_with_stdin(
                                request.args(),
                                ChildKind::Command,
                                spec,
                                cx.background_executor(),
                            )
                        })
                        .map_err(|_| "BroScribe is closing.".to_owned()),
                    Err(message) => Err(message),
                };
                let status = match task {
                    Ok(task) => match task.await {
                        Ok(value) => ImportRunStatus::Done(ImportOutcome(value)),
                        Err(e) => ImportRunStatus::Failed(import_cause(&e)),
                    },
                    Err(message) => ImportRunStatus::Failed(message),
                };
                runs.update(cx, |r, cx| r.finish(id, status, cx));
                this.update(cx, |state, cx| {
                    state.kick_jobs(cx);
                    state.rerun_search(cx);
                    state.refresh_review_count(cx);
                })
                .ok();
            }
            runs.update(cx, |r, _| r.draining = false);
        })
        .detach();
    }
}

// ---- the sheet ----

const PARTICIPANT_HELP: &str = "Ana Example <ana@example.com>";

#[derive(Clone)]
struct ParticipantOption {
    label: SharedString,
    email: Option<String>,
    value: String,
    invalid: bool,
}

impl SearchableListItem for ParticipantOption {
    type Value = String;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &String {
        &self.value
    }

    fn disabled(&self) -> bool {
        self.invalid
    }

    fn render(&self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .min_w_0()
            .child(div().truncate().child(self.label.clone()))
            .when_some(self.email.clone(), |row, email| {
                row.child(
                    div()
                        .truncate()
                        .text_role(TextRole::Caption)
                        .text_color(cx.theme().muted_foreground)
                        .child(email),
                )
            })
    }
}

#[derive(Default)]
struct ParticipantList {
    people: PeopleIndex,
    shown: Vec<ParticipantOption>,
}

impl ParticipantList {
    fn update_query(&mut self, query: &str) {
        let matches = search(&self.people, query);
        self.shown = matches
            .shown
            .iter()
            .map(|&index| {
                let person = &self.people[index];
                let email = person.emails.first().cloned();
                ParticipantOption {
                    label: person.name.clone().into(),
                    value: ParticipantSpec {
                        name: Some(person.name.clone()),
                        email: email.clone(),
                    }
                    .to_arg(),
                    email,
                    invalid: false,
                }
            })
            .collect();
        if self.shown.is_empty() && !query.trim().is_empty() {
            let text = query.trim();
            self.shown.push(match parse_participant(text) {
                Ok(Some(person)) => ParticipantOption {
                    label: format!("Add “{text}”").into(),
                    value: person.to_arg(),
                    email: None,
                    invalid: false,
                },
                _ => ParticipantOption {
                    label: PARTICIPANT_HELP.into(),
                    value: String::new(),
                    email: None,
                    invalid: true,
                },
            });
        }
    }
}

impl SearchableListDelegate for ParticipantList {
    type Item = ParticipantOption;

    fn items_count(&self, section: usize) -> usize {
        if section == 0 { self.shown.len() } else { 0 }
    }

    fn item(&self, index: IndexPath) -> Option<&Self::Item> {
        self.shown.get(index.row)
    }

    fn position<V>(&self, value: &V) -> Option<IndexPath>
    where
        Self::Item: SearchableListItem<Value = V>,
        V: PartialEq,
    {
        self.shown
            .iter()
            .position(|item| item.value() == value)
            .map(IndexPath::new)
    }

    fn perform_search(&mut self, query: &str, _: &mut Window, _: &mut App) -> Task<()> {
        self.update_query(query);
        Task::ready(())
    }

    fn on_will_change(
        &mut self,
        selection: &mut Vec<(IndexPath, Self::Item)>,
        changes: &[gpui::component::searchable_list::SearchableListChange],
    ) {
        use gpui::component::searchable_list::SearchableListChange;
        for change in changes {
            match *change {
                SearchableListChange::Select { index } => {
                    if let Some(item) = self.item(index).filter(|item| !item.invalid).cloned()
                        && !selection
                            .iter()
                            .any(|(_, selected)| selected.value == item.value)
                    {
                        // Filtered row indices are reused across queries. Tokens need
                        // independent identities so removal always targets one person.
                        let next = selection
                            .iter()
                            .map(|(index, _)| index.row + 1)
                            .max()
                            .unwrap_or(0);
                        selection.push((IndexPath::new(next), item));
                    }
                }
                SearchableListChange::Deselect { index } => {
                    if let Some(item) = self.item(index) {
                        selection.retain(|(_, selected)| selected.value != item.value);
                    }
                }
            }
        }
    }
}

gpui::actions!(import_dialog, [SubmitImport]);

pub struct ImportSheet {
    state: Entity<AppState>,
    runs: Entity<ImportRuns>,
    paths: Vec<PathBuf>,
    info: Option<Vec<PathInfo>>,
    path_revision: u64,
    title: Entity<InputState>,
    date: Entity<DatePickerState>,
    /// What [`Self::apply_info`] put in the title.
    prefilled_title: Option<String>,
    language: Entity<InputState>,
    language_focus: Rc<RefCell<Option<FocusHandle>>>,
    handling: Handling,
    participants: Entity<ComboboxState<ParticipantList>>,
    errors: Vec<FieldError>,
    _participants_subscription: gpui::Subscription,
    focus: FocusHandle,
}

impl Focusable for ImportSheet {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl ImportSheet {
    pub fn new(
        state: Entity<AppState>,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (default_language, default_handling) = {
            let config = state.read(cx).config();
            (
                config.default_language.clone().unwrap_or_default(),
                config.import_handling,
            )
        };
        let title = cx.new(|cx| InputState::new(window, cx).placeholder("Title"));
        let date = cx.new(|cx| {
            DatePickerState::new(window, cx)
                .date_format("%Y-%m-%d")
                .time_precision(TimePrecision::Minute)
                .hour_cycle(HourCycle::H23)
        });
        let participants = cx.new(|cx| {
            ComboboxState::new(ParticipantList::default(), Vec::new(), window, cx)
                .multiple(true)
                .searchable(true)
        });
        let participants_subscription = cx.observe(&participants, |_, _, cx| cx.notify());
        let library = state.read(cx).library().clone();
        let read = cx
            .background_executor()
            .spawn(async move { load_people(&library) });
        let participant_state = participants.clone();
        cx.spawn_in(window, async move |_, cx| {
            let Ok(people) = read.await else { return };
            let _ = cx.update(|window, cx| {
                participant_state.update(cx, |picker, cx| {
                    let mut options = ParticipantList {
                        people,
                        ..ParticipantList::default()
                    };
                    options.update_query(&picker.query(cx));
                    picker.set_items(options, window, cx);
                    cx.notify();
                })
            });
        })
        .detach();
        let language = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Automatic")
                .default_value(default_language)
        });
        let runs = ImportRuns::global(cx);
        let mut sheet = Self {
            state,
            runs,
            paths,
            info: None,
            path_revision: 0,
            title,
            date,
            prefilled_title: None,
            language,
            language_focus: Rc::default(),
            handling: default_handling,
            participants,
            errors: Vec::new(),
            _participants_subscription: participants_subscription,
            focus: cx.focus_handle(),
        };
        sheet.read_paths(window, cx);
        sheet
    }

    pub(crate) fn initial_focus(&self, cx: &App) -> FocusHandle {
        if self.form(cx).single_file() {
            self.title.focus_handle(cx)
        } else if let Some(focus) = self.language_focus.borrow().as_ref().filter(|_| {
            self.info.is_some() && !self.manifest_only() && !self.form_items().is_empty()
        }) {
            focus.clone()
        } else {
            self.focus.clone()
        }
    }

    pub(crate) fn heading(&self) -> String {
        match self.form_items().as_slice() {
            [(path, PathInfo::Manifest { .. })] => {
                format!("Import manifest “{}”", file_label(path))
            }
            [(path, _)] => format!("Import “{}”", file_label(path)),
            items if !items.is_empty() => {
                let recordings = items
                    .iter()
                    .filter(|(_, kind)| matches!(kind, PathInfo::File { .. }))
                    .count();
                let folders = items
                    .iter()
                    .filter(|(_, kind)| matches!(kind, PathInfo::Folder { .. }))
                    .count();
                let manifests = items.len() - recordings - folders;
                let kinds = [
                    (recordings, "recording"),
                    (folders, "folder"),
                    (manifests, "import manifest"),
                ];
                let parts = kinds
                    .into_iter()
                    .filter(|(count, _)| *count > 0)
                    .map(|(count, kind)| {
                        format!("{count} {kind}{}", if count == 1 { "" } else { "s" })
                    })
                    .collect::<Vec<_>>();
                format!("Import {}", parts.join(" and "))
            }
            _ => "Import".to_owned(),
        }
    }

    fn form_items(&self) -> Vec<(&PathBuf, &PathInfo)> {
        importable_items(&self.paths, self.info.as_deref())
    }

    fn manifest_only(&self) -> bool {
        let items = self.form_items();
        !items.is_empty()
            && items
                .iter()
                .all(|(_, kind)| matches!(kind, PathInfo::Manifest { .. }))
    }

    fn add_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = pick_import_paths(
            PathPromptOptions {
                files: true,
                directories: false,
                multiple: true,
                prompt: Some("Add files".into()),
            },
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Some(paths)) = picked.await {
                let _ = this.update_in(cx, |this, window, cx| this.append_paths(paths, window, cx));
            }
        })
        .detach();
    }

    pub(crate) fn append_paths(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let before = self.paths.len();
        for path in paths {
            if !self.paths.contains(&path) {
                self.paths.push(path);
            }
        }
        if before != self.paths.len() {
            self.read_paths(window, cx);
        }
    }

    fn remove_path(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        self.paths.retain(|item| item != path);
        self.read_paths(window, cx);
    }

    fn read_paths(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.info = None;
        self.path_revision += 1;
        let revision = self.path_revision;
        let paths = self.paths.clone();
        let task = self
            .state
            .read(cx)
            .cli()
            .spawn(
                cx.background_executor(),
                async move { inspect_paths(&paths) },
            );
        cx.spawn_in(window, async move |this, cx| {
            let info = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.path_revision == revision {
                    this.apply_info(info, window, cx);
                }
            });
        })
        .detach();
        cx.notify();
    }

    fn apply_info(&mut self, info: Vec<PathInfo>, window: &mut Window, cx: &mut Context<Self>) {
        self.info = Some(info);
        if let [(path, PathInfo::File { .. })] = self.form_items().as_slice() {
            let title = default_title(path);
            let current_title = self.title.read(cx).value();
            if current_title.is_empty()
                || self.prefilled_title.as_deref() == Some(current_title.as_ref())
            {
                self.prefilled_title = Some(title.clone());
                self.title
                    .update(cx, |input, cx| input.set_value(title, window, cx));
            }
        }
        if self.focus.is_focused(window) {
            self.initial_focus(cx).focus(window, cx);
        }
        self.errors.clear();
        cx.notify();
        // The root dialog title uses the same inspected paths.
        window.refresh();
    }

    /// The form as currently filled in.
    pub fn form(&self, cx: &App) -> ImportForm {
        ImportForm {
            paths: self.paths.clone(),
            info: self.info.clone(),
            title: self.title.read(cx).value().to_string(),
            date: self
                .date
                .read(cx)
                .date_time()
                .start()
                .map(|date| date.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default(),
            prefilled_title: self.prefilled_title.clone(),
            language: self.language.read(cx).value().to_string(),
            handling: self.handling,
            participants: self.participants.read(cx).selected_values(),
        }
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if window.root::<Root>().flatten().is_some() {
            window.close_dialog(cx);
        }
    }

    fn submit_and_focus_problem(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.info.is_none() || self.form_items().is_empty() {
            return;
        }
        if self.submit(cx) {
            self.cancel(window, cx);
            return;
        }
        if let Some(error) = self.errors.first() {
            match error.field {
                Field::Title => self.title.focus_handle(cx).focus(window, cx),
                Field::Date => self.date.focus_handle(cx).focus(window, cx),
                Field::Participant(_) => self.participants.focus_handle(cx).focus(window, cx),
                Field::Language => self
                    .language_focus
                    .borrow()
                    .as_ref()
                    .unwrap_or(&self.focus)
                    .focus(window, cx),
                Field::Paths => self.focus.focus(window, cx),
            }
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) -> bool {
        match self.form(cx).validate() {
            Ok(requests) => {
                self.errors.clear();
                let runs = self.runs.clone();
                self.state
                    .update(cx, |state, cx| state.start_imports(requests, &runs, cx));
                true
            }
            Err(errors) => {
                self.errors = errors;
                cx.notify();
                false
            }
        }
    }

    fn set_handling(&mut self, handling: Handling, cx: &mut Context<Self>) {
        if self.handling != handling {
            self.handling = handling;
            cx.notify();
        }
    }

    // ---- rendering ----

    fn field_error(&self, field: Field, cx: &App) -> Option<Stateful<Div>> {
        // The sheet's own check of the field, not an error from a command.
        let problem = self.errors.iter().find(|e| e.field == field)?;
        Some(crate::ui::kit::status_line(
            // raw-text-allowed: an element id, never shown.
            SharedString::from(format!("import-error-{field:?}")),
            crate::ui::kit::StatusKind::Failed,
            problem.message.clone(),
            cx,
        ))
    }

    fn paths_list(&self, cx: &Context<Self>) -> Stateful<Div> {
        use gpui::assets::IconName;
        let mut list = div()
            .id("import-paths")
            .role(Role::List)
            .aria_label("Items to import")
            .flex()
            .flex_col()
            .gap_2();
        for (index, path) in self.paths.iter().enumerate() {
            let info = self.info.as_ref().and_then(|items| items.get(index));
            let name = file_label(path);
            let kind = match info {
                Some(PathInfo::Folder {
                    recordings: Some(count),
                }) => format!(
                    "Folder · {count} {}",
                    if *count == 1 {
                        "recording"
                    } else {
                        "recordings"
                    }
                ),
                Some(PathInfo::Folder { .. }) => "Folder".to_owned(),
                Some(PathInfo::File { bytes, .. }) => {
                    let video = path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .is_some_and(|ext| {
                            ["mp4", "mov", "m4v", "mkv", "webm"]
                                .contains(&ext.to_ascii_lowercase().as_str())
                        });
                    let kind = if video { "Video" } else { "Audio" };
                    format!("{kind} · {}", broscribe_core::display_units::bytes(*bytes))
                }
                Some(PathInfo::Manifest {
                    meetings: Some(count),
                }) => format!(
                    "Import manifest · {count} {}",
                    if *count == 1 { "meeting" } else { "meetings" }
                ),
                Some(PathInfo::Manifest { .. }) => "Import manifest".to_owned(),
                Some(PathInfo::Missing) => "Not found".to_owned(),
                Some(PathInfo::Unsupported) => "Skipped: not an audio or video file".to_owned(),
                None => "Reading…".to_owned(),
            };
            let icon = if matches!(info, Some(PathInfo::Folder { .. })) {
                IconName::FolderOpen
            } else {
                IconName::File
            };
            let target = path.clone();
            list = list.child(
                div()
                    .id(("import-path", index))
                    .role(Role::ListItem)
                    .aria_label(format!("{name} · {kind}"))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        ui::icons::icon(icon)
                            .size(px(16.))
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .text_role(TextRole::Label)
                                    .child(name.clone()),
                            )
                            .child(
                                div()
                                    .text_role(TextRole::Caption)
                                    .text_color(cx.theme().muted_foreground)
                                    .child(kind),
                            ),
                    )
                    .child(
                        Button::new(("remove-import-path", index))
                            .debug_selector(move || format!("remove-import-path-{index}"))
                            .xsmall()
                            .icon(ui::icons::icon(IconName::Close))
                            .ghost()
                            .accessibility_label(format!("Remove {name}"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.remove_path(&target, window, cx)
                            })),
                    ),
            );
        }
        list.children(self.field_error(Field::Paths, cx))
    }

    fn handling_choice(&self, cx: &mut Context<Self>) -> Div {
        use crate::views::settings::{recording_file_choices, recording_file_consequence};
        let manifest_only = self.manifest_only();
        let handling = if manifest_only && self.handling == Handling::Move {
            Handling::Copy
        } else {
            self.handling
        };
        crate::ui::kit::field(
            "import-storage-label",
            "Recording file",
            Some((
                "import-hint-storage".into(),
                recording_file_consequence(handling).into(),
            )),
            |_| {
                recording_file_choices(
                    "import-handling",
                    "Recording file",
                    handling,
                    !manifest_only,
                    cx,
                    |this, value, cx| this.set_handling(value, cx),
                )
            },
            cx,
        )
    }

    fn participants_section(&self, cx: &Context<Self>) -> Div {
        let tokens = self
            .participants
            .read(cx)
            .selection()
            .iter()
            .map(|(index, person)| {
                let index = *index;
                let combo = self.participants.clone();
                let label = person.value.clone();
                Button::new(("import-participant-token", index.row))
                    .xsmall()
                    .outline()
                    .label(label.clone())
                    .icon(ui::icons::icon(gpui::assets::IconName::Close))
                    .accessibility_label(format!("Remove {label}"))
                    .on_click(move |_, _, cx| {
                        combo.update(cx, |combo, cx| {
                            combo.remove_selected_index(index, cx);
                        });
                    })
            })
            .collect::<Vec<_>>();
        ui::kit::field(
            "import-participants-label",
            "Participants",
            Some(("import-hint-participants".into(),
                "Who attended. Applied to every recording. It helps name speakers but never confirms a voice.".into())),
            |_| Combobox::new(&self.participants)
                .placeholder("Add participants")
                .search_placeholder("Search People or type a name or email")
                .empty(|_, _| "No people yet. Type a name to add a participant.")
                .render_trigger(|_, _, _| {
                    text_line("import-add-participants", Role::Label, "Add participants")
                }),
            cx,
        ).child(div().flex().flex_wrap().gap_1().children(tokens))
    }
}

impl Render for ImportSheet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let single = self.form(cx).single_file();
        let loading = self.info.is_none();
        let empty = self.form_items().is_empty();
        let manifest_only = self.manifest_only();
        let paths = self.paths_list(cx);
        let title = ui::kit::field(
            "import-title-label",
            "Title",
            None,
            |label| Input::new(&self.title).aria_label(label),
            cx,
        )
        .children(self.field_error(Field::Title, cx));
        let modified = match self.form_items().as_slice() {
            [(_, PathInfo::File { modified, .. })] => *modified,
            _ => None,
        };
        let date_hint = modified.map_or_else(
            || "Leave empty to use the recording’s own creation time, or the file’s date.".to_owned(),
            |time| format!("Leave empty to use the recording’s own creation time, or the file’s date ({}).", default_date(time)),
        );
        let date = ui::kit::field(
            "import-date-label",
            "Date",
            Some(("import-hint-date".into(), date_hint.into())),
            |_| {
                DatePicker::new(&self.date)
                    .placeholder("2026-11-20 14:30")
                    .cleanable(true)
            },
            cx,
        )
        .children(self.field_error(Field::Date, cx));
        let language = ui::kit::field(
            "import-language-label",
            "Language",
            None,
            |label| {
                ui::select::Picker::languages(
                    "import-language-picker",
                    label,
                    &[],
                    &self.language.read(cx).value(),
                    cx,
                    |this, value, window, cx| {
                        this.language.update(cx, |input, cx| {
                            input.set_value(value.to_owned(), window, cx)
                        });
                        this.errors.retain(|error| error.field != Field::Language);
                        cx.notify();
                    },
                )
                .track_control_focus(self.language_focus.clone())
            },
            cx,
        )
        .children(self.field_error(Field::Language, cx));
        let handling = self.handling_choice(cx);
        let participants = self.participants_section(cx);
        let theme = cx.theme();
        div()
            .id("import-sheet")
            .role(Role::Group)
            .aria_label(self.heading())
            .debug_selector(|| "import-sheet".into())
            .key_context("BroScribeImport")
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let editing = this.title.focus_handle(cx).is_focused(window) || this.focus.is_focused(window);
                if ev.keystroke.key == "enter" && editing && !ev.keystroke.modifiers.modified() {
                    this.submit_and_focus_problem(window, cx);
                    cx.stop_propagation();
                }
            }))
            .on_action(cx.listener(|this, _: &SubmitImport, window, cx| {
                this.submit_and_focus_problem(window, cx);
                cx.stop_propagation();
            }))
            .on_drop(
                cx.listener(|this, paths: &gpui::ExternalPaths, window, cx| {
                    this.append_paths(dropped_paths(paths), window, cx);
                    cx.stop_propagation();
                }),
            )
            .flex()
            .flex_col()
            .flex_1()
            .size_full()
            .gap_2()
            .p_4()
            .max_w(px(640.))
            .child(
                div()
                    .id("import-sheet-body")
                    .debug_selector(|| "import-sheet-body".into())
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .gap_3()
                    .child(Button::new("import-add-files").label("Add files…").outline().self_start()
                        .on_click(cx.listener(|this, _, window, cx| this.add_files(window, cx))))
                    .child(paths)
                    .when(!loading && empty, |body| body.child(ui::kit::status_line(
                        "import-empty", ui::kit::StatusKind::Failed,
                        "None of these items is a recording, a folder or an import manifest.", cx)))
                    .when(single, |body| body.child(title).child(date))
                    .when(!manifest_only, |body| body.child(language))
                    .child(handling)
                    .when(!manifest_only, |body| body.child(participants))
                    .when(manifest_only, |body| body.child(text_line("import-manifest-help", Role::Label,
                        "An import manifest is a file that lists recordings to import, made by a converter.").text_role(TextRole::Caption)))
                    .when(loading, |body| {
                        body.child(crate::ui::kit::status_line(
                            "import-reading",
                            crate::ui::kit::StatusKind::Pending,
                            "Reading items…",
                            cx,
                        ))
                    })
                    .child(
                        text_line(
                            "import-transcription-note",
                            Role::Label,
                            "Import includes transcription and can take several minutes.",
                        )
                        .text_role(TextRole::Caption),
                    ),
            )
            .child(
                div().id("import-footer").debug_selector(|| "import-footer".into())
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .pt_2()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("import-cancel")
                            .label("Cancel")
                            .accessibility_label("Cancel import")
                            .outline()
                            .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))),
                    )
                    .child(
                        Button::new("import-submit")
                            .label("Import")
                            .accessibility_label("Start import")
                            .map(|button| crate::ui::kit::primary(button, loading || empty, cx))
                            .tooltip_with_action("Import", &SubmitImport, Some("BroScribeImport"))
                            .disabled(loading || empty)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_and_focus_problem(window, cx);
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(paths: &[&str], info: Vec<PathInfo>) -> ImportForm {
        ImportForm {
            paths: paths.iter().map(PathBuf::from).collect(),
            info: Some(info),
            title: String::new(),
            date: String::new(),
            prefilled_title: None,
            language: String::new(),
            handling: Handling::Copy,
            participants: Vec::new(),
        }
    }

    #[gpui::test]
    fn inspects_types_skips_non_media_and_submits_generic_manifests(cx: &mut gpui::TestAppContext) {
        use broscribe_platform::fakes::{FakeImportFileDrop, FakeImportPathPicker};
        let dir = tempfile::tempdir().unwrap();
        let recording = dir.path().join("recording.WAV");
        let folder = dir.path().join("Recordings");
        let unsupported = dir.path().join("notes.pdf");
        let missing = dir.path().join("missing.wav");
        let manifest = dir.path().join("team-export.json");
        let hidden = dir.path().join(".hidden.wav");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("inside.wav"), []).unwrap();
        for file in [&recording, &unsupported, &hidden] {
            std::fs::write(file, [0; 12]).unwrap();
        }
        std::fs::write(
            &manifest,
            serde_json::json!({
                "format": MANIFEST_FORMAT, "version": 2, "meetings": [{}, {}],
            })
            .to_string(),
        )
        .unwrap();
        let picked = FakeImportPathPicker(Some(vec![
            recording.clone(),
            folder.clone(),
            manifest.clone(),
        ]));
        let paths = futures::executor::block_on(picked.pick(ImportPathRequest {
            files: true,
            directories: true,
            multiple: true,
            prompt: None,
        }))
        .unwrap()
        .unwrap();
        let paths = unique_paths(&FakeImportFileDrop(
            paths
                .into_iter()
                .chain([recording.clone(), unsupported, hidden, missing])
                .collect(),
        ));
        let info = inspect_paths(&paths);
        assert!(matches!(info[0], PathInfo::File { bytes: 12, .. }));
        assert_eq!(
            info[1],
            PathInfo::Folder {
                recordings: Some(1)
            }
        );
        assert_eq!(info[2], PathInfo::Manifest { meetings: Some(2) });
        assert_eq!(
            info[3..],
            [
                PathInfo::Unsupported,
                PathInfo::Unsupported,
                PathInfo::Missing
            ]
        );
        let (state, _, _) = crate::views::person_picker::fixtures::state(cx, dir.path());
        let (sheet, visual) =
            cx.add_window_view(|window, cx| ImportSheet::new(state.clone(), paths, window, cx));
        visual.run_until_parked();
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            for name in [
                "recording.WAV · Audio · 12 B",
                "Recordings · Folder · 1 recording",
                "team-export.json · Import manifest · 2 meetings",
                "notes.pdf · Skipped: not an audio or video file",
                "missing.wav · Not found",
            ] {
                assert!(
                    tree.nodes
                        .iter()
                        .any(|node| node.role == "ListItem" && node.name == name),
                    "{name}"
                );
            }
            let requests = sheet.read(cx).form(cx).validate().unwrap();
            assert_eq!(requests.len(), 3);
            let manifest = &requests[2];
            assert!(manifest.manifest);
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&manifest.spec().unwrap()).unwrap(),
                serde_json::json!({"manifest": manifest.path.to_str().unwrap()})
            );
            let argv = manifest.args();
            assert!(argv.iter().any(|arg| arg == "--spec-stdin"));
            assert!(
                argv.iter().all(|arg| arg != manifest.path.as_os_str()
                    && arg != "--date"
                    && arg != "--lang")
            );
        });
        let (_, manifest_ui) =
            cx.add_window_view(|window, cx| ImportSheet::new(state, vec![manifest], window, cx));
        manifest_ui.run_until_parked();
        manifest_ui.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("import-sheet")).1,
                "Import manifest “team-export.json”"
            );
            for id in [
                "import-title-label",
                "import-date-label",
                "import-language-label",
                "import-participants-label",
                "handling-move",
            ] {
                assert!(tree.all(id).is_empty(), "{id}");
            }
            assert!(tree.nodes.iter().any(|node| node.name == "Keep in place"));
        });
    }

    #[gpui::test]
    fn participant_tokens_use_people_matching_and_keep_distinct_selection(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::component::searchable_list::SearchableListChange;
        let dir = tempfile::tempdir().unwrap();
        let (_, lib, _) = crate::views::person_picker::fixtures::state(cx, dir.path());
        let mut list = ParticipantList {
            people: load_people(&lib).unwrap(),
            ..Default::default()
        };
        let mut selected = Vec::new();
        list.update_query("ze");
        assert_eq!(list.shown[0].label.as_ref(), "José Pereira");
        list.on_will_change(
            &mut selected,
            &[SearchableListChange::Select {
                index: IndexPath::new(0),
            }],
        );
        list.update_query("Carla Nova <carla@example.org>");
        assert_eq!(
            list.shown[0].label.as_ref(),
            "Add “Carla Nova <carla@example.org>”"
        );
        list.on_will_change(
            &mut selected,
            &[SearchableListChange::Select {
                index: IndexPath::new(0),
            }],
        );
        assert_eq!(selected.len(), 2);
        assert_ne!(selected[0].0, selected[1].0);
        list.on_will_change(
            &mut selected,
            &[SearchableListChange::Deselect {
                index: IndexPath::new(0),
            }],
        );
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].1.label.as_ref(), "José Pereira");
        list.update_query("Carla Nova <invalid>");
        assert!(list.shown[0].disabled());
        assert_eq!(list.shown[0].label.as_ref(), PARTICIPANT_HELP);
    }

    #[gpui::test]
    fn modal_drop_keeps_the_meeting_and_drafts_and_restores_the_opener(
        cx: &mut gpui::TestAppContext,
    ) {
        use broscribe_platform::fakes::{FakeMicCapture, FakeSystemInfo};
        use gpui::{FileDropEvent, point};
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let (state, _, fixture) = crate::views::person_picker::fixtures::state(cx, dir.path());
        let first = dir.path().join("first.wav");
        let second = dir.path().join("second.wav");
        let unsupported = dir.path().join("skip.pdf");
        for path in [&first, &second, &unsupported] {
            std::fs::write(path, [0; 12]).unwrap();
        }
        let deps = super::super::setup::SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        let (main, visual) = {
            let main = Rc::new(RefCell::new(None));
            let capture = main.clone();
            let host = state.clone();
            let (_, visual) = cx.add_window_view(move |window, cx| {
                let view = cx.new(|cx| ui::MainWindow::new(host, deps, window, cx));
                *capture.borrow_mut() = Some(view.clone());
                Root::new(view, window, cx)
            });
            let view = main.borrow().as_ref().unwrap().clone();
            (view, visual)
        };
        let route = crate::state::Route::meeting(fixture.meeting);
        state.update(visual, |state, cx| state.navigate(route.clone(), cx));
        visual.run_until_parked();
        let meeting = main.read_with(visual, |main, _| main.meeting_view().unwrap().clone());
        let opener = visual.update(|window, cx| {
            let focus = meeting.read(cx).detail_focus().clone();
            focus.focus(window, cx);
            focus
        });
        visual.refresh().unwrap();
        let position = point(px(280.), px(160.));
        let enter = |path: &Path| FileDropEvent::Entered {
            position,
            paths: gpui::ExternalPaths([path.to_path_buf()].into_iter().collect()),
        };
        visual.simulate_event(enter(&unsupported));
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(tree.read(tree.find("drop-hint-title")).0, "Alert");
        });
        visual.simulate_event(FileDropEvent::Exited);
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert!(tree.all("drop-hint-title").is_empty());
        });

        visual.simulate_event(enter(&first));
        let redraws = Rc::new(std::cell::Cell::new(0));
        let subscription = visual.update(|_, cx| {
            let redraws = redraws.clone();
            cx.observe(&main, move |_, _| redraws.set(redraws.get() + 1))
        });
        visual.simulate_event(FileDropEvent::Pending { position });
        visual.simulate_event(FileDropEvent::Pending { position });
        assert_eq!(redraws.get(), 0);
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("drop-hint-title")),
                ("Status", "Drop to import")
            );
            assert!(opener.is_focused(window));
        });
        visual.simulate_event(FileDropEvent::Submit { position });
        visual.refresh().unwrap();
        assert_eq!(
            main.read_with(visual, |main, _| main.meeting_view().unwrap().entity_id()),
            meeting.entity_id()
        );
        let sheet = main.read_with(visual, |main, _| main.open_import_for_test().unwrap());
        let body = visual.debug_bounds("import-sheet-body").unwrap();
        let footer = visual.debug_bounds("import-footer").unwrap();
        assert!(body.bottom() <= footer.top());
        sheet.update_in(visual, |sheet, window, cx| {
            sheet
                .title
                .update(cx, |title, cx| title.set_value("Draft title", window, cx));
        });
        let position = visual.debug_bounds("import-sheet").unwrap().center();
        visual.simulate_event(FileDropEvent::Entered {
            position,
            paths: gpui::ExternalPaths([second.clone(), first.clone()].into_iter().collect()),
        });
        visual.simulate_event(FileDropEvent::Submit { position });
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert!(tree.all("import-title-label").is_empty());
            assert!(tree.all("import-date-label").is_empty());
            assert!(tree.all("drop-hint-title").is_empty());
            assert_eq!(sheet.read(cx).paths, [first.clone(), second.clone()]);
        });
        let remove = visual.debug_bounds("remove-import-path-1").unwrap();
        visual.simulate_click(remove.center(), Default::default());
        visual.run_until_parked();
        assert_eq!(
            sheet.read_with(visual, |sheet, cx| sheet.form(cx).title),
            "Draft title"
        );
        visual.simulate_keystrokes("escape");
        visual.update(|window, cx| {
            assert!(!window.has_active_dialog(cx));
            assert!(opener.is_focused(window));
            assert_eq!(state.read(cx).route(), &route);
        });
        drop(subscription);
    }

    #[gpui::test]
    fn import_validation_and_reading_leave_focus_where_it_belongs(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.wav");
        std::fs::write(&path, []).unwrap();
        let (state, _, _) = crate::views::person_picker::fixtures::state(cx, dir.path());
        let (sheet, visual) =
            cx.add_window_view(|window, cx| ImportSheet::new(state, vec![path], window, cx));
        visual.run_until_parked();
        visual.update(|window, cx| {
            crate::ui::a11y_tree::draw_and_read(window, cx);
            sheet.update(cx, |sheet, cx| {
                let date = sheet.date.focus_handle(cx);
                date.focus(window, cx);
                sheet.read_paths(window, cx);
                sheet.submit_and_focus_problem(window, cx);
                assert!(sheet.runs.read(cx).runs().is_empty());
                assert!(date.is_focused(window));
            });
            crate::ui::a11y_tree::draw_and_read(window, cx);
            let submit = gpui::base::test_support::snapshots(window)
                .into_iter()
                .find(|node| node.label() == Some("Start import"))
                .unwrap();
            assert_eq!(submit.disabled(), Some(true));
            sheet.update(cx, |sheet, cx| {
                let date = sheet.date.focus_handle(cx);
                sheet.apply_info(
                    vec![PathInfo::File {
                        modified: None,
                        bytes: 0,
                    }],
                    window,
                    cx,
                );
                assert!(date.is_focused(window));
                sheet.language.update(cx, |input, cx| {
                    input.set_value("invalid language", window, cx)
                });
                sheet.submit_and_focus_problem(window, cx);
                assert_eq!(sheet.errors[0].field, Field::Language);
                assert!(
                    sheet
                        .language_focus
                        .borrow()
                        .as_ref()
                        .unwrap()
                        .is_focused(window)
                );
            });
        });
    }

    #[gpui::test]
    fn waiting_imports_cancel_without_stopping_the_active_run(cx: &mut gpui::TestAppContext) {
        let runs = cx.update(ImportRuns::global);
        runs.update(cx, |runs, cx| {
            for name in ["one.wav", "two.wav", "three.wav", "four.wav"] {
                runs.add(
                    ImportRequest {
                        path: name.into(),
                        handling: Handling::Copy,
                        title: None,
                        date: None,
                        language: None,
                        participants: Vec::new(),
                        manifest: false,
                    },
                    cx,
                );
            }
            let (active, request) = runs.start_next(cx).unwrap();
            assert_eq!(
                runs.progress().map(|(n, total, _)| (n, total)),
                Some((1, 4))
            );
            let waiting = runs.runs()[1].id;
            runs.cancel_waiting(Some(active), cx);
            runs.cancel_waiting(Some(waiting), cx);
            assert_eq!(
                runs.progress().map(|(n, total, _)| (n, total)),
                Some((1, 3))
            );
            runs.cancel_waiting(None, cx);
            assert!(runs.start_next(cx).is_none());
            assert_eq!(runs.runs()[0].status, ImportRunStatus::Running);
            runs.finish(
                active,
                ImportRunStatus::Failed("Synthetic failure.".into()),
                cx,
            );
            assert_eq!(runs.retry_request(active), Some(request));
            assert!(runs.retry_request(waiting).is_none());
        });
    }

    #[gpui::test]
    fn return_and_native_modifier_return_submit_from_the_import_form(
        cx: &mut gpui::TestAppContext,
    ) {
        use broscribe_platform::HostPlatform;
        use gpui::{OperatingSystem, PlatformInfo, PlatformTarget};
        let dir = tempfile::tempdir().unwrap();
        let (state, _, _) = crate::views::person_picker::fixtures::state(cx, dir.path());
        let runs = cx.update(ImportRuns::global);
        let cases = [
            (OperatingSystem::Mac, HostPlatform::MacOs, "enter"),
            (OperatingSystem::Mac, HostPlatform::MacOs, "cmd-enter"),
            (
                OperatingSystem::Windows,
                HostPlatform::Windows,
                "ctrl-enter",
            ),
            (OperatingSystem::Linux, HostPlatform::Linux, "ctrl-enter"),
        ];
        for (i, (os, host, stroke)) in cases.into_iter().enumerate() {
            cx.set_platform_info(PlatformInfo {
                os,
                target: PlatformTarget::Native,
            });
            cx.update(|cx| cx.bind_keys(ui::keymap::key_bindings_for(host, cx)));
            let path = dir.path().join(format!("recording-{i}.wav"));
            std::fs::write(&path, [0; 12]).unwrap();
            let (sheet, visual) = cx.add_window_view(|window, cx| {
                ImportSheet::new(state.clone(), vec![path.clone()], window, cx)
            });
            visual.run_until_parked();
            visual.update(|window, cx| {
                sheet.read(cx).title.focus_handle(cx).focus(window, cx);
            });
            visual.refresh().unwrap();
            visual.simulate_keystrokes(stroke);
            assert_eq!(
                runs.read_with(visual, |runs, _| runs.runs().len()),
                i + 1,
                "{os:?} {stroke}"
            );
            assert_eq!(
                runs.read_with(visual, |runs, _| runs.runs().last().unwrap().path.clone()),
                path
            );
        }
    }

    #[gpui::test]
    fn import_status_collapses_details_and_binds_actions_to_each_run(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::views::library::{LibraryEvent, LibraryView};
        let dir = tempfile::tempdir().unwrap();
        let (state, _, fixture) = crate::views::person_picker::fixtures::state(cx, dir.path());
        let (lib, visual) =
            cx.add_window_view(|window, cx| LibraryView::new(state.clone(), window, cx));
        let runs = visual.update(|_, cx| ImportRuns::global(cx));
        let (active, failed) = runs.update(visual, |runs, cx| {
            for name in ["success.wav", "failure.wav", "waiting.wav"] {
                runs.add(
                    ImportRequest {
                        path: name.into(),
                        handling: Handling::Copy,
                        language: None,
                        title: None,
                        date: None,
                        participants: Vec::new(),
                        manifest: false,
                    },
                    cx,
                );
            }
            (runs.start_next(cx).unwrap().0, runs.runs()[1].id)
        });
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("library-import-summary")),
                ("Status", "Importing 1 of 3 · success.wav")
            );
            assert_eq!(
                tree.read(tree.find("library-import-progress")).1,
                "Imports: 0 of 3 finished"
            );
            assert!(tree.all(&format!("library-import-{active}")).is_empty());
            assert!(
                tree.read(tree.find("library-import-wait"))
                    .1
                    .contains("several minutes")
            );
        });
        lib.update_in(visual, |lib, window, cx| lib.focus_imports(window, cx));
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find(&format!("library-import-{active}"))).1,
                "Importing and transcribing success.wav…"
            );
        });
        runs.update(visual, |runs, cx| {
            runs.finish(
                active,
                ImportRunStatus::Done(ImportOutcome(serde_json::json!({
                    "meeting_id": fixture.meeting.as_str(), "already_exists": true,
                }))),
                cx,
            );
            assert_eq!(runs.start_next(cx).unwrap().0, failed);
            runs.finish(
                failed,
                ImportRunStatus::Failed("Synthetic failure.".into()),
                cx,
            );
            runs.cancel_waiting(None, cx);
        });
        visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("library-import-summary")).1,
                "Imported 1 of 2 · 1 failed"
            );
            assert!(tree.all("library-import-progress").is_empty());
            assert_eq!(
                tree.read(tree.find(&format!("import-open-{active}"))).1,
                "Open meeting imported from success.wav"
            );
            assert_eq!(
                tree.read(tree.find(&format!("import-retry-{failed}"))).1,
                "Try importing failure.wav again"
            );
        });
        let opened = Rc::new(RefCell::new(None));
        let observed = opened.clone();
        let _subscription = visual.update(|_, cx| {
            cx.subscribe(&lib, move |_, event: &LibraryEvent, _| {
                let LibraryEvent::SelectMeeting { id, .. } = event;
                *observed.borrow_mut() = Some(id.clone());
            })
        });
        let open = visual.debug_bounds("import-open-meeting").unwrap();
        visual.simulate_click(open.center(), Default::default());
        assert_eq!(opened.borrow().as_ref(), Some(&fixture.meeting));
        let retry = visual.debug_bounds("import-retry-run").unwrap();
        visual.simulate_click(retry.center(), Default::default());
        assert!(runs.read_with(visual, |runs, _| {
            runs.runs()
                .iter()
                .any(|run| run.id != failed && run.path == Path::new("failure.wav"))
        }));
    }

    #[test]
    fn file_and_folder_requests_preserve_the_language_choice() {
        let paths = ["/synthetic/clip.wav", "/synthetic/folder"];
        let mut input = form(
            &paths,
            vec![
                PathInfo::File {
                    modified: None,
                    bytes: 0,
                },
                PathInfo::Folder {
                    recordings: Some(2),
                },
            ],
        );
        for request in input.validate().unwrap() {
            assert!(request.args().iter().any(|arg| arg == "--auto-language"));
        }
        input.language = "pt-BR".into();
        for request in input.validate().unwrap() {
            assert!(
                request
                    .args()
                    .windows(2)
                    .any(|pair| pair[0] == "--lang" && pair[1] == "pt-BR")
            );
        }
    }

    #[test]
    fn parses_participants() {
        let p = |s| parse_participant(s).unwrap().unwrap();
        assert_eq!(
            p(" Ana Lima <ana@example.org> "),
            ParticipantSpec {
                name: Some("Ana Lima".into()),
                email: Some("ana@example.org".into())
            }
        );
        assert_eq!(p("Ana").email, None);
        assert_eq!(p("<ana@example.org>").name, None);
        assert_eq!(
            p("ana@example.org").email.as_deref(),
            Some("ana@example.org")
        );
        assert_eq!(p("Ana <ana@example.org>").to_arg(), "Ana <ana@example.org>");
        assert_eq!(p("ana@example.org").to_arg(), "<ana@example.org>");
        assert_eq!(parse_participant("   ").unwrap(), None);
        for bad in [
            "Ana <ana@example.org",
            "Ana ana@example.org>",
            "Ana <not-an-email>",
            "Ana <a@b> trailing",
            "<>",
        ] {
            assert!(parse_participant(bad).is_err(), "{bad}");
        }
    }

    #[gpui::test]
    fn recording_file_options_are_named_checked_and_apply_to_the_import(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::views::person_picker::fixtures;

        let dir = tempfile::tempdir().unwrap();
        let (state, _, _) = fixtures::state(cx, dir.path());
        let path = dir.path().join("synthetic.wav");
        std::fs::write(&path, []).unwrap();
        let (sheet, visual) =
            cx.add_window_view(|window, cx| ImportSheet::new(state, vec![path], window, cx));
        visual.run_until_parked();

        let inspect = |visual: &mut gpui::VisualTestContext, checked: &str| {
            visual.update(|window, cx| {
                let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
                let group = tree.find("import-handling");
                assert_eq!(tree.read(group), ("RadioGroup", "Recording file"));
                let radio_ids = ["handling-copy", "handling-move", "handling-reference"];
                for id in radio_ids {
                    let index = tree.find(id);
                    assert!(tree.subtree(group).contains(&index));
                    assert_eq!(tree.nodes[index].role, "RadioButton");
                }
                let mut nodes = gpui::base::test_support::snapshots(window)
                    .into_iter()
                    .filter(|node| node.role() == Some(Role::RadioButton))
                    .collect::<Vec<_>>();
                nodes.sort_by_key(|node| node.bounds().top());
                assert_eq!(
                    nodes
                        .iter()
                        .map(|node| node.label().unwrap())
                        .collect::<Vec<_>>(),
                    [
                        "Copy into the library",
                        "Move into the library",
                        "Keep in place"
                    ]
                );
                assert_eq!(
                    nodes
                        .iter()
                        .filter(|node| node.checked() == Some(true))
                        .map(|node| node.label().unwrap())
                        .collect::<Vec<_>>(),
                    [checked]
                );
                nodes[1].bounds().center()
            })
        };
        let point = inspect(visual, "Copy into the library");
        visual.simulate_click(point, Default::default());
        inspect(visual, "Move into the library");
        assert_eq!(
            sheet.read_with(visual, |sheet, cx| sheet.form(cx).validate().unwrap()[0]
                .handling),
            Handling::Move
        );
    }

    #[test]
    fn language_and_date_checks() {
        for ok in ["en", "pt-BR", "pt-PT", "zh-Hant-TW", "yue"] {
            assert!(is_language_tag(ok), "{ok}");
        }
        for bad in ["", "e", "english", "pt_BR", "pt-", "pt-BRAZILIAN1"] {
            assert!(!is_language_tag(bad), "{bad}");
        }
        assert!(is_import_date("2026-11-14"));
        assert!(is_import_date("2026-11-14 09:30"));
        assert!(is_import_date("2026-11-14T09:30:00-03:00"));
        assert!(!is_import_date("14/11/2026"));
        assert!(!is_import_date("yesterday"));
    }

    /// The sheet's actual import children: flags in argv, while the path,
    /// title and participants (names and emails) travel only in the stdin
    /// spec; handling, language and date keep their flags.
    #[cfg(unix)]
    #[gpui::test]
    fn import_children_keep_identity_data_off_argv(cx: &mut gpui::TestAppContext) {
        use std::os::unix::fs::PermissionsExt as _;

        use crate::views::person_picker::fixtures;

        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("synthetic-cli");
        let d = dir.path().display();
        std::fs::write(
            &cli,
            format!(
                "#!/bin/sh\n[ \"$4\" = import ] || {{ printf '{{}}'; exit 0; }}\n\
                 printf '%s\\n' \"$*\" >> '{d}/args.log'\n\
                 cat >> '{d}/stdin.log'\nprintf '\\n' >> '{d}/stdin.log'\n\
                 printf '{{\"job_id\":\"j-synthetic\"}}'\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (state, lib, _) = fixtures::state_with_cli(cx, dir.path(), cli);
        let file = dir.path().join("synthetic-marker-path.m4a");
        let folder = dir.path().join("synthetic-marker-folder");
        let mut single = form(
            &[file.to_str().unwrap()],
            vec![PathInfo::File {
                modified: None,
                bytes: 0,
            }],
        );
        single.title = " synthetic-marker-title ".into();
        single.date = "2026-11-14 09:30".into();
        single.language = "pt-BR".into();
        single.handling = Handling::Reference;
        single.participants = vec![
            "synthetic-marker-name <synthetic-marker@example.org>".into(),
            String::new(),
            "synthetic-marker-only@example.org".into(),
        ];
        let mut whole = form(
            &[folder.to_str().unwrap()],
            vec![PathInfo::Folder { recordings: None }],
        );
        whole.participants = vec!["synthetic-marker-other".into()];
        let mut requests = single.validate().unwrap();
        requests.extend(whole.validate().unwrap());
        let runs = cx.update(ImportRuns::global);
        state.update(cx, |s, cx| s.start_imports(requests, &runs, cx));
        cx.run_until_parked();
        let done =
            ImportRunStatus::Done(ImportOutcome(serde_json::json!({"job_id": "j-synthetic"})));
        runs.read_with(cx, |r, _| {
            assert_eq!(r.runs().len(), 2);
            assert!(
                r.runs().iter().all(|run| run.status == done),
                "{:?}",
                r.runs()
            );
        });
        let args = std::fs::read_to_string(dir.path().join("args.log")).unwrap();
        let root = lib.root().display();
        assert_eq!(
            args.lines().collect::<Vec<_>>(),
            [
                format!(
                    "--library {root} --json import --handling reference --lang pt-BR --date 2026-11-14 09:30 --spec-stdin"
                ),
                format!(
                    "--library {root} --json import --handling copy --auto-language --spec-stdin"
                ),
            ]
        );
        assert!(!args.contains("synthetic-marker"), "{args}");
        let specs: Vec<serde_json::Value> = std::fs::read_to_string(dir.path().join("stdin.log"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            specs,
            [
                serde_json::json!({
                    "path": file.to_str().unwrap(),
                    "title": "synthetic-marker-title",
                    "participants": [
                        "synthetic-marker-name <synthetic-marker@example.org>",
                        "<synthetic-marker-only@example.org>"
                    ]
                }),
                serde_json::json!({
                    "path": folder.to_str().unwrap(),
                    "title": null,
                    "participants": ["synthetic-marker-other"]
                }),
            ]
        );
    }

    #[test]
    fn several_items_ignore_title_and_date() {
        let mut f = form(
            &["/rec/a.m4a", "/rec/folder"],
            vec![
                PathInfo::File {
                    modified: None,
                    bytes: 0,
                },
                PathInfo::Folder { recordings: None },
            ],
        );
        f.title = "Ignored".into();
        f.date = "not a date".into();
        let requests = f.validate().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests
                .iter()
                .all(|r| r.title.is_none() && r.date.is_none())
        );
        assert_eq!(requests[1].path, PathBuf::from("/rec/folder"));
    }

    #[test]
    fn invalid_fields_are_reported_per_field() {
        let mut f = form(
            &["/rec/a.m4a"],
            vec![PathInfo::File {
                modified: None,
                bytes: 0,
            }],
        );
        f.date = "tomorrow".into();
        f.language = "Portuguese".into();
        f.participants = vec!["ok".into(), "Bad <x>".into()];
        let errors = f.validate().unwrap_err();
        let fields: Vec<Field> = errors.iter().map(|e| e.field).collect();
        assert_eq!(
            fields,
            [Field::Date, Field::Language, Field::Participant(1)]
        );

        let missing = form(&["/gone.m4a"], vec![PathInfo::Missing]);
        assert_eq!(missing.validate().unwrap_err()[0].field, Field::Paths);
        let mut loading = form(&["/rec/a.m4a"], Vec::new());
        loading.info = None;
        assert_eq!(loading.validate().unwrap_err()[0].field, Field::Paths);
    }

    #[test]
    fn defaults_from_file_name() {
        assert_eq!(
            default_title(Path::new("/rec/Weekly sync.m4a")),
            "Weekly sync"
        );
        assert_eq!(default_title(Path::new("/rec/notes")), "notes");
        let date =
            default_date(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(86_400 * 365));
        assert!(is_import_date(&date), "{date}");
    }

    #[test]
    fn import_outcomes_keep_meeting_ids_off_screen_and_keep_them_for_open() {
        let success = ImportOutcome(serde_json::json!({
            "meeting_id": "m-00000a", "title": "Synthetic standup",
            "duration_s": 420, "speakers": 3, "auto_named": 2,
            "notes": ["--lang en has no effect on recognition"]
        }));
        assert_eq!(success.meetings().next().unwrap().0.as_str(), "m-00000a");
        assert_eq!(
            success.words("standup.wav"),
            "Imported “Synthetic standup” · 7 min · 2 speakers named automatically, 1 needs a name. The speech model detects the language automatically."
        );
        let missing_title = ImportOutcome(serde_json::json!({"meeting_id": "m-00000a"}));
        assert_eq!(missing_title.words("standup.wav"), "Imported “standup.wav”");
        let existing =
            ImportOutcome(serde_json::json!({"meeting_id": "m-00000a", "already_exists": true}));
        assert_eq!(
            existing.words("standup.wav"),
            "standup.wav is already in the library."
        );
        assert_eq!(
            existing.meetings().next().map(|(id, _)| id),
            success.meetings().next().map(|(id, _)| id)
        );
        let folder = ImportOutcome(serde_json::json!({
            "imported": [
                {"meeting_id": "m-00000a", "title": "First recording"},
                {"meeting_id": "m-00000b", "source_path": "/private/synthetic/next.wav"}
            ],
            "failed": [{"path": "/private/synthetic/zq9.wav",
                "code": "synthetic_zq9_code", "message": "invalid input: /private/synthetic/zq9.wav is not a recording"}],
            "notes": ["--lang has no effect"],
        }));
        assert_eq!(
            folder.words("Recordings"),
            "From Recordings: 2 imported, 1 couldn’t be read. zq9.wav is not a recording."
        );
        assert_eq!(
            folder
                .meetings()
                .map(|(id, title)| (id.to_string(), title))
                .collect::<Vec<_>>(),
            [
                ("m-00000a".into(), "First recording".into()),
                ("m-00000b".into(), "next.wav".into())
            ],
        );
    }

    #[gpui::test]
    fn imports_run_one_at_a_time_and_report_failures(cx: &mut gpui::TestAppContext) {
        use broscribe_core::Library;
        use broscribe_core::config::Config;
        use broscribe_platform::fakes::FakePaths;
        use broscribe_platform::resolve_paths;

        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path().join("lib")).unwrap();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Config::default(),
                    library,
                    dir.path().join("no-such-cli"),
                ),
                cx,
            )
        });
        let runs = cx.update(ImportRuns::global);
        let request = |p: &str| ImportRequest {
            path: PathBuf::from(p),
            handling: Handling::Copy,
            language: None,
            title: None,
            date: None,
            participants: Vec::new(),
            manifest: false,
        };
        state.update(cx, |s, cx| {
            s.start_imports(vec![request("/a.m4a"), request("/b.m4a")], &runs, cx);
        });
        runs.read_with(cx, |r, _| {
            assert!(r.is_busy());
            assert_eq!(r.runs().len(), 2);
            assert_eq!(r.runs()[1].status, ImportRunStatus::Waiting);
        });
        cx.run_until_parked();
        runs.read_with(cx, |r, _| {
            assert!(!r.is_busy());
            assert!(!r.draining);
            // The CLI binary does not exist: both runs fail with a spawn error.
            assert!(
                r.runs()
                    .iter()
                    .all(|run| matches!(run.status, ImportRunStatus::Failed(_)))
            );
        });
        let first = runs.read_with(cx, |r, _| r.runs()[0].id);
        runs.update(cx, |r, cx| r.dismiss(first, cx));
        runs.read_with(cx, |r, _| assert_eq!(r.runs().len(), 1));
    }

    #[test]
    fn untouched_title_and_empty_date_are_not_sent() {
        let mut f = form(
            &["/rec/standup.m4a"],
            vec![PathInfo::File {
                modified: None,
                bytes: 0,
            }],
        );
        f.prefilled_title = Some("standup".into());
        f.title = "standup".into();
        let request = &f.validate().unwrap()[0];
        assert_eq!(
            (request.title.as_deref(), request.date.as_deref()),
            (None, None)
        );
        assert!(!request.args().iter().any(|arg| arg == "--date"));
        f.date = "2026-11-13 08:00".into();
        assert_eq!(
            f.validate().unwrap()[0].date.as_deref(),
            Some("2026-11-13 08:00")
        );
    }

    #[gpui::test]
    fn sheet_sends_only_edited_title_and_date(cx: &mut gpui::TestAppContext) {
        use broscribe_core::Library;
        use broscribe_core::config::Config;
        use broscribe_platform::fakes::FakePaths;
        use broscribe_platform::resolve_paths;

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("synthetic call.wav");
        std::fs::write(&file, b"RIFF").unwrap();
        let library = Library::open(dir.path().join("lib")).unwrap();
        let state = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    Config::default(),
                    library,
                    dir.path().join("no-such-cli"),
                ),
                cx,
            )
        });
        cx.update(gpui::init);
        let window =
            cx.add_window(|window, cx| ImportSheet::new(state, vec![file.clone()], window, cx));
        cx.run_until_parked();
        window
            .update(cx, |sheet, window, cx| {
                let form = sheet.form(cx);
                assert_eq!(form.title, "synthetic call");
                assert!(form.date.is_empty(), "the default leaves the picker empty");
                let request = &form.validate().unwrap()[0];
                assert_eq!(
                    (request.title.as_deref(), request.date.as_deref()),
                    (None, None)
                );
                sheet
                    .title
                    .update(cx, |input, cx| input.set_value("Chosen title", window, cx));
                let request = &sheet.form(cx).validate().unwrap()[0];
                assert_eq!(request.title.as_deref(), Some("Chosen title"));
                assert_eq!(request.date, None);
                let picked = NaiveDate::from_ymd_opt(2026, 11, 13)
                    .unwrap()
                    .and_hms_opt(8, 5, 0)
                    .unwrap();
                sheet.date.update(cx, |picker, cx| {
                    picker.set_date_time(
                        gpui::component::date_picker::DateTime::Single(Some(picked)),
                        window,
                        cx,
                    )
                });
                assert_eq!(
                    sheet.form(cx).validate().unwrap()[0].date.as_deref(),
                    Some("2026-11-13 08:05")
                );
            })
            .unwrap();
    }
}
