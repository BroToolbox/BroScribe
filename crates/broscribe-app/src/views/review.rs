//! Review queue (design §4 "Review queue", plan §4.4): speakers across
//! meetings grouped by voice, most distinct meetings first (`ops::identity_groups`).
//! Each row shows the meeting, the speaker's first words, the suggestion and
//! voice samples (the shared [`SampleControls`]: the longest line first,
//! Previous / Next through `ops::sample_clips`, the line played shown under
//! the row). Keys on the focused list: `Enter` accepts the suggestion or
//! opens the picker when there is none, `R` rejects, and `N` names the
//! speaker (the shared [`PersonPicker`]: search
//! People, Return or a click chooses an existing match by ID, and the final
//! option offers a typed name. Escape cancels), `D` is "Can't tell"
//! (dismisses the speaker from review without naming it; "Dismissed — Undo"
//! follows in the message area), the navigation keys move between rows,
//! and `Space` plays or stops the sample. A choice names the speaker the picker was opened for, never the
//! row selected meanwhile; leaving the review queue cancels the picker.
//! The Dismissed tab lists every dismissed speaker with Restore.
//!
//! Accepting and naming apply the speaker's centroid immediately and queue a
//! `confirm-samples` job for the queue worker (plan §8 item 18) through the
//! meeting view's [`name_speaker_and_queue`]; core requests the speaker
//! recheck in the same write. The message says only what was saved ("name
//! saved as …, checking other speakers"); the recheck block below it
//! follows the recheck's job (this view's meeting first, then the other
//! meetings, links to the meetings it changed, meetings waiting while
//! busy), across navigation and agent reconnects, and rows reload as its
//! writes land. Rejecting goes through `ops::reject_suggestion`
//! (rejections are remembered by core); "Can't tell" and Restore through
//! `ops::dismiss_review` / `ops::restore_review`. Every decision is fenced
//! by the speakers as read with the row. All library work runs on the
//! background executor.
//!
//! Mounting: `cx.new(|cx| ReviewView::new(state.clone(), window, cx))` once
//! per window, rendered for [`Route::ReviewQueue`](crate::state::Route::ReviewQueue);
//! focus it on navigation (`view.focus_handle(cx).focus(window, cx)`) so the
//! keys work without a click.

mod identity;
mod identity_checks;

use crate::ui::kit::{self, BadgeTone, OpenRowMenu, RowPopup, StatusKind, button};
use std::collections::HashMap;
use std::sync::Arc;

use broscribe_core::index::{Index, ReviewItem};
use broscribe_core::live_identity::{
    LiveReviewMeeting, RecordedSpeakerNaming, live_reviews_and_namings,
};
use broscribe_core::ops::{
    self, DismissOutcome, DismissedRow, ReviewEntry, SampleClip, SpeakerFence,
};
use broscribe_core::queue::{JobId, JobState};
use broscribe_core::schema::{Assignment, Meeting, RecordingState};
use broscribe_core::{Error, Library, MeetingId, PersonId, Result, SpeakerId};
use gpui::component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    input::{Redo, Undo},
    kbd::Kbd,
    menu::{PopupMenu, PopupMenuItem},
};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, Focusable, FontWeight,
    InteractiveElement as _, IntoElement, KeyDownEvent, Keystroke, ParentElement as _, Render,
    Role, ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription,
    Window, div, px,
};

use crate::jobs::{core_user_text, failure_text};
use crate::learning::{self, Focus, RecheckText};
use crate::state::{AppState, Route};
use crate::ui::format::{local_date, local_date_time, spoken_date_time};
use crate::ui::title;
use crate::ui::type_scale::{TextRole, TextRoleExt as _};
use crate::views::meeting::speakers::speech_length;
use crate::views::meeting::{Naming, name_with_history, regrouped_refusal, reject_proposal};
use crate::views::people::{ClipSource, SamplePlayback};
use crate::views::person_picker::{PersonChoice, PersonPicker, PersonPickerEvent, VoiceTarget};
use crate::views::speaker_review::{
    SampleControls, SampleCursor, SampleKeyboard, clip_of, set_review_dismissed,
};

// Keep this shorter than the core retention grace so Undo can still restore.
const REVIEW_UNDO_WINDOW: std::time::Duration = std::time::Duration::from_secs(
    broscribe_core::retention::DISMISS_GRACE.num_seconds() as u64 - 60,
);

gpui::actions!(
    review,
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

// ---- model (pure, tested) ----

/// What a key does in the focused review list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewKey {
    Accept,
    Reject,
    Name,
    Dismiss,
    Up,
    Down,
    First,
    Last,
    PageUp,
    PageDown,
    Play,
    Previous,
    Next,
}

/// The review action of `keystroke`; `None` for other keys and for
/// Command / Control / Option / Fn chords (Shift is allowed: `R` = `r`).
pub fn review_key(keystroke: &Keystroke) -> Option<ReviewKey> {
    let m = &keystroke.modifiers;
    if m.control || m.alt || m.platform || m.function {
        return None;
    }
    Some(match keystroke.key.as_str() {
        "enter" => ReviewKey::Accept,
        "r" => ReviewKey::Reject,
        "n" => ReviewKey::Name,
        "d" => ReviewKey::Dismiss,
        "up" => ReviewKey::Up,
        "down" => ReviewKey::Down,
        "home" => ReviewKey::First,
        "end" => ReviewKey::Last,
        "pageup" => ReviewKey::PageUp,
        "pagedown" => ReviewKey::PageDown,
        "space" => ReviewKey::Play,
        "left" if !m.shift => ReviewKey::Previous,
        "right" if !m.shift => ReviewKey::Next,
        _ => return None,
    })
}

/// The selection after moving one row up or down in a list of `len` rows.
pub fn step(selected: usize, len: usize, down: bool) -> usize {
    if len == 0 {
        0
    } else if down {
        (selected + 1).min(len - 1)
    } else {
        selected.saturating_sub(1)
    }
}

/// The person `Enter` accepts: the auto-name or the suggestion.
pub fn accept_target(item: &ReviewItem) -> Option<&PersonId> {
    match item.assignment {
        Assignment::Auto => item.person_id.as_ref(),
        Assignment::Suggested => item.suggested_person_id.as_ref(),
        Assignment::Confirmed | Assignment::None => None,
    }
}

/// One speaker waiting for review, with the clips played as their samples.
#[derive(Clone, Debug, PartialEq)]
pub struct ReviewRow {
    pub entry: ReviewEntry,
    /// The recording the samples play from; missing without one.
    pub source: Option<ClipSource>,
    /// `ops::sample_clips` of the speaker, in rank order.
    pub samples: Vec<SampleClip>,
    /// The meeting's speakers (analysis and grouping) as read with this
    /// row: a decision is refused once they changed. Missing when the
    /// canonical meeting could not be read.
    pub fence: Option<Arc<SpeakerFence>>,
    /// The speaker as read with this row, for the picker's closest voices;
    /// missing when the meeting could not be read.
    pub voice: Option<Arc<VoiceTarget>>,
}

impl ReviewRow {
    pub fn key(&self) -> RowKey {
        (
            self.entry.item.meeting_id.clone(),
            self.entry.item.speaker_id,
        )
    }
}

/// A row's identity across reloads.
pub type RowKey = (MeetingId, SpeakerId);

/// Primary naming work, separately reachable unresolved transcription
/// quality, the speakers dismissed with "Can't tell", and the meetings with
/// names given during the recording that were not applied.
#[derive(Default)]
pub struct ReviewSnapshot {
    pub rows: Vec<ReviewRow>,
    pub without_transcript: Vec<ReviewEntry>,
    pub dismissed: Vec<DismissedEntry>,
    /// `live_identity::live_reviews`: reviewed in each meeting's view.
    pub live: Vec<LiveReviewMeeting>,
    latest_live_naming: Option<RecordedSpeakerNaming>,
    identity: identity::Snapshot,
    identity_checks: identity_checks::Snapshot,
}

/// A speaker dismissed from review, with the meeting's speakers as read
/// with it (Restore is refused once they changed; missing when the meeting
/// could not be read).
#[derive(Clone, Debug, PartialEq)]
pub struct DismissedEntry {
    pub row: DismissedRow,
    pub fence: Option<Arc<SpeakerFence>>,
    pub source: Option<ClipSource>,
    pub samples: Vec<SampleClip>,
}

impl DismissedEntry {
    pub fn key(&self) -> RowKey {
        (self.row.meeting_id.clone(), self.row.speaker_id)
    }
}

/// How many names a meeting's live review holds ("2 names you gave during
/// the recording were not applied").
pub fn live_review_count(n: usize) -> String {
    crate::ui::format::count(
        n as u64,
        "name you gave while recording needs review",
        "names you gave while recording need review",
    )
}

/// The status line: what waits for review, and how many speakers were
/// dismissed ("No speakers to review · 3 dismissed").
pub fn review_summary(rows: usize, without_transcript: usize, dismissed: usize) -> String {
    let mut summary = if rows == 0 {
        "No speakers to review".to_owned()
    } else {
        crate::ui::format::count(rows as u64, "speaker to review", "speakers to review")
    };
    if rows == 0 && without_transcript > 0 {
        summary.push_str(" · ");
        summary.push_str(&crate::ui::format::count(
            without_transcript as u64,
            "voice without a transcript",
            "voices without a transcript",
        ));
    }
    if dismissed > 0 {
        summary.push_str(&format!(" · {dismissed} dismissed"));
    }
    summary
}

/// The review queue with sample clips, and the dismissed speakers (reads the
/// disk: background executor). Merged members never appear: their group is
/// reviewed as its kept speaker.
#[cfg(test)]
pub fn load_review(lib: &Library) -> Result<ReviewSnapshot> {
    load_review_with_identity(lib, None)
}

fn load_review_with_identity(
    lib: &Library,
    config: Option<(&str, Option<&broscribe_core::config::Thresholds>)>,
) -> Result<ReviewSnapshot> {
    let entries = match ops::review_queue(lib, None) {
        Ok(entries) => entries,
        Err(Error::NotFound(_)) if !Index::path(lib.root()).exists() => Vec::new(),
        Err(e) => return Err(e),
    };
    let mut identity = identity::Snapshot::default();
    let identity_checks = config.map(|(model, thresholds)| {
        ops::identity_checks(lib, None, model, thresholds).map_err(|e| core_user_text(&e))
    });
    let groups = config.map(|(model, thresholds)| {
        identity.load_duplicates(lib, model, thresholds);
        ops::identity_groups(lib, &entries, model, thresholds)
    });
    let mut meetings: HashMap<MeetingId, Option<(Meeting, Arc<SpeakerFence>)>> = HashMap::new();
    let mut rows = Vec::new();
    let mut without_transcript = Vec::new();
    for entry in entries {
        let item = &entry.item;
        let meeting = read_meeting(&mut meetings, lib, &item.meeting_id);
        if meeting
            .as_ref()
            .is_some_and(|(m, _)| ops::is_grouped_member(m, item.speaker_id))
        {
            continue;
        }
        if entry.without_transcript {
            without_transcript.push(entry);
            continue;
        }
        rows.push(ReviewRow {
            source: meeting.as_ref().and_then(|(m, _)| sample_source(m)),
            samples: meeting
                .as_ref()
                .map(|(m, _)| ops::sample_clips(m, item.speaker_id))
                .unwrap_or_default(),
            fence: meeting.as_ref().map(|(_, fence)| Arc::clone(fence)),
            voice: meeting
                .as_ref()
                .and_then(|(m, _)| VoiceTarget::of(m, item.speaker_id))
                .map(Arc::new),
            entry,
        });
    }
    let dismissed = ops::dismissed_speakers(lib)?
        .into_iter()
        .map(|row| {
            let meeting = read_meeting(&mut meetings, lib, &row.meeting_id);
            DismissedEntry {
                source: meeting.as_ref().and_then(|(m, _)| sample_source(m)),
                samples: meeting
                    .as_ref()
                    .map_or_else(Vec::new, |(m, _)| ops::sample_clips(m, row.speaker_id)),
                fence: meeting.as_ref().map(|(_, fence)| Arc::clone(fence)),
                row,
            }
        })
        .collect();
    if let Some(groups) = groups {
        identity.apply_groups(groups, &mut rows);
    }
    let (live, namings) = live_reviews_and_namings(lib)?;
    Ok(ReviewSnapshot {
        rows,
        without_transcript,
        dismissed,
        live,
        latest_live_naming: namings.into_iter().next(),
        identity,
        identity_checks,
    })
}

fn sample_source(meeting: &Meeting) -> Option<ClipSource> {
    let rec = meeting
        .recording
        .as_ref()
        .filter(|r| r.state != RecordingState::Deleted)?;
    Some(ClipSource::Meeting {
        meeting_id: meeting.id.clone(),
        sha256: Some(rec.sha256.clone()),
    })
}

/// The meeting `id` with its speakers' fence, read once per load; `None`
/// when it cannot be read.
fn read_meeting<'a>(
    meetings: &'a mut HashMap<MeetingId, Option<(Meeting, Arc<SpeakerFence>)>>,
    lib: &Library,
    id: &MeetingId,
) -> &'a Option<(Meeting, Arc<SpeakerFence>)> {
    meetings.entry(id.clone()).or_insert_with(|| {
        lib.load_meeting(id).ok().map(|meeting| {
            let fence = Arc::new(SpeakerFence::of(&meeting));
            (meeting, fence)
        })
    })
}

fn speaker_label(item: &ReviewItem) -> String {
    label_of(item.speaker_id, item.meeting_title.as_deref())
}

fn speaker_name(speaker: SpeakerId) -> String {
    format!("Speaker {}", speaker.number())
}

fn label_of(speaker: SpeakerId, meeting_title: Option<&str>) -> String {
    let name = speaker_name(speaker);
    match meeting_title.filter(|title| !title.trim().is_empty()) {
        Some(title) => format!("{name} in “{title}”"),
        None => format!("{name} in an untitled meeting"),
    }
}

fn meeting_title(title: Option<&str>) -> &str {
    title
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("Untitled meeting")
}

fn proposal(item: &ReviewItem) -> Option<(&str, &str, Option<f32>)> {
    match item.assignment {
        Assignment::Auto => Some((
            "Named automatically",
            item.person_name.as_deref()?,
            item.score,
        )),
        Assignment::Suggested => Some((
            "Suggested",
            item.suggested_name.as_deref()?,
            item.suggested_score,
        )),
        _ => None,
    }
}

// ---- view ----

#[derive(Clone)]
enum Action {
    Accept(PersonId),
    Reject(PersonId),
    Name {
        choice: PersonChoice,
        label: Option<String>,
        /// The meeting's speakers when the picker opened.
        fence: Arc<SpeakerFence>,
    },
    /// "Can't tell" (`true`) or Restore (`false`).
    Dismiss(bool),
    UndoDismiss,
}

enum RowCommand {
    Open,
    Play,
    Name,
    Decide(Action),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Decision {
    Accept,
    Reject,
    Name,
    Dismiss,
    Restore,
    Undo,
}

impl Decision {
    fn label(self, saving: bool) -> &'static str {
        match (self, saving) {
            (Self::Accept, false) => "Accept",
            (Self::Reject, false) => "Reject",
            (Self::Name, false) => "Name…",
            (Self::Dismiss, false) => "Can't tell",
            (Self::Restore, false) => "Restore",
            (Self::Undo, false) => "Undo",
            (Self::Accept, true) => "Confirming…",
            (Self::Reject, true) => "Rejecting…",
            (Self::Name, true) => "Naming…",
            (Self::Dismiss, true) => "Dismissing…",
            (Self::Restore | Self::Undo, true) => "Restoring…",
        }
    }
}

impl Action {
    fn decision(&self) -> Decision {
        match self {
            Self::Accept(_) => Decision::Accept,
            Self::Reject(_) => Decision::Reject,
            Self::Name { .. } => Decision::Name,
            Self::Dismiss(true) => Decision::Dismiss,
            Self::Dismiss(false) => Decision::Restore,
            Self::UndoDismiss => Decision::Undo,
        }
    }
}

enum Done {
    Named(Naming, Option<Box<ops::ReviewEdit>>),
    /// Whether a `refresh-vectors` job was queued.
    Rejected {
        queued: bool,
    },
    Dismissed(DismissOutcome),
}

/// "Dismissed — Undo" after "Can't tell": restores the speaker as dismissed.
struct UndoDismiss {
    key: RowKey,
    label: String,
    fence: Arc<SpeakerFence>,
    /// Its offer ends after [`REVIEW_UNDO_WINDOW`] unless a newer one replaced it.
    generation: u64,
}

/// The picker open on one row; a choice names that row's speaker.
struct NamingSession {
    key: RowKey,
    fence: Arc<SpeakerFence>,
    generation: u64,
    picker: Entity<PersonPicker>,
    _subscription: Subscription,
}

pub struct ReviewView {
    state: Entity<AppState>,
    embedded: bool,
    focus_handle: FocusHandle,
    selected_row_focus: FocusHandle,
    scroll: ScrollHandle,
    row_bounds: ScrollHandle,
    dismissed_selection: usize,
    row_popup: Option<RowPopup>,
    reveal_selection: bool,
    rows: Vec<ReviewRow>,
    without_transcript: Vec<ReviewEntry>,
    show_without_transcript: bool,
    loading: bool,
    loaded: bool,
    error: Option<String>,
    generation: u64,
    seen_revisions: (u64, u64),
    /// The library changed since the last load while the queue was hidden:
    /// it reloads once when the queue is shown again.
    stale: bool,
    selected: usize,
    naming: Option<NamingSession>,
    /// Bumped by every naming start and end; a picker's choice only counts
    /// while its session is current.
    naming_generation: u64,
    /// An accept / reject / name / dismissal is being written.
    saving: Option<(RowKey, Decision)>,
    /// Result of the last action.
    message: Option<String>,
    message_kind: StatusKind,
    /// The meeting of the last naming made here and its title: its own
    /// recheck result comes first.
    recheck_focus: Option<(MeetingId, String)>,
    /// The recheck result the user hid (job, requested, processed): shown
    /// again once the recheck moves on.
    recheck_hidden: Option<(JobId, u64, u64)>,
    /// Undo offered for the last "Can't tell".
    undo: Option<UndoDismiss>,
    undo_generation: u64,
    route_generation: u64,
    shown: bool,
    /// The speaker to select once the next reload lists it (a restored one).
    reselect: Option<RowKey>,
    /// Speakers dismissed with "Can't tell" (`ops::dismissed_speakers`).
    dismissed: Vec<DismissedEntry>,
    show_dismissed: bool,
    identity: identity::ReviewIdentity,
    identity_checks: identity_checks::Snapshot,
    /// Meetings whose live naming decisions wait for a review.
    live: Vec<LiveReviewMeeting>,
    latest_live_naming: Option<RecordedSpeakerNaming>,
    playback: SamplePlayback,
    /// The speaker whose samples are current and their clip.
    cursor: SampleCursor,
    _subscriptions: Vec<Subscription>,
}

fn playback_of(view: &mut ReviewView) -> &mut SamplePlayback {
    &mut view.playback
}

fn revisions(state: &Entity<AppState>, cx: &App) -> (u64, u64) {
    let s = state.read(cx);
    (s.people_revision(), s.meetings_revision())
}

fn playback_key(key: &RowKey) -> SharedString {
    format!("{}/{}", key.0, key.1).into()
}

impl ReviewView {
    pub(crate) fn embedded(mut self) -> Self {
        self.embedded = true;
        self
    }

    pub fn new(state: Entity<AppState>, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let seen_revisions = revisions(&state, cx);
        let subscriptions = vec![cx.observe(&state, |this: &mut Self, state, cx| {
            // The view outlives its route: leaving it cancels naming, and a
            // change while hidden only marks it stale (reloaded once shown).
            let shown = *state.read(cx).route() == Route::ReviewQueue;
            if !shown && this.shown {
                this.end_naming(cx);
                this.stop_identity_check_sample(cx);
                this.identity.clear_preview();
                this.undo = None;
                this.row_popup = None;
                this.route_generation += 1;
                cx.notify();
            }
            this.shown = shown;
            let now = revisions(&state, cx);
            if now != this.seen_revisions {
                this.seen_revisions = now;
                this.stale = true;
            }
            if this.stale && shown {
                this.reload(cx);
            }
        })];
        let shown = *state.read(cx).route() == Route::ReviewQueue;
        let mut view = Self {
            state,
            embedded: false,
            focus_handle: cx.focus_handle(),
            selected_row_focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            row_bounds: ScrollHandle::new(),
            dismissed_selection: 0,
            row_popup: None,
            reveal_selection: false,
            rows: Vec::new(),
            without_transcript: Vec::new(),
            show_without_transcript: false,
            loading: false,
            loaded: false,
            error: None,
            generation: 0,
            seen_revisions,
            stale: false,
            selected: 0,
            naming: None,
            naming_generation: 0,
            saving: None,
            message: None,
            message_kind: StatusKind::Info,
            recheck_focus: None,
            recheck_hidden: None,
            undo: None,
            undo_generation: 0,
            route_generation: 0,
            shown,
            reselect: None,
            dismissed: Vec::new(),
            show_dismissed: false,
            identity: identity::ReviewIdentity::default(),
            identity_checks: None,
            live: Vec::new(),
            latest_live_naming: None,
            playback: SamplePlayback::default(),
            cursor: SampleCursor::default(),
            _subscriptions: subscriptions,
        };
        view.reload(cx);
        view
    }

    #[cfg(test)]
    pub(crate) fn rows(&self) -> &[ReviewRow] {
        &self.rows
    }

    #[cfg(test)]
    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    /// The last action's message ("… name saved as …").
    #[cfg(test)]
    pub(crate) fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// The speaker recheck as this view shows it: the last status read
    /// (still checking; the meeting named in here first, then the other
    /// meetings; meetings waiting while busy), links to the meetings it
    /// changed, and a failed newer read, reported under that status, which
    /// stays shown as its last known state (never cleared, never hidden by
    /// it). `None` when there is nothing to say: no recheck and no error,
    /// or a settled one without results, or one the user hid.
    pub fn recheck(&self, cx: &App) -> Option<ShownRecheck> {
        let state = self.state.read(cx);
        let shown = self.recheck_status(state);
        let failure = state
            .recheck_error()
            .map(|cause| learning::recheck_failure(cause, shown.is_some()));
        let RecheckText {
            outcome,
            lines,
            links,
        } = match (shown, &failure) {
            (Some(text), _) => text,
            (None, Some(_)) => RecheckText::default(),
            (None, None) => return None,
        };
        Some(ShownRecheck {
            outcome,
            lines,
            links,
            failure,
        })
    }

    /// [`Self::recheck`] from the last status read alone.
    fn recheck_status(&self, state: &AppState) -> Option<RecheckText> {
        let recheck = state.recheck();
        let status = recheck.status.as_ref()?;
        if self.recheck_hidden.as_ref()
            == Some(&(status.job_id.clone(), status.requested, status.processed))
        {
            return None;
        }
        let request = state
            .recheck_request()
            .filter(|r| r.job_id == status.job_id);
        let since = request.map(|r| r.generation);
        if request.is_none()
            && !learning::pending(status, None)
            && status.report.is_none()
            && status.deferred.is_empty()
            && status.state != JobState::Failed
        {
            return None;
        }
        let focus = self
            .recheck_focus
            .as_ref()
            .filter(|(m, _)| request.is_some_and(|r| r.source_meeting.as_ref() == Some(m)))
            .map(|(meeting, label)| Focus {
                meeting,
                label: label.as_str(),
            });
        Some(RecheckText {
            // Said only when the pass processed the generation requested
            // here: an older settled result shown again, or a later one
            // requested elsewhere on the same job, says nothing.
            outcome: request.and_then(|r| learning::recheck_outcome(status, r)),
            lines: learning::recheck_lines(status, focus, since, state.recheck_progress()),
            links: learning::affected_links(recheck, since),
        })
    }

    /// Hides the recheck's result until it moves on.
    fn hide_recheck(&mut self, cx: &mut Context<Self>) {
        let hidden = self
            .state
            .read(cx)
            .recheck()
            .status
            .as_ref()
            .map(|s| (s.job_id.clone(), s.requested, s.processed));
        self.recheck_hidden = hidden;
        cx.notify();
    }

    /// Reloads the queue on the background executor, keeping the selected
    /// speaker selected when it is still there.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.stale = false;
        self.error = None;
        self.generation += 1;
        let generation = self.generation;
        // The speaker may change: a voice result in flight is dropped; the
        // scores shown stay until the reload completes.
        if let Some(naming) = &self.naming {
            naming
                .picker
                .update(cx, |picker, _| picker.begin_voice_reload());
        }
        self.loading = true;
        let app = self.state.read(cx);
        let lib = app.library().clone();
        let model = app.config().profile.speaker_embedding.clone();
        let thresholds = app.config().thresholds(&model).copied();
        let task = cx.background_executor().spawn(async move {
            load_review_with_identity(&lib, Some((&model, thresholds.as_ref())))
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |view, cx| view.apply_rows(generation, result, cx))
                .ok();
        })
        .detach();
        cx.notify();
    }

    /// Applies a reload; `false` when a newer one was started.
    pub fn apply_rows(
        &mut self,
        generation: u64,
        result: Result<ReviewSnapshot>,
        cx: &mut Context<Self>,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        self.loading = false;
        match result {
            Ok(snapshot) => {
                self.loaded = true;
                let restored = self
                    .reselect
                    .take()
                    .filter(|key| snapshot.rows.iter().any(|row| row.key() == *key));
                let reveal_restored = restored.is_some();
                let old_waiting_index = self.selected;
                let old_dismissed_index = self.dismissed_selection;
                let previous =
                    restored.or_else(|| self.rows.get(self.selected).map(ReviewRow::key));
                let previous_dismissed = self
                    .dismissed
                    .get(self.dismissed_selection)
                    .map(DismissedEntry::key);
                self.rows = snapshot.rows;
                self.without_transcript = snapshot.without_transcript;
                self.dismissed = snapshot.dismissed;
                self.dismissed_selection = previous_dismissed
                    .and_then(|key| self.dismissed.iter().position(|row| row.key() == key))
                    .unwrap_or(self.dismissed_selection)
                    .min(self.dismissed.len().saturating_sub(1));
                self.live = snapshot.live;
                self.latest_live_naming = snapshot.latest_live_naming;
                self.identity.snapshot = snapshot.identity;
                if self.identity_checks != snapshot.identity_checks {
                    self.stop_identity_check_sample(cx);
                }
                self.identity_checks = snapshot.identity_checks;
                self.error = None;
                // The current sample follows its clip; a clip that is no
                // longer a candidate stops (its text would describe another).
                if let Some(key) = self.cursor.current_key().cloned() {
                    let clips = self
                        .rows
                        .iter()
                        .find(|r| playback_key(&r.key()) == key)
                        .map(|r| r.samples.as_slice())
                        .or_else(|| {
                            self.dismissed
                                .iter()
                                .find(|row| playback_key(&row.key()) == key)
                                .map(|row| row.samples.as_slice())
                        });
                    if !self.cursor.reconcile(clips) && self.playback.active() == Some(&key) {
                        self.playback.stop(&self.state, cx);
                    }
                }
                self.selected = previous
                    .and_then(|k| self.rows.iter().position(|r| r.key() == k))
                    .unwrap_or(self.selected)
                    .min(self.rows.len().saturating_sub(1));
                self.reveal_selection |= if self.show_dismissed {
                    old_dismissed_index != self.dismissed_selection
                } else {
                    reveal_restored || old_waiting_index != self.selected
                };
                match self.naming.as_ref().map(|naming| {
                    self.rows.iter().find(|r| {
                        r.key() == naming.key && r.fence.as_deref() == Some(naming.fence.as_ref())
                    })
                }) {
                    Some(None) => {
                        self.end_naming(cx);
                    }
                    // Every reload recomputes the closest voices from the
                    // speaker as read now.
                    Some(Some(row)) => {
                        let voice = row.voice.clone();
                        if let Some(naming) = &self.naming {
                            naming
                                .picker
                                .update(cx, |picker, cx| picker.set_voice_target(voice, cx));
                        }
                    }
                    None => {}
                }
            }
            Err(e) => {
                tracing::warn!(code = e.code(), "reading the review queue");
                self.error = Some(core_user_text(&e));
                // The speaker was not re-read: keep its voices current.
                if let Some(naming) = &self.naming {
                    naming
                        .picker
                        .update(cx, |picker, cx| picker.resume_voices(cx));
                }
            }
        }
        cx.notify();
        true
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.rows.len() && ix != self.selected {
            self.selected = ix;
            self.reveal_selection = true;
            cx.notify();
        }
    }

    fn select_key(&mut self, key: &RowKey, cx: &mut Context<Self>) -> bool {
        match self.rows.iter().position(|r| r.key() == *key) {
            Some(ix) => {
                self.select(ix, cx);
                true
            }
            None => false,
        }
    }

    /// Handles a key on the focused list (see the module docs).
    pub fn handle_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.naming.is_some() {
            // Keys go to the picker.
            if ev.keystroke.key == "escape" {
                self.cancel_naming(window, cx);
                cx.stop_propagation();
            }
            return;
        }
        let Some(key) = review_key(&ev.keystroke) else {
            return;
        };
        self.dispatch_key(key, window, cx);
    }

    fn dispatch_key(&mut self, key: ReviewKey, window: &mut Window, cx: &mut Context<Self>) {
        if self.naming.is_some() || self.row_popup.is_some() {
            cx.propagate();
            return;
        }
        if matches!(key, ReviewKey::Accept | ReviewKey::Play)
            && self.selected_row_focus.contains_focused(window, cx)
        {
            cx.propagate();
            return;
        }
        cx.stop_propagation();
        let (selected, len) = if self.show_dismissed {
            (self.dismissed_selection, self.dismissed.len())
        } else {
            (self.selected, self.rows.len())
        };
        let page_size = || {
            self.row_bounds
                .bounds_for_item(selected)
                .filter(|row| row.size.height > px(0.))
                .map_or(5, |row| {
                    ((self.scroll.bounds().size.height / row.size.height) as usize).max(5)
                })
        };
        let next = match key {
            ReviewKey::Up | ReviewKey::Down => Some(step(selected, len, key == ReviewKey::Down)),
            ReviewKey::First => Some(0),
            ReviewKey::Last => Some(len.saturating_sub(1)),
            ReviewKey::PageUp => Some(selected.saturating_sub(page_size())),
            ReviewKey::PageDown => Some(
                selected
                    .saturating_add(page_size())
                    .min(len.saturating_sub(1)),
            ),
            _ => None,
        };
        if let Some(next) = next {
            if self.show_dismissed {
                self.dismissed_selection = next;
                self.reveal_selection = true;
                cx.notify();
            } else {
                self.select(next, cx);
            }
            self.reveal_laid_out_selection(cx);
            self.focus_handle.focus(window, cx);
            return;
        }
        if self.saving.is_some() || self.identity.saving {
            self.wait_for_write(cx);
            return;
        }
        match key {
            ReviewKey::Play => self.play_selected(cx),
            ReviewKey::Previous | ReviewKey::Next => self.step_selected(key == ReviewKey::Next, cx),
            ReviewKey::Accept if self.show_dismissed => {
                if let Some(key) = self
                    .dismissed
                    .get(self.dismissed_selection)
                    .map(DismissedEntry::key)
                {
                    self.restore(key, cx);
                }
            }
            _ if self.show_dismissed => {}
            ReviewKey::Accept => self.accept_selected(window, cx),
            ReviewKey::Reject => self.reject_selected(cx),
            ReviewKey::Name => self.start_naming(window, cx),
            ReviewKey::Dismiss => self.dismiss_selected(cx),
            _ => {}
        }
    }

    fn wait_for_write(&mut self, cx: &mut Context<Self>) {
        self.message_kind = StatusKind::Info;
        self.message = Some("Wait for the current change to finish.".into());
        cx.notify();
    }

    fn accept_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        match accept_target(&row.entry.item).cloned() {
            Some(person) => self.run(row.key(), Action::Accept(person), cx),
            None => self.start_naming(window, cx),
        }
    }

    pub fn reject_selected(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        if let Some(person) = accept_target(&row.entry.item).cloned() {
            self.run(row.key(), Action::Reject(person), cx);
        } else {
            self.message_kind = StatusKind::Info;
            self.message = Some("This speaker has no suggestion to reject.".into());
            cx.notify();
        }
    }

    fn selected_samples(&self) -> Option<(RowKey, Option<ClipSource>, Vec<SampleClip>)> {
        if self.show_dismissed {
            let row = self.dismissed.get(self.dismissed_selection)?;
            Some((row.key(), row.source.clone(), row.samples.clone()))
        } else {
            let row = self.rows.get(self.selected)?;
            Some((row.key(), row.source.clone(), row.samples.clone()))
        }
    }

    /// Plays (or stops) the selected row's current sample.
    pub fn play_selected(&mut self, cx: &mut Context<Self>) {
        let Some((row_key, source, samples)) = self.selected_samples() else {
            return;
        };
        let key = playback_key(&row_key);
        if self.playback.is_playing(&key) {
            self.playback.stop(&self.state, cx);
            cx.notify();
            return;
        }
        let Some(source) = source else {
            self.message_kind = StatusKind::Info;
            self.message = Some("No voice sample is available for this speaker.".into());
            cx.notify();
            return;
        };
        let Some(ix) = self.cursor.select(&key, &samples) else {
            self.message_kind = StatusKind::Info;
            self.message = Some("No voice sample is available for this speaker.".into());
            cx.notify();
            return;
        };
        let clip = clip_of(&source, &samples[ix]);
        let state = self.state.clone();
        self.playback.play(key, clip, &state, cx, playback_of);
    }

    /// Steps the selected row to its next (or previous) sample and plays it.
    pub fn step_selected(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some((row_key, source, samples)) = self.selected_samples() else {
            return;
        };
        let key = playback_key(&row_key);
        let (Some(source), Some(ix)) = (source, self.cursor.step(&key, &samples, forward)) else {
            return;
        };
        let clip = clip_of(&source, &samples[ix]);
        let state = self.state.clone();
        self.playback.play(key, clip, &state, cx, playback_of);
    }

    /// "Can't tell": dismisses the selected speaker from review.
    pub fn dismiss_selected(&mut self, cx: &mut Context<Self>) {
        if let Some(key) = self.rows.get(self.selected).map(ReviewRow::key) {
            self.run(key, Action::Dismiss(true), cx);
        }
    }

    /// Returns a dismissed speaker (`key`) to the review queue.
    pub fn restore(&mut self, key: RowKey, cx: &mut Context<Self>) {
        self.run(key, Action::Dismiss(false), cx);
    }

    /// Undoes the last "Can't tell" while it is offered.
    pub fn undo_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.saving.is_some() || self.identity.saving {
            self.wait_for_write(cx);
            return;
        }
        if let Some(undo) = self.undo.take() {
            self.set_dismissed_tab(false, cx);
            self.run_with(
                undo.key,
                undo.label,
                Some(undo.fence),
                Action::UndoDismiss,
                cx,
            );
        }
    }

    pub(crate) fn can_undo_for_focus(&self, window: &Window, cx: &App) -> bool {
        self.undo.is_some()
            && self.saving.is_none()
            && self.naming.is_none()
            && self.focus_handle.contains_focused(window, cx)
    }

    fn set_dismissed_tab(&mut self, dismissed: bool, cx: &mut Context<Self>) {
        if self.show_dismissed != dismissed || self.identity.show_duplicates {
            self.identity.show_duplicates = false;
            self.identity.clear_preview();
            self.playback.stop(&self.state, cx);
            self.end_naming(cx);
            self.row_popup = None;
            self.show_dismissed = dismissed;
            self.scroll.set_offset(gpui::Point::default());
            self.reveal_selection = !dismissed;
            cx.notify();
        }
    }

    #[cfg(test)]
    fn toggle_dismissed(&mut self, cx: &mut Context<Self>) {
        self.set_dismissed_tab(!self.show_dismissed, cx);
    }

    #[cfg(test)]
    pub(crate) fn dismissed(&self) -> &[DismissedEntry] {
        &self.dismissed
    }

    /// The status line ("No speakers to review · 3 dismissed").
    pub fn summary(&self) -> String {
        if let Some(cause) = &self.error {
            failure_text("Couldn't read the review queue.", cause, "")
        } else if self.loading && !self.loaded {
            "Reading the review queue…".to_owned()
        } else if let Some(cause) = &self.identity.snapshot.group_error {
            failure_text("Couldn't compare voices across meetings.", cause, "")
        } else {
            let mut summary = review_summary(
                self.rows.len(),
                self.without_transcript.len(),
                self.dismissed.len(),
            );
            if let Some(Ok(checks)) = &self.identity_checks
                && !checks.alerts.is_empty()
            {
                summary.push_str(" · ");
                summary.push_str(&crate::ui::format::count(
                    checks.alerts.len() as u64,
                    "confirmed name to check",
                    "confirmed names to check",
                ));
            }
            summary
        }
    }

    /// Opens the person picker for the selected row.
    pub fn start_naming(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving.is_some() || self.identity.saving {
            self.wait_for_write(cx);
            return;
        }
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        self.identity.identify = None;
        let key = row.key();
        let label = format!("Name for {}", speaker_label(&row.entry.item));
        let voice = row.voice.clone();
        let Some(fence) = row.fence.clone() else {
            self.end_naming(cx);
            self.message_kind = StatusKind::Attention;
            self.message =
                Some("Couldn't read this meeting. Reload it before naming its speakers.".into());
            cx.notify();
            return;
        };
        self.naming_generation += 1;
        let generation = self.naming_generation;
        let state = self.state.clone();
        let picker = cx.new(|cx| {
            let mut picker = PersonPicker::new(state, label, window, cx);
            picker.set_voice_target(voice, cx);
            picker
        });
        let subscription = cx.subscribe_in(
            &picker,
            window,
            move |this: &mut Self, _, ev: &PersonPickerEvent, window, cx| match ev {
                PersonPickerEvent::Picked(choice) => {
                    this.choose(generation, choice.clone(), window, cx)
                }
                PersonPickerEvent::Cancelled => {
                    if this.naming_is(generation) {
                        this.cancel_naming(window, cx);
                    }
                }
            },
        );
        self.message = None;
        self.naming = Some(NamingSession {
            key,
            fence,
            generation,
            picker,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// The open picker, if any.
    #[cfg(test)]
    pub(crate) fn picker(&self) -> Option<&Entity<PersonPicker>> {
        self.naming.as_ref().map(|n| &n.picker)
    }

    fn naming_is(&self, generation: u64) -> bool {
        self.naming
            .as_ref()
            .is_some_and(|n| n.generation == generation)
    }

    /// A choice from the picker of session `generation`: names the row the
    /// picker was opened for, if the session is still open.
    fn choose(
        &mut self,
        generation: u64,
        choice: PersonChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.naming_is(generation) {
            return;
        }
        if *self.state.read(cx).route() != Route::ReviewQueue {
            self.end_naming(cx);
            return;
        }
        if self.saving.is_some() || self.identity.saving {
            self.wait_for_write(cx);
            return;
        }
        let Some(naming) = self.naming.as_ref() else {
            return;
        };
        let key = naming.key.clone();
        let fence = Arc::clone(&naming.fence);
        let label = match &choice {
            PersonChoice::Typed(name) => Some(name.clone()),
            PersonChoice::Existing(id) => naming
                .picker
                .read(cx)
                .candidates()
                .find(|p| p.id == *id)
                .map(|p| p.name.clone()),
        };
        self.end_naming(cx);
        self.focus_handle.focus(window, cx);
        if self.select_key(&key, cx) {
            self.run(
                key,
                Action::Name {
                    choice,
                    label,
                    fence,
                },
                cx,
            );
        } else {
            self.message_kind = StatusKind::Info;
            self.message = Some("This speaker is no longer waiting for review.".into());
        }
        cx.notify();
    }

    fn cancel_naming(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.end_naming(cx) {
            self.focus_handle.focus(window, cx);
        }
    }

    /// Closes the picker; later choices from it are ignored.
    fn end_naming(&mut self, cx: &mut Context<Self>) -> bool {
        if self.naming.take().is_none() {
            return false;
        }
        self.naming_generation += 1;
        cx.notify();
        true
    }

    /// Writes the decision for the row `key` (a waiting or dismissed
    /// speaker) on the background executor.
    fn run(&mut self, key: RowKey, action: Action, cx: &mut Context<Self>) {
        let found = self
            .rows
            .iter()
            .find(|r| r.key() == key)
            .map(|r| (speaker_label(&r.entry.item), r.fence.clone()))
            .or_else(|| {
                self.dismissed.iter().find(|d| d.key() == key).map(|d| {
                    (
                        label_of(d.row.speaker_id, d.row.meeting_title.as_deref()),
                        d.fence.clone(),
                    )
                })
            });
        if let Some((label, fence)) = found {
            let choice = match &action {
                Action::Accept(person) => Some((PersonChoice::Existing(person.clone()), None)),
                Action::Name { choice, label, .. } => Some((choice.clone(), label.clone())),
                _ => None,
            };
            if let Some((choice, name)) = choice
                && self.identity.snapshot.groups.contains_key(&key)
            {
                if self.saving.is_none() && !self.identity.saving {
                    self.end_naming(cx);
                    self.select_key(&key, cx);
                    self.preview_identification(key, choice, name, cx);
                }
                return;
            }
            self.identity.identify = None;
            self.run_with(key, label, fence, action, cx);
        }
    }

    /// [`Self::run`] for the speaker `key` named `label`, as read with
    /// `fence` (`None`: the meeting could not be read).
    fn run_with(
        &mut self,
        key: RowKey,
        label: String,
        fence: Option<Arc<SpeakerFence>>,
        action: Action,
        cx: &mut Context<Self>,
    ) {
        if self.saving.is_some() || self.identity.saving {
            self.wait_for_write(cx);
            return;
        }
        // Decisions act on the speakers as they were shown.
        let Some(shown) = fence else {
            self.message_kind = StatusKind::Attention;
            self.message =
                Some("Couldn't read this meeting. Reload it before naming its speakers.".into());
            cx.notify();
            return;
        };
        self.playback.stop(&self.state, cx);
        let (lib, model, thresholds, cli) = {
            let s = self.state.read(cx);
            let model = s.config().profile.speaker_embedding.clone();
            let thresholds = s.config().thresholds(&model).copied();
            (s.library().clone(), model, thresholds, s.cli().clone())
        };
        self.saving = Some((key.clone(), action.decision()));
        let route_generation = self.route_generation;
        self.message = None;
        self.undo = None;
        cx.notify();
        let undo_fence = Arc::clone(&shown);
        let (meeting_id, speaker_id) = key;
        // The meeting named in comes first in the recheck's result.
        let focus = (
            meeting_id.clone(),
            self.rows
                .iter()
                .find(|r| r.entry.item.meeting_id == meeting_id)
                .and_then(|r| r.entry.item.meeting_title.clone())
                .unwrap_or_else(|| "Untitled meeting".to_owned()),
        );
        let task = cx.background_executor().spawn(async move {
            match action {
                Action::Accept(person) => name_with_history(
                    &lib,
                    &meeting_id,
                    speaker_id,
                    person.as_str(),
                    &model,
                    Some(&person),
                    &shown,
                    true,
                )
                .map(|(naming, edit)| Done::Named(naming, edit.map(Box::new))),
                Action::Name { choice, fence, .. } => name_with_history(
                    &lib,
                    &meeting_id,
                    speaker_id,
                    choice.person_ref(),
                    &model,
                    None,
                    &fence,
                    true,
                )
                .map(|(naming, edit)| Done::Named(naming, edit.map(Box::new))),
                Action::Reject(person) => reject_proposal(
                    &lib,
                    &meeting_id,
                    speaker_id,
                    &person,
                    &model,
                    thresholds.as_ref(),
                    &shown,
                    &cli,
                )
                .map(|queued| Done::Rejected { queued }),
                Action::Dismiss(_) | Action::UndoDismiss => set_review_dismissed(
                    &lib,
                    &meeting_id,
                    speaker_id,
                    &shown,
                    matches!(action, Action::Dismiss(true)),
                )
                .map(Done::Dismissed),
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |view, cx| {
                view.saving = None;
                view.message_kind = if result.is_ok() {
                    StatusKind::Done
                } else {
                    StatusKind::Failed
                };
                view.message = Some(match &result {
                    // Saved now; the others are only being checked.
                    Ok(Done::Named(naming, _)) => naming.message(&label),
                    Ok(Done::Rejected { .. }) => format!("{label}: suggestion rejected."),
                    Ok(Done::Dismissed(outcome)) if outcome.dismissed => {
                        format!("{label}: dismissed.")
                    }
                    Ok(Done::Dismissed(_)) => format!("{label} is back in the review queue."),
                    Err(e) => match e.code() {
                        code @ ("speakers_regrouped" | "speakers_reanalysed") => {
                            format!("{label}: {}", regrouped_refusal(code))
                        }
                        _ => failure_text(
                            &format!("Couldn't update {label}."),
                            &core_user_text(e),
                            "",
                        ),
                    },
                });
                match &result {
                    Ok(Done::Dismissed(outcome))
                        if outcome.dismissed
                            && view.shown
                            && view.route_generation == route_generation =>
                    {
                        view.offer_undo(
                            (outcome.meeting_id.clone(), outcome.speaker_id),
                            label,
                            undo_fence,
                            cx,
                        )
                    }
                    // A restored speaker is selected again.
                    Ok(Done::Dismissed(outcome)) if !outcome.dismissed => {
                        view.reselect = Some((outcome.meeting_id.clone(), outcome.speaker_id));
                    }
                    _ => {}
                }
                match result {
                    // The name is saved: run its confirm-samples job and
                    // the recheck it requested, and follow the recheck.
                    Ok(Done::Named(naming, edit)) => {
                        view.identity
                            .remember("identification", edit.map(|edit| *edit));
                        view.recheck_focus = Some(focus);
                        view.recheck_hidden = None;
                        let request = naming.learning.clone();
                        view.state
                            .update(cx, |s, cx| s.recheck_requested(request, cx));
                    }
                    // Runs the queued refresh-vectors job.
                    Ok(Done::Rejected { queued: true }) => {
                        view.state.update(cx, |s, cx| s.kick_jobs(cx));
                    }
                    _ => {}
                }
                view.state.update(cx, |s, cx| s.refresh_review_count(cx));
                view.reload(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Offers "Dismissed — Undo" for [`REVIEW_UNDO_WINDOW`].
    fn offer_undo(
        &mut self,
        key: RowKey,
        label: String,
        fence: Arc<SpeakerFence>,
        cx: &mut Context<Self>,
    ) {
        self.undo_generation += 1;
        let generation = self.undo_generation;
        self.undo = Some(UndoDismiss {
            key,
            label,
            fence,
            generation,
        });
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REVIEW_UNDO_WINDOW).await;
            this.update(cx, |view, cx| {
                if view
                    .undo
                    .as_ref()
                    .is_some_and(|u| u.generation == generation)
                {
                    view.undo = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    // ---- rendering ----

    fn select_in_tab(&mut self, key: &RowKey, cx: &mut Context<Self>) -> bool {
        if !self.show_dismissed {
            return self.select_key(key, cx);
        }
        let Some(ix) = self.dismissed.iter().position(|row| row.key() == *key) else {
            return false;
        };
        if self.dismissed_selection != ix {
            self.dismissed_selection = ix;
            self.reveal_selection = true;
            cx.notify();
        }
        true
    }

    fn open_meeting(&mut self, key: &RowKey, cx: &mut Context<Self>) {
        if self.select_in_tab(key, cx) {
            self.state.update(cx, |state, cx| {
                state.navigate(Route::meeting(key.0.clone()), cx)
            });
        }
    }

    fn close_row_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(popup) = self.row_popup.take() {
            popup.restore_focus(window, cx);
            cx.notify();
        }
    }

    fn open_selected_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (ix, key) = if self.show_dismissed {
            (
                self.dismissed_selection,
                self.dismissed
                    .get(self.dismissed_selection)
                    .map(DismissedEntry::key),
            )
        } else {
            (
                self.selected,
                self.rows.get(self.selected).map(ReviewRow::key),
            )
        };
        if let Some((key, bounds)) = key.zip(self.row_bounds.bounds_for_item(ix)) {
            let viewport = self.scroll.bounds();
            let origin = gpui::point(
                bounds.left() + px(12.),
                bounds.bottom().clamp(viewport.top(), viewport.bottom()),
            );
            self.open_row_menu(key, origin, window, cx);
        }
    }

    fn open_row_menu(
        &mut self,
        key: RowKey,
        origin: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.naming.is_some() || !self.select_in_tab(&key, cx) {
            return;
        }
        let (fence, commands) = if self.show_dismissed {
            let row = &self.dismissed[self.dismissed_selection];
            let fence = row.fence.clone();
            let commands = (fence.is_some() && self.saving.is_none())
                .then(|| ("Restore".into(), RowCommand::Decide(Action::Dismiss(false))))
                .into_iter()
                .collect::<Vec<_>>();
            (fence, commands)
        } else {
            let row = &self.rows[self.selected];
            let fence = row.fence.clone();
            let mut commands = Vec::new();
            if fence.is_some() && self.saving.is_none() {
                if let Some(person) = accept_target(&row.entry.item).cloned() {
                    let name = proposal(&row.entry.item).map_or("suggestion", |(_, name, _)| name);
                    commands.push((
                        format!("Accept {name}"),
                        RowCommand::Decide(Action::Accept(person.clone())),
                    ));
                    commands.push((
                        format!("Reject {name}"),
                        RowCommand::Decide(Action::Reject(person)),
                    ));
                }
                commands.push(("Name…".into(), RowCommand::Name));
                commands.push((
                    "Can't tell".into(),
                    RowCommand::Decide(Action::Dismiss(true)),
                ));
            }
            (fence, commands)
        };
        let can_play = !self.show_dismissed
            && self
                .rows
                .get(self.selected)
                .is_some_and(|row| row.source.is_some() && !row.samples.is_empty());
        let view = cx.weak_entity();
        let dismissed = self.show_dismissed;
        let item = |label, command: RowCommand| {
            let view = view.clone();
            let key = key.clone();
            let fence = fence.clone();
            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.close_row_menu(window, cx);
                    if dismissed != this.show_dismissed || !this.select_in_tab(&key, cx) {
                        return;
                    }
                    match &command {
                        RowCommand::Open => this.open_meeting(&key, cx),
                        RowCommand::Play => this.play_selected(cx),
                        RowCommand::Name | RowCommand::Decide(_) => {
                            let (label, current) = if dismissed {
                                let row = &this.dismissed[this.dismissed_selection];
                                (
                                    label_of(row.row.speaker_id, row.row.meeting_title.as_deref()),
                                    &row.fence,
                                )
                            } else {
                                let row = &this.rows[this.selected];
                                (speaker_label(&row.entry.item), &row.fence)
                            };
                            if current != &fence {
                                this.message_kind = StatusKind::Attention;
                                this.message =
                                    Some(format!("{label}: the speaker changed. Try again."));
                                cx.notify();
                                return;
                            }
                            match &command {
                                RowCommand::Name => this.start_naming(window, cx),
                                RowCommand::Decide(action) => this.run_with(
                                    key.clone(),
                                    label,
                                    fence.clone(),
                                    action.clone(),
                                    cx,
                                ),
                                _ => unreachable!(),
                            }
                        }
                    }
                });
            })
        };
        self.row_popup = Some(RowPopup::open(
            origin,
            &self.focus_handle,
            window,
            cx,
            |mut menu: PopupMenu| {
                let has_decisions = !commands.is_empty();
                for (label, command) in commands {
                    menu = menu.item(item(label, command));
                }
                menu.when(has_decisions && !dismissed, PopupMenu::separator)
                    .when(can_play, |menu| {
                        menu.item(item("Play sample".into(), RowCommand::Play))
                    })
                    .item(item("Open meeting".into(), RowCommand::Open))
            },
            Self::close_row_menu,
        ));
        cx.notify();
    }

    fn reveal_laid_out_selection(&mut self, cx: &mut Context<Self>) {
        let index = if self.show_dismissed {
            self.dismissed_selection
        } else {
            self.selected
        };
        let empty = if self.show_dismissed {
            self.dismissed.is_empty()
        } else {
            self.rows.is_empty()
        };
        let Some(row) = self.row_bounds.bounds_for_item(index).filter(|_| !empty) else {
            return;
        };
        let viewport = self.scroll.bounds();
        let mut offset = self.scroll.offset();
        if row.top() < viewport.top() || row.size.height > viewport.size.height {
            offset.y += viewport.top() - row.top();
        } else if row.bottom() > viewport.bottom() {
            offset.y += viewport.bottom() - row.bottom();
        } else {
            return;
        }
        self.scroll.set_offset(offset);
        cx.notify();
    }

    fn decision_button(
        &self,
        id: (&'static str, usize),
        decision: Decision,
        key: &RowKey,
        fence: Option<&Arc<SpeakerFence>>,
        cx: &Context<Self>,
        apply: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Button {
        let saving = self
            .saving
            .as_ref()
            .is_some_and(|(owner, action)| owner == key && *action == decision);
        let key = key.clone();
        let fence = fence.cloned();
        let dismissed = self.show_dismissed;
        button(
            id,
            decision.label(saving),
            fence.is_some() && self.saving.is_none(),
            cx,
            move |this, window, cx| {
                if dismissed != this.show_dismissed || !this.select_in_tab(&key, cx) {
                    return;
                }
                let current = if this.show_dismissed {
                    &this.dismissed[this.dismissed_selection].fence
                } else {
                    &this.rows[this.selected].fence
                };
                if current == &fence {
                    if decision != Decision::Name {
                        this.focus_handle.focus(window, cx);
                    }
                    apply(this, window, cx);
                } else {
                    this.message_kind = StatusKind::Attention;
                    this.message =
                        Some("The speaker changed. Review the row and try again.".into());
                    cx.notify();
                }
            },
        )
        .small()
    }

    fn decision_label(&self, key: &RowKey, decision: Decision, ready: String) -> String {
        if self
            .saving
            .as_ref()
            .is_some_and(|(owner, action)| owner == key && *action == decision)
        {
            format!("{} {ready}", decision.label(true))
        } else {
            ready
        }
    }

    fn shortcut(
        &self,
        button: Button,
        tooltip: &str,
        action: &dyn gpui::Action,
        selected: bool,
        window: &Window,
    ) -> Button {
        button
            .tooltip_with_action(tooltip.to_owned(), action, Some("ReviewQueue"))
            .children(
                selected
                    .then(|| Kbd::binding_for_action_in(action, &self.focus_handle, window))
                    .flatten(),
            )
    }

    fn waiting_actions(
        &self,
        ix: usize,
        row: &ReviewRow,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let item = &row.entry.item;
        let key = row.key();
        let selected = ix == self.selected;
        let readable = row.fence.is_some();
        let decidable = readable && self.saving.is_none();
        let decorate = |button: Button, tooltip: &str, action: &dyn gpui::Action| {
            self.shortcut(
                button,
                if readable {
                    tooltip
                } else {
                    "Can’t read this meeting"
                },
                action,
                selected,
                window,
            )
            .tab_stop(selected)
        };
        let mut decisions = div().flex().flex_wrap().items_center().gap_2();
        if let Some(person) = accept_target(item) {
            let name = proposal(item).map_or("the suggestion", |(_, name, _)| name);
            let accept = self
                .decision_button(
                    ("review-accept", ix),
                    Decision::Accept,
                    &key,
                    row.fence.as_ref(),
                    cx,
                    {
                        let key = key.clone();
                        let person = person.clone();
                        move |this, _, cx| this.run(key.clone(), Action::Accept(person.clone()), cx)
                    },
                )
                .when_else(
                    selected && readable,
                    |control| kit::primary(control, !decidable, cx),
                    |control| control.outline(),
                )
                .accessibility_label(self.decision_label(
                    &key,
                    Decision::Accept,
                    format!("Accept {name} as {}", speaker_label(item)),
                ));
            let reject = self
                .decision_button(
                    ("review-reject", ix),
                    Decision::Reject,
                    &key,
                    row.fence.as_ref(),
                    cx,
                    {
                        let key = key.clone();
                        let person = person.clone();
                        move |this, _, cx| this.run(key.clone(), Action::Reject(person.clone()), cx)
                    },
                )
                .outline()
                .accessibility_label(self.decision_label(
                    &key,
                    Decision::Reject,
                    format!("Reject {name} for {}", speaker_label(item)),
                ));
            decisions = decisions
                .child(decorate(
                    accept,
                    "BroScribe learns this voice from the speaker’s samples",
                    &Decide,
                ))
                .child(decorate(
                    reject,
                    &format!("BroScribe won’t suggest {name} for this speaker again"),
                    &Reject,
                ));
        }
        let name = self
            .decision_button(
                ("review-name", ix),
                Decision::Name,
                &key,
                row.fence.as_ref(),
                cx,
                Self::start_naming,
            )
            .outline()
            .accessibility_label(self.decision_label(
                &key,
                Decision::Name,
                format!("Name {}", speaker_label(item)),
            ));
        let name = decorate(name, "Choose who this is", &Name).children(
            (selected && accept_target(item).is_none())
                .then(|| Kbd::binding_for_action_in(&Decide, &self.focus_handle, window))
                .flatten(),
        );
        let dismiss = self
            .decision_button(
                ("review-dismiss", ix),
                Decision::Dismiss,
                &key,
                row.fence.as_ref(),
                cx,
                {
                    let key = key.clone();
                    move |this, _, cx| this.run(key.clone(), Action::Dismiss(true), cx)
                },
            )
            .outline()
            .accessibility_label(self.decision_label(
                &key,
                Decision::Dismiss,
                format!(
                    "Dismiss {} from review without naming it",
                    speaker_name(item.speaker_id)
                ),
            ));
        decisions.child(name).child(decorate(
            dismiss,
            "Skip without naming. You can restore it from Dismissed.",
            &Dismiss,
        ))
    }

    fn render_waiting(
        &self,
        ix: usize,
        row: &ReviewRow,
        focused: bool,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        use gpui::base::{TestSupportExt as _, TestTextExt as _};

        let item = &row.entry.item;
        let origin = row
            .entry
            .live_suggestion_lines
            .map(crate::views::meeting::live_name_source);
        let proposal = proposal(item)
            .map(|(kind, name, score)| (kind, name, origin.is_none().then_some(score).flatten()));
        let summary = proposal
            .map(|(kind, name, score)| {
                let score = score.map_or_else(String::new, |value| {
                    format!(", voice similarity {value:.2}")
                });
                format!("{kind} {name}{score}")
            })
            .unwrap_or_else(|| "No suggestion".into());
        let common = RowContent {
            key: row.key(),
            title: item.meeting_title.as_deref(),
            started_at: &item.started_at,
            talk_s: item.talk_s,
            summary,
            quotes: &row.entry.sample_text,
            source: row.source.is_some(),
            samples: &row.samples,
            readable: row.fence.is_some(),
            position: ix,
            count: self.rows.len(),
            selected: ix == self.selected,
            focused,
            id: "review-row",
        };
        let group = self.identity.snapshot.groups.get(&common.key);
        let row_conflict = row.entry.identity_conflict.clone();
        let row = self.render_row(
            &common,
            div().flex().items_center().gap_2().map(|line| {
                let line = line.children(
                    group.map(|group| kit::badge(BadgeTone::Neutral, group.caption(), cx)),
                );
                if let Some((kind, name, score)) = proposal {
                    line.child(
                        kit::badge(BadgeTone::Suggested, format!("{kind} · {name}"), cx)
                            .id(("review-proposal", ix))
                            .test_support(),
                    )
                    .children(score.map(|score| {
                        div()
                            .id(("review-voice-score", ix))
                            .role(Role::Label)
                            .aria_label(format!("Voice similarity {score:.2}"))
                            .text_role(TextRole::Caption)
                            .text_color(cx.theme().muted_foreground)
                            .map(|label| {
                                // The concise visual unit has the same precision as the picker.
                                label.test_text(crate::views::person_picker::voice_score(score))
                            })
                    }))
                } else {
                    line.child(
                        crate::ui::text_line(
                            ("review-no-suggestion", ix),
                            Role::Label,
                            "No suggestion",
                        )
                        .text_role(TextRole::Caption)
                        .text_color(cx.theme().muted_foreground),
                    )
                }
            }),
            self.waiting_actions(ix, row, window, cx),
            window,
            cx,
        );
        row.children(origin.map(|text| {
            crate::ui::text_line(("review-live-origin", ix), Role::Label, text)
                .text_role(TextRole::Caption)
        }))
        .children(row_conflict.map(|text| {
            crate::ui::text_line(("review-speaker-conflict", ix), Role::Label, text)
                .text_role(TextRole::Caption)
        }))
        .children(
            self.naming
                .as_ref()
                .filter(|n| n.key == common.key)
                .map(|n| n.picker.clone()),
        )
        .children(self.render_identity_preview(&common.key, cx))
    }

    fn render_dismissed(
        &self,
        ix: usize,
        entry: &DismissedEntry,
        focused: bool,
        window: &Window,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let row = &entry.row;
        let key = entry.key();
        let selected = ix == self.dismissed_selection;
        let label = label_of(row.speaker_id, row.meeting_title.as_deref());
        let restore = self
            .decision_button(
                ("review-restore", ix),
                Decision::Restore,
                &key,
                entry.fence.as_ref(),
                cx,
                {
                    let key = key.clone();
                    move |this, _, cx| this.restore(key.clone(), cx)
                },
            )
            .outline()
            .tab_stop(selected)
            .accessibility_label(self.decision_label(
                &key,
                Decision::Restore,
                format!("Restore {label} to the review queue"),
            ))
            .debug_selector(move || format!("review-restore-{ix}"));
        let summary = format!("Dismissed {}", local_date_time(&row.dismissed_at));
        let caption = crate::ui::text_line(("review-dismissed-date", ix), Role::Label, summary)
            .text_role(TextRole::Caption)
            .text_color(kit::row_metadata_colour(selected, cx));
        self.render_row(
            &RowContent {
                key,
                title: row.meeting_title.as_deref(),
                started_at: &row.started_at,
                talk_s: row.talk_s,
                summary: format!("Dismissed {}", spoken_date_time(&row.dismissed_at)),
                quotes: &row.sample_text,
                source: entry.source.is_some(),
                samples: &entry.samples,
                readable: entry.fence.is_some(),
                position: ix,
                count: self.dismissed.len(),
                selected,
                focused,
                id: "review-dismissed",
            },
            caption,
            restore,
            window,
            cx,
        )
    }

    fn render_row(
        &self,
        row: &RowContent<'_>,
        detail: impl IntoElement,
        decisions: impl IntoElement,
        window: &Window,
        cx: &Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let ix = row.position;
        let selected = row.selected;
        let focused = row.focused;
        let key = row.key.clone();
        let label = label_of(key.1, row.title);
        let quote = |text: &String| format!("“{}”", text.trim());
        let quotes: Vec<_> = row.quotes.iter().take(2).map(quote).collect();
        let aria = format!(
            "{label}, {}, {} of speech. {}. {}",
            spoken_date_time(row.started_at),
            speech_length(row.talk_s, true),
            row.summary,
            row.quotes.iter().map(quote).collect::<Vec<_>>().join(" "),
        );
        let pkey = playback_key(&key);
        let samples = SampleControls {
            key: &pkey,
            speaker: &label,
            clips: row.samples,
            playable: row.source,
            tab_stop: selected,
            keyboard: Some(SampleKeyboard {
                focus: &self.focus_handle,
                window,
                actions: [&PlaySample, &PreviousSample, &NextSample],
                context: "ReviewQueue",
            }),
        };
        let sample_buttons = samples.buttons(
            ("review-samples", ix),
            &self.cursor,
            &self.playback,
            cx,
            {
                let key = key.clone();
                move |this: &mut Self, _, cx| {
                    if this.select_in_tab(&key, cx) {
                        this.play_selected(cx);
                    }
                }
            },
            {
                let key = key.clone();
                move |this: &mut Self, forward, _, cx| {
                    if this.select_in_tab(&key, cx) {
                        this.step_selected(forward, cx);
                    }
                }
            },
        );
        let title = meeting_title(row.title).to_owned();
        let link = button(("review-open", ix), title.clone(), true, cx, {
            let key = key.clone();
            move |this: &mut Self, _, cx| this.open_meeting(&key, cx)
        })
        .link()
        .small()
        .tab_stop(selected)
        .accessibility_label(format!("Open {title}"));
        let saving = self.saving.as_ref().is_some_and(|(owner, _)| *owner == key);
        let style = div()
            .id((row.id, ix))
            .role(Role::ListBoxOption)
            .aria_label(aria)
            .aria_selected(selected)
            .aria_position_in_set(ix + 1)
            .aria_size_of_set(row.count)
            .when(selected && focused, |row| row.aria_active_descendant())
            .when(selected, |row| row.track_focus(&self.selected_row_focus))
            .key_context("ReviewActions")
            .flex()
            .flex_col()
            .flex_none()
            .gap_1()
            .px_3()
            .py_3()
            .border_b_1()
            .border_color(cx.theme().border);
        let select_key = key.clone();
        kit::selected_row(style, selected, self.focus_handle.is_focused(window), cx)
            .on_click(cx.listener(move |this, _, window, cx| {
                this.select_in_tab(&select_key, cx);
                if this.naming.is_none() {
                    this.focus_handle.focus(window, cx);
                }
            }))
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, window, cx| {
                    this.open_row_menu(key.clone(), ev.position, window, cx);
                    cx.stop_propagation();
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        crate::ui::text_line(
                            ("review-speaker", ix),
                            Role::Label,
                            speaker_name(row.key.1),
                        )
                        .text_role(TextRole::Label)
                        .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(link)
                    .child(
                        div()
                            .text_role(TextRole::Caption)
                            .text_color(kit::row_metadata_colour(selected, cx))
                            .child(format!(
                                "· {} · {} of speech",
                                local_date(row.started_at),
                                speech_length(row.talk_s, false)
                            )),
                    ),
            )
            .map(|el| {
                if row.readable {
                    el.child(detail)
                } else {
                    el.child(kit::retry_line_with_id(
                        ("review-unreadable", ix),
                        ("review-retry", ix),
                        "Can’t read this meeting",
                        cx,
                        |this, _, cx| this.reload(cx),
                    ))
                }
            })
            .children(
                quotes
                    .into_iter()
                    .map(|q| div().text_role(TextRole::Body).line_clamp(2).child(q)),
            )
            .when(
                row.readable && (!row.source || row.samples.is_empty()),
                |el| {
                    el.child(
                        crate::ui::text_line(
                            ("review-no-sample", ix),
                            Role::Label,
                            if row.source {
                                "No voice sample for this speaker."
                            } else {
                                "Recording unavailable."
                            },
                        )
                        .text_role(TextRole::Caption)
                        .text_color(kit::row_metadata_colour(selected, cx)),
                    )
                },
            )
            .when(saving, |el| {
                el.child(kit::status_line(
                    ("review-saving", ix),
                    StatusKind::Pending,
                    "Saving…",
                    cx,
                ))
            })
            .children(self.playback.status_of(&pkey).map(|words| {
                kit::status_line(
                    ("review-sample-status", ix),
                    StatusKind::Info,
                    words.to_owned(),
                    cx,
                )
            }))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .key_context("ReviewActions")
                    .child(sample_buttons)
                    .child(decisions),
            )
            .children(samples.line(("review-sample-line", ix), &self.cursor, cx))
    }

    fn render_live(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.live.is_empty() {
            return None;
        }
        let rows = self.live.iter().enumerate().map(|(ix, meeting)| {
            let id = meeting.meeting_id.clone();
            let title = meeting_title(meeting.title.as_deref());
            let names = live_review_count(meeting.items.len());
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(
                            crate::ui::text_line(
                                ("review-live-label", ix),
                                Role::Label,
                                title.to_owned(),
                            )
                            .text_role(TextRole::Label)
                            .font_weight(FontWeight::MEDIUM),
                        )
                        .child(
                            crate::ui::text_line(
                                ("review-live-caption", ix),
                                Role::Label,
                                format!("{names} · {}", local_date(&meeting.started_at)),
                            )
                            .text_role(TextRole::Caption)
                            .text_color(cx.theme().muted_foreground),
                        ),
                )
                .child(
                    button(
                        ("review-live-open", ix),
                        "Review in meeting",
                        true,
                        cx,
                        move |this: &mut Self, _, cx| {
                            this.state.update(cx, |state, cx| {
                                state.navigate(Route::meeting(id.clone()), cx)
                            });
                        },
                    )
                    .outline()
                    .small()
                    .accessibility_label(format!(
                        "Open {title} to check the names given during its recording"
                    )),
                )
        });
        Some(
            div()
                .id("review-live")
                .role(Role::Group)
                .aria_label("Names given during recordings that need review")
                .flex()
                .flex_col()
                .gap_1()
                .pt_3()
                .child(crate::ui::heading(
                    "review-live-heading",
                    format!("Names from recordings ({})", self.live.len()),
                ))
                .children(rows)
                .into_any_element(),
        )
    }

    fn render_quality(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.without_transcript.is_empty() {
            return None;
        }
        let total = self
            .without_transcript
            .iter()
            .map(|entry| entry.item.talk_s)
            .sum::<f64>();
        let heading = format!(
            "Voices without a transcript ({} · {})",
            self.without_transcript.len(),
            speech_length(total, false)
        );
        Some(div().id("review-quality").role(Role::Group).aria_label("Voices without a transcript")
            .flex().flex_col().gap_1().pt_3()
            .child(div().id("review-quality-heading").role(Role::Heading).aria_label(heading.clone()).aria_level(2).child(
                kit::disclosure("review-quality-toggle", heading, self.show_without_transcript, cx, |this, _, cx| {
                    this.show_without_transcript = !this.show_without_transcript;
                    cx.notify();
                }).debug_selector(|| "review-quality-toggle".into())
            ))
            .when(self.show_without_transcript, |section| section
                .child(crate::ui::text_line("review-coverage-hint", Role::Label,
                    "No transcript or playable samples for these speakers. Open the meeting for details.")
                    .text_role(TextRole::Caption).text_color(cx.theme().muted_foreground))
                .child(div().id("review-quality-list").role(Role::List).aria_label("Voices without a transcript")
                    .children(self.without_transcript.iter().enumerate().map(|(ix, entry)| {
                        let id = entry.item.meeting_id.clone();
                        let talk = entry.item.talk_s;
                        let label = speaker_label(&entry.item);
                        div().id(("review-quality-speaker", ix)).role(Role::ListItem)
                            .aria_label(format!("{label} · {}", speech_length(talk, true)))
                            .flex().flex_wrap().items_center().gap_2().px_3().py_2()
                            .border_b_1().border_color(cx.theme().border)
                            .child(div().text_role(TextRole::Body).child(format!("{label} · {}", speech_length(talk, false))))
                            .child(button(("review-quality-open", ix), "Open meeting", true, cx,
                                move |this: &mut Self, _, cx| {
                                    this.state.update(cx, |state, cx| state.navigate(Route::meeting(id.clone()), cx));
                                },
                            ).outline().small().accessibility_label(format!("Open meeting for {label}")))
                    }))))
            .into_any_element())
    }
}

struct RowContent<'a> {
    key: RowKey,
    title: Option<&'a str>,
    started_at: &'a chrono::DateTime<chrono::FixedOffset>,
    talk_s: f64,
    summary: String,
    quotes: &'a [String],
    source: bool,
    samples: &'a [SampleClip],
    readable: bool,
    position: usize,
    count: usize,
    selected: bool,
    focused: bool,
    id: &'static str,
}

impl Focusable for ReviewView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ReviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list_focused = self.focus_handle.is_focused(window);
        let selection_focused = self.focus_handle.contains_focused(window, cx);
        if std::mem::take(&mut self.reveal_selection) {
            let view = cx.weak_entity();
            window.defer(cx, move |_, cx| {
                let _ = view.update(cx, |view, cx| view.reveal_laid_out_selection(cx));
            });
        }
        let theme = cx.theme();
        let summary = if self.identity.show_duplicates {
            self.identity
                .duplicate_summary(self.loading && !self.loaded)
        } else {
            self.summary()
        };
        let tabs = kit::tabs(
            "review-tabs",
            "Review queue sections",
            [
                format!("Identify ({})", self.rows.len()).into(),
                format!("Dismissed ({})", self.dismissed.len()).into(),
                format!(
                    "Possible duplicates ({})",
                    self.identity.snapshot.duplicates.len()
                )
                .into(),
            ],
            if self.identity.show_duplicates {
                2
            } else {
                usize::from(self.show_dismissed)
            },
            cx.listener(|this, ix: &usize, _, cx| {
                if *ix == 2 {
                    this.show_duplicates(cx);
                } else {
                    this.set_dismissed_tab(*ix == 1, cx);
                }
            }),
            cx,
        );
        let restoring = self
            .saving
            .as_ref()
            .is_some_and(|(_, decision)| *decision == Decision::Undo);
        let undo = if restoring {
            Some(
                button(
                    "review-undo",
                    Decision::Undo.label(true),
                    false,
                    cx,
                    |_, _, _| {},
                )
                .small()
                .outline()
                .tab_stop(false),
            )
        } else {
            self.undo.as_ref().map(|undo| {
                button(
                    "review-undo",
                    "Undo",
                    self.saving.is_none(),
                    cx,
                    |this: &mut Self, window, cx| {
                        this.undo_dismiss(cx);
                        this.focus_handle.focus(window, cx);
                    },
                )
                .small()
                .outline()
                .accessibility_label(format!("Undo: return {} to the review queue", undo.label))
                .children(Kbd::binding_for_action_in(
                    &Undo,
                    &self.focus_handle,
                    window,
                ))
            })
        };
        let recheck = self.recheck(cx).map(|shown| {
            let status = self.state.read(cx).recheck().status.as_ref();
            learning::render_recheck(
                "review-recheck",
                RecheckText {
                    outcome: shown.outcome,
                    lines: shown.lines,
                    links: shown.links,
                },
                shown.failure,
                learning::recheck_phase(status, None),
                &self.state,
                |this: &mut Self, window, cx| {
                    this.hide_recheck(cx);
                    this.focus_handle.focus(window, cx);
                },
                cx,
            )
            .border_0()
            .p_0()
        });
        let rows: Vec<_> = if self.identity.show_duplicates {
            Vec::new()
        } else if self.show_dismissed {
            self.dismissed
                .iter()
                .enumerate()
                .map(|(ix, row)| {
                    self.render_dismissed(ix, row, selection_focused, window, cx)
                        .into_any_element()
                })
                .collect()
        } else {
            self.rows
                .iter()
                .enumerate()
                .map(|(ix, row)| {
                    self.render_waiting(ix, row, selection_focused, window, cx)
                        .into_any_element()
                })
                .collect()
        };
        let loading = self.loading && !self.loaded;
        let empty = rows.is_empty() && self.error.is_none() && !loading;
        let list = div()
            .id(if self.show_dismissed {
                "review-dismissed-list"
            } else {
                "review-list"
            })
            .role(Role::ListBox)
            .aria_label(if self.show_dismissed {
                "Dismissed speakers"
            } else {
                "Speakers waiting for review"
            })
            .aria_keyshortcuts(if self.show_dismissed {
                "Enter ArrowUp ArrowDown ArrowLeft ArrowRight Space Home End PageUp PageDown"
            } else {
                "Enter R N D ArrowUp ArrowDown ArrowLeft ArrowRight Space Home End PageUp PageDown"
            })
            .track_focus(&self.focus_handle.clone().tab_index(0).tab_stop(true))
            .track_scroll(&self.row_bounds)
            .key_context("ReviewQueue")
            .flex()
            .flex_col()
            .flex_none()
            .when(!rows.is_empty(), |list| {
                list.on_action(cx.listener(|this, _: &OpenRowMenu, window, cx| {
                    this.open_selected_menu(window, cx);
                    cx.stop_propagation();
                }))
            })
            .on_action(cx.listener(|this, _: &Undo, window, cx| {
                if this.can_undo_for_focus(window, cx) {
                    this.undo_dismiss(cx);
                    this.focus_handle.focus(window, cx);
                    cx.stop_propagation();
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &Decide, window, cx| {
                this.dispatch_key(ReviewKey::Accept, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Reject, window, cx| {
                this.dispatch_key(ReviewKey::Reject, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Name, window, cx| {
                this.dispatch_key(ReviewKey::Name, window, cx)
            }))
            .on_action(cx.listener(|this, _: &Dismiss, window, cx| {
                this.dispatch_key(ReviewKey::Dismiss, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PlaySample, window, cx| {
                this.dispatch_key(ReviewKey::Play, window, cx)
            }))
            .on_action(cx.listener(|this, _: &PreviousSample, window, cx| {
                this.dispatch_key(ReviewKey::Previous, window, cx)
            }))
            .on_action(cx.listener(|this, _: &NextSample, window, cx| {
                this.dispatch_key(ReviewKey::Next, window, cx)
            }))
            .capture_key_down(cx.listener(Self::handle_key))
            .when(empty && list_focused, |el| {
                el.focus_visible(crate::ui::kit::focus_halo(theme.ring))
            })
            .children(rows);
        let checks = self.render_identity_checks(cx);
        let no_work =
            self.without_transcript.is_empty() && self.live.is_empty() && checks.is_none();
        div()
            .id("review")
            .role(Role::Main)
            .aria_label("Review queue")
            .on_action(cx.listener(|this, _: &Undo, _, cx| {
                if this.naming.is_none() && this.identity.can_replay(false) {
                    this.replay_identity(false, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_action(cx.listener(|this, _: &Redo, _, cx| {
                if this.naming.is_none() && this.identity.can_replay(true) {
                    this.replay_identity(true, cx);
                } else {
                    cx.propagate();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .p(kit::DESK_PADDING)
            .text_color(theme.foreground)
            .when(!self.embedded, |el| {
                el.child(title("review-title", "Review queue"))
            })
            .child(
                div()
                    .id("review-header")
                    .role(Role::Group)
                    .aria_label("Review queue controls")
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(tabs)
                    .child(self.render_identity_history(cx))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .map(|el| {
                                if self.error.is_some()
                                    || if self.identity.show_duplicates {
                                        self.identity.snapshot.duplicate_error.is_some()
                                    } else {
                                        self.identity.snapshot.group_error.is_some()
                                    }
                                {
                                    el.child(kit::retry_line(
                                        "review-status",
                                        summary,
                                        cx,
                                        |this, _, cx| this.reload(cx),
                                    ))
                                } else if loading {
                                    el.child(kit::status_line(
                                        "review-status",
                                        StatusKind::Pending,
                                        summary,
                                        cx,
                                    ))
                                } else {
                                    el.child(
                                        crate::ui::text_line(
                                            "review-status",
                                            Role::Status,
                                            summary,
                                        )
                                        .text_role(TextRole::Caption),
                                    )
                                }
                            })
                            .when(
                                !self.show_dismissed
                                    && !self.identity.show_duplicates
                                    && self.rows.is_empty()
                                    && !self.dismissed.is_empty(),
                                |el| {
                                    el.child(
                                        button(
                                            "review-show-dismissed",
                                            "Show dismissed",
                                            true,
                                            cx,
                                            |this: &mut Self, window, cx| {
                                                this.set_dismissed_tab(true, cx);
                                                this.focus_handle.focus(window, cx);
                                            },
                                        )
                                        .link()
                                        .small(),
                                    )
                                },
                            ),
                    ),
            )
            .when(
                self.message.is_some()
                    || self.identity.saving
                    || undo.is_some()
                    || recheck.is_some(),
                |el| {
                    el.child(
                        div()
                            .id("review-activity")
                            .role(Role::Group)
                            .aria_label("Review activity")
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap_2()
                                    .when(self.identity.saving, |el| {
                                        el.child(kit::status_line(
                                            "identity-saving",
                                            StatusKind::Pending,
                                            "Saving…",
                                            cx,
                                        ))
                                    })
                                    .children(self.message.clone().map(|words| {
                                        kit::status_line(
                                            "review-message",
                                            self.message_kind,
                                            words,
                                            cx,
                                        )
                                    }))
                                    .when(restoring, |el| {
                                        el.child(kit::status_line(
                                            "review-undo-pending",
                                            StatusKind::Pending,
                                            "Saving…",
                                            cx,
                                        ))
                                    })
                                    .children(undo),
                            )
                            .children(recheck),
                    )
                },
            )
            .child(
                // The queue is a card on the desk; what compares voices or
                // profiles inside it is inset.
                kit::desk_card("review-scroll", cx)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .map(|el| {
                        if self.identity.show_duplicates {
                            el.child(self.render_duplicates(cx))
                        } else if self.show_dismissed {
                            el.child(list)
                        } else {
                            el.children(checks).child(list)
                        }
                    })
                    .when(
                        !self.identity.show_duplicates && empty && (self.show_dismissed || no_work),
                        |el| {
                            let (heading, hint) = if self.show_dismissed {
                                (
                                    "No dismissed speakers",
                                    "Speakers you skip with Can't tell appear here.",
                                )
                            } else {
                                (
                                    "No speakers to review",
                                    "Speakers who need a name appear here.",
                                )
                            };
                            el.child(kit::empty_state(
                                "review-empty-title",
                                heading,
                                crate::ui::text_line("review-empty-hint", Role::Label, hint),
                                true,
                                None,
                                None,
                                cx,
                            ))
                        },
                    )
                    .when(
                        !self.show_dismissed && !self.identity.show_duplicates,
                        |el| {
                            el.children(self.render_live(cx))
                                .children(self.render_quality(cx))
                        },
                    ),
            )
            .children(
                self.row_popup
                    .as_ref()
                    .map(|p| p.render("review-row-menu", cx, Self::close_row_menu)),
            )
    }
}

/// The speaker recheck as the review queue shows it ([`ReviewView::recheck`]).
pub struct ShownRecheck {
    /// How a recheck requested here ended ([`learning::recheck_outcome`]).
    pub outcome: Option<&'static str>,
    /// The last status read: still checking, then committed counts. Empty
    /// when none could be read.
    pub lines: Vec<String>,
    /// The meetings it changed, with their link labels.
    pub links: Vec<(MeetingId, String)>,
    /// A newer read that failed, said under `lines`, which stay as its last
    /// known state.
    pub failure: Option<String>,
}

#[cfg(test)]
#[path = "review/u45_tests.rs"]
mod u45_tests;

#[cfg(test)]
mod tests {
    use broscribe_core::index::ReviewItem;
    use broscribe_core::schema::Segment;
    use broscribe_platform::PlayerState;
    use chrono::DateTime;
    use gpui::{TestAppContext, WindowHandle};

    use super::*;
    use crate::views::person_picker::fixtures::{self, Synthetic, sid};

    fn seg(id: u32, speaker: u32, start: f64, end: f64, text: &str) -> Segment {
        Segment {
            speech_overlap: Vec::new(),
            id,
            track: "t0".into(),
            speaker: Some(SpeakerId::new(speaker).unwrap()),
            start,
            end,
            text: text.into(),
            words: Vec::new(),
        }
    }

    fn item(assignment: Assignment) -> ReviewItem {
        ReviewItem {
            meeting_id: MeetingId::random(),
            meeting_title: Some("Weekly sync".into()),
            started_at: DateTime::parse_from_rfc3339("2026-03-02T10:00:00+00:00").unwrap(),
            speaker_id: SpeakerId::new(2).unwrap(),
            talk_s: 125.0,
            assignment,
            person_id: Some(PersonId::random()),
            person_name: Some("Rui".into()),
            score: Some(0.91),
            suggested_person_id: Some(PersonId::random()),
            suggested_name: Some("Ana".into()),
            suggested_score: Some(0.82),
        }
    }

    #[test]
    fn keys_map_to_actions() {
        let key = |s: &'static str| review_key(&Keystroke::parse(s).unwrap());
        assert_eq!(key("enter"), Some(ReviewKey::Accept));
        assert_eq!(key("r"), Some(ReviewKey::Reject));
        assert_eq!(key("shift-r"), Some(ReviewKey::Reject));
        assert_eq!(key("n"), Some(ReviewKey::Name));
        assert_eq!(key("d"), Some(ReviewKey::Dismiss));
        assert_eq!(key("shift-d"), Some(ReviewKey::Dismiss));
        assert_eq!(key("up"), Some(ReviewKey::Up));
        assert_eq!(key("down"), Some(ReviewKey::Down));
        assert_eq!(key("space"), Some(ReviewKey::Play));
        assert_eq!(key("cmd-r"), None);
        assert_eq!(key("ctrl-n"), None);
        assert_eq!(key("x"), None);
    }

    #[test]
    fn navigation_stays_in_bounds() {
        assert_eq!(step(0, 0, true), 0);
        assert_eq!(step(0, 3, false), 0);
        assert_eq!(step(0, 3, true), 1);
        assert_eq!(step(2, 3, true), 2);
        assert_eq!(step(2, 3, false), 1);
    }

    #[test]
    fn accept_targets_follow_the_assignment() {
        let auto = item(Assignment::Auto);
        assert_eq!(accept_target(&auto), auto.person_id.as_ref());
        let suggested = item(Assignment::Suggested);
        assert_eq!(
            accept_target(&suggested),
            suggested.suggested_person_id.as_ref()
        );
        assert_eq!(accept_target(&item(Assignment::None)), None);
        assert_eq!(
            proposal(&suggested).map(|(kind, _, _)| kind),
            Some("Suggested")
        );
        assert_eq!(
            proposal(&auto).map(|(kind, _, _)| kind),
            Some("Named automatically")
        );
    }

    #[test]
    fn the_status_line_counts_dismissed_speakers() {
        assert_eq!(review_summary(0, 0, 0), "No speakers to review");
        assert_eq!(
            review_summary(0, 0, 3),
            "No speakers to review · 3 dismissed"
        );
        assert_eq!(review_summary(1, 0, 1), "1 speaker to review · 1 dismissed");
        assert_eq!(review_summary(2, 1, 0), "2 speakers to review");
        assert_eq!(
            review_summary(0, 2, 1),
            "No speakers to review · 2 voices without a transcript · 1 dismissed"
        );
    }

    #[gpui::test]
    fn review_tabs_fit_at_largest_and_keep_keyboard_activation(cx: &mut TestAppContext) {
        let (window, _app, _lib, _people, _dir) = open_review(cx);
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_resize(gpui::size(px(600.), px(720.)));
        visual.update(|window, cx| {
            crate::ui::apply_reading_size(broscribe_core::config::ReadingSize::Largest, cx);
            // The fixture mounts the view directly, so there is no Root
            // plugin to apply the text size to the window's rem.
            window.set_rem_size(cx.theme().font_size);
        });
        crate::ui::kit::dialog_gate::draw(&mut visual);
        let identify = visual.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            assert_eq!(
                tree.read(tree.find("review-tabs")),
                ("TabList", "Review queue sections")
            );
            let nodes = gpui::base::test_support::snapshots(window);
            let viewport = window.viewport_size();
            let mut identify = None;
            for name in ["Identify", "Dismissed", "Possible duplicates"] {
                let tab = nodes
                    .iter()
                    .find(|node| {
                        node.role() == Some(Role::Tab)
                            && node.label().is_some_and(|label| label.starts_with(name))
                    })
                    .unwrap();
                let bounds = tab.bounds();
                assert!(
                    bounds.size.height >= window.rem_size() * 1.45,
                    "{name} has no space beyond a line of text: {bounds:?}"
                );
                assert!(bounds.top() >= px(0.) && bounds.bottom() <= viewport.height);
                assert!(bounds.left() >= px(0.) && bounds.right() <= viewport.width);
                if name == "Identify" {
                    identify = Some(bounds.center());
                }
            }
            identify.unwrap()
        });
        visual.simulate_click(identify, Default::default());
        crate::ui::kit::dialog_gate::draw(&mut visual);
        visual.update(|window, cx| {
            for _ in 0..16 {
                if gpui::base::test_support::snapshots(window)
                    .iter()
                    .any(|node| {
                        node.role() == Some(Role::Tab)
                            && node
                                .label()
                                .is_some_and(|name| name.starts_with("Identify"))
                            && node.focused() == Some(true)
                    })
                {
                    return;
                }
                window.focus_next(cx);
                window.draw(cx).clear(cx);
            }
            panic!("Identify was never focused");
        });
        visual.simulate_keystrokes("end");
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                gpui::base::test_support::snapshots(window)
                    .iter()
                    .any(|node| {
                        node.role() == Some(Role::Tab)
                            && node
                                .label()
                                .is_some_and(|name| name.starts_with("Possible duplicates"))
                            && node.focused() == Some(true)
                    })
            );
        });
        visual.simulate_keystrokes("space");
        visual.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("space").unwrap(),
        });
        crate::ui::kit::dialog_gate::draw(&mut visual);
        visual.update(|window, _| {
            assert!(
                gpui::base::test_support::snapshots(window)
                    .iter()
                    .any(|node| {
                        node.role() == Some(Role::Tab)
                            && node
                                .label()
                                .is_some_and(|name| name.starts_with("Possible duplicates"))
                            && node.selected() == Some(true)
                    })
            );
        });
    }

    #[gpui::test]
    fn review_queue_draws_one_title(cx: &mut TestAppContext) {
        let (window, _app, _lib, _people, _dir) = open_review(cx);
        cx.update_window(window.into(), |_, window, cx| {
            crate::ui::a11y_tree::draw_and_read(window, cx).assert_title("review-title");
        })
        .unwrap();
    }

    pub(super) fn open_review(
        cx: &mut TestAppContext,
    ) -> (
        WindowHandle<ReviewView>,
        Entity<AppState>,
        Library,
        Synthetic,
        tempfile::TempDir,
    ) {
        open_review_with(cx, |_, _| {})
    }

    /// [`open_review`] after `prepare` changed the seeded library.
    pub(super) fn open_review_with(
        cx: &mut TestAppContext,
        prepare: impl FnOnce(&Library, &Synthetic),
    ) -> (
        WindowHandle<ReviewView>,
        Entity<AppState>,
        Library,
        Synthetic,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| cx.bind_keys(crate::ui::keymap::key_bindings(cx)));
        let (app, lib, s) = fixtures::state(cx, dir.path());
        prepare(&lib, &s);
        app.update(cx, |st, cx| st.navigate(Route::ReviewQueue, cx));
        let window = cx.add_window({
            let app = app.clone();
            |window, cx| ReviewView::new(app, window, cx)
        });
        cx.run_until_parked();
        (window, app, lib, s, dir)
    }

    #[gpui::test]
    fn review_and_dismissed_dates_do_not_depend_on_the_stored_offset(cx: &mut TestAppContext) {
        let (window, _app, _lib, _synthetic, _dir) = open_review(cx);
        window
            .update(cx, |view, win, cx| {
                let at = DateTime::parse_from_rfc3339("2026-01-01T23:30:00Z").unwrap();
                let other = DateTime::parse_from_rfc3339("2026-01-02T01:30:00+02:00").unwrap();
                let mut row = view.rows()[0].clone();
                let read = |row: &ReviewRow| {
                    let element = view.render_waiting(0, row, false, win, cx).into_element();
                    assert_eq!(
                        gpui::Element::a11y_role(&element),
                        Some(Role::ListBoxOption)
                    );
                    let mut node = gpui::accesskit::Node::new(Role::ListBoxOption);
                    gpui::Element::write_a11y_info(&element, &mut node);
                    node.label().expect("review's accessible text").to_owned()
                };
                row.entry.item.started_at = at;
                let first = read(&row);
                row.entry.item.started_at = other;
                assert_eq!(read(&row), first);
                assert!(first.contains(&spoken_date_time(&at)), "{first}");

                let mut dismissed = DismissedEntry {
                    row: DismissedRow {
                        meeting_id: row.entry.item.meeting_id.clone(),
                        meeting_title: row.entry.item.meeting_title.clone(),
                        started_at: at,
                        speaker_id: row.entry.item.speaker_id,
                        talk_s: row.entry.item.talk_s,
                        assignment: row.entry.item.assignment,
                        person_id: None,
                        dismissed_at: at,
                        sample_text: Vec::new(),
                    },
                    fence: row.fence.clone(),
                    source: None,
                    samples: Vec::new(),
                };
                let read = |row: &DismissedEntry| {
                    let element = view.render_dismissed(0, row, false, win, cx).into_element();
                    let mut node = gpui::accesskit::Node::new(Role::ListBoxOption);
                    gpui::Element::write_a11y_info(&element, &mut node);
                    node.label()
                        .expect("dismissed row’s accessible text")
                        .to_owned()
                };
                let first = read(&dismissed);
                dismissed.row.started_at = other;
                assert_eq!(read(&dismissed), first);
                assert!(first.contains(&spoken_date_time(&at)), "{first}");
            })
            .unwrap();
    }

    fn keys(view: &ReviewView) -> Vec<SpeakerId> {
        view.rows()
            .iter()
            .map(|r| r.entry.item.speaker_id)
            .collect()
    }

    pub(super) fn dismissed_at(lib: &Library, s: &Synthetic, n: u32) -> bool {
        let m = lib.load_meeting(&s.meeting).unwrap();
        m.speaker(sid(n)).unwrap().review_dismissed_at.is_some()
    }

    fn badge(app: &Entity<AppState>, cx: &mut TestAppContext) -> Option<usize> {
        app.update(cx, |s, cx| s.refresh_review_count(cx));
        cx.run_until_parked();
        app.read_with(cx, |s, _| s.review_count())
    }

    /// Speaker 1 gets three lines (4 s at 20 s, 3 s at 5 s, 2 s at 30 s)
    /// and the meeting a natively playable recording; a fake player plays.
    pub(super) fn with_audio(lib: &Library, s: &Synthetic) {
        use broscribe_core::schema::{Handling, Recording};
        lib.write(|tx| {
            let mut m = tx.library().load_meeting(&s.meeting)?;
            m.segments.push(seg(4, 1, 20.0, 24.0, "the longest line"));
            m.segments.push(seg(5, 1, 30.0, 32.0, "the shortest line"));
            m.recording = Some(Recording {
                state: RecordingState::Available,
                handling: Handling::Copy,
                path: "audio.m4a".into(),
                original_path: None,
                sha256: "ab".repeat(32),
                source_sha256: None,
                sources: Vec::new(),
                bytes: 9,
                retention_override: None,
                delete_after: None,
            });
            tx.save_meeting(&m)
        })
        .unwrap();
        std::fs::write(
            lib.meeting_dir(&s.meeting).unwrap().join("audio.m4a"),
            b"synthetic",
        )
        .unwrap();
    }

    fn player_position(app: &Entity<AppState>, cx: &mut TestAppContext) -> PlayerState {
        app.update(cx, |s, _| s.player_mut().unwrap().poll())
    }

    /// The line shown under row `ix`, if its samples are current.
    fn shown_line(view: &ReviewView, ix: usize) -> Option<String> {
        let row = &view.rows()[ix];
        let key = playback_key(&row.key());
        SampleControls {
            key: &key,
            speaker: &speaker_label(&row.entry.item),
            clips: &row.samples,
            playable: row.source.is_some(),
            tab_stop: ix == view.selected,
            keyboard: None,
        }
        .current_line(&view.cursor)
        .map(|(text, _)| text)
    }

    #[gpui::test]
    fn review_keys_follow_selection_when_another_rows_button_has_focus(cx: &mut TestAppContext) {
        let (window, app, _lib, _synthetic, _dir) = open_review_with(cx, with_audio);
        app.update(cx, |state, _| {
            state.set_player(Box::new(broscribe_platform::fakes::FakeAudioPlayer::new(
                60.0,
            )));
        });
        window
            .update(cx, |view, win, cx| view.focus_handle.focus(win, cx))
            .unwrap();
        cx.run_until_parked();
        window.update(cx, |_, win, cx| win.focus_next(cx)).unwrap();
        window
            .update(cx, |view, win, cx| {
                assert!(view.selected_row_focus.contains_focused(win, cx));
                // A reload may move selection while a control retains focus.
                view.select(1, cx);
            })
            .unwrap();
        cx.run_until_parked();
        cx.simulate_keystrokes(window.into(), "space");
        cx.run_until_parked();
        assert_eq!(
            player_position(&app, cx),
            PlayerState::Playing { position_s: 10.0 }
        );
        cx.simulate_keystrokes(window.into(), "down");
        window
            .update(cx, |view, win, _| {
                assert_eq!(view.selected, 2);
                assert!(view.focus_handle.is_focused(win));
            })
            .unwrap();
    }

    #[gpui::test]
    fn samples_step_in_rank_order_and_another_sample_stops_the_first(cx: &mut TestAppContext) {
        let (window, app, _lib, _s, _dir) = open_review_with(cx, with_audio);
        app.update(cx, |st, _| {
            st.set_player(Box::new(broscribe_platform::fakes::FakeAudioPlayer::new(
                60.0,
            )))
        });
        let play = |cx: &mut TestAppContext,
                    f: &dyn Fn(&mut ReviewView, &mut Context<ReviewView>)| {
            window.update(cx, |view, _, cx| f(view, cx)).unwrap();
            cx.run_until_parked();
        };
        window
            .update(cx, |view, _, _| {
                assert_eq!(keys(view), [sid(1), sid(2), sid(3)]);
                assert_eq!(view.rows()[0].samples.len(), 3);
                assert_eq!(shown_line(view, 0), None, "nothing played yet");
            })
            .unwrap();
        // Play sample: the longest line first.
        play(cx, &|view, cx| view.play_selected(cx));
        assert_eq!(
            player_position(&app, cx),
            PlayerState::Playing { position_s: 20.0 }
        );
        let line = |cx: &mut TestAppContext| {
            window
                .update(cx, |view, _, _| shown_line(view, 0).unwrap())
                .unwrap()
        };
        assert_eq!(
            line(cx),
            "Sample 1 of 3 · longest · 0:20 Speaker 1 in “Synthetic sync”: the longest line"
        );
        // Next steps in rank order and plays; it wraps around.
        play(cx, &|view, cx| view.step_selected(true, cx));
        assert_eq!(
            player_position(&app, cx),
            PlayerState::Playing { position_s: 5.0 }
        );
        assert_eq!(
            line(cx),
            "Sample 2 of 3 · 0:05 Speaker 1 in “Synthetic sync”: line 1"
        );
        play(cx, &|view, cx| view.step_selected(true, cx));
        assert!(line(cx).starts_with("Sample 3 of 3 · 0:30"));
        play(cx, &|view, cx| view.step_selected(true, cx));
        assert!(line(cx).starts_with("Sample 1 of 3 · longest"));
        // Previous goes back (wrapping too).
        play(cx, &|view, cx| view.step_selected(false, cx));
        assert!(line(cx).starts_with("Sample 3 of 3"));
        assert_eq!(
            player_position(&app, cx),
            PlayerState::Playing { position_s: 30.0 }
        );
        // Another speaker's sample stops the first.
        play(cx, &|view, cx| {
            view.select(1, cx);
            view.play_selected(cx);
        });
        window
            .update(cx, |view, _, _| {
                let first = playback_key(&view.rows()[0].key());
                let second = playback_key(&view.rows()[1].key());
                assert!(!view.playback.is_playing(&first));
                assert!(view.playback.is_playing(&second));
                assert_eq!(shown_line(view, 0), None);
                assert_eq!(
                    shown_line(view, 1).unwrap(),
                    "Sample · 0:10 Speaker 2 in “Synthetic sync”: line 2"
                );
            })
            .unwrap();
        assert_eq!(
            player_position(&app, cx),
            PlayerState::Playing { position_s: 10.0 }
        );
    }

    #[gpui::test]
    fn cant_tell_removes_the_row_and_undo_restores_it(cx: &mut TestAppContext) {
        let (window, app, lib, s, _dir) = open_review(cx);
        assert_eq!(badge(&app, cx), Some(3));
        window
            .update(cx, |view, _, cx| view.dismiss_selected(cx))
            .unwrap();
        cx.run_until_parked();
        assert!(dismissed_at(&lib, &s, 1));
        window
            .update(cx, |view, _, _| {
                assert_eq!(keys(view), [sid(2), sid(3)]);
                assert_eq!(
                    view.message.as_deref(),
                    Some("Speaker 1 in “Synthetic sync”: dismissed.")
                );
                assert!(view.undo.is_some());
                assert_eq!(view.dismissed().len(), 1);
                assert_eq!(view.summary(), "2 speakers to review · 1 dismissed");
            })
            .unwrap();
        assert_eq!(badge(&app, cx), Some(2), "the badge excludes dismissed");
        // Nothing else changed: the speaker stays unnamed.
        assert_eq!(named(&lib, &s, 1), (Assignment::None, None));

        window
            .update(cx, |view, _, cx| view.undo_dismiss(cx))
            .unwrap();
        cx.run_until_parked();
        assert!(!dismissed_at(&lib, &s, 1));
        window
            .update(cx, |view, _, _| {
                assert_eq!(keys(view), [sid(1), sid(2), sid(3)]);
                assert_eq!(view.selected(), 0, "the restored speaker is selected again");
                assert!(view.undo.is_none());
                assert!(view.dismissed().is_empty());
                assert_eq!(
                    view.message.as_deref(),
                    Some("Speaker 1 in “Synthetic sync” is back in the review queue.")
                );
            })
            .unwrap();
        assert_eq!(badge(&app, cx), Some(3));

        // The Undo offer ends after its window.
        window
            .update(cx, |view, _, cx| view.dismiss_selected(cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| assert!(view.undo.is_some()))
            .unwrap();
        cx.executor().advance_clock(REVIEW_UNDO_WINDOW);
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| assert!(view.undo.is_none()))
            .unwrap();
        assert!(dismissed_at(&lib, &s, 1));
    }

    #[gpui::test]
    fn empty_queue_counts_dismissed_and_the_list_restores(cx: &mut TestAppContext) {
        let (window, app, lib, s, _dir) = open_review(cx);
        for _ in 0..3 {
            window
                .update(cx, |view, _, cx| view.dismiss_selected(cx))
                .unwrap();
            cx.run_until_parked();
        }
        assert!((1..=3).all(|n| dismissed_at(&lib, &s, n)));
        assert_eq!(badge(&app, cx), Some(0));
        let restored = window
            .update(cx, |view, _, cx| {
                assert!(view.rows().is_empty());
                assert_eq!(view.summary(), "No speakers to review · 3 dismissed");
                assert!(!view.show_dismissed);
                view.toggle_dismissed(cx);
                assert!(view.show_dismissed);
                let listed: Vec<SpeakerId> =
                    view.dismissed().iter().map(|d| d.row.speaker_id).collect();
                assert_eq!(listed, [sid(1), sid(2), sid(3)]);
                let key = view.dismissed()[1].key();
                view.restore(key.clone(), cx);
                key
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(restored.1, sid(2));
        assert!(!dismissed_at(&lib, &s, 2));
        window
            .update(cx, |view, _, _| {
                assert_eq!(keys(view), [sid(2)]);
                assert_eq!(view.dismissed().len(), 2);
                assert_eq!(view.summary(), "1 speaker to review · 2 dismissed");
                // Restoring offers no Undo.
                assert!(view.undo.is_none());
            })
            .unwrap();
        assert_eq!(badge(&app, cx), Some(1));
    }

    #[gpui::test]
    fn cant_tell_on_a_reanalysed_meeting_is_refused(cx: &mut TestAppContext) {
        let (window, _app, lib, s, _dir) = open_review(cx);
        // Reanalysed elsewhere; the view has not reloaded.
        lib.write(|tx| {
            let mut meeting = tx.library().load_meeting(&s.meeting)?;
            meeting.speaker_namespace = "replacement-analysis".into();
            tx.save_meeting(&meeting)
        })
        .unwrap();
        window
            .update(cx, |view, _, cx| view.dismiss_selected(cx))
            .unwrap();
        cx.run_until_parked();
        assert!(!dismissed_at(&lib, &s, 1));
        window
            .update(cx, |view, _, _| {
                assert_eq!(
                    view.message.as_deref(),
                    Some(
                        "Speaker 1 in “Synthetic sync”: This meeting was reanalysed; review its \
                         speakers again."
                    )
                );
                assert!(view.undo.is_none());
            })
            .unwrap();
    }

    #[gpui::test]
    fn a_reload_follows_the_playing_clip_or_stops_it(cx: &mut TestAppContext) {
        let (window, app, lib, s, _dir) = open_review_with(cx, with_audio);
        app.update(cx, |st, _| {
            st.set_player(Box::new(broscribe_platform::fakes::FakeAudioPlayer::new(
                60.0,
            )))
        });
        let edit = |cx: &mut TestAppContext, change: &dyn Fn(&mut Meeting)| {
            lib.write(|tx| {
                let mut m = tx.library().load_meeting(&s.meeting)?;
                change(&mut m);
                tx.save_meeting(&m)
            })
            .unwrap();
            window.update(cx, |view, _, cx| view.reload(cx)).unwrap();
            cx.run_until_parked();
        };
        let state = |cx: &mut TestAppContext| {
            window
                .update(cx, |view, _, _| {
                    let key = playback_key(&view.rows()[0].key());
                    (view.playback.is_playing(&key), shown_line(view, 0))
                })
                .unwrap()
        };
        window
            .update(cx, |view, _, cx| {
                view.play_selected(cx);
                view.step_selected(true, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            player_position(&app, cx),
            PlayerState::Playing { position_s: 5.0 }
        );
        // Moved: a longer line ranks first; the line shown follows the
        // clip still playing.
        edit(cx, &|m| {
            m.segments.push(seg(6, 1, 40.0, 46.0, "a new long line"))
        });
        let (playing, line) = state(cx);
        assert!(playing);
        assert_eq!(
            line.unwrap(),
            "Sample 3 of 4 · 0:05 Speaker 1 in “Synthetic sync”: line 1"
        );
        assert_eq!(
            player_position(&app, cx),
            PlayerState::Playing { position_s: 5.0 }
        );
        // Removed: the playing line is no longer a candidate; it stops and no
        // other clip's text is shown for it.
        edit(cx, &|m| m.segments.retain(|seg| seg.id != 1));
        assert_eq!(state(cx), (false, None));
        app.update(cx, |s, _| assert!(!s.player_mut().unwrap().is_playing()));
        // Playing again starts at the longest.
        window
            .update(cx, |view, _, cx| view.play_selected(cx))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            player_position(&app, cx),
            PlayerState::Playing { position_s: 40.0 }
        );
    }

    #[gpui::test]
    fn many_dismissed_speakers_scroll_and_every_restore_is_reachable(cx: &mut TestAppContext) {
        const EXTRA: u32 = 30;
        let dir = tempfile::tempdir().unwrap();
        let (app, lib, s) = fixtures::state(cx, dir.path());
        lib.write(|tx| {
            let mut m = tx.library().load_meeting(&s.meeting)?;
            for n in 4..4 + EXTRA {
                let mut speaker = broscribe_core::schema::Speaker::new(sid(n), "t0");
                speaker.talk_s = 2.0;
                speaker.review_dismissed_at = Some(broscribe_core::schema::now());
                m.speakers.push(speaker);
                m.segments.push(seg(
                    n,
                    n,
                    f64::from(n) * 2.0,
                    f64::from(n) * 2.0 + 1.0,
                    "hm",
                ));
            }
            tx.save_meeting(&m)
        })
        .unwrap();
        app.update(cx, |st, cx| st.navigate(Route::ReviewQueue, cx));
        let window = cx.open_window(gpui::size(gpui::px(700.), gpui::px(400.)), {
            let app = app.clone();
            move |window, cx| ReviewView::new(app, window, cx)
        });
        cx.run_until_parked();
        window
            .update(cx, |view, _, cx| {
                assert_eq!(view.dismissed().len(), EXTRA as usize);
                view.toggle_dismissed(cx);
            })
            .unwrap();
        let mut vcx = gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.run_until_parked();
        let tree = vcx.update(crate::ui::a11y_tree::draw_and_read);
        assert!(
            tree.nodes
                .iter()
                .any(|node| { node.role == "TabList" && node.name == "Review queue sections" })
        );
        let list = window
            .update(&mut vcx, |view, _, _| view.scroll.clone())
            .unwrap();
        let bounds = list.bounds();
        assert!(
            bounds.size.height > gpui::px(0.) && bounds.bottom() <= gpui::px(640.),
            "the dismissed list is not bounded: {bounds:?}"
        );
        assert!(list.max_offset().y > gpui::px(0.), "nothing to scroll");
        let last = "review-restore-29";
        assert!(vcx.debug_bounds(last).unwrap().top() >= bounds.bottom());
        // The wheel scrolls the list to its end; the last Restore shows and
        // takes a click.
        vcx.simulate_event(gpui::ScrollEvent {
            position: bounds.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(gpui::px(0.), gpui::px(-5000.))),
            ..Default::default()
        });
        vcx.run_until_parked();
        assert_eq!(list.offset().y, -list.max_offset().y);
        let restore = vcx.debug_bounds(last).unwrap();
        assert!(
            restore.top() >= bounds.top() && restore.bottom() <= bounds.bottom(),
            "the last Restore is not in view: {restore:?} in {bounds:?}"
        );
        vcx.simulate_click(restore.center(), gpui::Modifiers::none());
        vcx.run_until_parked();
        let last_speaker = sid(4 + EXTRA - 1);
        assert!(
            lib.load_meeting(&s.meeting)
                .unwrap()
                .speaker(last_speaker)
                .unwrap()
                .review_dismissed_at
                .is_none()
        );
    }

    /// Opens the picker on row `row` and lets it load.
    fn start(
        window: &WindowHandle<ReviewView>,
        cx: &mut TestAppContext,
        row: usize,
    ) -> Entity<PersonPicker> {
        let picker = window
            .update(cx, |view, window, cx| {
                view.select(row, cx);
                view.start_naming(window, cx);
                view.picker().unwrap().clone()
            })
            .unwrap();
        cx.run_until_parked();
        picker
    }

    pub(super) fn named(lib: &Library, s: &Synthetic, n: u32) -> (Assignment, Option<PersonId>) {
        let m = lib.load_meeting(&s.meeting).unwrap();
        let speaker = m.speaker(sid(n)).unwrap();
        (speaker.assignment, speaker.person_id.clone())
    }

    #[gpui::test]
    fn naming_picks_an_existing_person_for_the_row_it_was_opened_on(cx: &mut TestAppContext) {
        let (window, _app, lib, s, _dir) = open_review(cx);
        let before = fixtures::people_count(&lib);
        window
            .update(cx, |view, _, _| {
                let speakers: Vec<SpeakerId> = view
                    .rows()
                    .iter()
                    .map(|r| r.entry.item.speaker_id)
                    .collect();
                assert_eq!(speakers, [sid(1), sid(2), sid(3)]);
            })
            .unwrap();
        let picker = start(&window, cx, 0);
        window
            .update(cx, |view, window, cx| {
                // The user moves to another row before choosing.
                view.select(1, cx);
                picker.update(cx, |p, cx| {
                    p.set_query("silva", window, cx);
                    assert_eq!(p.candidates().count(), 2);
                    let ix = p.candidates().position(|c| c.id == s.ana_other).unwrap();
                    p.pick(ix, cx);
                });
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            named(&lib, &s, 1),
            (Assignment::Confirmed, Some(s.ana_other.clone()))
        );
        assert_eq!(named(&lib, &s, 2).0, Assignment::Suggested);
        assert_eq!(fixtures::people_count(&lib), before);
        window
            .update(cx, |view, _, _| {
                assert!(view.picker().is_none());
                assert!(
                    view.rows()
                        .iter()
                        .all(|r| r.entry.item.speaker_id != sid(1))
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn closed_pickers_and_unmatched_enter_name_nobody(cx: &mut TestAppContext) {
        let (window, app, lib, s, _dir) = open_review(cx);
        let before = fixtures::people_count(&lib);
        // Opened on speaker 1, then the user leaves the review queue.
        let left = start(&window, cx, 0);
        app.update(cx, |st, cx| st.navigate(Route::library(), cx));
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| assert!(view.picker().is_none()))
            .unwrap();
        app.update(cx, |st, cx| st.navigate(Route::ReviewQueue, cx));
        // Cancelled.
        let cancelled = start(&window, cx, 0);
        cancelled.update(cx, |p, cx| p.cancel(cx));
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| assert!(view.picker().is_none()))
            .unwrap();
        // Replaced by a picker on speaker 3.
        let replaced = start(&window, cx, 0);
        let current = start(&window, cx, 2);
        // Late choices from closed pickers change nothing.
        for old in [&left, &cancelled, &replaced] {
            old.update(cx, |p, cx| p.pick(0, cx));
        }
        cx.run_until_parked();
        assert_eq!(named(&lib, &s, 1), (Assignment::None, None));
        assert_eq!(named(&lib, &s, 3), (Assignment::None, None));
        // Return on an unmatched name leaves the speaker and People unchanged.
        window
            .update(cx, |_, window, cx| {
                current.update(cx, |p, cx| {
                    p.set_query("Carla Nova", window, cx);
                    p.confirm_highlighted(cx);
                })
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(named(&lib, &s, 3), (Assignment::None, None));
        assert_eq!(fixtures::people_count(&lib), before);
        // The open picker names its own speaker through an alias match.
        window
            .update(cx, |view, window, cx| {
                assert!(view.picker().is_some());
                current.update(cx, |p, cx| {
                    p.set_query("ze", window, cx);
                    p.confirm_highlighted(cx);
                })
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            named(&lib, &s, 3),
            (Assignment::Confirmed, Some(s.jose.clone()))
        );
        assert_eq!(named(&lib, &s, 1), (Assignment::None, None));
        assert_eq!(fixtures::people_count(&lib), before);
    }

    #[gpui::test]
    fn cached_review_picker_cannot_name_a_reanalysed_speaker(cx: &mut TestAppContext) {
        let (window, _app, lib, s, _dir) = open_review(cx);
        let picker = start(&window, cx, 0);
        let before = fixtures::people_count(&lib);
        lib.write(|tx| {
            let mut meeting = tx.library().load_meeting(&s.meeting)?;
            meeting.speaker_namespace = "replacement-analysis".into();
            tx.save_meeting(&meeting)
        })
        .unwrap();
        // The index row and view have not refreshed: the locked check must refuse.
        picker.update(cx, |p, cx| p.pick(0, cx));
        cx.run_until_parked();
        assert_eq!(named(&lib, &s, 1), (Assignment::None, None));
        assert_eq!(fixtures::people_count(&lib), before);
    }

    #[gpui::test]
    fn review_hides_merged_members_and_refuses_a_regrouped_row(cx: &mut TestAppContext) {
        let (window, _app, lib, s, _dir) = open_review(cx);
        let before = fixtures::people_count(&lib);
        let picker = start(&window, cx, 0);
        // Speaker 3 merged into speaker 1 elsewhere; the index still lists
        // speaker 3 and the view has not reloaded.
        lib.write(|tx| {
            let mut m = tx.library().load_meeting(&s.meeting)?;
            m.speaker_groups
                .push(broscribe_core::schema::SpeakerGroup::new(
                    sid(1),
                    [sid(3)],
                    broscribe_core::schema::GroupOrigin::User,
                ));
            tx.save_meeting(&m)
        })
        .unwrap();
        picker.update(cx, |p, cx| p.pick(0, cx));
        cx.run_until_parked();
        assert_eq!(named(&lib, &s, 1), (Assignment::None, None));
        assert_eq!(named(&lib, &s, 3), (Assignment::None, None));
        assert_eq!(fixtures::people_count(&lib), before);
        let m = lib.load_meeting(&s.meeting).unwrap();
        window
            .update(cx, |view, _, _| {
                assert!(
                    view.message.as_deref().unwrap().contains("regrouped"),
                    "{:?}",
                    view.message
                );
                // Reloaded after the refusal: the member has no row, and the
                // rows carry the meeting's speakers as read now.
                let speakers: Vec<SpeakerId> = view
                    .rows()
                    .iter()
                    .map(|r| r.entry.item.speaker_id)
                    .collect();
                assert_eq!(speakers, [sid(1), sid(2)]);
                assert_eq!(view.rows()[0].fence.as_deref(), Some(&SpeakerFence::of(&m)));
            })
            .unwrap();
        assert!(
            load_review(&lib)
                .unwrap()
                .rows
                .iter()
                .all(|r| r.entry.item.speaker_id != sid(3))
        );
    }

    #[gpui::test]
    fn review_reload_closes_picker_when_speaker_number_is_reused(cx: &mut TestAppContext) {
        let (window, _app, lib, s, _dir) = open_review(cx);
        let picker = start(&window, cx, 0);
        lib.write(|tx| {
            let mut meeting = tx.library().load_meeting(&s.meeting)?;
            meeting.speaker_namespace = "replacement-analysis".into();
            tx.save_meeting(&meeting)
        })
        .unwrap();
        window.update(cx, |view, _, cx| view.reload(cx)).unwrap();
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| {
                assert!(view.rows.iter().any(|r| r.entry.item.speaker_id == sid(1)));
                assert!(view.picker().is_none());
            })
            .unwrap();
        picker.update(cx, |p, cx| p.pick(0, cx));
        cx.run_until_parked();
        assert_eq!(named(&lib, &s, 1), (Assignment::None, None));
    }

    /// The retained queue reads no meetings while another route shows: a
    /// change (watcher or window activation) only marks it stale, and
    /// showing the queue again reloads once, keeping the selected speaker.
    #[gpui::test]
    fn a_hidden_queue_reloads_once_when_shown_again(cx: &mut TestAppContext) {
        let (window, app, lib, s, _dir) = open_review(cx);
        let loaded = window
            .update(cx, |view, _, cx| {
                assert_eq!(keys(view), [sid(1), sid(2), sid(3)]);
                view.select(1, cx);
                view.generation
            })
            .unwrap();
        app.update(cx, |st, cx| st.navigate(Route::library(), cx));
        // Speaker 1 merged into speaker 2 elsewhere: its row goes on reload.
        lib.write(|tx| {
            let mut m = tx.library().load_meeting(&s.meeting)?;
            m.speaker_groups
                .push(broscribe_core::schema::SpeakerGroup::new(
                    sid(2),
                    [sid(1)],
                    broscribe_core::schema::GroupOrigin::User,
                ));
            tx.save_meeting(&m)
        })
        .unwrap();
        app.update(cx, |st, cx| {
            st.library_changed(
                &crate::watch::Changes {
                    index: true,
                    people: true,
                    ..Default::default()
                },
                cx,
            );
            st.refresh_all(cx);
        });
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| {
                assert_eq!(view.generation, loaded, "no read while hidden");
                assert_eq!(keys(view), [sid(1), sid(2), sid(3)]);
            })
            .unwrap();
        app.update(cx, |st, cx| st.navigate(Route::ReviewQueue, cx));
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| {
                assert_eq!(view.generation, loaded + 1, "one reload on return");
                let mut shown = keys(view);
                shown.sort();
                assert_eq!(shown, [sid(2), sid(3)]);
                assert_eq!(view.rows()[view.selected()].entry.item.speaker_id, sid(2));
            })
            .unwrap();
    }

    #[gpui::test]
    fn review_picker_lists_the_rows_closest_voices(cx: &mut TestAppContext) {
        let (window, app, lib, s, _dir) = open_review(cx);
        lib.write(|tx| {
            let mut m = tx.library().load_meeting(&s.meeting)?;
            fixtures::set_voice(&mut m, 1, &fixtures::voice(1.0));
            tx.save_meeting(&m)
        })
        .unwrap();
        fixtures::enroll(&lib, &s.ana, &fixtures::voice(1.0));
        fixtures::enroll(&lib, &s.jose, &fixtures::voice(0.6));
        app.update(cx, |st, cx| {
            st.library_changed(
                &crate::watch::Changes {
                    index: true,
                    people: true,
                    ..Default::default()
                },
                cx,
            )
        });
        cx.run_until_parked();
        let row = window
            .update(cx, |view, _, _| {
                view.rows()
                    .iter()
                    .position(|r| r.entry.item.speaker_id == sid(1))
                    .unwrap()
            })
            .unwrap();
        let picker = start(&window, cx, row);
        let closest: Vec<PersonId> = picker.read_with(cx, |p, _| {
            p.closest_voices().map(|c| c.id.clone()).collect()
        });
        assert_eq!(closest, [s.ana.clone(), s.jose.clone()]);
    }

    /// The recheck's lines as assistive technology gets them: the view's
    /// own recheck through the nodes its render shows, each identified,
    /// with a unique ID, as (role, label).
    fn spoken_recheck(
        window: &WindowHandle<ReviewView>,
        cx: &mut TestAppContext,
    ) -> Vec<(Role, String)> {
        window
            .update(cx, |view, _, cx| {
                let ShownRecheck {
                    outcome,
                    lines,
                    failure,
                    ..
                } = view.recheck(cx).unwrap();
                let nodes: Vec<(String, Role, String)> =
                    learning::recheck_nodes("review-recheck", outcome, lines, failure, cx)
                        .iter()
                        .map(|node| {
                            crate::ui::a11y_name(node).expect("every recheck line is identified")
                        })
                        .collect();
                let ids: std::collections::BTreeSet<&str> =
                    nodes.iter().map(|(id, ..)| id.as_str()).collect();
                assert_eq!(ids.len(), nodes.len(), "IDs are unique: {nodes:?}");
                nodes
                    .into_iter()
                    .map(|(_, role, label)| (role, label))
                    .collect()
            })
            .unwrap()
    }

    /// The Review queue naming route end to end: the name saves at once and
    /// the message says only that; core stores the recheck job (no names in
    /// it, none in the worker's argv); the queue worker's committed writes
    /// and progress reload the rows and show this meeting's result first,
    /// then the other meeting's, with links; a weaker match stays a pending
    /// suggestion; leaving and returning, and a fresh read as after an
    /// agent reconnect, keep the committed counts. A newer read that fails
    /// (a linked meeting became unreadable) is shown, and reaches assistive
    /// technology as an alert, under the last known lines, which stay.
    #[gpui::test]
    fn naming_in_review_rechecks_other_speakers_and_keeps_committed_counts(
        cx: &mut TestAppContext,
    ) {
        use std::cell::RefCell;
        use std::ffi::OsString;
        use std::rc::Rc;

        use broscribe_core::config::{Config, Thresholds};
        use broscribe_core::learning::REIDENTIFY_KIND;
        use broscribe_core::queue::Queue;
        use broscribe_core::schema::{ProcessingStatus, Source, Speaker};
        use broscribe_platform::fakes::FakePaths;
        use broscribe_platform::resolve_paths;

        use crate::views::person_picker::fixtures::{set_voice, voice, voice_model};

        let dir = tempfile::tempdir().unwrap();
        // The app's local worker, started on the test thread: its argv is
        // recorded, and its exit is reported through the same channel.
        let launches: Rc<RefCell<Vec<Vec<OsString>>>> = Rc::default();
        let mut config = Config::default();
        config.identity.insert(
            voice_model(),
            Thresholds {
                auto_name: Some(0.90),
                suggest: Some(0.80),
                margin: Some(0.10),
                ..Default::default()
            },
        );
        // The worker reads the active model and thresholds from the data
        // folder's config, as the agent does, never from the app's copy.
        config.save(&dir.path().join("data")).unwrap();
        let library = Library::open(dir.path().join("lib")).unwrap();
        let s = fixtures::seed(&library);
        // Speakers 1 and 3 of the seeded meeting share a voice; an older
        // meeting has that voice (S1) and a weaker match (S2).
        let mut a = library.load_meeting(&s.meeting).unwrap();
        set_voice(&mut a, 1, &voice(1.0));
        set_voice(&mut a, 3, &voice(1.0));
        let mut b = Meeting::new(
            MeetingId::random(),
            Source::Import,
            DateTime::parse_from_rfc3339("2026-11-01T10:00:00Z").unwrap(),
        );
        b.title = Some("Synthetic review".into());
        b.duration_s = 60.0;
        b.processing.status = ProcessingStatus::Done;
        for n in 1..=2 {
            let mut speaker = Speaker::new(sid(n), "t0");
            speaker.talk_s = 20.0;
            b.speakers.push(speaker);
            b.segments.push(seg(
                n,
                n,
                f64::from(n) * 5.0,
                f64::from(n) * 5.0 + 3.0,
                "words",
            ));
        }
        set_voice(&mut b, 1, &voice(1.0));
        set_voice(&mut b, 2, &voice(0.85));
        library
            .write(|tx| {
                tx.save_meeting(&a)?;
                tx.create_meeting_dir(&b)?;
                tx.save_meeting(&b)?;
                broscribe_core::index::Index::rebuild(tx).map(|_| ())
            })
            .unwrap();
        let lib = library.clone();
        let app = cx.new(|cx| {
            AppState::new(
                crate::test_support::app_init(
                    resolve_paths(&FakePaths::new(dir.path())).unwrap(),
                    config.clone(),
                    library,
                    dir.path().join("broscribe"),
                ),
                cx,
            )
        });
        app.update(cx, |st, _| {
            let launches = launches.clone();
            st.worker_mut().set_launcher(move |_, args, _| {
                launches.borrow_mut().push(args.to_vec());
                Ok(())
            });
        });
        cx.update(gpui::init);
        app.update(cx, |st, cx| st.navigate(Route::ReviewQueue, cx));
        let window = cx.add_window({
            let app = app.clone();
            |window, cx| ReviewView::new(app, window, cx)
        });
        cx.run_until_parked();
        let waiting = |window: &WindowHandle<ReviewView>, cx: &mut TestAppContext| {
            window
                .update(cx, |view, _, _| {
                    view.rows()
                        .iter()
                        .map(|r| (r.entry.item.meeting_id.clone(), r.entry.item.speaker_id))
                        .collect::<Vec<_>>()
                })
                .unwrap()
        };
        // The three close voices now share a row. Keep just the first
        // occurrence in this decision so the other two exercise recheck.
        assert!(waiting(&window, cx).contains(&(s.meeting.clone(), sid(1))));
        assert!(!waiting(&window, cx).contains(&(s.meeting.clone(), sid(3))));
        assert!(!waiting(&window, cx).contains(&(b.id.clone(), sid(1))));
        let picker = start(&window, cx, 0);
        window
            .update(cx, |_, window, cx| {
                picker.update(cx, |p, cx| {
                    p.set_query("ana@example", window, cx);
                    let ix = p.candidates().position(|c| c.id == s.ana).unwrap();
                    p.pick(ix, cx);
                });
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |view, _, cx| {
                view.select_identification(&(s.meeting.clone(), sid(1)));
                view.save_identification(cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            named(&lib, &s, 1),
            (Assignment::Confirmed, Some(s.ana.clone()))
        );
        // Nobody else is named by the naming itself; the message says so.
        assert_eq!(named(&lib, &s, 3), (Assignment::None, None));
        window
            .update(cx, |view, _, cx| {
                let message = view.message().unwrap();
                assert!(
                    message.contains("Linked Ana Silva in 1 meeting"),
                    "{message}"
                );
                assert!(message.contains("Checking other speakers"), "{message}");
                let ShownRecheck {
                    outcome,
                    lines,
                    links,
                    failure,
                } = view.recheck(cx).unwrap();
                // Still checking: nothing ended yet.
                assert_eq!(outcome, None);
                assert_eq!(lines[0], "Checking other speakers, this meeting first…");
                assert!(links.is_empty());
                assert_eq!(failure, None);
            })
            .unwrap();
        let request = app.read_with(cx, |st, _| st.recheck_request().cloned().unwrap());
        let queue = Queue::open(lib.root()).unwrap();
        let job = queue.get(&request.job_id).unwrap();
        assert_eq!(job.kind, REIDENTIFY_KIND);
        let stored = job.input.to_string();
        assert!(
            !stored.contains("Ana") && !stored.contains("line 1"),
            "{stored}"
        );
        // The kick started the app's worker child: flags and the library only.
        let argv: Vec<String> = launches
            .borrow()
            .iter()
            .map(|args| {
                args.iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();
        assert_eq!(argv.len(), 1, "{argv:?}");
        assert!(argv[0].ends_with("queue work --json-progress"), "{argv:?}");
        assert!(
            !argv[0].contains("Ana") && !argv[0].contains("line"),
            "{argv:?}"
        );
        // That child exits; the agent's worker takes the job here.
        let worker = app.update(cx, |st, _| st.worker_mut().sender());
        worker.send(crate::jobs::WorkerMsg::Finished);
        cx.run_until_parked();

        // The queue worker (the agent's, here in-process) runs the recheck;
        // its final event reaches the app as the agent forwards it.
        let decoders = broscribe_platform::DecoderChain::new(Vec::new());
        let probe = broscribe_platform::fakes::FakeProcessProbe::new(std::process::id());
        let execution = broscribe_models::ExecutionContext::new(
            std::sync::Arc::new(broscribe_platform::fakes::FakeProcessTreeControl::default()),
            std::sync::Arc::new(broscribe_platform::fakes::FakeTrustStore::default()),
        );
        let env = broscribe_pipeline::Env {
            lib: &lib,
            config: &config,
            decoders: &decoders,
            probe: &probe,
            progress: None,
            execution: &execution,
        };
        let options = broscribe_pipeline::worker::WorkerOptions::new(
            dir.path().join("cache"),
            std::sync::Arc::new(broscribe_platform::encoder::FakeEncoder::default()),
            dir.path().join("data"),
            std::sync::Arc::new(broscribe_platform::fakes::FakeSecretStore::default()),
            std::sync::Arc::new(broscribe_platform::FakeCalendarAuthority::default()),
        );
        let mut last_message = None;
        for _ in 0..5 {
            broscribe_pipeline::worker::work_job(
                &env,
                &broscribe_models::fakes::FakeModelProvider::default(),
                &options,
                &request.job_id,
                &mut |e: &broscribe_pipeline::worker::WorkerEvent| {
                    if e.message.is_some() {
                        last_message.clone_from(&e.message);
                    }
                },
            )
            .unwrap();
            if queue.get(&request.job_id).unwrap().state == JobState::Done {
                break;
            }
        }
        assert_eq!(queue.get(&request.job_id).unwrap().state, JobState::Done);
        let message = last_message.unwrap();
        assert!(!message.contains("Ana"), "{message}");
        app.update(cx, |st, cx| {
            st.daemon_progress(
                &broscribe_ipc::JobProgress {
                    job_id: request.job_id.to_string(),
                    kind: REIDENTIFY_KIND.into(),
                    meeting_id: None,
                    phase: "done".into(),
                    fraction: Some(1.0),
                    message: Some(message),
                },
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(named(&lib, &s, 3), (Assignment::Auto, Some(s.ana.clone())));
        let rows = waiting(&window, cx);
        // Trusted automatic names leave the queue; the weaker match waits.
        assert!(!rows.contains(&(s.meeting.clone(), sid(3))), "{rows:?}");
        assert!(!rows.contains(&(b.id.clone(), sid(1))), "{rows:?}");
        assert!(rows.contains(&(b.id.clone(), sid(2))), "{rows:?}");
        let b_saved = lib.load_meeting(&b.id).unwrap();
        assert_eq!(
            b_saved.speaker(sid(2)).unwrap().assignment,
            Assignment::Suggested
        );
        let shown = |window: &WindowHandle<ReviewView>, cx: &mut TestAppContext| {
            window
                .update(cx, |view, _, cx| view.recheck(cx).unwrap())
                .unwrap()
        };
        let ShownRecheck {
            outcome,
            lines,
            links,
            failure,
        } = shown(&window, cx);
        assert_eq!(outcome, Some(learning::FINISHED));
        assert_eq!(failure, None);
        assert!(
            lines[0].starts_with("Synthetic sync: 1 named automatically"),
            "{lines:?}"
        );
        assert_eq!(
            lines[1], "Other meetings: 1 named automatically, 1 suggestion.",
            "{lines:?}"
        );
        assert_eq!(links.len(), 2, "{links:?}");
        assert_eq!(links[0].0, s.meeting);
        assert_eq!(links[1].0, b.id);
        for line in lines.iter().chain(links.iter().map(|(_, l)| l)) {
            assert!(!line.contains("Ana"), "{line}");
        }

        // Leaving and coming back keeps the result.
        app.update(cx, |st, cx| st.navigate(Route::people(), cx));
        cx.run_until_parked();
        app.update(cx, |st, cx| st.navigate(Route::ReviewQueue, cx));
        cx.run_until_parked();
        assert_eq!(shown(&window, cx).lines, lines);
        // A fresh view and read, as after an agent reconnect: the committed
        // counts come from the job (meetings named in first).
        let fresh = cx.add_window({
            let app = app.clone();
            |window, cx| ReviewView::new(app, window, cx)
        });
        app.update(cx, |st, cx| st.refresh_recheck(cx));
        cx.run_until_parked();
        let again = shown(&fresh, cx).lines;
        // Assistive technology hears every line, not only the section's
        // name: how the recheck ended as a Status, then each line as a label.
        let finished = (Role::Status, learning::FINISHED.to_owned());
        assert_eq!(
            spoken_recheck(&fresh, cx),
            std::iter::once(finished.clone())
                .chain(again.iter().map(|line| (Role::Label, line.clone())))
                .collect::<Vec<_>>()
        );
        assert!(
            again[0].starts_with("Meetings named in: 1 named automatically"),
            "{again:?}"
        );
        assert_eq!(again[1], lines[1]);

        // A newer read fails (a linked meeting became unreadable): the error
        // shows under the last known result, which stays as it was, and
        // assistive technology hears both: the kept lines, then the failure
        // as an alert.
        let path = lib
            .meeting_dir(&b.id)
            .unwrap()
            .join(broscribe_core::paths::MEETING_JSON);
        std::fs::write(&path, b"{ not json").unwrap();
        app.update(cx, |st, cx| st.refresh_recheck(cx));
        cx.run_until_parked();
        let failed = shown(&fresh, cx);
        assert_eq!(failed.lines, again);
        let failure = failed.failure.expect("the failed read is shown");
        assert!(
            failure.starts_with("Couldn't update the speaker recheck. "),
            "{failure}"
        );
        assert!(
            failure.ends_with("The lines above are its last known state."),
            "{failure}"
        );
        let spoken = spoken_recheck(&fresh, cx);
        assert_eq!(
            spoken,
            std::iter::once(finished)
                .chain(again.iter().map(|line| (Role::Label, line.clone())))
                .chain([(Role::Alert, failure)])
                .collect::<Vec<_>>()
        );
    }
}
