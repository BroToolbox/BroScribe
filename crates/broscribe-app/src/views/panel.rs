//! Recording controls are IPC writes; closing this floating window never stops capture.
//!
//! - Every control (session buttons, language, title, participants, tags,
//!   notes, discard, retry, live decisions) keeps its own pending / done /
//!   failed status per recording (and per live row), shown beside it
//!   (`Status` / `Alert` roles) until the next action on that control for
//!   that recording; meters, other recordings, other rows and other events
//!   never clear it. A control waiting for the agent ignores a second
//!   submit for the same recording and never blocks another recording.
//! - Each request names the session it was made for. Its answer only
//!   changes drafts, the roster or tags while that session is still the one
//!   the control acts on: a result for an older session never touches a
//!   newer one. A new session clears the drafts and statuses of the
//!   previous one and resets the language to its own.
//! - A draft (note, tag, participant search) is cleared only after the
//!   agent accepted it, and only while it still holds exactly what was
//!   sent: text typed meanwhile stays. Enter submits; empty input is
//!   disabled.
//! - Participants are chosen with the shared [`PersonPicker`]: a registered
//!   person by ID, or a typed name through its explicit "Use typed name"
//!   (the agent creates a name-only person; no voice is learned). This
//!   confirms attendance only, never a voice.
//! - Tags are added literally from comma-separated input (`+tag` on the
//!   wire, so a typed `-x` adds `-x`) and removed with the chip's Remove.
//! - Titles and names reach the agent over IPC only, never a child's argv.
//!   Every recording (the active one and each stopped one not saved yet)
//!   has its own title field, renamed as itself.
//! - "Stop and save" queues the final pass; "Discard" asks first, says that
//!   capture continues until confirmed, and is offered for a stopped
//!   recording until it is saved. A stopped recording says whether it waits,
//!   is being processed, retries (with its countdown) or failed (with an
//!   explicit Retry); a saved one offers Open / Delete meeting… and says how
//!   many live names await review in the meeting.
//! - The live transcript's model, the speaker analysis and the meeting
//!   reader report separately (loading, unavailable, degraded, turned off),
//!   with each track's capture, lost audio and the disk's headroom, and the
//!   low-disk warning before the protective stop. None of it ends capture.
//! - Live speaker turns (plan D) replace the track-only lines once they
//!   exist: the client keeps them from the authoritative `record.live`
//!   snapshot and `live_turn` upserts (never a lower revision), bounded like
//!   the agent's: the turns it published first go first (`record.live` lists
//!   them in that order), whatever their time, while rows show in time
//!   order. Name…, Confirm and Clear act on one row exactly as it was shown
//!   (revision, epoch, evidence); a row that changes, or leaves the preview,
//!   under an open picker closes it with nothing saved. A live name labels
//!   that line in the live transcript. Final processing can name the
//!   saved speaker and learn from agreeing live names.
//! - Only rows in view are rendered; the list measures each row once and
//!   again only when it changed, and meters or the clock never rebuild the
//!   rows. The clock ticks once a second only while something shown counts
//!   (recording, an automatic-stop or retry countdown) in a presented window.
//! - The live transcript follows the newest line while it is scrolled to
//!   the bottom. Scrolling up keeps the view where the user put it until
//!   they scroll back down; rows added or dropped elsewhere never move the
//!   rows in view. While a naming picker is open nothing scrolls by itself
//!   (the row, typed name and focus stay); closing it follows the newest
//!   line again only if the view did when it opened.
//! - Once the capture shown ends (the agent lists it as stopped or saved;
//!   a pending automatic stop is not an end) the panel closes, unless the
//!   user is still using it: a request in flight, an unsaved title, an open
//!   confirmation, a failure to read, a notification shown here. The final
//!   pass, notifications and recovery go on without it; the menu or a
//!   notification reopens it, and then it stays until the user closes it.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use broscribe_core::MeetingId;
use broscribe_core::names::folded_name;
use broscribe_ipc::live::{
    LiveAction, LiveAttributeOutcome, LiveAttributeParams, LiveIdentityState, LiveIdentityStatus,
    LiveSnapshot, LiveTurn, LiveTurnStatus, SuggestionSource, Track, UnresolvedReason,
};
use broscribe_ipc::protocol::{
    ParticipantAction, ReaderStatus, SavedRecording, StoppedRecording, StoppedRecordingState,
    WorkerProblem,
};
use broscribe_ipc::{
    AutoStopApp, AutoStopReason, AutoStopStatus, Call, CaptureWarning, DiskHealth, DiskLevel,
    Event, IpcError, JournalIssue, LiveSpeaker, PreviewState, RecordAutoStopParams, RecordingState,
    RecordingStatus, RequestId, TrackHealth, TrackState,
};
use broscribe_platform::overlay::{
    CaptureExclusion, OVERLAY_INSET, OverlayCapability, OverlayFocus, OverlayPlacement,
    OverlayPresence, OverlayRect, RecordingOverlay, right_center,
};
use broscribe_readers::{IncompleteReason, ManualAccessibility, ReaderHealth};
use gpui::StatefulInteractiveElement as _;
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::input::{Input, InputEvent, InputState};
use gpui::component::{ActiveTheme as _, Disableable as _, Root};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, Entity, ExternalPaths, FollowMode,
    InteractiveElement as _, IntoElement, ListAlignment, ListState, ParentElement as _, Pixels,
    Render, Role, SharedString, Stateful, StyleRefinement, Styled as _, Subscription,
    TitlebarOptions, WeakEntity, Window, WindowBounds, WindowHandle, WindowKind, WindowOptions,
    div, list, px, size,
};
use serde_json::Value;

use crate::daemon::{DaemonClient, agent_error_text};
use crate::jobs::{core_user_text, failure_text};
use crate::launch::AgentStarter;
use crate::notify::NotificationHost;
use crate::state::{AppState, Route};
use crate::ui::format::{format_clock, spoken_position};
use crate::ui::kit;
use crate::ui::{spoken_seconds, text_line, timer_line};
use crate::views::delete_meeting::{DeleteEvent, DeleteMeetingPrompt};
use crate::views::meeting::typed_tags;
use crate::views::person_picker::{PersonChoice, PersonPicker, PersonPickerEvent};

/// The panel view: its controls, the live transcript list and rendering.
pub mod dock;
mod dock_view;
mod view;

#[derive(Clone, Debug, PartialEq)]
pub enum PreviewLine {
    Transcript {
        at_s: f64,
        speaker: String,
        text: String,
    },
    Gap {
        from_s: f64,
        to_s: f64,
        track: Track,
    },
}
impl PreviewLine {
    fn at_s(&self) -> f64 {
        match self {
            Self::Transcript { at_s, .. } => *at_s,
            Self::Gap { from_s, .. } => *from_s,
        }
    }

    fn presentation(&self) -> (String, Option<&'static str>, &str, String) {
        match self {
            Self::Transcript {
                at_s,
                speaker,
                text,
            } => (
                format!("{} · {speaker}", format_clock(*at_s)),
                Some("Preview"),
                text,
                format!("{speaker} at {}, Preview: {text}", spoken_position(*at_s)),
            ),
            Self::Gap {
                from_s,
                to_s,
                track,
            } => (
                format!(
                    "{}–{} · {}",
                    format_clock(*from_s),
                    format_clock(*to_s),
                    track_label(*track)
                ),
                None,
                "Not shown live. The recording has this part.",
                format!(
                    "{} from {} to {}. Not shown live. The recording has this part.",
                    track_label(*track),
                    spoken_position(*from_s),
                    spoken_position(*to_s)
                ),
            ),
        }
    }
}

/// Track-only preview lines kept (the preview is ephemeral; memory stays
/// bounded whatever the recording's length).
pub const PREVIEW_LINES_MAX: usize = 200;

/// Live turns kept: the agent's own bound (`LiveIdentitySettings::max_turns`).
/// The agent drops its oldest turns first, in the order it published them,
/// and the panel drops the same ones, so it never shows (or acts on) a row
/// the agent no longer has.
pub fn live_turns_max() -> usize {
    broscribe_ipc::live::LiveIdentitySettings::default().max_turns
}

/// What a capture warning is about: a later warning about the same thing
/// replaces the earlier one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum WarningSlot {
    Track(Track),
    Disk,
    Preview,
}

impl WarningSlot {
    fn of(warning: &CaptureWarning) -> Self {
        match warning {
            CaptureWarning::TrackDown { track, .. }
            | CaptureWarning::TrackResumed { track, .. } => Self::Track(*track),
            CaptureWarning::DiskLow { .. } => Self::Disk,
            CaptureWarning::PreviewUnavailable => Self::Preview,
        }
    }
}

#[derive(Default)]
pub struct PanelModel {
    detection_enabled: bool,
    pub recording: Option<RecordingState>,
    pub(crate) access: broscribe_ipc::CaptureAccess,
    /// Stopped recordings not saved yet (queued, processing, retrying,
    /// failed, or discarding).
    pub stopped: Vec<StoppedRecording>,
    /// Recordings this agent run saved as meetings, newest first.
    pub saved: Vec<SavedRecording>,
    /// Journals the agent's recovery could not bring back (or whose final
    /// job it could not queue): a damaged one is kept for repair, another
    /// is tried again by the agent's next sweep ([`save_failure`]).
    pub journal_issues: Vec<JournalIssue>,
    /// The live preview's ASR worker (loading, ready, restarting, gone).
    pub preview: PreviewState,
    pub lines: VecDeque<PreviewLine>,
    /// How many lines were ever dropped from the front of `lines` (bounded,
    /// or cleared for a new session): `lines[i]` is line number
    /// `line_base + i` for as long as it is shown.
    line_base: u64,
    pub people: BTreeMap<String, String>,
    pub meters: [i16; 2],
    /// Live speaker turns of the active session, in time order; once any
    /// exist they replace the track-only transcript lines.
    pub turns: Vec<LiveTurn>,
    /// The turn IDs of `turns` in the order the agent first published
    /// them: its eviction order.
    arrivals: VecDeque<String>,
    pub identity: LiveIdentityState,
    /// Updates this subscriber missed (it read too slowly) since the last
    /// authoritative state.
    pub missed_updates: Option<u64>,
    /// The latest capture warnings of the active session, one per track,
    /// disk and preview feed.
    warnings: BTreeMap<WarningSlot, CaptureWarning>,
    last_meter: Option<Instant>,
    pending_meters: Option<[i16; 2]>,
    elapsed_anchor: Option<Instant>,
    /// A `record.live` snapshot is due (reconnect, lag, new session, stale
    /// decision).
    snapshot_wanted: bool,
    /// Turns updated since the snapshot in flight was requested.
    snapshot_pending: Option<BTreeSet<String>>,
    /// Bumped by every change except meters: the panel redoes its
    /// bookkeeping only when it moved.
    revision: u64,
    /// Bumped whenever the transcript's rows (turns or lines) change.
    rows_revision: u64,
}
impl PanelModel {
    /// Whole-dB display values suppress subpixel jitter; accepted changes ≤10 Hz.
    pub fn meters(&mut self, me: f32, others: f32, now: Instant) -> bool {
        let values = [me, others].map(|v| {
            if v.is_finite() {
                v.clamp(-120., 0.).round() as i16
            } else {
                -120
            }
        });
        if self
            .last_meter
            .is_some_and(|last| now.duration_since(last) < Duration::from_millis(100))
        {
            self.pending_meters = Some(values);
            return false;
        }
        self.pending_meters = None;
        self.last_meter = Some(now);
        if values == self.meters {
            return false;
        }
        self.meters = values;
        true
    }

    pub fn meter_flush_delay(&self, now: Instant) -> Option<Duration> {
        self.pending_meters?;
        Some(self.last_meter.map_or(Duration::ZERO, |last| {
            Duration::from_millis(100).saturating_sub(now.duration_since(last))
        }))
    }

    pub fn flush_meters(&mut self, now: Instant) -> bool {
        let Some(values) = self.pending_meters.take() else {
            return false;
        };
        self.meters(values[0] as f32, values[1] as f32, now)
    }

    /// Applies one event; `true` when anything shown changed. Meters never
    /// move [`Self::revision`] or the transcript's rows.
    pub fn apply(&mut self, event: &Event, now: Instant) -> bool {
        if let Event::Meters {
            me_dbfs,
            others_dbfs,
        } = event
        {
            return self.meters(*me_dbfs, *others_dbfs, now);
        }
        let changed = self.apply_content(event, now);
        if changed {
            self.revision += 1;
        }
        changed
    }

    fn apply_content(&mut self, event: &Event, now: Instant) -> bool {
        match event {
            Event::State(state) => {
                if self.recording.as_ref().map(|r| &r.session_id)
                    != state.recording.as_ref().map(|r| &r.session_id)
                {
                    self.line_base += self.lines.len() as u64;
                    self.lines.clear();
                    self.people.clear();
                    self.turns.clear();
                    self.arrivals.clear();
                    self.identity = LiveIdentityState::default();
                    self.warnings.clear();
                    self.snapshot_pending = None;
                    self.snapshot_wanted = state.recording.is_some();
                    self.last_meter = None;
                    self.meters = [-120; 2];
                    self.pending_meters = None;
                    self.rows_revision += 1;
                }
                // The authoritative state: whatever was missed is in it,
                // except live rows (the snapshot request brings those).
                let changed = self.recording != state.recording
                    || self.access != state.access
                    || self.detection_enabled != state.detection_enabled
                    || self.preview != state.preview
                    || self.stopped != state.stopped
                    || self.saved != state.saved
                    || self.journal_issues != state.journal_issues
                    || self.missed_updates.is_some();
                self.missed_updates = None;
                self.elapsed_anchor = Some(now);
                self.recording = state.recording.clone();
                self.access = state.access;
                self.detection_enabled = state.detection_enabled;
                self.stopped = state.stopped.clone();
                self.saved = state.saved.clone();
                self.journal_issues = state.journal_issues.clone();
                self.preview = state.preview.clone();
                // A disk warning stays until a snapshot reports the disk Ok,
                // so one that arrives after an Ok snapshot shows at once
                // (the agent sends the two in either order). Limit: nothing
                // orders a snapshot against the warning, so a stale Ok
                // snapshot arriving after the warning clears the alert
                // until a later Warning (or Low) snapshot shows it again.
                if self
                    .recording
                    .as_ref()
                    .and_then(|r| r.health.disk)
                    .is_some_and(|d| d.level == DiskLevel::Ok)
                {
                    self.warnings.remove(&WarningSlot::Disk);
                }
                changed
            }
            // Live-preview events name the recording that produced them: a
            // stopped one's late output never changes the shown recording.
            Event::Preview {
                session_id,
                preview,
            } => {
                if self.active_session() != Some(session_id.as_str()) {
                    return false;
                }
                let changed = self.preview != *preview;
                self.preview = preview.clone();
                changed
            }
            Event::CaptureWarning {
                session_id,
                warning,
            } => {
                if self.active_session() != Some(session_id.as_str()) {
                    return false;
                }
                let slot = WarningSlot::of(warning);
                self.warnings.insert(slot, warning.clone()).as_ref() != Some(warning)
            }
            Event::LiveTranscript {
                session_id,
                segment,
            } => {
                if self.active_session() != Some(session_id.as_str()) {
                    return false;
                }
                let speaker = match &segment.speaker {
                    LiveSpeaker::Me => "Me".to_owned(),
                    LiveSpeaker::Others => "Others".to_owned(),
                    LiveSpeaker::Person { person_id, name } => {
                        self.people.insert(person_id.clone(), name.clone());
                        name.clone()
                    }
                };
                self.push(PreviewLine::Transcript {
                    at_s: segment.start_s,
                    speaker,
                    text: segment.text.clone(),
                });
                true
            }
            Event::PreviewGap {
                session_id,
                track,
                from_s,
                to_s,
            } => {
                if self.active_session() != Some(session_id.as_str()) {
                    return false;
                }
                self.push(PreviewLine::Gap {
                    from_s: *from_s,
                    to_s: *to_s,
                    track: *track,
                });
                true
            }
            Event::Lagged { dropped } => {
                self.missed_updates = Some(self.missed_updates.unwrap_or(0) + dropped);
                self.want_snapshot();
                true
            }
            Event::LiveTurn(turn) => self.upsert_turn((**turn).clone()),
            Event::LiveIdentity {
                session_id,
                identity,
            } => {
                if self.active_session() != Some(session_id.as_str()) {
                    return false;
                }
                let changed = self.identity != *identity;
                self.identity = identity.clone();
                changed
            }
            _ => false,
        }
    }

    /// The connection to the agent was lost: the rows stay as they were and
    /// the authoritative ones are fetched again on reconnect. Nothing is
    /// claimed about the preview meanwhile (the panel says the agent is
    /// disconnected).
    pub fn disconnected(&mut self) {
        let elapsed = self.elapsed_s(Instant::now());
        if let Some(recording) = &mut self.recording {
            recording.elapsed_s = elapsed;
        }
        self.elapsed_anchor = None;
        self.pending_meters = None;
        self.revision += 1;
    }

    /// Bumped by every change except meters.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Bumped whenever the transcript's rows change.
    pub fn rows_revision(&self) -> u64 {
        self.rows_revision
    }

    /// The latest capture warnings of the active session.
    pub fn capture_warnings(&self) -> impl Iterator<Item = &CaptureWarning> {
        self.warnings.values()
    }

    fn push(&mut self, line: PreviewLine) {
        if self.lines.len() == PREVIEW_LINES_MAX {
            self.lines.pop_front();
            self.line_base += 1;
        }
        self.lines.push_back(line);
        self.rows_revision += 1;
    }

    /// The line numbered `key` (see `line_base`), while it is shown.
    pub fn line(&self, key: u64) -> Option<&PreviewLine> {
        let ix = usize::try_from(key.checked_sub(self.line_base)?).ok()?;
        self.lines.get(ix)
    }

    /// The shown lines with their stable numbers.
    pub fn numbered_lines(&self) -> impl Iterator<Item = (u64, &PreviewLine)> {
        (self.line_base..).zip(&self.lines)
    }

    /// `record.lang` for `session` as shown.
    pub fn language_action(session_id: &str, language: &str) -> Call {
        Call::RecordLanguage {
            session_id: session_id.to_owned(),
            language: language.trim().to_owned(),
        }
    }

    pub fn elapsed_s(&self, now: Instant) -> f64 {
        self.recording.as_ref().map_or(0., |recording| {
            recording.elapsed_s
                + if recording.status == RecordingStatus::Recording {
                    self.elapsed_anchor
                        .map_or(0., |anchor| now.duration_since(anchor).as_secs_f64())
                } else {
                    0.
                }
        })
    }

    pub fn active_session(&self) -> Option<&str> {
        self.recording.as_ref().map(|r| r.session_id.as_str())
    }

    /// Time since the last state snapshot arrived (countdowns shown from it).
    pub fn since_state(&self, now: Instant) -> Duration {
        self.elapsed_anchor.map_or(Duration::ZERO, |anchor| {
            now.saturating_duration_since(anchor)
        })
    }

    /// Whether `session` is the active recording or a stopped one not saved yet.
    pub fn knows_session(&self, session: &str) -> bool {
        self.active_session() == Some(session)
            || self.stopped.iter().any(|s| s.session_id == session)
    }

    /// The stopped, unsaved recording `session`.
    pub fn stopped_session(&self, session: &str) -> Option<&StoppedRecording> {
        self.stopped.iter().find(|s| s.session_id == session)
    }

    /// Every session a title editor renames, with its title: the active
    /// recording, then each stopped one not being discarded (oldest first).
    pub fn title_targets(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.recording
            .iter()
            .map(|r| (r.session_id.as_str(), r.title.as_deref()))
            .chain(
                self.stopped
                    .iter()
                    .filter(|s| s.state.is_pending_save())
                    .map(|s| (s.session_id.as_str(), s.title.as_deref())),
            )
    }

    /// Whether `session` can still be renamed, discarded or retried
    /// (recording, or stopped and not being discarded).
    pub fn discardable(&self, session: &str) -> bool {
        self.active_session() == Some(session)
            || self
                .stopped_session(session)
                .is_some_and(|s| s.state.is_pending_save())
    }

    /// Inserts or replaces a turn of the active session: a lower revision
    /// than the one held is a late update and is ignored. A new turn may
    /// push the oldest ones out, as the agent does.
    pub fn upsert_turn(&mut self, turn: LiveTurn) -> bool {
        if self.active_session() != Some(turn.session_id.as_str()) {
            return false;
        }
        if let Some(pending) = &mut self.snapshot_pending {
            pending.insert(turn.turn_id.clone());
        }
        if self.turn(&turn.turn_id).is_some() {
            return self.replace_turn(turn);
        }
        self.arrivals.push_back(turn.turn_id.clone());
        let at = self
            .turns
            .partition_point(|t| (t.start_s, t.track) <= (turn.start_s, turn.track));
        self.turns.insert(at, turn);
        self.evict_turns();
        self.rows_revision += 1;
        self.revision += 1;
        true
    }

    /// Replaces a turn still held (the answer to a decision on it); a turn
    /// already dropped is never brought back.
    pub fn replace_turn(&mut self, turn: LiveTurn) -> bool {
        if self.active_session() != Some(turn.session_id.as_str()) {
            return false;
        }
        let Some(held) = self.turns.iter_mut().find(|t| t.turn_id == turn.turn_id) else {
            return false;
        };
        if held.revision > turn.revision || *held == turn {
            return false;
        }
        *held = turn;
        self.rows_revision += 1;
        self.revision += 1;
        true
    }

    /// Drops the oldest published turns beyond the agent's bound.
    fn evict_turns(&mut self) {
        let max = live_turns_max();
        if self.arrivals.len() <= max {
            return;
        }
        let mut gone = BTreeSet::new();
        while self.arrivals.len() > max {
            gone.extend(self.arrivals.pop_front());
        }
        self.turns.retain(|t| !gone.contains(&t.turn_id));
    }

    /// Asks for an authoritative `record.live` snapshot.
    pub fn want_snapshot(&mut self) {
        self.snapshot_wanted = self.recording.is_some();
    }

    /// The session whose snapshot should be requested now, if one is due
    /// and none is in flight.
    pub fn take_snapshot_request(&mut self) -> Option<String> {
        if !self.snapshot_wanted || self.snapshot_pending.is_some() {
            return None;
        }
        let session = self.active_session()?.to_owned();
        self.snapshot_wanted = false;
        self.snapshot_pending = Some(BTreeSet::new());
        Some(session)
    }

    /// Applies the answer to the snapshot request made for `session`
    /// (`None`: it failed). An answer for a session that is no longer active
    /// changes nothing. The snapshot replaces every row, except turns that
    /// changed after it was requested and are newer than its copy; the
    /// agent's bound applies to the result.
    pub fn apply_snapshot(&mut self, session: &str, snapshot: Option<LiveSnapshot>) -> bool {
        if self.active_session() != Some(session) {
            return false;
        }
        let Some(pending) = self.snapshot_pending.take() else {
            return false;
        };
        let Some(snapshot) = snapshot.filter(|s| s.session_id == session) else {
            return false;
        };
        let mut turns = snapshot.turns;
        turns.retain(|t| t.session_id == snapshot.session_id);
        // The agent's own order (it publishes turns as their segments
        // close), then what arrived after the snapshot was requested.
        let mut arrivals: VecDeque<String> = turns.iter().map(|t| t.turn_id.clone()).collect();
        let held = std::mem::take(&mut self.turns);
        let held_order = std::mem::take(&mut self.arrivals);
        for id in held_order.into_iter().filter(|id| pending.contains(id)) {
            let Some(held) = held.iter().find(|t| t.turn_id == id) else {
                continue;
            };
            match turns.iter_mut().find(|t| t.turn_id == id) {
                Some(t) if t.revision < held.revision => *t = held.clone(),
                Some(_) => {}
                None => {
                    turns.push(held.clone());
                    arrivals.push_back(id);
                }
            }
        }
        turns.sort_by(|a, b| {
            (a.start_s, a.track)
                .partial_cmp(&(b.start_s, b.track))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        self.turns = turns;
        self.arrivals = arrivals;
        self.evict_turns();
        self.identity = snapshot.identity;
        self.rows_revision += 1;
        self.revision += 1;
        true
    }

    pub fn turn(&self, turn_id: &str) -> Option<&LiveTurn> {
        self.turns.iter().find(|t| t.turn_id == turn_id)
    }

    /// Applies the answer to `record.auto_stop` for `session`: a newer
    /// snapshot of the binding wins over it.
    pub fn apply_auto_stop(&mut self, session: &str, answer: AutoStopStatus) -> bool {
        let Some(recording) = self.recording.as_mut().filter(|r| r.session_id == session) else {
            return false;
        };
        if recording
            .auto_stop
            .as_ref()
            .is_some_and(|held| held.generation > answer.generation)
        {
            return false;
        }
        recording.auto_stop = Some(answer);
        self.revision += 1;
        true
    }
}

/// A command on the whole recording; [`SessionCommand::call`] names the
/// session the panel shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionCommand {
    Stop,
    Pause,
    Resume,
    ProcessNow,
}

impl SessionCommand {
    pub fn call(self, session_id: String) -> Call {
        match self {
            Self::Stop => Call::RecordStop { session_id },
            Self::Pause => Call::RecordPause { session_id },
            Self::Resume => Call::RecordResume { session_id },
            Self::ProcessNow => Call::RecordProcessNow { session_id },
        }
    }
}

// ---- controls (pure, tested) ----

/// A group of panel controls with one status line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Control {
    Start,
    /// Pause / Resume, Stop and save, Process now.
    Session,
    Language,
    /// Rename one recording (active, or stopped and not saved yet).
    Title,
    /// Confirm a participant (picker).
    Participant,
    /// Remove a confirmed participant.
    Roster,
    Note,
    Tag,
    Discard,
    /// Run a stopped recording's failed processing again.
    Retry,
    /// Resolve a legacy journal with no library identity.
    ResolveJournal,
    /// A decision on one live turn (name, confirm, clear).
    Live,
    /// Automatic stop: turn on/off, link a call, Keep recording.
    AutoStop,
}

impl Control {
    /// Controls of one recording that outlive the capture: they act on a
    /// stopped recording too, and their result belongs to that recording
    /// wherever it is listed.
    fn per_recording(self) -> bool {
        matches!(
            self,
            Self::Session | Self::Title | Self::Discard | Self::Retry | Self::ResolveJournal
        )
    }
}

/// What one status belongs to: a control, the recording it acted on and,
/// for a live decision, the turn. A result never shows for, or is cleared
/// by, another recording or turn.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ControlKey {
    control: Control,
    session: Option<String>,
    turn: Option<String>,
}

impl ControlKey {
    fn new(control: Control, session: Option<&str>) -> Self {
        Self {
            control,
            session: session.map(str::to_owned),
            turn: None,
        }
    }

    fn live(session: &str, turn: &str) -> Self {
        Self {
            control: Control::Live,
            session: Some(session.to_owned()),
            turn: Some(turn.to_owned()),
        }
    }
}

/// A saved meeting a status can open, or offer to delete.
#[derive(Clone, Debug, PartialEq)]
pub struct MeetingLink {
    pub meeting_id: String,
    pub offer_delete: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ControlStatus {
    Pending,
    Done {
        text: String,
        meeting: Option<MeetingLink>,
    },
    Failed(String),
}

impl ControlStatus {
    fn done(text: impl Into<String>) -> Self {
        Self::Done {
            text: text.into(),
            meeting: None,
        }
    }
}

struct ControlState {
    status: ControlStatus,
    generation: u64,
}

/// A registered person in the confirmed roster.
#[derive(Clone, Debug, PartialEq)]
pub struct RosterPerson {
    pub name: String,
    pub emails: Vec<String>,
}

/// What a failed control request tells the user: by the agent's code, and
/// otherwise the agent's message as a sentence, never the code (WRT-04).
pub fn control_error(error: &IpcError) -> String {
    match error.code.as_str() {
        "stale_session" => {
            "That recording has ended or was replaced; nothing was changed.".to_owned()
        }
        "stale_turn" | "stale_revision" | "stale_evidence" => {
            "This line changed before your choice was saved; nothing was changed. The preview \
             was refreshed: check the line and choose again."
                .to_owned()
        }
        // The agent's message names the person's ID and the inner
        // refusal's code: neither is shown.
        "partial_outcome" => failure_text(
            "A new name-only person was created, but this line's name wasn't saved.",
            "",
            "Choose them again to retry.",
        ),
        "stale_binding" => {
            "Automatic stop changed meanwhile; nothing was changed. Check it and choose again."
                .to_owned()
        }
        "not_found" => "Not found: choose a registered person or use the typed name.".into(),
        "not_retryable" => {
            "This recording is not waiting for a retry any more; nothing was changed.".to_owned()
        }
        _ => agent_error_text(error),
    }
}

/// The speaker label of a live turn's track.
pub fn track_label(track: Track) -> &'static str {
    match track {
        Track::Mic => "Me",
        Track::System => "Others",
    }
}

/// The short status on the row. Evidence belongs on a separate caption.
pub fn turn_badge(turn: &LiveTurn) -> String {
    let voice = turn.voice.map(|n| format!("Voice {n}"));
    match turn.status {
        LiveTurnStatus::Named => match &turn.named {
            Some(person) => format!("Named by you · {}", person.name),
            None => "Named by you".to_owned(),
        },
        LiveTurnStatus::Suggested => turn.suggestion.as_ref().map_or_else(
            || "Suggested".to_owned(),
            |s| format!("Suggested · {}", s.person.name),
        ),
        LiveTurnStatus::Voice => voice.unwrap_or_else(|| "Voice".to_owned()),
        LiveTurnStatus::Unresolved => "Unresolved".to_owned(),
        LiveTurnStatus::Preview => "Preview".to_owned(),
    }
}

fn turn_qualifier(turn: &LiveTurn) -> Option<String> {
    let mut parts = Vec::new();
    match turn.status {
        LiveTurnStatus::Suggested => {
            parts.extend(turn.voice.map(|n| format!("Voice {n}")));
            if let Some(s) = &turn.suggestion {
                if s.source == SuggestionSource::Session {
                    parts.push("from this recording".into());
                }
                if s.uncalibrated {
                    parts.push("closest voice · not reliable yet".into());
                }
            }
        }
        LiveTurnStatus::Unresolved => parts.push(
            match turn.unresolved {
                Some(UnresolvedReason::Mixed) => "more than one voice",
                Some(UnresolvedReason::Overlap) => "voices overlap",
                Some(UnresolvedReason::Unsupported) | None => "too little clear speech",
            }
            .into(),
        ),
        _ => {}
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The accessible name for one live row.
fn spoken_turn_line(turn: &LiveTurn) -> String {
    format!(
        "{} · {} · {}{}: {}",
        spoken_position(turn.start_s),
        track_label(turn.track),
        turn_badge(turn),
        turn_qualifier(turn).map_or_else(String::new, |text| format!(" · {text}")),
        turn.text
    )
}

/// The row as the user saw it when acting on it: what
/// `record.live_attribute` must name so a changed row is refused.
#[derive(Clone, Debug, PartialEq)]
pub struct ShownTurn {
    pub session_id: String,
    pub turn_id: String,
    pub revision: u64,
    pub epoch: u64,
    pub evidence: String,
    pub start_s: f64,
    pub end_s: f64,
    pub track: Track,
}

impl ShownTurn {
    pub fn of(turn: &LiveTurn) -> Self {
        Self {
            session_id: turn.session_id.clone(),
            turn_id: turn.turn_id.clone(),
            revision: turn.revision,
            epoch: turn.epoch,
            evidence: turn.evidence.clone(),
            start_s: turn.start_s,
            end_s: turn.end_s,
            track: turn.track,
        }
    }

    /// `record.live_attribute` for this exact row.
    pub fn call(&self, action: LiveAction, person_ref: Option<String>) -> Call {
        Call::RecordLiveAttribute(LiveAttributeParams {
            session_id: self.session_id.clone(),
            request_id: RequestId::random(),
            turn_id: self.turn_id.clone(),
            revision: self.revision,
            epoch: self.epoch,
            evidence: self.evidence.clone(),
            action,
            person_ref,
        })
    }
}

/// The status of an answered live decision.
pub fn live_outcome(outcome: &LiveAttributeOutcome, action: LiveAction) -> ControlStatus {
    let name = outcome
        .turn
        .named
        .as_ref()
        .map_or("that person", |p| p.name.as_str());
    ControlStatus::done(match action {
        LiveAction::Name if outcome.created_person_id.is_some() => format!(
            "Named this line {name} and added them to People. Agreeing names can teach BroScribe their voice after the meeting."
        ),
        LiveAction::Name => format!(
            "Named this line {name}. Agreeing names can teach BroScribe their voice after the meeting."
        ),
        LiveAction::Confirm => format!(
            "Confirmed {name} for this line. Agreeing names can teach BroScribe their voice after the meeting."
        ),
        LiveAction::Clear => {
            "Your name was removed; this line shows the model's suggestion again.".to_owned()
        }
    })
}

/// Why a live worker (preview or speaker analysis) is not running.
pub fn worker_problem(problem: WorkerProblem) -> &'static str {
    match problem {
        WorkerProblem::StartFailed => "It couldn't start.",
        WorkerProblem::Exited => "It quit.",
        WorkerProblem::Silent => "It stopped responding.",
        WorkerProblem::Stalled => "It stalled.",
        WorkerProblem::LoadTimeout => "Its models took too long to load.",
        WorkerProblem::InvalidSettings => "Its settings aren't valid.",
        WorkerProblem::ModelMissing => "Its model isn't downloaded.",
    }
}

/// What live speaker analysis is doing, when there is anything to say. It
/// is told apart from the transcript preview and the meeting reader.
pub fn identity_notice(identity: &LiveIdentityState) -> Option<String> {
    Some(match &identity.status {
        LiveIdentityStatus::Idle | LiveIdentityStatus::Ready => return None,
        LiveIdentityStatus::Disabled => "Live speaker identification is off in Settings: lines \
             show Me / Others only. Recording and the live transcript go on."
            .to_owned(),
        LiveIdentityStatus::Loading { .. } => {
            "Loading the speaker models. Lines show Me and Others until they’re ready.".to_owned()
        }
        LiveIdentityStatus::Behind => "Speaker analysis is catching up: some older audio was \
             skipped. Recording is unaffected."
            .to_owned(),
        LiveIdentityStatus::Restarting { reason } => format!(
            "Speaker analysis stopped and is restarting. {} Lines show Me / Others meanwhile. \
             Recording is unaffected.",
            worker_problem(*reason)
        ),
        LiveIdentityStatus::Unavailable { reason } => format!(
            "Speaker analysis is unavailable. {} Lines show Me / Others only. Recording is \
             unaffected.",
            worker_problem(*reason)
        ),
    })
}

/// What the live transcript preview is doing while recording, when there is
/// anything to say: its model still loading, a restart, or no preview at all.
pub fn preview_notice(preview: &PreviewState) -> Option<String> {
    Some(match preview {
        PreviewState::Ready { .. } => return None,
        PreviewState::Off => "Live transcript off for this recording; the recording and its \
             final transcript are unaffected."
            .to_owned(),
        PreviewState::Loading { .. } => "Loading the live transcription model. Lines appear when it’s ready. Recording is unaffected.".into(),
        PreviewState::Restarting { reason, .. } => format!(
            "Live transcript interrupted. {} Restarting. Recording is unaffected.",
            worker_problem(*reason)
        ),
        PreviewState::Unavailable { reason } => format!(
            "Live transcript unavailable for this recording. {} Recording is unaffected; the \
             full transcript is made after the meeting.",
            worker_problem(*reason)
        ),
    })
}

/// What the recording's meeting reader is doing, when it matters: automatic
/// stop relies on the microphone alone while it can't read the call. A
/// degraded reader says why, from the diagnostics of its latest batch
/// (`health`, never call evidence).
pub fn reader_notice(
    reader: Option<&ReaderStatus>,
    health: Option<&ReaderHealth>,
    app_name: Option<&str>,
) -> Option<String> {
    use broscribe_readers::ReaderStartRefusal;

    let cause = match reader? {
        ReaderStatus::Reading | ReaderStatus::Paused => return None,
        ReaderStatus::Starting => return Some("Connecting to the meeting window…".into()),
        ReaderStatus::Refused { refusal } => Some(match refusal {
            ReaderStartRefusal::Untrusted => "Accessibility access is off.",
            ReaderStartRefusal::NoRule => {
                "BroScribe doesn't know how to read this app's window yet."
            }
            ReaderStartRefusal::Prepare
            | ReaderStartRefusal::Attach
            | ReaderStartRefusal::Observe => "Reading the window failed.",
        }),
        ReaderStatus::Unavailable => None,
        ReaderStatus::Degraded { .. } => {
            health
                .and_then(|health| health.incomplete)
                .map(|reason| match reason {
                    IncompleteReason::IdentityCap
                    | IncompleteReason::NodeCap
                    | IncompleteReason::DepthCap => "The window has too much to read.",
                    IncompleteReason::TimeBudget | IncompleteReason::Transient => {
                        "The app is slow to answer. BroScribe keeps trying."
                    }
                    IncompleteReason::Clipped | IncompleteReason::ReadFailed => {
                        "Part of the window couldn't be read."
                    }
                    IncompleteReason::Failed => "Reading the window failed.",
                    IncompleteReason::NoCallRule => {
                        "BroScribe doesn't know how to read this app's window yet."
                    }
                })
        }
    };
    let window = app_name.map_or_else(|| "meeting window".into(), |app| format!("{app} window"));
    let mut notice = format!(
        "BroScribe can't read the {window}, so automatic stop uses the microphone only. Recording isn't affected."
    );
    if let Some(cause) = cause {
        notice.push(' ');
        notice.push_str(cause);
    }
    if health.is_some_and(|health| health.manual_accessibility == ManualAccessibility::Failed) {
        notice.push_str(" BroScribe couldn't turn on the app's accessibility mode.");
    }
    Some(notice)
}

/// A duration of audio or a countdown: "45 s", "12 min", "2 h 05 min".
pub fn duration_text(seconds: f64) -> String {
    crate::ui::format::length(seconds)
}

fn track_name(track: Track) -> &'static str {
    match track {
        Track::Mic => "Me (microphone)",
        Track::System => "Others (system audio)",
    }
}

/// Live status text for one track's state change, without the retry countdown
/// or dropped-frame counter (which live in separate non-live nodes, ACC-18).
/// Returns `(text, attention)`.
pub fn track_status_text(health: &TrackHealth) -> (String, bool) {
    match health.state {
        TrackState::Capturing => (format!("{}: recording", track_name(health.track)), false),
        TrackState::Restarting => (
            format!(
                "{} lost its device. Restarting it now. The other track keeps recording.",
                track_name(health.track)
            ),
            true,
        ),
        TrackState::Waiting => (
            format!(
                "{} lost its device. Trying again. The other track keeps recording.",
                track_name(health.track)
            ),
            true,
        ),
    }
}

/// Live alert text when the journal disk changes to `Warning` or `Low`; `None`
/// while `Ok`. Free bytes and headroom stay in [`disk_space_text`] (`Role::Label`)
/// so the alert announces only when the level changes (ACC-18).
pub fn disk_alert_text(level: DiskLevel) -> Option<&'static str> {
    match level {
        DiskLevel::Ok => None,
        DiskLevel::Warning => Some(
            "Disk space is low. The recording stops and saves itself before the disk fills; \
             free some space to keep going.",
        ),
        DiskLevel::Low => {
            Some("Disk space is too low: the recording is stopping and saving what it has.")
        }
    }
}

/// Non-live disk free-space and headroom label (`Role::Label`, ACC-18).
pub fn disk_space_text(disk: &DiskHealth) -> String {
    disk_space_words(disk, crate::ui::format::estimate)
}

fn spoken_disk_space(disk: &DiskHealth) -> String {
    disk_space_words(disk, crate::ui::format::spoken_estimate)
}

fn disk_space_words(disk: &DiskHealth, estimate: impl Fn(f64) -> String) -> String {
    let left = estimate(disk.headroom_s);
    let free = crate::ui::format::bytes(disk.free_bytes);
    match disk.level {
        DiskLevel::Ok => format!("Disk · {free} free · {left} of recording"),
        DiskLevel::Warning => format!("Disk · {free} free · {left} of recording left"),
        DiskLevel::Low => format!("Disk · {free} free"),
    }
}

/// Live notice while the panel catches up on live updates it missed: its
/// words stay put however many more are missed (ACC-18).
pub fn missed_updates_notice() -> &'static str {
    "The panel missed some live updates; the lines are being reloaded. Recording is \
     unaffected."
}

/// Non-live count of the live updates missed (`Role::Label`, ACC-18).
pub fn missed_updates_count(dropped: u64) -> String {
    crate::ui::format::count(dropped, "update missed", "updates missed")
}

/// Non-live stop thresholds carried by the agent's one-off low-disk warning
/// (`Role::Label`); the level itself is [`disk_alert_text`]'s one alert.
fn disk_stop_text(
    stop_below_bytes: u64,
    stop_below_s: f64,
    format_duration: impl Fn(f64) -> String,
) -> String {
    format!(
        "The recording stops and saves itself below {} free or {} of recording left.",
        crate::ui::format::bytes(stop_below_bytes),
        format_duration(stop_below_s)
    )
}

/// Seconds remaining until the next automatic retry attempt.
pub fn retry_countdown_remaining(retry_in_s: u64, since_state: Duration) -> u64 {
    retry_in_s.saturating_sub(since_state.as_secs())
}

/// One track's capture health; `true` with it when it needs attention.
pub fn track_health(health: &TrackHealth) -> (String, bool) {
    let (mut text, attention) = track_status_text(health);
    if health.dropped_frames > 0 {
        text.push_str(&format!(
            " · {} audio frames lost before saving",
            health.dropped_frames
        ));
    }
    (text, attention || health.dropped_frames > 0)
}

/// The journal disk's headroom; `true` with it while it is low (warned
/// before the protective stop).
pub fn disk_health(disk: &DiskHealth) -> (String, bool) {
    let left = crate::ui::format::estimate(disk.headroom_s);
    let free = crate::ui::format::bytes(disk.free_bytes);
    match disk.level {
        DiskLevel::Ok => (format!("Disk · {free} free · {left} of recording"), false),
        DiskLevel::Warning => (
            format!(
                "Disk space is low: {free} free, {left} of recording left. The recording \
                 stops and saves itself before the disk fills; free some space to keep going."
            ),
            true,
        ),
        DiskLevel::Low => (
            format!(
                "Disk space is too low ({free} free): the recording is stopping and saving what \
                 it has."
            ),
            true,
        ),
    }
}

/// A problem the capture survived, as it was reported.
pub fn capture_warning_text(warning: &CaptureWarning) -> String {
    match warning {
        CaptureWarning::TrackDown { track, .. } => format!(
            "{} lost its device. Trying again. The other track keeps recording.",
            track_name(*track)
        ),
        CaptureWarning::TrackResumed { track, gap_s } => format!(
            "{} is back. The {} gap is kept as silence.",
            if *track == Track::Mic { "The microphone" } else { "System audio" },
            duration_text(*gap_s)
        ),
        CaptureWarning::DiskLow {
            free_bytes,
            headroom_s,
            stop_below_bytes,
            stop_below_s,
        } => format!(
            "Disk space is low: {} free, about {} of recording. The recording stops and saves \
             itself below {} or {} of recording left.",
            crate::ui::format::bytes(*free_bytes),
            duration_text(*headroom_s),
            crate::ui::format::bytes(*stop_below_bytes),
            duration_text(*stop_below_s)
        ),
        CaptureWarning::PreviewUnavailable => {
            "The live transcript can't run for this recording. Recording isn't affected; the transcript is made after the meeting."
                .to_owned()
        }
    }
}

/// How a stopped recording that couldn't be saved is told, and what it
/// offers (WRT-04): the agent's state for it and its journal issue, if any,
/// tell an automatic retry, a manual one and a recording kept for repair
/// apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveFailure {
    /// The agent tries again by itself: the next attempt of its final job
    /// (`attempts` spent), or its next recovery sweep (`attempts` 0).
    Automatic { attempts: u32 },
    /// Failed for good: only the user's Retry runs it again.
    Manual,
    /// Its journal is damaged: kept untouched for repair, never retried
    /// (the agent refuses `record.retry`).
    Repair,
}

/// The [`SaveFailure`] of a stopped recording in `state` with the agent's
/// journal `issue` for it; `None` while it is queued, saving or discarded.
pub fn save_failure(
    state: &StoppedRecordingState,
    issue: Option<&JournalIssue>,
) -> Option<SaveFailure> {
    match (state, issue) {
        (
            StoppedRecordingState::Failed { .. } | StoppedRecordingState::Retrying { .. },
            Some(i),
        ) if i.corrupt => Some(SaveFailure::Repair),
        (StoppedRecordingState::Retrying { attempts, .. }, _) => Some(SaveFailure::Automatic {
            attempts: *attempts,
        }),
        (StoppedRecordingState::Failed { .. }, Some(_)) => {
            Some(SaveFailure::Automatic { attempts: 0 })
        }
        (StoppedRecordingState::Failed { .. }, None) => Some(SaveFailure::Manual),
        _ => None,
    }
}

/// Why a stopped recording couldn't be saved, in words, from the error its
/// state carries: a journal issue's code, or the pipeline's error text
/// (`e.to_string()` of the job's failure, which can name files). Only a few
/// known causes are said; anything else is left out, never shown raw.
pub use broscribe_ipc::recording_words::transcription_cause as stopped_cause;

/// The line of a stopped recording that couldn't be saved: "Couldn't save
/// the recording. <Cause>. <What happens next>.", the cause from
/// [`stopped_cause`] (left out when unknown), never a code. It changes only
/// with the attempt or the cause, never on countdown ticks (ACC-18).
pub fn save_failure_text(failure: SaveFailure, error: Option<&str>) -> String {
    let cause = stopped_cause(error).unwrap_or_default();
    match failure {
        SaveFailure::Automatic { attempts: 0 } => failure_text(
            "Couldn't save the recording.",
            cause,
            "BroScribe tries again automatically.",
        ),
        SaveFailure::Automatic { attempts } => failure_text(
            &format!("Couldn't save the recording (attempt {attempts})."),
            cause,
            "BroScribe tries again automatically.",
        ),
        SaveFailure::Manual => failure_text(
            "Couldn't save the recording.",
            cause,
            "The recording is kept. Try again.",
        ),
        SaveFailure::Repair => failure_text(
            "Couldn't save the recording.",
            "Its files are damaged, so trying again can't help.",
            "They're kept as they are for repair; discard them only if you don't need this \
             recording.",
        ),
    }
}

/// What a stopped, unsaved recording is doing (RT24): queued, processing,
/// retrying, failed (only an explicit Retry runs it again), or discarding.
/// A failure without its journal issue: the panel row tells a recording
/// kept for repair apart ([`save_failure`]).
pub fn stopped_summary(state: &StoppedRecordingState) -> String {
    match state {
        StoppedRecordingState::Queued { ahead: 0 } => "Queued · next".to_owned(),
        StoppedRecordingState::Queued { ahead: 1 } => "Queued · 1 job ahead".into(),
        StoppedRecordingState::Queued { ahead } => {
            format!("Queued · {ahead} jobs ahead")
        }
        StoppedRecordingState::Running { fraction } => fraction
            .and_then(crate::ui::format::progress_percent)
            .map_or_else(
                || "Transcribing…".into(),
                |percent| format!("Transcribing · {percent}"),
            ),
        StoppedRecordingState::Retrying { .. } => "Retrying…".into(),
        StoppedRecordingState::Failed { .. } => "Couldn't transcribe".into(),
        StoppedRecordingState::Discarding => "Discarding…".into(),
    }
}

/// How live names relate to the saved meeting, in the Transcript info popover.
pub const LIVE_EXPLANATION: &str = "A name you give while recording appears on that line. After the meeting, BroScribe can name the speaker and learn their voice when enough lines agree. Check names that need a decision in Review.";

// ---- automatic stop (plan H) ----

/// `record.auto_stop` for `session` as shown: fenced by the shown binding
/// generation. `app_id` links a fresh call (only with `enabled`); `None`
/// keeps the binding.
pub fn auto_stop_call(
    session_id: &str,
    shown: &AutoStopStatus,
    enabled: bool,
    app_id: Option<String>,
) -> Call {
    Call::RecordAutoStop(RecordAutoStopParams {
        session_id: session_id.to_owned(),
        request_id: RequestId::random(),
        expected_generation: shown.generation,
        enabled,
        app_id,
    })
}

/// What a pending automatic stop says about why: a reader-confirmed end
/// is told apart from the microphone-release fallback.
pub fn auto_stop_reason(reason: AutoStopReason, app: Option<&AutoStopApp>) -> String {
    let app = app.map_or("The meeting app", |a| a.app_name.as_str());
    match reason {
        AutoStopReason::CallEnded => format!("{app} shows the call has ended"),
        AutoStopReason::MicReleasedRuleChanged => {
            format!("{app} released the microphone and no longer matches a call")
        }
        AutoStopReason::MicReleasedTimeout => {
            format!("{app} released the microphone (the call window could not confirm the end)")
        }
    }
}

/// The auto-stop status line, honest about what it relies on.
pub fn auto_stop_summary(status: Option<&AutoStopStatus>) -> String {
    let Some(status) = status else {
        return "Automatic stop needs a newer BroScribe Agent.".to_owned();
    };
    if status.app.is_none() {
        return "Not linked to a call. This recording stops only when you stop it.".to_owned();
    };
    if status.reader_available {
        "Watches the call window and the microphone."
    } else {
        "Watches the microphone only (the call window can’t be read)."
    }
    .to_owned()
}

/// The countdown shown for a pending stop: the agent's remaining grace less
/// the time since that state arrived. Display only: the agent decides.
pub fn auto_stop_remaining(remaining_s: u64, since_state: Duration) -> u64 {
    remaining_s.saturating_sub(since_state.as_secs())
}

/// The status of an answered `record.auto_stop`.
pub fn auto_stop_outcome(status: &AutoStopStatus) -> ControlStatus {
    ControlStatus::done(match (&status.app, status.enabled) {
        (Some(app), true) => format!(
            "Automatic stop is on: saving when the {} call ends.",
            app.app_name
        ),
        _ => "Automatic stop is off for this recording; it keeps recording until you stop it."
            .to_owned(),
    })
}

/// `record.tag` adding the comma-separated tags of `input` literally;
/// `None` when nothing was typed.
pub fn tag_call(session_id: &str, input: &str) -> Option<Call> {
    let tags = typed_tags(input);
    (!tags.is_empty()).then(|| Call::RecordTag {
        tags: tags.into_iter().map(|t| format!("+{t}")).collect(),
        session_id: session_id.to_owned(),
    })
}

/// `record.tag` removing exactly `tag`.
pub fn remove_tag_call(session_id: &str, tag: &str) -> Call {
    Call::RecordTag {
        tags: vec![format!("-{tag}")],
        session_id: session_id.to_owned(),
    }
}

/// The status of an answered participant confirmation for `label`.
pub fn participant_outcome(value: &Value, label: &str) -> ControlStatus {
    match value["status"].as_str() {
        // The agent's message ends in the refusal's code: never shown.
        Some("partial") => ControlStatus::Failed(format!(
            "{label} was added as a new person, but attendance wasn't confirmed. Choose them \
             again to retry."
        )),
        Some("removed") => ControlStatus::done(format!("Removed {label} from attendance.")),
        _ if value["created"] == true => ControlStatus::done(format!(
            "Added {label} as a new person (name only, no voice) and confirmed attendance."
        )),
        _ => ControlStatus::done(format!("Confirmed attendance: {label}.")),
    }
}

/// The status of an answered `record.title`.
pub fn title_outcome(value: &Value) -> ControlStatus {
    match (value["status"].as_str(), value["meeting_id"].as_str()) {
        (Some("already_published"), Some(meeting_id)) => ControlStatus::Done {
            text: "This recording is already saved as a meeting: rename it there.".into(),
            meeting: Some(MeetingLink {
                meeting_id: meeting_id.to_owned(),
                offer_delete: false,
            }),
        },
        _ => ControlStatus::done("Title saved. It replaces calendar and window titles."),
    }
}

/// The status of an answered `record.discard`.
pub fn discard_outcome(value: &Value) -> ControlStatus {
    match (value["status"].as_str(), value["meeting_id"].as_str()) {
        (Some("already_saved"), Some(meeting_id)) => ControlStatus::Done {
            text: "This recording was already saved as a meeting, so nothing was discarded. \
                   You can delete the meeting instead."
                .into(),
            meeting: Some(MeetingLink {
                meeting_id: meeting_id.to_owned(),
                offer_delete: true,
            }),
        },
        _ if value["cleanup"] == "pending" => ControlStatus::done(
            "Recording discarded; nothing was saved. Its files are being removed in the \
             background.",
        ),
        _ => ControlStatus::done("Recording discarded; nothing was saved."),
    }
}

/// Whether an observed roster member is one of the confirmed people (same
/// email, or the same name under case/accent folding).
pub fn observed_confirmed(
    member: &broscribe_readers::RosterMember,
    confirmed: &BTreeMap<String, RosterPerson>,
) -> bool {
    let name = folded_name(&member.name);
    confirmed.values().any(|person| {
        member
            .email
            .as_deref()
            .is_some_and(|email| person.emails.iter().any(|e| e.eq_ignore_ascii_case(email)))
            || (!name.is_empty() && folded_name(&person.name) == name)
    })
}

// ---- view ----

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RecordingPanelTab {
    #[default]
    Transcript,
    Details,
}

pub struct RecordingPanel {
    tab: RecordingPanelTab,
    compact: bool,
    compact_saving: bool,
    compact_error: Option<String>,
    temporarily_expanded: bool,
    compact_height: Option<Pixels>,
    expanded_height: Pixels,
    compact_focus: gpui::FocusHandle,
    details_scroll: gpui::ScrollHandle,
    header_scroll: gpui::ScrollHandle,
    other_recordings_anchor: gpui::ScrollAnchor,
    notice_anchor: gpui::ScrollAnchor,
    first_control: gpui::FocusHandle,
    urgent_control: gpui::FocusHandle,
    following_visible: bool,
    start_refusal: Option<crate::daemon::StartRefusal>,
    stop_reason_dismissed: Option<String>,
    notice_target: Option<String>,
    notice_focus: gpui::FocusHandle,
    consent_dismissed: Option<String>,
    daemon: Entity<DaemonClient>,
    notifications: Entity<NotificationHost>,
    /// The app's one "Start recording agent" request (shared with the menu).
    agent: Entity<AgentStarter>,
    state: Entity<AppState>,
    language_profiles: Vec<crate::views::settings::ProfileChoice>,
    /// One title editor per recording it renames: the active one and each
    /// stopped one not saved yet, so every recording is renamed as itself.
    titles: BTreeMap<String, TitleEditor>,
    note: Entity<InputState>,
    tag: Entity<InputState>,
    picker: Entity<PersonPicker>,
    roster_open: bool,
    roster_focus: gpui::FocusHandle,
    confirmed: BTreeMap<String, RosterPerson>,
    /// A live person chip chosen to fix: replaced by the next confirmation.
    selected_person: Option<String>,
    roster_key: (Option<String>, Vec<String>),
    consent_reminder: bool,
    controls: BTreeMap<ControlKey, ControlState>,
    generation: u64,
    /// The active session last seen.
    session_key: Option<String>,
    /// Tags of the active session: the latest snapshot or tag answer.
    tags: Option<(String, Vec<String>)>,
    snapshot_tags: Option<(String, Vec<String>)>,
    /// Changes of the snapshot's tags seen so far. A tag answer requested
    /// before the latest change is older than that snapshot and never
    /// replaces it; the agent's later states carry any newer change.
    tag_snapshots: u64,
    /// The session whose "Discard" confirmation is open.
    discard_confirm: Option<String>,
    discard_modal: bool,
    more_open: bool,
    more_focus: gpui::FocusHandle,
    delete: Option<(Entity<DeleteMeetingPrompt>, Subscription)>,
    /// The people picker open on one live row, frozen to the row as shown.
    live_naming: Option<LiveNaming>,
    /// The "Speaking now" dock: the turn it names (the one being spoken,
    /// or an older one chosen with *Who's this?*).
    dock_target: dock::DockTarget,
    /// The order people were first listed in, for the dock's session: the
    /// number keys keep pointing at the same person.
    dock_order: (Option<String>, Vec<String>),
    /// Registered people for the dock, and the people revision they were
    /// read at (or are being read at).
    dock_known: Rc<Vec<dock::KnownPerson>>,
    dock_known_revision: Option<u64>,
    dock_focus: gpui::FocusHandle,
    /// The live row of the latest live decision or picker outcome.
    live_status_turn: Option<String>,
    /// Bumped whenever a live row's status changes.
    live_revision: u64,
    /// The live transcript (USR03). It follows the newest line while the
    /// view is at the bottom; scrolling up leaves the view where the user
    /// put it until they scroll back down; rows added or dropped elsewhere
    /// never move the rows in view.
    transcript: ListState,
    transcript_focus: gpui::FocusHandle,
    transcript_page: std::rc::Rc<std::cell::Cell<gpui::Pixels>>,
    transcript_selected: Option<RowKey>,
    transcript_hovered: Option<RowKey>,
    transcript_menu: Option<(
        String,
        Entity<gpui::component::menu::PopupMenu>,
        Subscription,
    )>,
    transcript_marks: VecDeque<TranscriptMark>,
    transcript_mark_number: u64,
    /// The cached view that draws `transcript` (it draws again only when the
    /// transcript changed, never for meters or the clock).
    transcript_view: Entity<view::TranscriptView>,
    /// The rows `transcript` holds, in order.
    transcript_rows: Vec<TranscriptRow>,
    /// What `transcript_rows` was built from (rows revision, the row with
    /// the open picker, live statuses): frames that change none of them
    /// (meters, the clock) never rebuild the rows.
    transcript_key: Option<(u64, Option<String>, u64)>,
    /// While a naming picker is open nothing scrolls by itself: whether the
    /// transcript followed the newest line when the picker opened. Closing
    /// the picker restores that.
    naming_followed: Option<bool>,
    /// The capture this panel showed has ended (USR02): the panel closes once
    /// the agent lists it as stopped or saved and the user is not using the
    /// panel. A new recording, or the user asking for the panel, keeps it.
    capture_ended: Option<EndedCapture>,
    /// Live names of each saved recording's meeting still awaiting the
    /// user's review (partial, conflicting or unmapped decisions), read from
    /// the published meeting off the UI thread; by meeting ID.
    live_reviews: BTreeMap<String, LiveReviews>,
    /// The model revision the panel's bookkeeping last followed.
    seen_revision: Option<u64>,
    /// Whether the window is presented: nothing ticks while it is not.
    visible: bool,
    /// Native overlay state (observers) of this panel; dropped with it.
    _overlay: Option<Box<dyn OverlayPresence>>,
    platform_overlay: Option<Rc<dyn RecordingOverlay>>,
    exclusion_setting: bool,
    exclusion_applied: Option<bool>,
    exclusion_pending: bool,
    exclusion_error: Option<String>,
    /// Why this window is not the always-on-top overlay, when it failed.
    overlay_warning: Option<String>,
    /// That this platform has no persistent overlay (informational).
    overlay_note: Option<String>,
    _subscriptions: Vec<Subscription>,
    /// The once-a-second redraw, only while something shown counts (the
    /// recording clock, an automatic-stop or retry countdown) and the
    /// window is presented.
    ticker: Option<gpui::Task<()>>,
    /// How often the panel was drawn.
    #[cfg(test)]
    renders: usize,
}

/// The title editor of one recording.
struct TitleEditor {
    input: Entity<InputState>,
    revealed: bool,
    rename_focus: gpui::FocusHandle,
    discard_focus: gpui::FocusHandle,
    /// The agent's title last written into `input`: a draft that differs
    /// is the user's, and a new title never replaces it.
    seen: Option<String>,
    _subscription: Subscription,
}

/// The live decisions of a saved meeting that need the user's review.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveReviews {
    /// Being read.
    Reading,
    /// Read: this many need review.
    Count(usize),
    /// The meeting could not be read: nothing is claimed.
    Unknown,
}

/// What a saved recording says about its live names left for review, when
/// any are.
pub fn live_review_notice(count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some("Open the meeting to check 1 name you gave while recording.".to_owned()),
        n => Some(format!(
            "Open the meeting to check {n} names you gave while recording."
        )),
    }
}

/// "Name…" on a live row.
struct LiveNaming {
    shown: ShownTurn,
    picker: Entity<PersonPicker>,
    reveal_after_layout: std::cell::Cell<bool>,
    _subscription: Subscription,
}

struct TranscriptMark {
    number: u64,
    at_s: f64,
    kind: &'static str,
    text: String,
}

/// A capture that ended while the panel showed it.
struct EndedCapture {
    session: String,
    /// The control generation when it ended: a control used after that is
    /// the user working in the panel.
    generation: u64,
}

/// What the transcript list knows a row by.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum RowKey {
    Turn(String),
    /// A track-only line, by its stable number ([`PanelModel::line`]).
    Line(u64),
    Mark(u64),
}

/// One row of the live transcript as the list last measured it: a kept row
/// whose revision, open picker or status changed is measured again.
#[derive(Clone, Debug, PartialEq)]
struct TranscriptRow {
    key: RowKey,
    revision: u64,
    naming: bool,
    selected: bool,
    hovered: bool,
    status: Option<ControlStatus>,
}

impl TranscriptRow {
    fn plain(key: RowKey) -> Self {
        Self {
            key,
            revision: 0,
            naming: false,
            selected: false,
            hovered: false,
            status: None,
        }
    }
}

/// Height of the live transcript list.
const TRANSCRIPT_OVERDRAW: f32 = 260.;

/// Tells `list` how its rows `old` became `new`, matching rows by key: runs
/// of dropped and added rows are spliced, so the scroll position stays on
/// the rows that remain, and a kept row that looks different is measured
/// again in place. A dropped row is never stood in for by another one.
fn sync_rows(list: &ListState, old: &[TranscriptRow], new: &[TranscriptRow]) {
    let at: std::collections::HashMap<&RowKey, usize> =
        new.iter().enumerate().map(|(j, r)| (&r.key, j)).collect();
    // Old rows still present, in an order both lists agree on.
    let mut kept: Vec<(usize, usize)> = Vec::new();
    for (i, row) in old.iter().enumerate() {
        if let Some(&j) = at.get(&row.key)
            && kept.last().is_none_or(|&(_, last)| j > last)
        {
            kept.push((i, j));
        }
    }
    // From the end, so the old positions still to splice stay valid.
    let mut end = (old.len(), new.len());
    let mut added = Vec::new();
    for &(i, j) in kept.iter().rev() {
        added.extend(splice_run(list, (i + 1, j + 1), end));
        end = (i, j);
    }
    added.extend(splice_run(list, (0, 0), end));
    // Rows added out of view are measured on the next layout too (the list
    // measures every row): the scroll extent always reaches the real last
    // row, so scrolling to the bottom finds it and follows again.
    for range in added {
        list.remeasure_items(range);
    }
    for &(i, j) in &kept {
        if old[i] != new[j] {
            list.remeasure_items(j..j + 1);
        }
    }
}

/// Replaces the old rows `start.0..end.0` with the new `start.1..end.1`;
/// the added rows' range in the new list, if any.
fn splice_run(
    list: &ListState,
    start: (usize, usize),
    end: (usize, usize),
) -> Option<std::ops::Range<usize>> {
    if start != end {
        list.splice(start.0..end.0, end.1 - start.1);
    }
    (end.1 > start.1).then_some(start.1..end.1)
}

/// Why the panel is being shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelReason {
    /// A new recording session or a notification fallback.
    Automatic,
    /// The user asked via the status item or a notification. The macOS
    /// nonactivating panel comes forward without becoming key.
    Explicit,
    /// The Show recording controls command asks for keyboard focus.
    Command,
}

/// How a new panel window is created: a nonactivating panel is GPUI's
/// `PopUp` (an `NSPanel` with the nonactivating style, which only exists at
/// construction), created hidden and unfocused so the platform shows it
/// without activating BroScribe. Elsewhere an ordinary floating window, shown
/// without focus; undecorated popups are never used as a stand-in.
pub fn panel_window_kind(capability: OverlayCapability) -> (WindowKind, bool, bool) {
    match capability {
        OverlayCapability::NonactivatingPanel => (WindowKind::PopUp, false, false),
        OverlayCapability::DecoratedTopmost => (WindowKind::Floating, false, false),
        OverlayCapability::Standard => (WindowKind::Floating, true, false),
    }
}

/// What showing an already open panel does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExistingPresentation {
    /// Bring it to the front without activating BroScribe.
    OrderFrontRegardless,
    /// Activate the app and the window (the user asked; nothing else can
    /// raise it on this platform).
    Activate,
    /// Leave it where it is.
    Leave,
}

pub fn existing_presentation(
    capability: OverlayCapability,
    reason: PanelReason,
) -> ExistingPresentation {
    match (capability, reason) {
        (_, PanelReason::Command) => ExistingPresentation::Activate,
        (OverlayCapability::NonactivatingPanel, _) => ExistingPresentation::OrderFrontRegardless,
        (OverlayCapability::DecoratedTopmost, PanelReason::Automatic) => {
            ExistingPresentation::OrderFrontRegardless
        }
        (OverlayCapability::DecoratedTopmost, PanelReason::Explicit) => {
            ExistingPresentation::Activate
        }
        (OverlayCapability::Standard, PanelReason::Explicit) => ExistingPresentation::Activate,
        (OverlayCapability::Standard, PanelReason::Automatic) => ExistingPresentation::Leave,
    }
}

fn focus_from_command(window: &mut Window, reason: PanelReason, panel: WeakEntity<RecordingPanel>) {
    if reason == PanelReason::Command {
        window.on_next_frame(move |window, cx| {
            if window.focused(cx).is_none() {
                panel
                    .update(cx, |panel, cx| {
                        if panel.has_urgent_action(cx) {
                            panel.urgent_control.focus(window, cx);
                        } else if panel.daemon.read(cx).connected
                            || crate::launch::AgentStartStatus::unavailable(
                                panel.daemon.read(cx),
                                panel.agent.read(cx).status(),
                            )
                            .is_some_and(|status| status.recovery().is_some())
                        {
                            panel.first_control.focus(window, cx);
                        } else {
                            window.focus_next(cx);
                        }
                    })
                    .ok();
            }
        });
    }
}

fn focus_panel(
    window: &mut Window,
    mode: OverlayCapability,
    overlay: &dyn RecordingOverlay,
    reason: PanelReason,
    panel: WeakEntity<RecordingPanel>,
) {
    if reason == PanelReason::Automatic
        || reason == PanelReason::Explicit
            && overlay.capability() == OverlayCapability::NonactivatingPanel
    {
        return;
    }
    let adapter: &dyn RecordingOverlay = if mode == overlay.capability() {
        overlay
    } else {
        &broscribe_platform::overlay::StandardOverlay
    };
    let focus = adapter.focus_for_user_request(&*window);
    match focus {
        Ok(OverlayFocus::Native) => focus_from_command(window, reason, panel),
        Ok(OverlayFocus::ActivateWindow) => {
            window.activate_window();
            focus_from_command(window, PanelReason::Command, panel);
        }
        Ok(OverlayFocus::Ignored) => {}
        Err(error) => tracing::warn!(code = error.code(), "focusing recording controls"),
    }
}

/// The panel's content size.
const PANEL_SIZE: (f32, f32) = (420., 712.);

fn panel_size(
    reading: broscribe_core::config::ReadingSize,
    display_width: Option<f32>,
) -> (f32, f32) {
    let width = PANEL_SIZE.0 * reading.scale();
    let width = display_width.map_or(width, |display| width.min(display / 2.));
    (width.max(crate::ui_state::PANEL_MIN.0), PANEL_SIZE.1)
}

/// Where a floating (non-overlay) panel opens: the right edge of the primary
/// display's visible area, vertically centred. A nonactivating panel is
/// placed natively instead (screen under the pointer, complete frame).
fn overlay_rect(bounds: Bounds<gpui::Pixels>) -> OverlayRect {
    OverlayRect::new(
        bounds.origin.x.to_f64(),
        bounds.origin.y.to_f64(),
        bounds.size.width.to_f64(),
        bounds.size.height.to_f64(),
    )
}

fn standard_bounds(
    dimensions: (f32, f32),
    overlay: &dyn RecordingOverlay,
    main: Option<Bounds<gpui::Pixels>>,
    cx: &App,
) -> Bounds<gpui::Pixels> {
    let content = size(px(dimensions.0), px(dimensions.1));
    let Some(display) = cx.primary_display() else {
        return Bounds::centered(None, content, cx);
    };
    let visible = overlay_rect(display.visible_bounds());
    let default = right_center(
        visible,
        f64::from(dimensions.0),
        f64::from(dimensions.1),
        OVERLAY_INSET,
    );
    let r = overlay.default_frame(
        visible,
        default,
        main.map(overlay_rect),
        f64::from(crate::ui_state::PANEL_MIN.0),
    );
    Bounds::new(
        gpui::point(px(r.x as f32), px(r.y as f32)),
        size(px(r.width as f32), px(r.height as f32)),
    )
}

/// The open recording panel and how it was actually presented (an overlay
/// that failed leaves an ordinary window behind).
/// The panel is held weakly: closing the window drops the panel and, with
/// it, its native overlay state (observers) and subscriptions.
#[derive(Clone)]
pub struct PanelWindow {
    pub handle: WindowHandle<Root>,
    pub panel: WeakEntity<RecordingPanel>,
    pub mode: OverlayCapability,
}

impl PanelWindow {
    pub fn show_failed_recording(&self, session: &str, cx: &mut App) -> bool {
        self.handle
            .update(cx, |_, window, cx| {
                self.panel
                    .update(cx, |panel, cx| {
                        panel.show_failed_recording(session, window, cx)
                    })
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    }
}

/// Shows `warning` at the top of `panel`, while it exists.
fn warn_in_panel(panel: &WeakEntity<RecordingPanel>, warning: String, cx: &mut App) {
    panel
        .update(cx, |panel, cx| {
            panel.overlay_warning = Some(warning);
            cx.notify();
        })
        .ok();
}

/// What a panel on a platform without a nonactivating panel says, always
/// (not an error: the capability is simply not there).
pub const STANDARD_OVERLAY_NOTE: &str =
    "On this system this window can’t stay above other apps. Recording is unaffected.";

/// The app's own notice when the overlay is unavailable (one id: a repeat
/// replaces nothing until the user dismisses it).
pub const OVERLAY_NOTICE_ID: &str = "recording-overlay-unavailable";

/// Shared copy for the panel and the notice when the overlay failed.
pub fn overlay_warning() -> &'static str {
    "Other apps can cover the recording controls, because they can't be kept on top. Recording isn't affected."
}

fn overlay_notice() -> broscribe_ipc::NotificationRequest {
    broscribe_ipc::NotificationRequest {
        id: OVERLAY_NOTICE_ID.into(),
        title: broscribe_platform_native::NativePlatform::notification_title(
            "Recording Controls Can't Stay on Top",
            "Recording controls can't stay on top",
        ),
        body: overlay_warning().into(),
        actions: Vec::new(),
        meeting_id: None,
    }
}

/// A durable notice: kept until the user dismisses it; clicking it opens the
/// recording controls (a deliberate action).
fn post_overlay_notice(notifications: &Entity<NotificationHost>, cx: &mut App) {
    notifications.update(cx, |host, cx| host.post_local(overlay_notice(), cx));
}

/// The recording controls window's title: in the native title bar and,
/// where the window system leaves decorations to the app, in the bar
/// BroScribe draws ([`crate::ui::chrome::framed_panel`]).
const WINDOW_TITLE: &str = "Recording controls";

/// Where and how a panel window of `mode` opens, and how the overlay then
/// places it.
fn panel_window_options(
    mode: OverlayCapability,
    reading: broscribe_core::config::ReadingSize,
    overlay: &dyn RecordingOverlay,
    main: Option<Bounds<gpui::Pixels>>,
    cx: &App,
) -> (WindowOptions, OverlayPlacement) {
    let (kind, show, focus) = panel_window_kind(mode);
    let dimensions = panel_size(
        reading,
        cx.primary_display()
            .map(|display| display.visible_bounds().size.width.to_f64() as f32),
    );
    let (bounds, display_id, placement) = match crate::ui_state::panel_bounds(cx) {
        Some((bounds, display)) => (bounds, Some(display), OverlayPlacement::AsOpened),
        None => {
            let bounds = match mode {
                // Placed natively before it is first shown.
                OverlayCapability::NonactivatingPanel => {
                    Bounds::centered(None, size(px(dimensions.0), px(dimensions.1)), cx)
                }
                OverlayCapability::Standard | OverlayCapability::DecoratedTopmost => {
                    standard_bounds(dimensions, overlay, main, cx)
                }
            };
            (bounds, None, OverlayPlacement::RightEdge)
        }
    };
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        display_id,
        window_min_size: Some(size(px(crate::ui_state::PANEL_MIN.0), px(160.))),
        kind,
        show,
        focus,
        is_minimizable: false,
        titlebar: Some(TitlebarOptions {
            title: Some(crate::ui::windows::title(WINDOW_TITLE).into()),
            ..Default::default()
        }),
        ..crate::ui::assets::window_options()
    };
    (options, placement)
}

/// Opens one panel window of `mode`; a nonactivating panel is created hidden
/// and unfocused, an ordinary one shown without focus. It opens at the frame
/// it was last left at on the display it opens on, when that display is
/// present and the frame still fits it (PNL-56, [`crate::ui_state::panel_bounds`]);
/// otherwise at today's place. The returned placement tells the overlay
/// which. Expanded controls keep the 360×440 minimum (SHL-11).
#[allow(clippy::too_many_arguments)]
fn open_panel(
    mode: OverlayCapability,
    overlay: &Rc<dyn RecordingOverlay>,
    main: Option<Bounds<gpui::Pixels>>,
    daemon: &Entity<DaemonClient>,
    notifications: &Entity<NotificationHost>,
    state: &Entity<AppState>,
    agent: &Entity<AgentStarter>,
    cx: &mut App,
) -> Option<(WindowHandle<Root>, Entity<RecordingPanel>, OverlayPlacement)> {
    let daemon = daemon.clone();
    let notifications = notifications.clone();
    let state = state.clone();
    let agent = agent.clone();
    let (options, placement) = panel_window_options(
        mode,
        state.read(cx).config().appearance.reading_size,
        overlay.as_ref(),
        main,
        cx,
    );
    let overlay = overlay.clone();
    let mut created = None;
    let opened = cx.open_window(options, |window, cx| {
        crate::ui::theme::follow_appearance(window, cx);
        let view = cx.new(|cx| {
            // Its frame on each display, written once it stops moving.
            cx.observe_window_bounds(window, |panel: &mut RecordingPanel, window, cx| {
                if !panel.compact_layout()
                    && panel.compact_height.is_none()
                    && window.viewport_size().height >= px(crate::ui_state::PANEL_MIN.1)
                {
                    crate::ui_state::remember_panel(window, cx);
                }
            })
            .detach();
            let mut panel = RecordingPanel::new(daemon, notifications, state, agent, window, cx);
            panel.set_platform_overlay(overlay.clone(), window, cx);
            panel
        });
        window.on_window_should_close(cx, move |window, _| {
            let _ = overlay.return_user_focus(&*window);
            true
        });
        created = Some(view.clone());
        cx.new(|cx| Root::new(view, window, cx))
    });
    match (opened, created) {
        (Ok(handle), Some(view)) => Some((handle, view, placement)),
        (Ok(_), None) => None,
        (Err(error), _) => {
            tracing::error!(%error, "opening recording panel");
            None
        }
    }
}

/// Shows the recording panel, creating it when needed. Where the platform
/// has a nonactivating panel, nothing here activates BroScribe. When the
/// overlay fails, it fails closed: the still-hidden popup is closed unseen,
/// an ordinary window takes its place (focused only when the user asked),
/// and the panel and a durable notice say why. Closing the panel never stops
/// capture.
#[allow(clippy::too_many_arguments)]
pub fn show(
    window: &mut Option<PanelWindow>,
    overlay: &Rc<dyn RecordingOverlay>,
    reason: PanelReason,
    main: Option<Bounds<gpui::Pixels>>,
    daemon: &Entity<DaemonClient>,
    notifications: &Entity<NotificationHost>,
    state: &Entity<AppState>,
    agent: &Entity<AgentStarter>,
    cx: &mut App,
) {
    if let Some(open) = window.clone() {
        let shown = open.handle.update(cx, |_, window, _| {
            match existing_presentation(open.mode, reason) {
                ExistingPresentation::OrderFrontRegardless => {
                    overlay.present_existing(&*window).err().map(|e| {
                        tracing::warn!(code = e.code(), "presenting the recording panel");
                        core_user_text(&e)
                    })
                }
                ExistingPresentation::Activate => {
                    focus_panel(
                        window,
                        open.mode,
                        overlay.as_ref(),
                        reason,
                        open.panel.clone(),
                    );
                    None
                }
                ExistingPresentation::Leave => {
                    if let Err(error) = overlay.present_standard(&*window) {
                        tracing::warn!(
                            code = error.code(),
                            "presenting floating recording controls"
                        );
                    }
                    None
                }
            }
        });
        match shown {
            Ok(failure) => {
                // Left where it is (still the configured panel); never
                // activated as a substitute.
                if failure.is_some() {
                    warn_in_panel(&open.panel, overlay_warning().into(), cx);
                    post_overlay_notice(notifications, cx);
                }
                // The user asked for it: it stays open even after the
                // capture it showed ended.
                if reason != PanelReason::Automatic {
                    open.panel.update(cx, |panel, _| panel.keep_open()).ok();
                }
                return;
            }
            // Closed by the user: open a new one below.
            Err(_) => *window = None,
        }
    }
    let capability = overlay.capability();
    let Some((handle, view, placement)) = open_panel(
        capability,
        overlay,
        main,
        daemon,
        notifications,
        state,
        agent,
        cx,
    ) else {
        return;
    };
    if capability == OverlayCapability::Standard {
        // The persistent overlay is unavailable here: said in the panel.
        view.update(cx, |panel, cx| {
            panel.overlay_note = Some(STANDARD_OVERLAY_NOTE.to_owned());
            cx.notify();
        });
        if reason != PanelReason::Automatic {
            handle
                .update(cx, |_, window, _| {
                    focus_panel(
                        window,
                        capability,
                        overlay.as_ref(),
                        reason,
                        view.downgrade(),
                    );
                })
                .ok();
        }
        *window = Some(PanelWindow {
            handle,
            panel: view.downgrade(),
            mode: capability,
        });
        return;
    }
    let overlay = overlay.clone();
    let presented = handle.update(cx, |_, window, _| overlay.present_new(&*window, placement));
    match presented {
        // The presence lives (and its observer stays) exactly as long as
        // the panel.
        Ok(Ok(presence)) => {
            view.update(cx, |panel, _| panel._overlay = Some(presence));
            if reason != PanelReason::Automatic {
                handle
                    .update(cx, |_, window, _| {
                        focus_panel(
                            window,
                            capability,
                            overlay.as_ref(),
                            reason,
                            view.downgrade(),
                        );
                    })
                    .ok();
            }
            *window = Some(PanelWindow {
                handle,
                panel: view.downgrade(),
                mode: capability,
            });
            return;
        }
        Ok(Err(e)) => {
            tracing::warn!(code = e.code(), "recording overlay unavailable");
        }
        Err(_) => {
            tracing::warn!("recording overlay unavailable: the window closed");
        }
    };
    // The popup was never shown nor reconfigured: close it unseen.
    handle
        .update(cx, |_, window, _| window.remove_window())
        .ok();
    drop(view);
    let fallback = open_panel(
        OverlayCapability::Standard,
        &overlay,
        main,
        daemon,
        notifications,
        state,
        agent,
        cx,
    );
    if let Some((fallback, panel, _)) = fallback {
        let panel = panel.downgrade();
        warn_in_panel(&panel, overlay_warning().into(), cx);
        // Only the user's own request may focus the ordinary window.
        if reason != PanelReason::Automatic {
            fallback
                .update(cx, |_, window, _| {
                    focus_panel(
                        window,
                        OverlayCapability::Standard,
                        overlay.as_ref(),
                        reason,
                        panel.clone(),
                    );
                })
                .ok();
        }
        *window = Some(PanelWindow {
            handle: fallback,
            panel,
            mode: OverlayCapability::Standard,
        });
    }
    post_overlay_notice(notifications, cx);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use broscribe_platform::fakes::FakeNotifier;
    use gpui::{TestAppContext, WindowHandle};
    use serde_json::json;

    use crate::daemon::{EventStream, Transport};
    use crate::views::person_picker::fixtures::{self, Synthetic};

    #[test]
    fn meters_throttle_to_ten_hz_and_ignore_unchanged_values() {
        let mut panel = PanelModel::default();
        let now = Instant::now();
        assert!(panel.meters(-12., -20., now));
        assert!(!panel.meters(-1., -2., now + Duration::from_millis(99)));
        assert_eq!(panel.meters, [-12, -20]);
        assert!(panel.meters(-1., -2., now + Duration::from_millis(100)));
        assert!(!panel.meters(-1.1, -2.1, now + Duration::from_millis(200)));
    }
    #[test]
    fn trailing_meter_sample_is_coalesced_not_lost() {
        let mut panel = PanelModel::default();
        let now = Instant::now();
        assert!(panel.meters(-10., -20., now));
        assert!(!panel.meters(-120., -120., now + Duration::from_millis(99)));
        assert_eq!(
            panel.meter_flush_delay(now + Duration::from_millis(99)),
            Some(Duration::from_millis(1))
        );
        assert!(panel.flush_meters(now + Duration::from_millis(100)));
        assert_eq!(panel.meters, [-120, -120]);
        assert!(!panel.flush_meters(now + Duration::from_millis(200)));
    }
    #[test]
    fn language_picker_uses_timestamped_daemon_span_action() {
        assert_eq!(
            PanelModel::language_action("s1", " pt-BR "),
            Call::RecordLanguage {
                session_id: "s1".into(),
                language: "pt-BR".into()
            }
        );
    }
    #[test]
    fn preview_gap_is_visible_and_does_not_imply_recording_loss() {
        let mut panel = PanelModel::default();
        panel.apply(&recording("s1"), Instant::now());
        assert!(panel.apply(
            &Event::PreviewGap {
                session_id: "s1".into(),
                track: broscribe_ipc::Track::System,
                from_s: 3.,
                to_s: 8.
            },
            Instant::now()
        ));
        assert_eq!(panel.lines[0].presentation().0, "0:03–0:08 · Others");
    }

    /// A stopped recording's supervisors may still publish after the next
    /// one started: the panel showing the new one never takes that late
    /// transcript, gap, preview or identity state as its own, and still
    /// shows the new recording's own.
    #[test]
    fn a_stopped_recordings_late_live_events_never_change_the_shown_recording() {
        let now = Instant::now();
        let mut panel = PanelModel::default();
        panel.apply(&recording("s-a"), now);
        panel.apply(&recording("s-b"), now);
        let ready = broscribe_ipc::PreviewState::Ready {
            model: "synthetic".into(),
        };
        assert!(panel.apply(
            &Event::Preview {
                session_id: "s-b".into(),
                preview: ready.clone(),
            },
            now
        ));
        let identity = |status| LiveIdentityState { status, epoch: 1 };
        assert!(panel.apply(
            &Event::LiveIdentity {
                session_id: "s-b".into(),
                identity: identity(broscribe_ipc::live::LiveIdentityStatus::Ready),
            },
            now
        ));
        let (revision, rows) = (panel.revision(), panel.rows_revision());
        let segment = |text: &str| broscribe_ipc::LiveSegment {
            track: broscribe_ipc::Track::Mic,
            start_s: 1.,
            end_s: 2.,
            text: text.into(),
            speaker: LiveSpeaker::Me,
            words: Vec::new(),
        };
        for late in [
            Event::LiveTranscript {
                session_id: "s-a".into(),
                segment: segment("late synthetic words"),
            },
            Event::PreviewGap {
                session_id: "s-a".into(),
                track: broscribe_ipc::Track::System,
                from_s: 3.,
                to_s: 8.,
            },
            Event::Preview {
                session_id: "s-a".into(),
                preview: broscribe_ipc::PreviewState::Off,
            },
            Event::LiveIdentity {
                session_id: "s-a".into(),
                identity: identity(broscribe_ipc::live::LiveIdentityStatus::Idle),
            },
        ] {
            assert!(!panel.apply(&late, now), "{late:?}");
        }
        assert_eq!((panel.revision(), panel.rows_revision()), (revision, rows));
        assert!(panel.lines.is_empty());
        assert_eq!(panel.preview, ready);
        assert_eq!(
            panel.identity,
            identity(broscribe_ipc::live::LiveIdentityStatus::Ready)
        );
        // The shown recording's own transcript and `idle` still arrive.
        assert!(panel.apply(
            &Event::LiveTranscript {
                session_id: "s-b".into(),
                segment: segment("own synthetic words"),
            },
            now
        ));
        assert_eq!(panel.lines.len(), 1);
        assert!(panel.apply(
            &Event::LiveIdentity {
                session_id: "s-b".into(),
                identity: identity(broscribe_ipc::live::LiveIdentityStatus::Idle),
            },
            now
        ));
    }

    fn member(name: &str, email: Option<&str>) -> broscribe_readers::RosterMember {
        broscribe_readers::RosterMember {
            name: name.into(),
            email: email.map(str::to_owned),
            is_self: false,
        }
    }

    #[test]
    fn observed_roster_matches_confirmed_attendance_by_email_or_name() {
        let attendee = member("Synthetic Attendee", Some("attendee@example.com"));
        let mut confirmed = BTreeMap::new();
        assert!(!observed_confirmed(&attendee, &confirmed));
        confirmed.insert(
            "p-1".to_owned(),
            RosterPerson {
                name: "Someone Else".into(),
                emails: vec!["ATTENDEE@example.com".into()],
            },
        );
        assert!(observed_confirmed(&attendee, &confirmed), "by email");
        let by_name = member("Sýnthetic Áttendee", None);
        confirmed.insert(
            "p-2".to_owned(),
            RosterPerson {
                name: "synthetic attendee".into(),
                emails: Vec::new(),
            },
        );
        assert!(observed_confirmed(&by_name, &confirmed), "by folded name");
    }

    #[test]
    fn tag_input_adds_literally_and_removal_is_explicit() {
        assert_eq!(tag_call("s1", " , "), None);
        assert_eq!(
            tag_call("s1", "alpha, -literal ,alpha,, +plus"),
            Some(Call::RecordTag {
                tags: vec!["+alpha".into(), "+-literal".into(), "++plus".into()],
                session_id: "s1".into(),
            })
        );
        assert_eq!(
            remove_tag_call("s1", "-literal"),
            Call::RecordTag {
                tags: vec!["--literal".into()],
                session_id: "s1".into(),
            }
        );
    }

    #[test]
    fn answers_become_explicit_statuses() {
        assert!(
            control_error(&IpcError::new("stale_session", "x")).contains("nothing was changed")
        );
        // WRT-04: an unknown refusal shows the agent's words, never its code.
        assert_eq!(
            control_error(&IpcError::new(
                "invalid_input",
                "plain failure (with words)"
            )),
            "Plain failure (with words)."
        );
        assert_eq!(
            participant_outcome(
                &json!({"status": "confirmed", "person_id": "p-1", "created": true}),
                "Synthetic Newcomer"
            ),
            ControlStatus::done(
                "Added Synthetic Newcomer as a new person (name only, no voice) and confirmed \
                 attendance."
            )
        );
        let ControlStatus::Failed(partial) = participant_outcome(
            &json!({"status": "partial", "person_id": "p-1", "created": true,
                    "attendance": false, "message": "journal busy"}),
            "Synthetic Newcomer",
        ) else {
            panic!("partial is a failure");
        };
        assert_eq!(
            partial,
            "Synthetic Newcomer was added as a new person, but attendance wasn't confirmed. \
             Choose them again to retry."
        );
        // WRT-04: the code the agent's message carries stays out.
        let partial = control_error(&IpcError::new(
            "partial_outcome",
            "person p-1 was created, but the live turn was not attributed (stale_turn)",
        ));
        assert_eq!(
            partial,
            "A new name-only person was created, but this line's name wasn't saved. Choose \
             them again to retry."
        );
        assert_eq!(
            title_outcome(&json!({"status": "already_published", "meeting_id": "m-1"})),
            ControlStatus::Done {
                text: "This recording is already saved as a meeting: rename it there.".into(),
                meeting: Some(MeetingLink {
                    meeting_id: "m-1".into(),
                    offer_delete: false
                }),
            }
        );
        let ControlStatus::Done { meeting, .. } =
            discard_outcome(&json!({"status": "already_saved", "meeting_id": "m-2"}))
        else {
            panic!("already saved is answered");
        };
        assert!(meeting.unwrap().offer_delete);
    }

    // ---- the panel through the real IPC seam, with a fake agent ----

    type Reply = Box<dyn FnMut(&Call) -> Result<Value, IpcError> + Send>;

    /// The agent's refusal written as its log line, "message (code)".
    fn refusal(text: &str) -> IpcError {
        let (message, code) = text
            .strip_suffix(')')
            .and_then(|inner| inner.rsplit_once(" ("))
            .expect("\"message (code)\"");
        IpcError::new(code, message)
    }

    /// The agent's side of the transport: records every call and answers it.
    struct FakeAgent {
        calls: Mutex<Vec<Call>>,
        reply: Mutex<Reply>,
    }
    impl FakeAgent {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
                reply: Mutex::new(Box::new(|_| Ok(json!({})))),
            })
        }
        fn answer(&self, reply: impl FnMut(&Call) -> Result<Value, IpcError> + Send + 'static) {
            *self.reply.lock().unwrap() = Box::new(reply);
        }
        /// Every call but the `record.live` snapshots the client fetches on
        /// its own (see [`Self::snapshots`]).
        fn calls(&self) -> Vec<Call> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|c| !matches!(c, Call::RecordLive { .. }))
                .cloned()
                .collect()
        }
        /// Sessions of the `record.live` snapshots requested so far.
        fn snapshots(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter_map(|c| match c {
                    Call::RecordLive { session_id } => Some(session_id.clone()),
                    _ => None,
                })
                .collect()
        }
    }
    impl Transport for FakeAgent {
        fn subscribe(&self) -> Result<Box<dyn EventStream>, IpcError> {
            Err(IpcError::new("unused", "events are delivered by the test"))
        }
        fn call(&self, call: Call) -> Result<Value, IpcError> {
            self.calls.lock().unwrap().push(call.clone());
            (self.reply.lock().unwrap())(&call)
        }
    }

    fn daemon_state(recording: Option<&str>, stopped: Value, saved: Value) -> Event {
        let recording = recording.map(|session| {
            json!({
                "session_id": session,
                "status": "recording",
                "started_at": "2026-11-14T10:00:00Z",
                "elapsed_s": 1.0,
                "language": null,
                "app": "teams",
                "title": format!("Synthetic {session}"),
                "setup": null,
                "participants": [],
                "observed_roster": [],
                "tags": ["kept"],
                "health": healthy()
            })
        });
        let state = serde_json::from_value(json!({
            "pid": 123,
            "version": "synthetic",
            "protocol": broscribe_ipc::PROTOCOL_VERSION,
            "library": "/synthetic/library",
            "recording": recording,
            "stopped": stopped,
            "saved": saved,
            "jobs": { "running": null, "waiting": 0, "deferred": 0 },
            "preview": { "state": "off" },
            "pending_notifications": []
        }))
        .unwrap();
        Event::State(Box::new(state))
    }

    /// Both tracks capturing, nothing lost, disk not measured yet.
    fn healthy() -> Value {
        json!({ "tracks": [
            { "track": "mic", "state": "capturing", "dropped_frames": 0, "failures": 0 },
            { "track": "system", "state": "capturing", "dropped_frames": 0, "failures": 0 }
        ] })
    }

    /// A stopped recording's state as the agent sends it.
    fn processing() -> Value {
        json!({ "phase": "running" })
    }

    fn recording(session: &str) -> Event {
        daemon_state(Some(session), json!([]), json!([]))
    }

    struct Harness {
        window: WindowHandle<RecordingPanel>,
        daemon: Entity<DaemonClient>,
        notifications: Entity<NotificationHost>,
        agent: Arc<FakeAgent>,
        state: Entity<AppState>,
        starter: Entity<AgentStarter>,
        _s: Synthetic,
        _dir: tempfile::TempDir,
    }

    impl Harness {
        fn new(cx: &mut TestAppContext) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let (state, _lib, s) = fixtures::state(cx, dir.path());
            let agent = FakeAgent::new();
            let transport: Arc<dyn Transport> = agent.clone();
            let daemon = cx.update(|cx| DaemonClient::detached(transport, cx));
            let notifications = cx.update(|cx| {
                NotificationHost::attach(
                    daemon.clone(),
                    Box::new(FakeNotifier::default()),
                    dir.path().join("data"),
                    cx,
                )
            });
            let starter = cx.new(|cx| AgentStarter::new(state.clone(), daemon.clone(), cx));
            let window = Self::open(cx, &daemon, &notifications, &state, &starter);
            Self {
                window,
                daemon,
                notifications,
                agent,
                state,
                starter,
                _s: s,
                _dir: dir,
            }
        }

        fn open(
            cx: &mut TestAppContext,
            daemon: &Entity<DaemonClient>,
            notifications: &Entity<NotificationHost>,
            state: &Entity<AppState>,
            starter: &Entity<AgentStarter>,
        ) -> WindowHandle<RecordingPanel> {
            let window = cx.add_window({
                let (daemon, notifications, state, starter) = (
                    daemon.clone(),
                    notifications.clone(),
                    state.clone(),
                    starter.clone(),
                );
                move |window, cx| {
                    RecordingPanel::new(daemon, notifications, state, starter, window, cx)
                }
            });
            cx.run_until_parked();
            window
        }

        /// Whether the panel window is still open.
        fn open_now(&self, cx: &mut TestAppContext) -> bool {
            self.window.update(cx, |_, _, _| ()).is_ok()
        }

        /// The user opens the panel again (menu Open): a new panel.
        fn reopen(&mut self, cx: &mut TestAppContext) {
            assert!(!self.open_now(cx), "the previous panel closed");
            self.window = Self::open(
                cx,
                &self.daemon,
                &self.notifications,
                &self.state,
                &self.starter,
            );
        }

        fn event(&self, cx: &mut TestAppContext, event: Event) {
            self.daemon.update(cx, |d, cx| d.received(event, cx));
            cx.run_until_parked();
        }

        fn act(
            &self,
            cx: &mut TestAppContext,
            f: impl FnOnce(&mut RecordingPanel, &mut Window, &mut Context<RecordingPanel>),
        ) {
            self.window
                .update(cx, |view, window, cx| f(view, window, cx))
                .unwrap();
            cx.run_until_parked();
        }

        fn status(&self, cx: &mut TestAppContext, control: Control) -> Option<ControlStatus> {
            self.window
                .update(cx, |view, _, cx| view.shown_status(control, cx).cloned())
                .unwrap()
        }

        fn type_into(
            &self,
            cx: &mut TestAppContext,
            input: impl Fn(&RecordingPanel) -> Entity<InputState>,
            text: &str,
        ) {
            let text = text.to_owned();
            self.act(cx, move |view, window, cx| {
                input(view).update(cx, |i, cx| i.set_value(text, window, cx))
            });
        }

        fn value(
            &self,
            cx: &mut TestAppContext,
            input: impl Fn(&RecordingPanel) -> Entity<InputState>,
        ) -> String {
            self.window
                .update(cx, |view, _, cx| input(view).read(cx).value().to_string())
                .unwrap()
        }
    }

    #[gpui::test]
    fn meter_events_never_clear_a_command_error(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        h.event(cx, recording("s1"));
        h.agent
            .answer(|_| Err(refusal("capture device busy (busy)")));
        // A menu command's failure is the agent-level error.
        h.daemon.update(cx, |d, cx| {
            d.send(
                Call::RecordStop {
                    session_id: "s1".into(),
                },
                cx,
            )
        });
        cx.run_until_parked();
        // A panel control's failure stays beside the control.
        h.act(cx, |view, window, cx| {
            view.session_action(SessionCommand::Pause, window, cx)
        });
        for _ in 0..30 {
            h.event(
                cx,
                Event::Meters {
                    me_dbfs: -20.,
                    others_dbfs: -30.,
                },
            );
        }
        h.event(cx, recording("s1"));
        assert_eq!(
            h.daemon.read_with(cx, |d, _| d.error.clone()),
            Some("Capture device busy.".into())
        );
        assert_eq!(
            h.status(cx, Control::Session),
            Some(ControlStatus::Failed("Capture device busy.".into()))
        );
        // Only a reconnection, or a later successful command, clears it.
        h.daemon.update(cx, |d, cx| d.lost(None, cx));
        h.event(cx, recording("s1"));
        assert_eq!(h.daemon.read_with(cx, |d, _| d.error.clone()), None);
        assert!(matches!(
            h.status(cx, Control::Session),
            Some(ControlStatus::Failed(_))
        ));
    }

    #[gpui::test]
    fn typed_participant_is_created_by_the_agent_and_the_draft_clears_on_success(
        cx: &mut TestAppContext,
    ) {
        let h = Harness::new(cx);
        h.event(cx, recording("s1"));
        let picker = h.window.update(cx, |v, _, _| v.picker.clone()).unwrap();
        h.act(cx, |_, window, cx| {
            picker.update(cx, |p, cx| p.set_query("Synthetic Newcomer", window, cx))
        });
        // An ambiguous name is refused: the draft stays for a fix.
        h.agent
            .answer(|_| Err(refusal("several people are called that (invalid_input)")));
        h.act(cx, |_, _, cx| picker.update(cx, |p, cx| p.use_typed(cx)));
        assert!(matches!(
            &h.agent.calls()[..],
            [Call::RecordParticipant { session_id, person_ref, action: ParticipantAction::Confirm, .. }]
                if session_id == "s1" && person_ref == "Synthetic Newcomer"
        ));
        assert!(matches!(
            h.status(cx, Control::Participant),
            Some(ControlStatus::Failed(text)) if text == "Several people are called that."
        ));
        assert_ne!(
            picker.read_with(cx, |p, _| p.typed()),
            crate::views::person_picker::TypedName::Empty
        );
        // Accepted: a name-only person, attendance only, the draft cleared.
        h.agent.answer(|_| {
            Ok(json!({"status": "confirmed", "person_id": "p-synthetic", "created": true}))
        });
        h.act(cx, |_, _, cx| picker.update(cx, |p, cx| p.use_typed(cx)));
        assert!(matches!(
            h.status(cx, Control::Participant),
            Some(ControlStatus::Done { text, .. }) if text.contains("name only, no voice")
        ));
        assert_eq!(
            picker.read_with(cx, |p, _| p.typed()),
            crate::views::person_picker::TypedName::Empty
        );
        let confirmed = h
            .window
            .update(cx, |v, _, _| v.confirmed.contains_key("p-synthetic"))
            .unwrap();
        assert!(confirmed);
        // A partial outcome says attendance was not recorded.
        h.agent.answer(|_| {
            Ok(
                json!({"status": "partial", "person_id": "p-other", "created": true,
                      "attendance": false, "message": "journal write failed"}),
            )
        });
        h.act(cx, |_, window, cx| {
            picker.update(cx, |p, cx| p.set_query("Synthetic Other", window, cx))
        });
        h.act(cx, |_, _, cx| picker.update(cx, |p, cx| p.use_typed(cx)));
        assert!(matches!(
            h.status(cx, Control::Participant),
            Some(ControlStatus::Failed(text)) if text.contains("attendance wasn't confirmed")
                && !text.contains("journal write failed")
        ));
        assert!(
            !h.window
                .update(cx, |v, _, _| v.confirmed.contains_key("p-other"))
                .unwrap()
        );
    }

    #[gpui::test]
    fn a_stale_session_answer_never_touches_a_newer_session(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        h.event(cx, recording("s1"));
        h.agent.answer(|_| Ok(json!({"tags": ["late"]})));
        h.type_into(cx, |v| v.tag.clone(), "late");
        // Submitted for s1; s2 replaces it before the answer is applied.
        h.window
            .update(cx, |view, window, cx| view.submit_tags(window, cx))
            .unwrap();
        h.daemon.update(cx, |d, cx| d.received(recording("s2"), cx));
        cx.run_until_parked();
        assert_eq!(h.status(cx, Control::Tag), None);
        let tags = h.window.update(cx, |v, _, cx| v.shown_tags(cx)).unwrap();
        assert_eq!(tags, ["kept"], "s2 shows its own snapshot");
        assert_eq!(h.value(cx, |v| v.tag.clone()), "");
        // The agent refuses a stale target explicitly.
        h.agent
            .answer(|_| Err(refusal("session s1 is not active (stale_session)")));
        h.type_into(cx, |v| v.note.clone(), "Synthetic note");
        h.act(cx, |view, window, cx| view.submit_note(window, cx));
        assert!(matches!(
            h.status(cx, Control::Note),
            Some(ControlStatus::Failed(text)) if text.contains("nothing was changed")
        ));
        assert_eq!(h.value(cx, |v| v.note.clone()), "Synthetic note");
    }

    #[gpui::test]
    fn tags_show_as_chips_and_are_added_and_removed_literally(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        h.event(cx, recording("s1"));
        // Empty input sends nothing.
        h.type_into(cx, |v| v.tag.clone(), " , ");
        h.act(cx, |view, window, cx| view.submit_tags(window, cx));
        assert!(h.agent.calls().is_empty());
        h.agent
            .answer(|_| Ok(json!({"tags": ["kept", "alpha", "-literal"]})));
        h.type_into(cx, |v| v.tag.clone(), "alpha, -literal");
        h.act(cx, |view, window, cx| view.submit_tags(window, cx));
        assert_eq!(
            h.agent.calls(),
            [Call::RecordTag {
                tags: vec!["+alpha".into(), "+-literal".into()],
                session_id: "s1".into(),
            }]
        );
        assert_eq!(h.value(cx, |v| v.tag.clone()), "");
        let tags = h.window.update(cx, |v, _, cx| v.shown_tags(cx)).unwrap();
        assert_eq!(tags, ["kept", "alpha", "-literal"]);
        h.agent.answer(|_| Ok(json!({"tags": ["kept", "alpha"]})));
        h.act(cx, |view, window, cx| {
            view.remove_tag("-literal".into(), window, cx)
        });
        assert_eq!(
            h.agent.calls()[1],
            Call::RecordTag {
                tags: vec!["--literal".into()],
                session_id: "s1".into(),
            }
        );
        let tags = h.window.update(cx, |v, _, cx| v.shown_tags(cx)).unwrap();
        assert_eq!(tags, ["kept", "alpha"]);
    }

    /// The title field of recording `session`.
    fn title_of(session: &'static str) -> impl Fn(&RecordingPanel) -> Entity<InputState> {
        move |v| v.title_input(session).expect("a title editor")
    }

    fn stopped_entry(session: &str, title: Option<&str>) -> Value {
        json!({ "session_id": session, "title": title, "tags": [],
                "stopped_at": "2026-11-14T11:00:00Z", "state": processing() })
    }

    #[gpui::test]
    fn titles_follow_the_session_and_link_a_published_meeting(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.event(cx, recording("s1"));
        assert_eq!(h.value(cx, title_of("s1")), "Synthetic s1");
        h.agent.answer(|call| match call {
            Call::RecordTitle { title, .. } => Ok(json!({
                "status": "saved", "session_id": "s1", "title": title
            })),
            _ => Ok(json!({})),
        });
        h.type_into(cx, title_of("s1"), "  Synthetic renamed  ");
        h.act(cx, |view, window, cx| view.submit_title("s1", window, cx));
        let Call::RecordTitle {
            session_id, title, ..
        } = &h.agent.calls()[0]
        else {
            panic!("a title call");
        };
        assert_eq!(
            (session_id.as_str(), title.as_str()),
            ("s1", "Synthetic renamed")
        );
        assert!(matches!(
            h.status(cx, Control::Title),
            Some(ControlStatus::Done { text, .. }) if text.starts_with("Title saved")
        ));
        // Stopped, still processing. Nothing unsaved is left in the panel
        // (its title was saved), so it closes with the capture.
        h.event(
            cx,
            daemon_state(
                None,
                json!([stopped_entry("s1", Some("Synthetic renamed"))]),
                json!([]),
            ),
        );
        assert!(!h.open_now(cx));
        // Reopened, its editor renames that stopped session.
        h.reopen(cx);
        assert_eq!(h.value(cx, title_of("s1")).trim(), "Synthetic renamed");
        h.agent
            .answer(|_| Ok(json!({"status": "already_published", "meeting_id": "m-synthetic"})));
        h.type_into(cx, title_of("s1"), "Synthetic final");
        h.act(cx, |view, window, cx| view.submit_title("s1", window, cx));
        assert!(matches!(
            &h.agent.calls()[1],
            Call::RecordTitle { session_id, .. } if session_id == "s1"
        ));
        assert!(matches!(
            h.status(cx, Control::Title),
            Some(ControlStatus::Done { meeting: Some(MeetingLink { meeting_id, .. }), .. })
                if meeting_id == "m-synthetic"
        ));
        // Nothing to rename once it has neither an active nor a stopped session.
        h.event(cx, daemon_state(None, json!([]), json!([])));
        h.act(cx, |view, window, cx| view.submit_title("s1", window, cx));
        assert_eq!(h.agent.calls().len(), 2);
    }

    #[gpui::test]
    fn every_stopped_recording_is_renamed_as_itself(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        h.event(cx, recording("s3"));
        h.event(
            cx,
            daemon_state(
                Some("s3"),
                json!([
                    stopped_entry("s1", Some("Synthetic one")),
                    stopped_entry("s2", None)
                ]),
                json!([]),
            ),
        );
        // Each recording has its own field, with its own title.
        assert_eq!(h.value(cx, title_of("s1")), "Synthetic one");
        assert_eq!(h.value(cx, title_of("s2")), "");
        assert_eq!(h.value(cx, title_of("s3")), "Synthetic s3");
        h.agent.answer(|call| match call {
            Call::RecordTitle {
                session_id, title, ..
            } => Ok(json!({ "status": "saved", "session_id": session_id, "title": title })),
            _ => Ok(json!({})),
        });
        h.type_into(cx, title_of("s2"), "Synthetic two");
        h.type_into(cx, title_of("s1"), "Synthetic one, edited");
        h.act(cx, |view, window, cx| view.submit_title("s2", window, cx));
        let calls = h.agent.calls();
        assert!(matches!(
            &calls[..],
            [Call::RecordTitle { session_id, title, .. }]
                if session_id == "s2" && title == "Synthetic two"
        ));
        let status = |h: &Harness, cx: &mut TestAppContext, session: &'static str| {
            h.window
                .update(cx, |v, _, _| {
                    v.session_status(Control::Title, session).cloned()
                })
                .unwrap()
        };
        assert!(matches!(
            status(&h, cx, "s2"),
            Some(ControlStatus::Done { .. })
        ));
        assert_eq!(status(&h, cx, "s1"), None, "another recording's result");
        // s1's unsaved draft is untouched by s2's rename and by a state that
        // still carries s1's old title.
        h.event(
            cx,
            daemon_state(
                Some("s3"),
                json!([
                    stopped_entry("s1", Some("Synthetic one")),
                    stopped_entry("s2", Some("Synthetic two"))
                ]),
                json!([]),
            ),
        );
        assert_eq!(h.value(cx, title_of("s1")), "Synthetic one, edited");
        assert_eq!(h.value(cx, title_of("s2")), "Synthetic two");
    }

    #[gpui::test]
    fn discard_asks_first_and_never_reaches_a_newer_session(cx: &mut TestAppContext) {
        let mut h = Harness::new(cx);
        h.event(cx, recording("s1"));
        // Cancel changes nothing.
        h.act(cx, |view, _, cx| view.open_discard("s1".into(), cx));
        h.act(cx, |view, _, cx| view.cancel_discard(cx));
        assert!(h.agent.calls().is_empty());
        // A confirmation opened for s1 closes when s2 replaces it.
        h.act(cx, |view, _, cx| view.open_discard("s1".into(), cx));
        h.event(cx, recording("s2"));
        assert_eq!(
            h.window
                .update(cx, |v, _, _| v.discard_target().map(str::to_owned))
                .unwrap(),
            None
        );
        h.act(cx, |view, window, cx| view.confirm_discard(window, cx));
        assert!(h.agent.calls().is_empty());
        // Confirmed for s2: exactly that session; the answer may be "saved".
        h.agent
            .answer(|_| Ok(json!({"status": "already_saved", "meeting_id": "m-synthetic"})));
        h.act(cx, |view, _, cx| view.open_discard("s2".into(), cx));
        h.act(cx, |view, window, cx| view.confirm_discard(window, cx));
        assert!(matches!(
            &h.agent.calls()[..],
            [Call::RecordDiscard { session_id, .. }] if session_id == "s2"
        ));
        assert!(matches!(
            h.status(cx, Control::Discard),
            Some(ControlStatus::Done {
                meeting: Some(MeetingLink {
                    offer_delete: true,
                    ..
                }),
                ..
            })
        ));
        // A stopped, unsaved recording can still be discarded, from the
        // panel opened again (it closed with the capture of s2).
        h.event(
            cx,
            daemon_state(
                None,
                json!([stopped_entry("s3", None)]),
                json!([{ "session_id": "s2", "meeting_id": "m-synthetic", "title": null,
                         "saved_at": "2026-11-14T11:00:00Z" }]),
            ),
        );
        h.reopen(cx);
        h.agent.answer(|_| {
            Ok(json!({"status": "discarded", "session_id": "s3", "cleanup": "pending"}))
        });
        h.act(cx, |view, _, cx| view.open_discard("s3".into(), cx));
        h.act(cx, |view, window, cx| view.confirm_discard(window, cx));
        assert!(matches!(
            h.status(cx, Control::Discard),
            Some(ControlStatus::Done { text, .. }) if text.contains("removed in the background")
        ));
        // A discarding session cannot be discarded again.
        h.event(
            cx,
            daemon_state(
                None,
                json!([{ "session_id": "s3", "title": null, "tags": [],
                         "stopped_at": "2026-11-14T11:00:00Z",
                         "state": { "phase": "discarding" } }]),
                json!([]),
            ),
        );
        h.act(cx, |view, _, cx| view.open_discard("s3".into(), cx));
        assert_eq!(
            h.window
                .update(cx, |v, _, _| v.discard_target().map(str::to_owned))
                .unwrap(),
            None
        );
    }

    #[gpui::test]
    fn discard_dialog_keeps_the_panel_and_returns_focus_to_the_recording(cx: &mut TestAppContext) {
        use crate::ui::kit::dialog_gate::{Expected, cancel_both_ways, click};

        let h = Harness::new(cx);
        h.event(
            cx,
            daemon_state(
                None,
                json!([stopped_entry("synthetic-stopped", None)]),
                json!([]),
            ),
        );
        let window = cx.add_window(|window, cx| {
            let panel = cx.new(|cx| {
                RecordingPanel::new(
                    h.daemon.clone(),
                    h.notifications.clone(),
                    h.state.clone(),
                    h.starter.clone(),
                    window,
                    cx,
                )
            });
            gpui::component::Root::new(panel, window, cx)
        });
        cx.run_until_parked();
        let vcx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        cancel_both_ways(
            vcx,
            "discard-synthetic-stopped",
            Expected {
                title: "Discard this recording?",
                cancel_id: "discard-cancel",
                action_id: "discard-confirm",
                action: "Discard recording",
                lines: &[
                    ("discard-voice-samples", "Voice samples from this recording"),
                    ("discard-audio", "The audio"),
                    (
                        "discard-disclosure",
                        "The live transcript and any draft transcript",
                    ),
                    (
                        "discard-people",
                        "People you confirmed as participants, including ones added by name",
                    ),
                ],
                headings: &["What goes", "What stays"],
            },
            |vcx| {
                click(vcx, "discard-synthetic-stopped", Role::Button);
                vcx.update(|window, cx| {
                    let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
                    assert_eq!(tree.read(tree.find("discard-cancel")), ("Button", "Cancel"));
                    assert_eq!(
                        tree.read(tree.find("discard-processing")),
                        ("Alert", "Its processing is cancelled.")
                    );
                });
            },
        );
        assert!(h.agent.calls().is_empty());
    }

    #[gpui::test]
    fn library_journal_actions_confirm_the_exact_session(cx: &mut TestAppContext) {
        use crate::ui::kit::dialog_gate::click;
        use broscribe_ipc::JournalResolution;
        use broscribe_ipc::recording_words::{JOURNAL_LIBRARY_MISMATCH, JOURNAL_LIBRARY_UNKNOWN};
        use gpui::component::WindowExt as _;

        let h = Harness::new(cx);
        let Event::State(mut state) = daemon_state(None, json!([]), json!([])) else {
            unreachable!()
        };
        let library_root = h
            .state
            .read_with(cx, |state, _| state.library().root().to_path_buf());
        let library_root = library_root.canonicalize().unwrap();
        state.library = library_root.to_string_lossy().into_owned();
        state.journal_issues = [
            ("s-legacy", JOURNAL_LIBRARY_UNKNOWN),
            ("s-foreign", JOURNAL_LIBRARY_MISMATCH),
        ]
        .map(|(session_id, code)| JournalIssue {
            session_id: session_id.into(),
            code: code.into(),
            corrupt: false,
        })
        .to_vec();
        h.event(cx, Event::State(state));
        let window = cx.add_window(|window, cx| {
            let panel = cx.new(|cx| {
                RecordingPanel::new(
                    h.daemon.clone(),
                    h.notifications.clone(),
                    h.state.clone(),
                    h.starter.clone(),
                    window,
                    cx,
                )
            });
            gpui::component::Root::new(panel, window, cx)
        });
        cx.run_until_parked();
        let vcx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            let legacy = tree.read(tree.find("journal-issue-s-legacy")).1;
            let foreign = tree.read(tree.find("journal-issue-s-foreign")).1;
            assert!(
                legacy.contains("before BroScribe tracked its library"),
                "{legacy}"
            );
            assert!(foreign.contains("belongs to another library"), "{foreign}");
        });
        click(vcx, "legacy-journal-discard-s-legacy", Role::Button);
        assert!(h.agent.calls().is_empty());
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        assert!(!vcx.update(|window, cx| window.has_active_dialog(cx)));
        assert!(h.agent.calls().is_empty());

        for (button, resolution) in [
            (
                "legacy-journal-discard-s-legacy",
                JournalResolution::Discard,
            ),
            ("legacy-journal-save-s-legacy", JournalResolution::Adopt),
        ] {
            click(vcx, button, Role::Button);
            let before = h.agent.calls().len();
            click(vcx, "legacy-journal-confirm", Role::Button);
            let calls = h.agent.calls();
            assert!(matches!(&calls[before..], [Call::RecordResolveJournal {
                session_id, library_root: target, resolution: requested, confirmed: true, ..
            }] if session_id == "s-legacy" && *requested == resolution && target == &library_root));
        }
        h.event(
            cx,
            daemon_state(None, json!([stopped_entry("s-legacy", None)]), json!([])),
        );
        let vcx = &mut gpui::VisualTestContext::from_window(window.into(), cx);
        vcx.update(|window, cx| {
            let tree = crate::ui::a11y_tree::draw_and_read(window, cx);
            let status = tree.find("status-resolvejournal-s-legacy-");
            assert_eq!(
                tree.read(status).1,
                "Recording added to this library for processing."
            );
            assert!(
                tree.subtree(tree.find("stopped-s-legacy"))
                    .contains(&status)
            );
        });
    }

    #[gpui::test]
    fn a_discard_belongs_to_its_recording_and_never_blocks_the_next(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        h.event(
            cx,
            daemon_state(Some("s2"), json!([stopped_entry("s1", None)]), json!([])),
        );
        h.agent.answer(|call| match call {
            Call::RecordDiscard { session_id, .. } if session_id == "s1" => {
                Ok(json!({"status": "discarded", "session_id": "s1", "cleanup": "complete"}))
            }
            Call::RecordDiscard { session_id, .. } => Err(refusal(&format!(
                "{session_id} could not be discarded: disk busy (busy)"
            ))),
            _ => Ok(json!({})),
        });
        // Discarding the stopped s1 is in flight (nothing has run yet) …
        h.window
            .update(cx, |view, window, cx| {
                view.open_discard("s1".into(), cx);
                view.confirm_discard(window, cx);
            })
            .unwrap();
        let status = |h: &Harness, cx: &mut TestAppContext, session: &'static str| {
            h.window
                .update(cx, |v, _, _| {
                    v.session_status(Control::Discard, session).cloned()
                })
                .unwrap()
        };
        assert_eq!(status(&h, cx, "s1"), Some(ControlStatus::Pending));
        // … and the active s2 can still be discarded on its own.
        let target = h
            .window
            .update(cx, |view, _, cx| {
                view.open_discard("s2".into(), cx);
                view.discard_target().map(str::to_owned)
            })
            .unwrap();
        assert_eq!(target.as_deref(), Some("s2"));
        h.act(cx, |view, window, cx| view.confirm_discard(window, cx));
        assert!(matches!(
            status(&h, cx, "s1"),
            Some(ControlStatus::Done { text, .. }) if text == "Recording discarded; nothing was saved."
        ));
        assert!(matches!(
            status(&h, cx, "s2"),
            Some(ControlStatus::Failed(text)) if text.contains("disk busy")
        ));
        // A later success on s1 never clears s2's failure.
        assert!(matches!(
            h.status(cx, Control::Discard),
            Some(ControlStatus::Failed(_))
        ));
    }

    #[gpui::test]
    fn closing_the_panel_never_stops_capture(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        h.event(cx, recording("s1"));
        h.window
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        assert!(h.agent.calls().is_empty());
    }

    #[gpui::test]
    fn the_panel_title_wraps_inside_the_minimum_width_at_largest(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        let mut visual = gpui::VisualTestContext::from_window(h.window.into(), cx);
        let (width, height) = crate::ui_state::PANEL_MIN;
        visual.simulate_resize(size(px(width), px(height)));
        // This harness has no component Root to map Text size to the rem,
        // so set the Largest (200 %) rem directly.
        visual.update(|window, _| window.set_rem_size(px(32.)));
        crate::ui::kit::dialog_gate::draw(&mut visual);
        visual.update(|window, _| {
            let title = gpui::base::test_support::snapshots(window)
                .into_iter()
                .find(|node| {
                    node.path()
                        .last()
                        .is_some_and(|part| part.to_string() == "recording-controls-title")
                })
                .expect("the panel title was laid out")
                .bounds();
            assert!(title.right() <= window.viewport_size().width, "{title:?}");
        });
    }

    #[gpui::test]
    fn narrow_recording_controls_keep_the_compact_toggle_and_tabs_in_view(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        h.event(cx, recording("synthetic"));
        let mut visual = gpui::VisualTestContext::from_window(h.window.into(), cx);
        visual.simulate_resize(size(px(459.), px(712.)));
        visual.update(|_, cx| {
            crate::ui::apply_reading_size(broscribe_core::config::ReadingSize::Largest, cx);
        });
        crate::ui::kit::dialog_gate::draw(&mut visual);
        let tabs = visual.debug_bounds("panel-tabs").unwrap();
        let transcript = visual.debug_bounds("live-transcript").unwrap();
        let status = visual.debug_bounds("live-transcript-status").unwrap();
        assert!(status.left() >= transcript.left() && status.right() <= transcript.right());
        h.window
            .update(&mut visual, |panel, window, _| {
                let header = panel.header_scroll.bounds();
                let toggle = gpui::base::test_support::snapshots(window)
                    .into_iter()
                    .find(|node| node.label() == Some("Hide transcript"))
                    .unwrap()
                    .bounds();
                assert!(toggle.bottom() <= header.bottom());
                assert!(header.size.height <= px(356.));
                assert!(tabs.top() >= header.bottom() && tabs.bottom() < px(712.));
            })
            .unwrap();
        crate::ui::kit::dialog_gate::click(&mut visual, "panel-compact", Role::Button);
        visual.run_until_parked();
        h.window
            .update(&mut visual, |panel, window, _| {
                assert!(panel.compact_layout());
                assert!(window.bounds().size.height <= px(712.));
            })
            .unwrap();
    }

    #[gpui::test]
    fn compact_panel_saves_config_and_keeps_the_header_attention_and_transcript_state(
        cx: &mut TestAppContext,
    ) {
        use crate::ui::kit::dialog_gate::{click, draw};
        let mut h = Harness::new(cx);
        let data = h
            .state
            .read_with(cx, |state, _| state.paths().data_dir.clone());
        let mut event = recording("synthetic");
        let Event::State(snapshot) = &mut event else {
            panic!("a state snapshot")
        };
        snapshot.recording.as_mut().unwrap().health.disk = Some(DiskHealth {
            level: DiskLevel::Low,
            free_bytes: 100_000_000,
            headroom_s: 45.,
        });
        h.event(cx, event);
        h.event(
            cx,
            Event::Notification(broscribe_ipc::NotificationRequest {
                id: "final-m-000123".into(),
                title: "Final transcript ready".into(),
                body: "A synthetic meeting is ready.".into(),
                actions: Vec::new(),
                meeting_id: Some("m-000123".into()),
            }),
        );

        let vcx = &mut gpui::VisualTestContext::from_window(h.window.into(), cx);
        let a11y =
            |vcx: &mut gpui::VisualTestContext| vcx.update(crate::ui::a11y_tree::draw_and_read);
        vcx.simulate_resize(size(px(420.), px(712.)));
        draw(vcx);
        let before = a11y(vcx);
        before.find("live-transcript");
        before.find("panel-attention");
        let transcript = h
            .window
            .update(vcx, |panel, _, _| panel.transcript_view.clone())
            .unwrap();

        click(vcx, "panel-compact", Role::Button);
        vcx.run_until_parked();
        assert!(
            broscribe_core::config::Config::load(&data)
                .unwrap()
                .panel
                .compact
        );
        let after = a11y(vcx);
        assert!(after.all("live-transcript").is_empty());
        assert!(after.all("panel-tabs").is_empty());
        after.find("panel-pause");
        after.find("recording-visor");
        after.assert_heading("panel-attention-heading", 2);
        assert_eq!(
            after.read(after.find("prompt-final-m-000123-open")),
            ("Button", "Open meeting")
        );
        h.window
            .update(vcx, |panel, window, _| {
                assert!(window.bounds().size.height < px(712.));
                assert_eq!(panel.transcript_view, transcript);
                assert!(panel.compact_focus.is_focused(window));
                assert!(panel.following_visible);
            })
            .unwrap();

        click(vcx, "panel-compact", Role::Button);
        vcx.run_until_parked();
        assert!(
            !broscribe_core::config::Config::load(&data)
                .unwrap()
                .panel
                .compact
        );
        a11y(vcx).find("live-transcript");
        h.window
            .update(vcx, |panel, window, _| {
                assert_eq!(window.bounds().size.height, px(712.));
                assert_eq!(panel.transcript_view, transcript);
                assert!(panel.compact_focus.is_focused(window));
            })
            .unwrap();

        // Use the same persisted value as a CLI change, then close and reopen.
        h.state.update(cx, |state, cx| {
            let (config, ()) = broscribe_core::config::Config::update(&data, |config| {
                config.panel.compact = true;
                Ok(())
            })
            .unwrap();
            state.set_config(config, cx);
        });
        h.act(cx, |_, window, _| window.remove_window());
        assert!(h.agent.calls().is_empty());
        h.reopen(cx);
        h.act(cx, |panel, _, _| assert!(panel.compact_layout()));
    }

    // ---- the bar under client-side decorations (SHL-03) ----

    /// macOS, Windows and a server-decorated Linux window keep the native
    /// title bar; the panel adds nothing of its own.
    #[gpui::test]
    fn server_side_decorations_add_no_bar_to_the_panel(cx: &mut TestAppContext) {
        use crate::ui::chrome::testing::{chrome, title_heading};

        let h = Harness::new(cx);
        h.event(cx, recording("s1"));
        chrome(h.window.into(), cx).assert_none();
        assert_eq!(title_heading(h.window, cx), None);
    }

    /// Where the compositor leaves decorations to the app the panel draws
    /// its own first row: the title and Close, never minimise or maximise
    /// (design §4), and nothing in it takes focus when the panel opens.
    #[gpui::test]
    fn client_side_decorations_give_the_panel_its_title_and_close_only(cx: &mut TestAppContext) {
        use crate::ui::chrome::PANEL_CLOSE_LABEL;
        use crate::ui::chrome::testing::{chrome, report_client_decorations, title_heading};

        let server = Harness::new(cx);
        server.event(cx, recording("s1"));
        let focused_without = chrome(server.window.into(), cx).focused;

        report_client_decorations(cx);
        let h = Harness::new(cx);
        h.event(cx, recording("s1"));
        let shown = chrome(h.window.into(), cx);
        shown.assert_first();
        assert_eq!(shown.control_labels(), [PANEL_CLOSE_LABEL]);
        assert_eq!(PANEL_CLOSE_LABEL, "Close recording controls");
        assert_eq!(
            shown.focused, focused_without,
            "the bar changed what the panel focuses on open"
        );
        assert_eq!(title_heading(h.window, cx).as_deref(), Some(WINDOW_TITLE));
    }

    #[gpui::test]
    fn closing_from_the_panels_own_bar_never_stops_capture(cx: &mut TestAppContext) {
        use crate::ui::chrome::PANEL_CLOSE_LABEL;
        use crate::ui::chrome::testing::{chrome, report_client_decorations};

        report_client_decorations(cx);
        let h = Harness::new(cx);
        h.event(cx, recording("s1"));
        let close = chrome(h.window.into(), cx)
            .control(PANEL_CLOSE_LABEL)
            .bounds;
        gpui::VisualTestContext::from_window(h.window.into(), cx)
            .simulate_click(close.center(), gpui::Modifiers::default());
        cx.run_until_parked();
        assert!(!h.open_now(cx), "Close left the panel open");
        assert!(
            h.agent.calls().is_empty(),
            "closing sent {:?}",
            h.agent.calls()
        );
    }

    // ---- live speaker turns (plan D) ----

    fn live_turn(id: &str, revision: u64, status: &str, extra: Value) -> Value {
        let (track, from) = id.split_once('-').unwrap();
        let from: u64 = from.parse().unwrap();
        let mut turn = json!({
            "session_id": "s1",
            "turn_id": id,
            "revision": revision,
            "epoch": 1,
            "track": track,
            "from_sample": from,
            "to_sample": from + 32_000,
            "start_s": from as f64 / 16_000.0,
            "end_s": from as f64 / 16_000.0 + 2.0,
            "text": format!("synthetic words r{revision}"),
            "status": status,
            "evidence": "ev1"
        });
        turn.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        turn
    }

    fn suggested(revision: u64) -> Value {
        live_turn(
            "system-16000",
            revision,
            "suggested",
            json!({ "voice": 2, "suggestion": {
                "person": { "person_id": "p-ana", "name": "Synthetic Ana" },
                "source": "session" } }),
        )
    }

    fn named_me(revision: u64) -> Value {
        live_turn(
            "mic-0",
            revision,
            "named",
            json!({ "named": { "person_id": "p-me", "name": "Synthetic Me" } }),
        )
    }

    fn live_event(turn: Value) -> Event {
        Event::LiveTurn(Box::new(serde_json::from_value(turn).unwrap()))
    }

    /// Answers `record.live` with the two synthetic rows; anything else
    /// with `other`.
    fn live_agent(
        h: &Harness,
        mut other: impl FnMut(&Call) -> Result<Value, IpcError> + Send + 'static,
    ) {
        h.agent.answer(move |call| match call {
            Call::RecordLive { session_id } => Ok(json!({
                "session_id": session_id,
                "identity": { "status": { "state": "ready" }, "epoch": 1 },
                "evidence": "ev1",
                "turns": [suggested(2), named_me(1)]
            })),
            call => other(call),
        });
    }

    fn turns(h: &Harness, cx: &mut TestAppContext) -> Vec<LiveTurn> {
        h.daemon.read_with(cx, |d, _| d.panel.turns.clone())
    }

    /// An uncalibrated suggestion (the opt-in closest stored voice) says so
    /// on the row and in the row's accessible label; a thresholded one
    /// does not. Nothing is named or confirmed by showing it.
    #[gpui::test]
    fn an_uncalibrated_suggestion_says_so_visibly_and_accessibly(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        live_agent(&h, |_| Ok(json!({})));
        h.event(cx, recording("s1"));
        h.event(
            cx,
            live_event(live_turn(
                "mic-32000",
                1,
                "suggested",
                json!({ "voice": 1, "suggestion": {
                    "person": { "person_id": "p-me", "name": "Synthetic Me" },
                    "source": "library", "uncalibrated": true } }),
            )),
        );
        let rows = turns(&h, cx);
        let row = |id: &str| rows.iter().find(|t| t.turn_id == id).unwrap().clone();
        let (uncalibrated, calibrated) = (row("mic-32000"), row("system-16000"));
        assert!(uncalibrated.suggestion.as_ref().unwrap().uncalibrated);
        assert_eq!(uncalibrated.status, LiveTurnStatus::Suggested);
        h.window
            .update(cx, |view, window, cx| {
                for (turn, flagged) in [(&uncalibrated, true), (&calibrated, false)] {
                    let element = view.transcript_row_frame(
                        SharedString::from(format!("live-turn-{}", turn.turn_id)),
                        &TranscriptRow::plain(RowKey::Turn(turn.turn_id.clone())),
                        spoken_turn_line(turn),
                        window,
                        cx,
                    );
                    let (id, role, label) =
                        crate::ui::a11y_name(&element).expect("an accessible row");
                    assert_eq!(id, format!("live-turn-{}", turn.turn_id));
                    assert_eq!(role, Role::ListItem);
                    assert_eq!(label, spoken_turn_line(turn));
                    assert_eq!(
                        label.contains("closest voice · not reliable yet"),
                        flagged,
                        "{label}"
                    );
                }
            })
            .unwrap();
        assert!(
            h.agent
                .calls()
                .iter()
                .all(|c| !matches!(c, Call::RecordLiveAttribute(_)))
        );
    }

    /// The shared agent start's progress and failure reach assistive
    /// technology as their own messages: a Status while it runs, an Alert
    /// when it fails; nothing when idle.
    #[gpui::test]
    fn agent_start_states_are_announced_with_their_message(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        let notice = |h: &Harness, cx: &mut TestAppContext| {
            h.window
                .update(cx, |view, _, cx| {
                    view.agent_start_notice(cx)
                        .map(|n| crate::ui::a11y_name(&n).expect("an accessible notice"))
                })
                .unwrap()
        };
        assert_eq!(
            notice(&h, cx),
            Some((
                "agent-start".into(),
                Role::Status,
                "Connecting to BroScribe Agent…".into()
            ))
        );
        h.starter
            .update(cx, |starter, cx| assert!(starter.start(cx)));
        let message = |h: &Harness, cx: &mut TestAppContext| {
            h.starter
                .read_with(cx, |s, _| s.status().message().unwrap())
        };
        assert_eq!(
            notice(&h, cx),
            Some(("agent-start".into(), Role::Status, message(&h, cx)))
        );
        // The harness's CLI does not exist: the start fails.
        cx.run_until_parked();
        assert!(h.starter.read_with(cx, |s, _| s.status().is_failure()));
        assert_eq!(
            notice(&h, cx),
            Some(("agent-start".into(), Role::Alert, message(&h, cx)))
        );
    }

    #[gpui::test]
    fn live_rows_follow_the_snapshot_and_newer_revisions(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        live_agent(&h, |_| Ok(json!({})));
        h.event(cx, recording("s1"));
        assert_eq!(h.agent.snapshots(), ["s1"]);
        let rows = turns(&h, cx);
        assert_eq!(
            rows.iter().map(|t| t.turn_id.as_str()).collect::<Vec<_>>(),
            ["mic-0", "system-16000"],
            "time order"
        );
        assert_eq!(
            spoken_turn_line(&rows[0]),
            "0 seconds · Me · Named by you · Synthetic Me: synthetic words r1"
        );
        assert_eq!(turn_badge(&rows[1]), "Suggested · Synthetic Ana");
        assert_eq!(
            turn_qualifier(&rows[1]).as_deref(),
            Some("Voice 2 · from this recording")
        );
        // A late, older revision is ignored; a newer one replaces the row.
        h.event(cx, live_event(suggested(1)));
        assert_eq!(turns(&h, cx)[1].revision, 2);
        h.event(
            cx,
            live_event(live_turn("system-16000", 3, "voice", json!({ "voice": 2 }))),
        );
        assert_eq!(turn_badge(&turns(&h, cx)[1]), "Voice 2");
        // Unresolved rows say why; unavailable analysis says what is shown.
        let mixed: LiveTurn = serde_json::from_value(live_turn(
            "system-48000",
            1,
            "unresolved",
            json!({ "unresolved": "mixed" }),
        ))
        .unwrap();
        assert_eq!(turn_badge(&mixed), "Unresolved");
        assert_eq!(
            turn_qualifier(&mixed).as_deref(),
            Some("more than one voice")
        );
        assert!(
            identity_notice(&LiveIdentityState {
                status: LiveIdentityStatus::Unavailable {
                    reason: WorkerProblem::Exited
                },
                epoch: 1,
            })
            .unwrap()
            .contains("Me / Others only")
        );
        // A lag asks for the authoritative rows again.
        h.event(cx, Event::Lagged { dropped: 4 });
        assert_eq!(h.agent.snapshots(), ["s1", "s1"]);
        assert_eq!(
            turns(&h, cx)[1].revision,
            2,
            "the snapshot is authoritative"
        );
        // Another session never shows these rows.
        h.event(cx, recording("s2"));
        assert!(turns(&h, cx).iter().all(|t| t.session_id != "s1"));
    }

    /// The "Speaking now" dock: the people of the call (meeting app,
    /// calendar, live names) as chips; 1–4 confirm or name the turn being
    /// spoken; *Who's this?* retargets it until *Back to speaking now*; a
    /// turn from the microphone is never named.
    #[gpui::test]
    fn the_speaking_now_dock_names_the_turn_being_spoken(cx: &mut TestAppContext) {
        use gpui::VisualTestContext;
        let h = Harness::new(cx);
        live_agent(&h, |call| match call {
            Call::RecordLiveAttribute(p) => {
                let (id, name) = match p.person_ref.as_deref() {
                    Some("p-ana") => ("p-ana", "Synthetic Ana"),
                    _ => ("p-noor", "Noor Example"),
                };
                Ok(json!({
                    "decision": 1,
                    "turn": live_turn(&p.turn_id, p.revision + 1, "named", json!({
                        "voice": 2, "named": { "person_id": id, "name": name } })),
                }))
            }
            _ => Ok(json!({})),
        });
        let member = |name: &str, email: Option<&str>| broscribe_readers::RosterMember {
            name: name.into(),
            email: email.map(Into::into),
            is_self: false,
        };
        h.event(
            cx,
            recording_with("s1", |r| {
                r.observed_roster = vec![
                    member("Synthetic Ana", None),
                    member("Noor Example", Some("noor@example.test")),
                ];
                // The same Noor from the calendar is one chip; Ben is expected.
                r.expected_roster = vec![
                    member("Noor Example", Some("noor@example.test")),
                    member("Ben Example", Some("ben@example.test")),
                ];
            }),
        );
        let dock = |h: &Harness, cx: &mut TestAppContext| {
            h.window
                .update(cx, |view, _, cx| {
                    let view = view.dock_view(cx).unwrap();
                    (
                        view.target.map(|t| t.turn_id),
                        view.retargeted,
                        view.chips
                            .iter()
                            .map(|c| (c.person.name.clone(), c.state.words()))
                            .collect::<Vec<_>>(),
                    )
                })
                .unwrap()
        };
        let (target, retargeted, chips) = dock(&h, cx);
        assert_eq!(target.as_deref(), Some("system-16000"));
        assert!(!retargeted);
        assert_eq!(
            chips,
            [
                ("Synthetic Ana".to_owned(), "tap to confirm".to_owned()),
                ("Noor Example".to_owned(), "not heard yet".to_owned()),
                ("Ben Example".to_owned(), "not heard yet".to_owned()),
            ]
        );
        let mut vcx = VisualTestContext::from_window(h.window.into(), cx);
        assert!(
            vcx.debug_bounds("speaking-dock").is_some(),
            "the dock is shown"
        );
        let focus = h
            .window
            .update(&mut vcx, |view, _, _| view.transcript_focus.clone())
            .unwrap();
        vcx.update(|window, cx| focus.focus(window, cx));
        // 1: Ana's chip confirms the suggestion on the turn being spoken.
        vcx.simulate_keystrokes("1");
        vcx.run_until_parked();
        let calls = h.agent.calls();
        let [Call::RecordLiveAttribute(confirm)] = &calls[..] else {
            panic!("{calls:?}");
        };
        assert_eq!(
            (
                confirm.action,
                confirm.turn_id.as_str(),
                confirm.person_ref.as_deref()
            ),
            (LiveAction::Confirm, "system-16000", Some("p-ana"))
        );
        // 2: Noor (not registered) is named by her e-mail; never a reject.
        vcx.simulate_keystrokes("2");
        vcx.run_until_parked();
        assert!(matches!(
            &h.agent.calls()[1],
            Call::RecordLiveAttribute(p) if p.action == LiveAction::Name
                && p.person_ref.as_deref() == Some("noor@example.test")
                && p.revision == 3
        ));
        // 5 is no person's key.
        vcx.simulate_keystrokes("5");
        vcx.run_until_parked();
        assert_eq!(h.agent.calls().len(), 2);
        let (_, _, chips) = dock(&h, cx);
        assert_eq!(chips[1].1, "named · 1 turn");

        // Who's this? on an older turn retargets the dock until Back.
        h.event(
            cx,
            live_event(live_turn("system-8000", 1, "voice", json!({ "voice": 3 }))),
        );
        h.act(cx, |view, window, cx| {
            view.open_live_naming("system-8000", window, cx)
        });
        let (target, retargeted, _) = dock(&h, cx);
        assert_eq!((target.as_deref(), retargeted), (Some("system-8000"), true));
        h.act(cx, |view, window, cx| view.dock_back(window, cx));
        let (target, retargeted, _) = dock(&h, cx);
        assert_eq!(
            (target.as_deref(), retargeted),
            (Some("system-16000"), false)
        );
        assert!(
            h.window
                .update(cx, |view, _, _| view.live_naming.is_none())
                .unwrap()
        );

        // Your microphone speaking: nothing to name.
        h.event(
            cx,
            live_event(live_turn("mic-64000", 1, "preview", json!({}))),
        );
        let mut vcx = VisualTestContext::from_window(h.window.into(), cx);
        vcx.update(|window, cx| focus.focus(window, cx));
        vcx.simulate_keystrokes("3");
        vcx.run_until_parked();
        assert_eq!(h.agent.calls().len(), 2, "{:?}", h.agent.calls());
    }

    #[gpui::test]
    fn live_decisions_name_only_the_row_as_shown(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        live_agent(&h, |call| match call {
            Call::RecordLiveAttribute(p) if p.action == LiveAction::Confirm => Ok(json!({
                "decision": 1,
                "turn": live_turn("system-16000", 3, "named", json!({
                    "voice": 2,
                    "named": { "person_id": "p-ana", "name": "Synthetic Ana" } })),
            })),
            Call::RecordLiveAttribute(p) if p.action == LiveAction::Clear => Ok(json!({
                "decision": 2,
                "turn": live_turn("mic-0", 2, "preview", json!({})),
            })),
            _ => Err(refusal("the row changed (stale_revision)")),
        });
        h.event(cx, recording("s1"));

        // Confirm sends the suggestion for exactly the row as shown.
        h.act(cx, |view, window, cx| {
            view.live_action("system-16000", LiveAction::Confirm, window, cx)
        });
        let calls = h.agent.calls();
        let [Call::RecordLiveAttribute(confirm)] = &calls[..] else {
            panic!("{calls:?}");
        };
        assert_eq!(
            (
                confirm.session_id.as_str(),
                confirm.turn_id.as_str(),
                confirm.revision,
                confirm.epoch,
                confirm.evidence.as_str(),
                confirm.person_ref.as_deref()
            ),
            ("s1", "system-16000", 2, 1, "ev1", Some("p-ana"))
        );
        assert!(matches!(
            h.status(cx, Control::Live),
            Some(ControlStatus::Done { text, .. }) if text == "Confirmed Synthetic Ana for this line. Agreeing names can teach BroScribe their voice after the meeting."
        ));
        assert_eq!(turns(&h, cx)[1].status, LiveTurnStatus::Named);
        // Clear works only on a named row and sends no person.
        h.act(cx, |view, window, cx| {
            view.live_action("mic-0", LiveAction::Clear, window, cx)
        });
        assert!(matches!(
            &h.agent.calls()[1],
            Call::RecordLiveAttribute(p) if p.action == LiveAction::Clear && p.person_ref.is_none() && p.revision == 1
        ));
        assert_eq!(turns(&h, cx)[0].status, LiveTurnStatus::Preview);
        h.act(cx, |view, window, cx| {
            view.live_action("mic-0", LiveAction::Clear, window, cx)
        });
        assert_eq!(h.agent.calls().len(), 2, "nothing to clear");

        // A picker on a row that changes closes without naming anyone.
        h.act(cx, |view, window, cx| {
            view.open_live_naming("system-16000", window, cx)
        });
        h.event(
            cx,
            live_event(live_turn("system-16000", 4, "voice", json!({ "voice": 2 }))),
        );
        assert!(
            h.window
                .update(cx, |v, _, _| v.live_picker().is_none())
                .unwrap()
        );
        assert!(matches!(
            h.status(cx, Control::Live),
            Some(ControlStatus::Failed(text)) if text.contains("changed while you were choosing")
        ));
        assert_eq!(h.agent.calls().len(), 2);

        // A typed new name goes with the revision the picker opened on; a
        // refusal refreshes the rows and saves nothing.
        h.act(cx, |view, window, cx| {
            view.open_live_naming("system-16000", window, cx)
        });
        let picker = h
            .window
            .update(cx, |v, _, _| v.live_picker().cloned())
            .unwrap()
            .unwrap();
        h.act(cx, |_, window, cx| {
            picker.update(cx, |p, cx| p.set_query("Synthetic Newcomer", window, cx))
        });
        let snapshots = h.agent.snapshots().len();
        h.act(cx, |_, _, cx| picker.update(cx, |p, cx| p.use_typed(cx)));
        assert!(matches!(
            &h.agent.calls()[2],
            Call::RecordLiveAttribute(p) if p.action == LiveAction::Name
                && p.person_ref.as_deref() == Some("Synthetic Newcomer")
                && p.revision == 4
        ));
        assert!(matches!(
            h.status(cx, Control::Live),
            Some(ControlStatus::Failed(text)) if text.contains("nothing was changed")
        ));
        assert_eq!(h.agent.snapshots().len(), snapshots + 1);
    }

    #[gpui::test]
    fn confirming_a_listed_participant_keeps_their_catalog_details(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        let ana = h._s.ana.as_str().to_owned();
        // Ana is already confirmed (loaded from the library with her email)
        // and also observed in the meeting app's roster by email.
        let Event::State(mut state) = recording("s1") else {
            unreachable!()
        };
        let recording = state.recording.as_mut().unwrap();
        recording.participants = vec![ana.clone()];
        recording.observed_roster = vec![
            serde_json::from_value(json!({
                "name": "Ana S.", "email": "ana@example.com", "is_self": false
            }))
            .unwrap(),
        ];
        h.event(cx, Event::State(state));
        let observed_confirmed_now = |h: &Harness, cx: &mut TestAppContext| {
            h.window
                .update(cx, |view, _, cx| {
                    let member = &view.model(cx).recording.as_ref().unwrap().observed_roster[0];
                    observed_confirmed(member, &view.confirmed)
                })
                .unwrap()
        };
        assert!(observed_confirmed_now(&h, cx), "matched by email");
        // Confirming her again (the agent answers as for any confirmation)
        // must not replace what the roster knows about her.
        let answer = ana.clone();
        h.agent.answer(move |_| {
            Ok(json!({ "status": "confirmed", "person_id": answer.clone(), "created": false }))
        });
        let id = h._s.ana.clone();
        h.act(cx, |view, window, cx| {
            view.confirm_participant(PersonChoice::Existing(id), window, cx)
        });
        assert!(matches!(
            h.status(cx, Control::Participant),
            Some(ControlStatus::Done { .. })
        ));
        assert!(observed_confirmed_now(&h, cx), "still matched by email");
    }

    // ---- automatic stop (plan H) ----

    fn recording_auto(session: &str, auto_stop: Value) -> Event {
        let Event::State(mut state) = recording(session) else {
            unreachable!()
        };
        state.recording.as_mut().unwrap().auto_stop =
            Some(serde_json::from_value(auto_stop).unwrap());
        Event::State(state)
    }

    fn teams() -> Value {
        json!({ "app_id": "teams", "app_name": "Synthetic Teams" })
    }

    fn pending_stop(generation: u64) -> Value {
        json!({
            "enabled": true, "app": teams(), "generation": generation,
            "reader_available": true,
            "pending": { "reason": "call_ended", "remaining_s": 10 },
            "candidates": []
        })
    }

    fn auto_stop(h: &Harness, cx: &mut TestAppContext) -> Option<AutoStopStatus> {
        h.daemon.read_with(cx, |d, _| {
            d.panel.recording.as_ref().and_then(|r| r.auto_stop.clone())
        })
    }

    #[test]
    fn countdown_never_becomes_negative_when_state_updates_are_delayed() {
        assert_eq!(auto_stop_remaining(10, Duration::from_millis(3_900)), 7);
        assert_eq!(auto_stop_remaining(10, Duration::from_secs(40)), 0);
    }

    #[gpui::test]
    fn recording_action_region_paints_one_primary_through_the_stop_grace(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        for (event, expected) in [
            (recording("synthetic"), "panel-stop"),
            (
                recording_with("synthetic", |recording| {
                    recording.status = RecordingStatus::Paused
                }),
                "panel-pause",
            ),
            (
                recording_auto("synthetic", pending_stop(1)),
                "auto-stop-keep",
            ),
            (recording("synthetic"), "panel-stop"),
        ] {
            h.event(cx, event);
            let actions = cx
                .update_window(h.window.into(), |_, window, cx| {
                    crate::ui::kit::painted_primary_actions(window, cx)
                })
                .unwrap();
            assert_eq!(actions, [expected]);
        }
    }

    #[gpui::test]
    fn keep_recording_cancels_the_pending_stop_for_this_session(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        h.event(cx, recording_auto("s1", pending_stop(3)));
        h.agent.answer(|call| match call {
            Call::RecordAutoStop(p) => Ok(json!({
                "enabled": p.enabled, "app": teams(), "generation": 4,
                "reader_available": true, "pending": null, "candidates": []
            })),
            _ => Ok(json!({})),
        });
        h.act(cx, |view, window, cx| {
            view.set_auto_stop(false, None, window, cx)
        });
        assert!(matches!(
            &h.agent.calls()[..],
            [Call::RecordAutoStop(p)] if p.session_id == "s1" && p.expected_generation == 3
                && !p.enabled && p.app_id.is_none()
        ));
        assert!(matches!(
            h.status(cx, Control::AutoStop),
            Some(ControlStatus::Done { text, .. }) if text.contains("off for this recording")
        ));
        let shown = auto_stop(&h, cx).unwrap();
        assert_eq!(
            (shown.enabled, shown.generation, shown.pending),
            (false, 4, None)
        );
        // A later explicit enable sends the new generation.
        h.act(cx, |view, window, cx| {
            view.set_auto_stop(true, None, window, cx)
        });
        assert!(matches!(
            &h.agent.calls()[1],
            Call::RecordAutoStop(p) if p.expected_generation == 4 && p.enabled
        ));
    }

    #[gpui::test]
    fn auto_stop_refusals_and_stale_sessions_change_nothing(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        h.event(cx, recording_auto("s1", pending_stop(3)));
        h.agent
            .answer(|_| Err(refusal("the binding changed (stale_binding)")));
        h.act(cx, |view, window, cx| {
            view.set_auto_stop(false, None, window, cx)
        });
        assert!(matches!(
            h.status(cx, Control::AutoStop),
            Some(ControlStatus::Failed(text)) if text.contains("changed meanwhile")
        ));
        assert_eq!(auto_stop(&h, cx).unwrap().generation, 3, "nothing changed");
        // An answer for s1 arriving after s2 replaced it never touches s2.
        h.agent.answer(|_| {
            Ok(json!({
                "enabled": false, "app": teams(), "generation": 9,
                "reader_available": true, "pending": null, "candidates": []
            }))
        });
        h.window
            .update(cx, |view, window, cx| {
                view.set_auto_stop(false, None, window, cx)
            })
            .unwrap();
        h.daemon.update(cx, |d, cx| {
            d.received(recording_auto("s2", pending_stop(1)), cx)
        });
        cx.run_until_parked();
        assert_eq!(h.status(cx, Control::AutoStop), None);
        let s2 = auto_stop(&h, cx).unwrap();
        assert_eq!((s2.generation, s2.pending.is_some()), (1, true));
    }

    #[gpui::test]
    fn unbound_recordings_link_only_by_choice_and_older_agents_offer_nothing(
        cx: &mut TestAppContext,
    ) {
        let h = Harness::new(cx);
        h.event(
            cx,
            recording_auto(
                "s1",
                json!({
                    "enabled": false, "app": null, "generation": 5,
                    "reader_available": false, "pending": null, "candidates": [teams()]
                }),
            ),
        );
        h.agent.answer(|_| {
            Ok(json!({
                "enabled": true, "app": teams(), "generation": 6,
                "reader_available": false, "pending": null, "candidates": []
            }))
        });
        h.act(cx, |view, window, cx| {
            view.set_auto_stop(true, Some("teams".into()), window, cx)
        });
        assert!(matches!(
            &h.agent.calls()[..],
            [Call::RecordAutoStop(p)] if p.expected_generation == 5 && p.enabled
                && p.app_id.as_deref() == Some("teams")
        ));
        assert!(matches!(
            h.status(cx, Control::AutoStop),
            Some(ControlStatus::Done { text, .. }) if text.contains("Synthetic Teams call ends")
        ));
        // An agent whose state has no automatic stop: nothing to send.
        h.event(cx, recording("s2"));
        assert_eq!(auto_stop(&h, cx), None);
        h.act(cx, |view, window, cx| {
            view.set_auto_stop(true, None, window, cx)
        });
        assert_eq!(h.agent.calls().len(), 1);
    }

    #[gpui::test]
    fn automatic_stop_switch_names_the_call_and_can_cancel_during_grace(cx: &mut TestAppContext) {
        use crate::ui::{a11y_tree, kit::dialog_gate::click};
        let h = Harness::new(cx);
        h.agent.answer(|_| Ok(json!({"enabled": false, "app": teams(), "generation": 4, "reader_available": false, "pending": null, "candidates": []})));
        h.event(cx, recording_auto("s1", pending_stop(3)));
        h.act(cx, |panel, _, cx| {
            panel.tab = RecordingPanelTab::Details;
            cx.notify();
        });
        let vcx = &mut gpui::VisualTestContext::from_window(h.window.into(), cx);
        vcx.simulate_resize(size(px(420.), px(1500.)));
        let tree = vcx.update(a11y_tree::draw_and_read);
        tree.assert_heading("panel-attention-heading", 2);
        assert_eq!(
            tree.read(tree.find("auto-stop-switch")),
            ("Switch", "Save when the Synthetic Teams call ends")
        );
        assert_eq!(
            tree.read(tree.find("auto-stop-summary")),
            ("Label", "Watches the call window and the microphone.")
        );
        click(vcx, "auto-stop-switch", Role::Switch);
        assert!(
            matches!(&h.agent.calls()[..], [Call::RecordAutoStop(params)] if params.session_id == "s1" && params.expected_generation == 3 && !params.enabled)
        );
        let tree = vcx.update(a11y_tree::draw_and_read);
        assert_eq!(
            tree.read(tree.find("auto-stop-summary")),
            (
                "Label",
                "Watches the microphone only (the call window can’t be read)."
            )
        );
        h.event(vcx, recording("s1"));
        let tree = vcx.update(a11y_tree::draw_and_read);
        assert!(tree.all("auto-stop-switch").is_empty());
        assert_eq!(
            tree.read(tree.find("auto-stop-summary")),
            ("Status", "Automatic stop needs a newer BroScribe Agent.")
        );
    }

    // ---- the panel as a nonactivating overlay (plan G) ----

    #[test]
    fn only_a_nonactivating_panel_becomes_a_hidden_popup() {
        assert_eq!(
            panel_window_kind(OverlayCapability::NonactivatingPanel),
            (WindowKind::PopUp, false, false)
        );
        assert_eq!(
            panel_window_kind(OverlayCapability::Standard),
            (WindowKind::Floating, true, false)
        );
        for reason in [PanelReason::Automatic, PanelReason::Explicit] {
            assert_eq!(
                existing_presentation(OverlayCapability::NonactivatingPanel, reason),
                ExistingPresentation::OrderFrontRegardless
            );
        }
        assert_eq!(
            existing_presentation(OverlayCapability::Standard, PanelReason::Automatic),
            ExistingPresentation::Leave
        );
        assert_eq!(
            existing_presentation(OverlayCapability::Standard, PanelReason::Explicit),
            ExistingPresentation::Activate
        );
    }

    struct Shown {
        panel: Option<PanelWindow>,
        overlay: Rc<dyn RecordingOverlay>,
        daemon: Entity<DaemonClient>,
        notifications: Entity<NotificationHost>,
        state: Entity<AppState>,
        starter: Entity<AgentStarter>,
        agent: Arc<FakeAgent>,
        _dir: tempfile::TempDir,
    }

    impl Shown {
        fn new(cx: &mut TestAppContext, overlay: Rc<dyn RecordingOverlay>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let (state, _lib, _s) = fixtures::state(cx, dir.path());
            let agent = FakeAgent::new();
            let transport: Arc<dyn Transport> = agent.clone();
            let daemon = cx.update(|cx| DaemonClient::detached(transport, cx));
            let notifications = cx.update(|cx| {
                NotificationHost::attach(
                    daemon.clone(),
                    Box::new(FakeNotifier::default()),
                    dir.path().join("data"),
                    cx,
                )
            });
            let starter = cx.new(|cx| AgentStarter::new(state.clone(), daemon.clone(), cx));
            Self {
                panel: None,
                overlay,
                daemon,
                notifications,
                state,
                starter,
                agent,
                _dir: dir,
            }
        }

        fn show(&mut self, cx: &mut TestAppContext, reason: PanelReason) {
            cx.update(|cx| {
                show(
                    &mut self.panel,
                    &self.overlay,
                    reason,
                    None,
                    &self.daemon,
                    &self.notifications,
                    &self.state,
                    &self.starter,
                    cx,
                )
            });
            cx.run_until_parked();
        }

        fn handle(&self) -> WindowHandle<Root> {
            self.panel.as_ref().unwrap().handle
        }

        fn mode(&self) -> OverlayCapability {
            self.panel.as_ref().unwrap().mode
        }

        fn close(&mut self, cx: &mut TestAppContext) {
            self.handle()
                .update(cx, |_, window, _| window.remove_window())
                .unwrap();
            cx.run_until_parked();
        }

        /// The window BroScribe activated, if any.
        fn active(&self, cx: &mut TestAppContext) -> Option<gpui::AnyWindowHandle> {
            cx.update(|cx| cx.active_window())
        }

        fn panel(&self) -> Entity<RecordingPanel> {
            self.panel
                .as_ref()
                .unwrap()
                .panel
                .upgrade()
                .expect("the open panel is alive")
        }

        /// Whether the panel last shown still exists (closing drops it).
        fn panel_alive(&self) -> bool {
            self.panel.as_ref().unwrap().panel.upgrade().is_some()
        }

        fn warning(&self, cx: &mut TestAppContext) -> Option<String> {
            self.panel()
                .read_with(cx, |panel, _| panel.overlay_warning.clone())
        }

        fn note(&self, cx: &mut TestAppContext) -> Option<String> {
            self.panel()
                .read_with(cx, |panel, _| panel.overlay_note.clone())
        }

        fn noticed(&self, cx: &mut TestAppContext) -> bool {
            self.notifications
                .read_with(cx, |host, _| host.fallbacks.contains_key(OVERLAY_NOTICE_ID))
        }
    }

    #[gpui::test]
    fn every_presentation_of_the_overlay_panel_avoids_activation(cx: &mut TestAppContext) {
        use broscribe_platform::fakes::{FakeRecordingOverlay, OverlayCall};
        let fake = Rc::new(FakeRecordingOverlay::new(
            OverlayCapability::NonactivatingPanel,
        ));
        let mut shown = Shown::new(cx, fake.clone());
        // A new recording: created hidden, presented by the overlay.
        shown.show(cx, PanelReason::Automatic);
        assert_eq!(fake.calls(), [OverlayCall::PresentNew]);
        let first = shown.handle();
        // Menu Open and the next new session reuse it, still nonactivating.
        shown.show(cx, PanelReason::Explicit);
        shown.show(cx, PanelReason::Automatic);
        assert_eq!(
            fake.calls(),
            [
                OverlayCall::PresentNew,
                OverlayCall::PresentExisting,
                OverlayCall::PresentExisting
            ]
        );
        assert_eq!(shown.handle(), first);
        assert_eq!(shown.active(cx), None, "nothing was activated");
        assert_eq!(shown.warning(cx), None);
        assert_eq!(shown.note(cx), None, "the overlay is available here");
        // Closing the panel drops it, releases its native state and stops
        // nothing, although the shell still remembers the closed window.
        shown.close(cx);
        assert!(!shown.panel_alive());
        assert_eq!(fake.calls().last(), Some(&OverlayCall::Released));
        assert!(
            shown.agent.calls().is_empty(),
            "closing never stops capture"
        );
        // The next presentation creates a new panel.
        shown.show(cx, PanelReason::Automatic);
        assert_eq!(fake.calls().last(), Some(&OverlayCall::PresentNew));
        assert_ne!(shown.handle(), first);
        assert_eq!(shown.active(cx), None);
    }

    #[gpui::test]
    fn a_failed_overlay_closes_the_hidden_popup_and_falls_back_without_focus(
        cx: &mut TestAppContext,
    ) {
        use broscribe_platform::fakes::{FakeRecordingOverlay, OverlayCall};
        use std::sync::atomic::Ordering;
        let fake = Rc::new(FakeRecordingOverlay::new(
            OverlayCapability::NonactivatingPanel,
        ));
        fake.fail_new.store(true, Ordering::SeqCst);
        let mut shown = Shown::new(cx, fake.clone());
        shown.show(cx, PanelReason::Automatic);
        assert_eq!(fake.calls(), [OverlayCall::PresentNew]);
        // The unconfigured popup is gone; one ordinary window replaces it.
        assert_eq!(cx.windows().len(), 1);
        assert_eq!(cx.windows(), [gpui::AnyWindowHandle::from(shown.handle())]);
        assert_eq!(shown.mode(), OverlayCapability::Standard);
        // Unfocused, and the failure is said, durably, in two places.
        assert_eq!(shown.active(cx), None);
        assert!(shown.warning(cx).is_some_and(|w| w
            == "Other apps can cover the recording controls, because they can't be kept on top. Recording isn't affected."));
        assert!(shown.noticed(cx));
        // Later automatic presentations still never focus it; the
        // overlay is not asked again for this ordinary window.
        shown.show(cx, PanelReason::Automatic);
        assert_eq!(shown.active(cx), None);
        assert_eq!(fake.calls(), [OverlayCall::PresentNew]);
        // The user's own request may.
        shown.show(cx, PanelReason::Command);
        assert_eq!(
            shown.active(cx),
            Some(gpui::AnyWindowHandle::from(shown.handle()))
        );
        assert!(shown.agent.calls().is_empty());
    }

    #[gpui::test]
    fn an_explicit_request_may_focus_the_fallback_window(cx: &mut TestAppContext) {
        use broscribe_platform::fakes::FakeRecordingOverlay;
        use std::sync::atomic::Ordering;
        let fake = Rc::new(FakeRecordingOverlay::new(
            OverlayCapability::NonactivatingPanel,
        ));
        fake.fail_new.store(true, Ordering::SeqCst);
        let mut shown = Shown::new(cx, fake.clone());
        shown.show(cx, PanelReason::Command);
        assert_eq!(cx.windows().len(), 1);
        assert_eq!(shown.mode(), OverlayCapability::Standard);
        assert_eq!(
            shown.active(cx),
            Some(gpui::AnyWindowHandle::from(shown.handle()))
        );
        assert!(shown.warning(cx).is_some());
        assert!(shown.noticed(cx));
    }

    #[gpui::test]
    fn a_failed_reshow_warns_and_never_activates(cx: &mut TestAppContext) {
        use broscribe_platform::fakes::{FakeRecordingOverlay, OverlayCall};
        use std::sync::atomic::Ordering;
        let fake = Rc::new(FakeRecordingOverlay::new(
            OverlayCapability::NonactivatingPanel,
        ));
        let mut shown = Shown::new(cx, fake.clone());
        shown.show(cx, PanelReason::Automatic);
        let first = shown.handle();
        fake.fail_existing.store(true, Ordering::SeqCst);
        shown.show(cx, PanelReason::Automatic);
        shown.show(cx, PanelReason::Explicit);
        assert_eq!(
            fake.calls(),
            [
                OverlayCall::PresentNew,
                OverlayCall::PresentExisting,
                OverlayCall::PresentExisting
            ]
        );
        // The configured panel stays; nothing is activated in its place.
        assert_eq!(shown.handle(), first);
        assert_eq!(shown.mode(), OverlayCapability::NonactivatingPanel);
        assert_eq!(cx.windows().len(), 1);
        assert_eq!(shown.active(cx), None);
        assert!(shown.warning(cx).is_some());
        assert!(shown.noticed(cx));
    }

    #[gpui::test]
    fn standard_platforms_keep_a_floating_window_and_activate_only_on_request(
        cx: &mut TestAppContext,
    ) {
        use broscribe_platform::fakes::FakeRecordingOverlay;
        let fake = Rc::new(FakeRecordingOverlay::new(OverlayCapability::Standard));
        let mut shown = Shown::new(cx, fake.clone());
        shown.show(cx, PanelReason::Automatic);
        let first = shown.handle();
        shown.show(cx, PanelReason::Automatic);
        assert_eq!(
            shown.active(cx),
            None,
            "automatic presentations never focus"
        );
        shown.show(cx, PanelReason::Explicit);
        assert_eq!(shown.handle(), first);
        assert_eq!(shown.active(cx), Some(gpui::AnyWindowHandle::from(first)));
        assert!(
            fake.calls().is_empty(),
            "the unsupported overlay is never asked to present"
        );
        assert_eq!(shown.warning(cx), None, "nothing failed: no warning");
        assert_eq!(
            shown.note(cx).as_deref(),
            Some(STANDARD_OVERLAY_NOTE),
            "the unavailable overlay is said in the panel"
        );
    }

    #[gpui::test]
    fn decorated_topmost_presentation_preserves_focus_until_the_user_requests_it(
        cx: &mut TestAppContext,
    ) {
        use broscribe_platform::fakes::{FakeRecordingOverlay, OverlayCall};
        assert_eq!(
            panel_window_kind(OverlayCapability::DecoratedTopmost),
            (WindowKind::Floating, false, false)
        );
        let fake = Rc::new(FakeRecordingOverlay::new(
            OverlayCapability::DecoratedTopmost,
        ));
        let mut shown = Shown::new(cx, fake.clone());
        let other = cx.add_window(|_, _| gpui::Empty);
        other
            .update(cx, |_, window, _| window.activate_window())
            .unwrap();
        shown.show(cx, PanelReason::Automatic);
        shown.show(cx, PanelReason::Automatic);
        assert_eq!(shown.active(cx), Some(other.into()));
        assert_eq!(
            fake.calls(),
            [OverlayCall::PresentNew, OverlayCall::PresentExisting]
        );
        shown.show(cx, PanelReason::Command);
        assert_eq!(shown.active(cx), Some(shown.handle().into()));
    }

    #[gpui::test]
    fn only_the_apps_own_command_requests_native_panel_focus_and_escape_returns_it(
        cx: &mut TestAppContext,
    ) {
        use broscribe_platform::fakes::{FakeRecordingOverlay, OverlayCall};
        let fake = Rc::new(FakeRecordingOverlay::new(
            OverlayCapability::NonactivatingPanel,
        ));
        let mut shown = Shown::new(cx, fake.clone());
        deliver(&shown.daemon, cx, recording("s1"));
        shown.show(cx, PanelReason::Automatic);
        shown.show(cx, PanelReason::Explicit);
        assert_eq!(
            fake.calls(),
            [OverlayCall::PresentNew, OverlayCall::PresentExisting]
        );
        shown.show(cx, PanelReason::Command);
        assert_eq!(fake.calls().last(), Some(&OverlayCall::FocusForUserRequest));
        let vcx = &mut gpui::VisualTestContext::from_window(shown.handle().into(), cx);
        vcx.update(|window, cx| window.simulate_next_frame(cx));
        let panel = shown.panel();
        assert!(vcx.update(|window, cx| panel.read(cx).first_control.is_focused(window)));
        vcx.simulate_keystrokes("escape");
        assert_eq!(fake.calls().last(), Some(&OverlayCall::ReturnUserFocus));
        assert!(shown.panel_alive());
        assert!(shown.agent.calls().is_empty());
    }

    #[gpui::test]
    fn screen_sharing_switch_follows_applied_state_and_saves_the_choice(cx: &mut TestAppContext) {
        use crate::ui::{a11y_tree, kit::dialog_gate::click};
        use broscribe_platform::fakes::{FakeRecordingOverlay, OverlayCall};
        use std::sync::atomic::Ordering;
        let fake = Rc::new(FakeRecordingOverlay::new(
            OverlayCapability::DecoratedTopmost,
        ));
        fake.capture_exclusion.store(true, Ordering::SeqCst);
        let mut shown = Shown::new(cx, fake.clone());
        deliver(&shown.daemon, cx, recording("s1"));
        shown.show(cx, PanelReason::Automatic);
        let vcx = &mut gpui::VisualTestContext::from_window(shown.handle().into(), cx);
        vcx.simulate_resize(size(px(480.), px(1600.)));
        shown.panel().update(vcx, |panel, cx| {
            panel.tab = RecordingPanelTab::Details;
            cx.notify();
        });
        let tree = vcx.update(a11y_tree::draw_and_read);
        assert_eq!(
            tree.read(tree.find("hide-from-sharing")),
            ("Switch", "Hide this window from screen sharing")
        );
        click(vcx, "hide-from-sharing", Role::Switch);
        assert_eq!(
            fake.calls(),
            [
                OverlayCall::PresentNew,
                OverlayCall::SetCaptureExcluded(true)
            ]
        );
        let paths = shown.state.read_with(vcx, |state, _| state.paths().clone());
        assert!(
            broscribe_core::config::Config::load(&paths.data_dir)
                .unwrap()
                .app
                .hide_controls_from_screen_sharing
        );
        fake.fail_capture_exclusion.store(true, Ordering::SeqCst);
        click(vcx, "hide-from-sharing", Role::Switch);
        assert_eq!(
            fake.calls().last(),
            Some(&OverlayCall::SetCaptureExcluded(false))
        );
        assert_eq!(
            shown
                .panel()
                .read_with(vcx, |panel, _| panel.exclusion_applied),
            Some(true)
        );
        assert!(
            !vcx.update(a11y_tree::draw_and_read)
                .all("capture-exclusion-error")
                .is_empty()
        );
        fake.fail_capture_exclusion.store(false, Ordering::SeqCst);
        shown.close(cx);
        shown.show(cx, PanelReason::Automatic);
        assert_eq!(
            &fake.calls()[fake.calls().len() - 2..],
            &[
                OverlayCall::SetCaptureExcluded(true),
                OverlayCall::PresentNew
            ]
        );

        let unsupported = Rc::new(FakeRecordingOverlay::new(OverlayCapability::Standard));
        let mut shown = Shown::new(cx, unsupported.clone());
        deliver(&shown.daemon, cx, recording("s2"));
        shown.show(cx, PanelReason::Automatic);
        shown.panel().update(cx, |panel, cx| {
            panel.tab = RecordingPanelTab::Details;
            cx.notify();
        });
        let tree = cx
            .update_window(shown.handle().into(), |_, window, cx| {
                a11y_tree::draw_and_read(window, cx)
            })
            .unwrap();
        assert!(tree.all("hide-from-sharing").is_empty());
        assert_eq!(
            tree.read(tree.find("capture-exclusion")),
            (
                "Status",
                "This system can’t hide windows from screen sharing."
            )
        );
        assert!(unsupported.calls().is_empty());
    }

    #[gpui::test]
    fn floating_panel_uses_the_linux_placement_policy_when_the_strip_fits(cx: &mut TestAppContext) {
        use broscribe_platform::fakes::FakeRecordingOverlay;
        use std::sync::atomic::Ordering;
        let overlay = FakeRecordingOverlay::new(OverlayCapability::Standard);
        overlay.beside_main_window.store(true, Ordering::SeqCst);
        cx.update(|cx| {
            let visible = cx.primary_display().unwrap().visible_bounds();
            let inset = px(OVERLAY_INSET as f32);
            let right = visible.right() - inset;
            let old = standard_bounds(PANEL_SIZE, &overlay, None, cx);
            let main = |available| {
                Bounds::new(
                    visible.origin,
                    size(
                        visible.size.width - inset - px(available),
                        visible.size.height,
                    ),
                )
            };
            let placed = standard_bounds(PANEL_SIZE, &overlay, Some(main(380.)), cx);
            assert_eq!(placed.left(), main(380.).right());
            assert_eq!(placed.right(), right);
            assert_eq!(placed.size.width, px(380.));
            assert_eq!(
                standard_bounds(PANEL_SIZE, &overlay, Some(main(280.)), cx),
                old
            );
        });
    }

    // ---- remembered frame (PNL-56) and Dock presence (SHL-15) ----

    /// The test display's key, as the window state file stores it.
    fn display_key(cx: &mut TestAppContext) -> String {
        cx.update(|cx| crate::ui_state::displays(cx)[0].0.key.clone())
    }

    fn panel_bounds(shown: &Shown, cx: &mut TestAppContext) -> gpui::Bounds<gpui::Pixels> {
        shown
            .handle()
            .update(cx, |_, window, _| window.bounds())
            .unwrap()
    }

    #[gpui::test]
    fn the_recording_controls_return_to_their_frame_on_a_present_display(cx: &mut TestAppContext) {
        use crate::ui_state::{FILE_NAME, Frame, UiState};
        use broscribe_platform::fakes::FakeRecordingOverlay;

        let data = tempfile::tempdir().unwrap();
        let fake = Rc::new(FakeRecordingOverlay::new(
            OverlayCapability::NonactivatingPanel,
        ));
        let mut shown = Shown::new(cx, fake.clone());
        cx.update(|cx| crate::ui_state::install(data.path(), cx));
        // Nothing remembered: today's placement. The native minimum leaves
        // room for compact mode and the expanded view enforces its own floor.
        let (options, placement) = cx.update(|cx| {
            panel_window_options(
                OverlayCapability::NonactivatingPanel,
                Default::default(),
                shown.overlay.as_ref(),
                None,
                cx,
            )
        });
        assert_eq!(placement, OverlayPlacement::RightEdge);
        assert_eq!(options.window_min_size, Some(size(px(360.), px(160.))));
        assert!(!options.is_minimizable);
        assert_eq!(options.display_id, None);
        shown.show(cx, PanelReason::Automatic);
        assert_eq!(fake.placements(), [OverlayPlacement::RightEdge]);

        // The user resizes it; once it settles, its frame is written for
        // this display.
        let left = panel_bounds(&shown, cx).origin;
        cx.simulate_window_resize(shown.handle().into(), size(px(480.), px(720.)));
        cx.run_until_parked();
        let path = data.path().join(FILE_NAME);
        assert!(!path.exists(), "not while it may still move");
        cx.executor().advance_clock(crate::ui_state::PANEL_SETTLE);
        cx.run_until_parked();
        let key = display_key(cx);
        let saved = UiState::load(&path).recording_controls;
        let expected = Frame::new(f32::from(left.x), f32::from(left.y), 480., 720.);
        assert_eq!(saved.frames.get(&key), Some(&expected));
        assert_eq!(saved.last_display.as_deref(), Some(key.as_str()));

        // Closed and shown again: where it was left, placed as opened.
        shown.close(cx);
        shown.show(cx, PanelReason::Automatic);
        assert_eq!(
            fake.placements(),
            [OverlayPlacement::RightEdge, OverlayPlacement::AsOpened]
        );
        assert_eq!(panel_bounds(&shown, cx), expected.bounds());
        assert_eq!(shown.active(cx), None, "still never focused");
        assert!(shown.agent.calls().is_empty(), "nothing reached the agent");
        shown.close(cx);

        // Next launch: its frame is for a display that is gone, or barely
        // on this one. Today's placement then.
        for frames in [
            r#"{"uuid:synthetic-gone":{"x":100,"y":100,"width":420,"height":680}}"#.to_owned(),
            format!(r#"{{"{key}":{{"x":1821,"y":100,"width":420,"height":680}}}}"#),
        ] {
            std::fs::write(
                &path,
                format!(r#"{{"recording_controls":{{"frames":{frames}}}}}"#),
            )
            .unwrap();
            cx.update(|cx| crate::ui_state::install(data.path(), cx));
            shown.show(cx, PanelReason::Automatic);
            assert_eq!(fake.placements().last(), Some(&OverlayPlacement::RightEdge));
            shown.close(cx);
        }

        // Elsewhere the floating window opens there too, at least 360×440
        // and at most half the display's width.
        std::fs::write(
            &path,
            format!(
                r#"{{"recording_controls":{{"frames":{{"{key}":{{"x":200,"y":100,"width":200,"height":1500}}}}}}}}"#
            ),
        )
        .unwrap();
        cx.update(|cx| crate::ui_state::install(data.path(), cx));
        let standard = Rc::new(FakeRecordingOverlay::new(OverlayCapability::Standard));
        let mut floating = Shown::new(cx, standard);
        floating.show(cx, PanelReason::Automatic);
        assert_eq!(
            panel_bounds(&floating, cx),
            Frame::new(200., 100., 360., 1080.).bounds()
        );
    }

    #[gpui::test]
    fn an_accessory_launch_is_in_the_dock_only_while_a_regular_window_is_open(
        cx: &mut TestAppContext,
    ) {
        use broscribe_platform::ActivationPolicy::{Accessory, Regular};
        use broscribe_platform::about::FakeAboutReader;
        use broscribe_platform::fakes::{
            ActivationCall, FakeActivation, FakeMicCapture, FakeRecordingOverlay, FakeSystemInfo,
        };

        let fake = Rc::new(FakeRecordingOverlay::new(
            OverlayCapability::NonactivatingPanel,
        ));
        let mut shown = Shown::new(cx, fake);
        shown.state.update(cx, |state, cx| {
            let mut config = state.config().clone();
            config
                .setup
                .completed_steps
                .push(crate::views::setup::ASSISTANT_DONE.into());
            state.set_config(config, cx);
        });
        let deps = crate::views::setup::SetupDeps {
            mic: Arc::new(FakeMicCapture::new(16_000)),
            system: Arc::new(FakeSystemInfo::default()),
        };
        let activation = Rc::new(FakeActivation::new());
        // The agent's launch for a notification: no Dock icon.
        cx.update(|cx| crate::ui::windows::install_presence(true, false, activation.clone(), cx));
        assert_eq!(activation.calls(), [ActivationCall::Policy(Accessory)]);
        activation.clear();

        // The recording controls, shown automatically or on request, never
        // change it.
        shown.show(cx, PanelReason::Automatic);
        shown.show(cx, PanelReason::Explicit);
        assert!(activation.calls().is_empty());

        // The main window, opened on purpose: Regular before activation.
        let mut main = None;
        cx.update(|cx| crate::ui::show_main_window(&mut main, &shown.state, &deps, cx));
        assert!(main.is_some());
        assert_eq!(
            activation.calls(),
            [ActivationCall::Policy(Regular), ActivationCall::Activate]
        );
        // About as well: already Regular, it only activates.
        activation.clear();
        let about = cx
            .update(|cx| {
                crate::ui::windows::activate_for_regular_window(cx);
                crate::views::about::open_about_window(
                    Arc::new(FakeAboutReader::failing("synthetic")),
                    None,
                    None,
                    cx,
                )
            })
            .unwrap();
        assert_eq!(activation.calls(), [ActivationCall::Activate]);
        activation.clear();

        // Closing the controls or one of two regular windows changes nothing.
        shown.close(cx);
        main.unwrap()
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        assert!(activation.calls().is_empty());
        // The last regular window closes: back to accessory.
        about
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        assert_eq!(activation.calls(), [ActivationCall::Policy(Accessory)]);
        activation.clear();
        // The controls still open as before, without changing it.
        shown.show(cx, PanelReason::Explicit);
        assert!(activation.calls().is_empty());

        // A launch by the user stays Regular when its windows close.
        let normal = Rc::new(FakeActivation::new());
        cx.update(|cx| crate::ui::windows::install_presence(false, false, normal.clone(), cx));
        let mut main = None;
        cx.update(|cx| crate::ui::show_main_window(&mut main, &shown.state, &deps, cx));
        main.unwrap()
            .update(cx, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
        assert_eq!(normal.calls(), [ActivationCall::Activate]);
    }

    // ---- tag answers vs snapshots (RT02) ----

    fn recording_tags(session: &str, tags: &[&str]) -> Event {
        let Event::State(mut state) = recording(session) else {
            unreachable!()
        };
        state.recording.as_mut().unwrap().tags = tags.iter().map(|t| (*t).to_owned()).collect();
        Event::State(state)
    }

    #[gpui::test]
    fn a_late_tag_answer_never_replaces_a_newer_tag_snapshot(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        h.event(cx, recording("s1"));
        h.agent.answer(|_| Ok(json!({"tags": ["kept", "alpha"]})));
        h.type_into(cx, |v| v.tag.clone(), "alpha");
        // Sent; before its answer is applied, a snapshot already carries a
        // later change (beta, added elsewhere after alpha).
        h.window
            .update(cx, |view, window, cx| view.submit_tags(window, cx))
            .unwrap();
        h.daemon.update(cx, |d, cx| {
            d.received(recording_tags("s1", &["kept", "alpha", "beta"]), cx)
        });
        cx.run_until_parked();
        assert!(matches!(
            h.status(cx, Control::Tag),
            Some(ControlStatus::Done { .. })
        ));
        assert_eq!(
            h.value(cx, |v| v.tag.clone()),
            "",
            "the accepted draft clears"
        );
        let tags = h.window.update(cx, |v, _, cx| v.shown_tags(cx)).unwrap();
        assert_eq!(tags, ["kept", "alpha", "beta"], "the newer snapshot stays");
        // With no newer snapshot, the answer shows at once.
        h.agent
            .answer(|_| Ok(json!({"tags": ["kept", "alpha", "beta", "gamma"]})));
        h.type_into(cx, |v| v.tag.clone(), "gamma");
        h.act(cx, |view, window, cx| view.submit_tags(window, cx));
        let tags = h.window.update(cx, |v, _, cx| v.shown_tags(cx)).unwrap();
        assert_eq!(tags, ["kept", "alpha", "beta", "gamma"]);
    }

    // ---- the panel after the capture ends (USR02) ----

    fn stopped(session: &str) -> Event {
        daemon_state(
            None,
            json!([stopped_entry(
                session,
                Some(&format!("Synthetic {session}"))
            )]),
            json!([]),
        )
    }

    fn saved(session: &str) -> Event {
        daemon_state(
            None,
            json!([]),
            json!([{ "session_id": session, "meeting_id": "m-synthetic", "title": null,
                     "saved_at": "2026-11-14T11:00:00Z" }]),
        )
    }

    fn deliver(daemon: &Entity<DaemonClient>, cx: &mut TestAppContext, event: Event) {
        daemon.update(cx, |d, cx| d.received(event, cx));
        cx.run_until_parked();
    }

    #[gpui::test]
    fn the_panel_closes_when_capture_ends_never_on_a_warning_or_for_a_newer_recording(
        cx: &mut TestAppContext,
    ) {
        use broscribe_platform::fakes::FakeRecordingOverlay;
        let fake = Rc::new(FakeRecordingOverlay::new(OverlayCapability::Standard));
        let mut shown = Shown::new(cx, fake);
        deliver(&shown.daemon, cx, recording("s1"));
        shown.show(cx, PanelReason::Automatic);
        // A pending automatic stop only warns: capture goes on, so does the
        // panel.
        deliver(&shown.daemon, cx, recording_auto("s1", pending_stop(3)));
        assert!(shown.panel_alive());
        // A lost connection, or a state without the recording that does not
        // list it as stopped or saved, is not known to be an end.
        shown.daemon.update(cx, |d, cx| d.lost(None, cx));
        deliver(&shown.daemon, cx, daemon_state(None, json!([]), json!([])));
        assert!(shown.panel_alive());
        // Listed as stopped: capture ended. The panel closes; the final pass
        // is the agent's, and closing sent nothing.
        deliver(&shown.daemon, cx, stopped("s1"));
        assert!(!shown.panel_alive());
        assert!(shown.agent.calls().is_empty());
        // Asked for again (menu Open), it shows that recording and stays,
        // also once it is saved.
        shown.show(cx, PanelReason::Explicit);
        assert!(shown.panel_alive());
        deliver(&shown.daemon, cx, saved("s1"));
        assert!(shown.panel_alive());
        shown.close(cx);

        // "Stop and save" for s2 is still in flight when s2 ends and s3
        // starts: its late answer never closes the panel of s3.
        deliver(&shown.daemon, cx, recording("s2"));
        shown.show(cx, PanelReason::Automatic);
        let panel = shown.panel();
        shown
            .handle()
            .update(cx, |_, window, cx| {
                panel.update(cx, |p, cx| {
                    p.session_action(SessionCommand::Stop, window, cx)
                })
            })
            .unwrap();
        shown
            .daemon
            .update(cx, |d, cx| d.received(stopped("s2"), cx));
        shown
            .daemon
            .update(cx, |d, cx| d.received(recording("s3"), cx));
        cx.run_until_parked();
        assert!(shown.panel_alive());
        assert!(matches!(
            &shown.agent.calls()[..],
            [Call::RecordStop { session_id }] if session_id == "s2"
        ));
        assert_eq!(
            panel.read_with(cx, |p, cx| p.model(cx).active_session().map(str::to_owned)),
            Some("s3".to_owned())
        );
        // s3 ends: the old answer is no use of the panel, so it closes.
        drop(panel);
        deliver(&shown.daemon, cx, stopped("s3"));
        assert!(!shown.panel_alive());
    }

    #[gpui::test]
    fn an_ended_capture_keeps_the_panel_only_while_the_user_is_using_it(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        h.event(cx, recording("s1"));
        // A title typed, not saved, when the capture ends: the panel and the
        // draft stay.
        h.type_into(cx, title_of("s1"), "Synthetic retitled");
        h.event(cx, stopped("s1"));
        assert!(h.open_now(cx));
        assert_eq!(h.value(cx, title_of("s1")), "Synthetic retitled");
        // Saved after the end: the user works here, the result stays.
        h.agent.answer(|call| match call {
            Call::RecordTitle { title, .. } => Ok(json!({
                "status": "saved", "session_id": "s1", "title": title
            })),
            _ => Ok(json!({})),
        });
        h.act(cx, |view, window, cx| view.submit_title("s1", window, cx));
        assert!(h.open_now(cx));
        assert!(matches!(
            h.status(cx, Control::Title),
            Some(ControlStatus::Done { text, .. }) if text.starts_with("Title saved")
        ));
        // The next recording reuses the panel. A "Discard" confirmation
        // open when it ends keeps it; cancelling it, with nothing else left,
        // closes it.
        h.event(cx, recording("s2"));
        h.act(cx, |view, _, cx| view.open_discard("s2".into(), cx));
        h.event(cx, stopped("s2"));
        assert!(h.open_now(cx));
        assert_eq!(
            h.window
                .update(cx, |v, _, _| v.discard_target().map(str::to_owned))
                .unwrap()
                .as_deref(),
            Some("s2")
        );
        h.act(cx, |view, _, cx| view.cancel_discard(cx));
        assert!(!h.open_now(cx));
        assert!(
            h.agent
                .calls()
                .iter()
                .all(|c| !matches!(c, Call::RecordStop { .. } | Call::RecordDiscard { .. })),
            "closing stops and discards nothing"
        );
    }

    #[gpui::test]
    fn unsent_note_tag_and_participant_drafts_each_hold_an_ended_panel_open(
        cx: &mut TestAppContext,
    ) {
        for draft in ["note", "tag", "participant"] {
            let h = Harness::new(cx);
            h.event(cx, recording("s1"));
            h.act(cx, |panel, window, cx| match draft {
                "note" => panel.note.update(cx, |input, cx| {
                    input.set_value("Synthetic note", window, cx)
                }),
                "tag" => panel
                    .tag
                    .update(cx, |input, cx| input.set_value("synthetic", window, cx)),
                _ => panel.picker.update(cx, |picker, cx| {
                    picker.set_query("Synthetic Person", window, cx)
                }),
            });
            h.event(cx, stopped("s1"));
            assert!(h.open_now(cx), "{draft}");
            h.act(cx, RecordingPanel::clear_unsent_drafts);
            assert!(!h.open_now(cx), "{draft}");
            assert!(h.agent.calls().is_empty());
        }
    }

    #[gpui::test]
    fn participant_rows_expose_a_separate_remove_and_open_the_picker_on_demand(
        cx: &mut TestAppContext,
    ) {
        use crate::ui::{a11y_tree, kit::dialog_gate::click};
        let h = Harness::new(cx);
        h.agent.answer(|call| match call {
            Call::RecordParticipant {
                action: ParticipantAction::Remove,
                ..
            } => Ok(json!({"status": "removed"})),
            _ => Ok(json!({})),
        });
        h.event(
            cx,
            recording_with("s1", |recording| {
                recording.observed_roster = vec![member("Synthetic Seen", None)];
            }),
        );
        h.act(cx, |panel, _, cx| {
            panel.confirmed.insert(
                "p-1".into(),
                RosterPerson {
                    name: "Synthetic Confirmed".into(),
                    emails: vec![],
                },
            );
            panel.tab = RecordingPanelTab::Details;
            cx.notify();
        });
        h.daemon.update(cx, |daemon, cx| {
            daemon
                .panel
                .people
                .insert("p-live".into(), "Synthetic Live".into());
            cx.notify();
        });
        let vcx = &mut gpui::VisualTestContext::from_window(h.window.into(), cx);
        vcx.simulate_resize(size(px(420.), px(1600.)));
        let tree = vcx.update(a11y_tree::draw_and_read);
        let list = tree.find("panel-people");
        assert_eq!(tree.read(list), ("List", "Recording participants"));
        assert_eq!(
            tree.read(tree.find("remove-p-1")),
            ("Button", "Remove Synthetic Confirmed from participants")
        );
        assert!(tree.all("person-picker").is_empty());
        let rows = tree.subtree(list);
        for (role, text) in [
            ("Label", "Synthetic Confirmed"),
            ("Status", "attended"),
            ("Label", "seen in the call · not confirmed"),
            ("Label", "named live · not confirmed"),
        ] {
            assert!(
                rows.iter().any(|&ix| tree.read(ix) == (role, text)),
                "{text}"
            );
        }
        click(vcx, "confirm-observed-person-0", Role::Button);
        assert!(h.agent.calls().is_empty());
        assert!(
            h.window
                .update(vcx, |panel, window, cx| {
                    let picker = panel.picker.read(cx);
                    picker.query() == "Synthetic Seen" && picker.search_focused(window, cx)
                })
                .unwrap()
        );
        let tree = vcx.update(a11y_tree::draw_and_read);
        let heading = tree.find("add-participant-title");
        let picker = tree.find("person-picker");
        assert_eq!(tree.nodes[heading].parent, tree.nodes[picker].parent);
        assert_eq!(tree.read(picker), ("Group", "Add a participant"));
        vcx.simulate_keystrokes("escape");
        assert!(
            vcx.update(a11y_tree::draw_and_read)
                .all("person-picker")
                .is_empty()
        );
        click(vcx, "remove-p-1", Role::Button);
        assert!(
            matches!(&h.agent.calls()[..], [Call::RecordParticipant { session_id, action: ParticipantAction::Remove, .. }] if session_id == "s1")
        );
    }

    #[gpui::test]
    fn note_enter_and_bookmark_click_use_the_agent_timestamp(cx: &mut TestAppContext) {
        use crate::ui::{a11y_tree, kit::dialog_gate::click};
        use gpui::Focusable as _;
        let h = Harness::new(cx);
        h.agent.answer(|_| Ok(json!({"at_s": 763})));
        h.event(cx, recording("s1"));
        let vcx = &mut gpui::VisualTestContext::from_window(h.window.into(), cx);
        vcx.simulate_resize(size(px(420.), px(850.)));
        let tree = vcx.update(a11y_tree::draw_and_read);
        assert!(
            tree.nodes
                .iter()
                .any(|node| node.role == "TextInput" && node.name == "Note")
        );
        assert!(tree.all("panel-note").is_empty());
        h.type_into(vcx, |panel| panel.note.clone(), "Synthetic note");
        h.act(vcx, |panel, window, cx| {
            panel.note.read(cx).focus_handle(cx).focus(window, cx)
        });
        vcx.simulate_keystrokes("enter");
        assert_eq!(
            h.status(vcx, Control::Note),
            Some(ControlStatus::done("Note added at 12:43."))
        );
        h.type_into(vcx, |panel| panel.note.clone(), "Synthetic mark");
        click(vcx, "panel-bookmark", Role::Button);
        assert_eq!(
            h.status(vcx, Control::Note),
            Some(ControlStatus::done("Bookmark added at 12:43."))
        );
        assert!(
            matches!(&h.agent.calls()[..], [Call::RecordNote { session_id, text }, Call::RecordBookmark { session_id: mark_session, label: Some(mark) }] if session_id == "s1" && mark_session == "s1" && text == "Synthetic note" && mark == "Synthetic mark")
        );
    }

    // ---- the live transcript list (USR03) ----

    #[gpui::test]
    fn live_transcript_keyboard_actions_and_picker_have_independent_accessible_names(
        cx: &mut TestAppContext,
    ) {
        use crate::ui::{a11y_tree, kit::dialog_gate::click};
        let h = Harness::new(cx);
        live_agent(&h, |_| Ok(json!({})));
        h.event(cx, recording("s1"));
        let vcx = &mut gpui::VisualTestContext::from_window(h.window.into(), cx);
        vcx.simulate_resize(size(px(420.), px(820.)));
        let tree = vcx.update(a11y_tree::draw_and_read);
        for (id, expected) in [
            (
                "live-clear-mic-0",
                "Remove your name from the line at 0 seconds",
            ),
            (
                "live-confirm-system-16000",
                "Accept Synthetic Ana for the line at 1 second",
            ),
            (
                "live-name-system-16000",
                "Name the speaker of the line at 1 second",
            ),
        ] {
            assert_eq!(tree.read(tree.find(id)), ("Button", expected));
        }
        assert!(tree.all("live-explanation").is_empty());
        click(vcx, "live-names-info-button", Role::Button);
        let tree = vcx.update(a11y_tree::draw_and_read);
        assert_eq!(
            tree.read(tree.find("live-explanation")),
            ("Label", LIVE_EXPLANATION)
        );
        vcx.simulate_keystrokes("escape");
        h.act(vcx, |panel, window, cx| {
            panel.transcript_focus.focus(window, cx)
        });
        vcx.simulate_keystrokes("home down");
        let selected = |vcx: &mut gpui::VisualTestContext| {
            h.window
                .update(vcx, |panel, _, _| panel.transcript_selected.clone())
                .unwrap()
        };
        assert_eq!(selected(vcx), Some(RowKey::Turn("system-16000".into())));
        vcx.simulate_keystrokes("up");
        assert_eq!(selected(vcx), Some(RowKey::Turn("mic-0".into())));
        vcx.simulate_keystrokes("end");
        assert_eq!(selected(vcx), Some(RowKey::Turn("system-16000".into())));
        vcx.simulate_keystrokes("shift-f10");
        assert!(
            h.window
                .update(vcx, |panel, _, _| panel.transcript_menu.is_some())
                .unwrap()
        );
        vcx.simulate_keystrokes("escape");
        assert!(
            h.window
                .update(vcx, |panel, window, _| panel
                    .transcript_focus
                    .is_focused(window))
                .unwrap()
        );
        vcx.simulate_keystrokes("n");
        let tree = vcx.update(a11y_tree::draw_and_read);
        let heading = tree.find("live-naming-title");
        let picker = tree.find("person-picker");
        assert_eq!(tree.nodes[heading].parent, tree.nodes[picker].parent);
        assert_eq!(
            tree.read(heading),
            ("Heading", "Name the speaker at 0:01 (Others)")
        );
        assert_eq!(
            tree.read(picker),
            ("Group", "Name the speaker at 0:01 (Others)")
        );
        assert!(
            vcx.debug_bounds("live-transcript").unwrap().bottom()
                <= vcx.debug_bounds("live-naming-dock").unwrap().top()
        );
        vcx.simulate_keystrokes("escape");
        assert!(
            h.window
                .update(vcx, |panel, window, _| panel.live_naming.is_none()
                    && panel.transcript_focus.is_focused(window))
                .unwrap()
        );
        vcx.update(|window, cx| window.focus_next(cx));
        assert!(
            h.window
                .update(vcx, |panel, window, cx| !panel
                    .transcript_focus
                    .contains_focused(window, cx))
                .unwrap()
        );
    }

    #[gpui::test]
    fn live_transcript_renders_empty_preview_and_accepted_marks_in_time_order(
        cx: &mut TestAppContext,
    ) {
        use crate::ui::a11y_tree;
        let h = Harness::new(cx);
        h.agent.answer(|call| match call {
            Call::RecordNote { .. } => Ok(json!({"at_s": 5})),
            Call::RecordBookmark { .. } => Ok(json!({"at_s": 2})),
            _ => Ok(json!({})),
        });
        h.event(cx, recording("s1"));
        let vcx = &mut gpui::VisualTestContext::from_window(h.window.into(), cx);
        vcx.simulate_resize(size(px(420.), px(900.)));
        h.event(
            vcx,
            Event::Preview {
                session_id: "s1".into(),
                preview: PreviewState::Off,
            },
        );
        let tree = vcx.update(a11y_tree::draw_and_read);
        assert_eq!(
            tree.read(tree.find("live-transcript-empty")),
            (
                "Status",
                "Live transcript is off for this recording. The full transcript is made after the meeting."
            )
        );
        h.event(
            vcx,
            Event::PreviewGap {
                session_id: "s1".into(),
                track: Track::System,
                from_s: 3.,
                to_s: 4.,
            },
        );
        h.type_into(vcx, |panel| panel.note.clone(), "Synthetic recap");
        h.act(vcx, |panel, window, cx| panel.submit_note(window, cx));
        h.act(vcx, |panel, window, cx| panel.bookmark(window, cx));
        let tree = vcx.update(a11y_tree::draw_and_read);
        let transcript = tree.subtree(tree.find("live-transcript"));
        let rows: Vec<_> = transcript
            .into_iter()
            .filter_map(|ix| {
                (tree.nodes[ix].role == "ListItem").then_some(tree.nodes[ix].name.as_str())
            })
            .collect();
        assert_eq!(
            rows,
            [
                "Bookmark at 2 seconds: ",
                "Others from 3 seconds to 4 seconds. Not shown live. The recording has this part.",
                "Note at 5 seconds: Synthetic recap"
            ]
        );
        h.event(vcx, recording("s2"));
        assert!(
            vcx.update(a11y_tree::draw_and_read)
                .all("live-mark-1")
                .is_empty()
        );
    }

    #[gpui::test]
    fn live_transcript_badge_fits_at_largest_text_in_a_narrow_panel(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        h.event(cx, recording("s1"));
        let vcx = &mut gpui::VisualTestContext::from_window(h.window.into(), cx);
        vcx.update(|_, cx| {
            crate::ui::apply_reading_size(broscribe_core::config::ReadingSize::Largest, cx)
        });
        vcx.simulate_resize(size(px(360.), px(900.)));
        let name = "Synthetic Longname ".repeat(12);
        h.event(vcx, live_event(live_turn("mic-0", 1, "suggested", json!({"voice": 2, "suggestion": {
            "person": {"person_id": "p1", "name": name}, "source": "session", "uncalibrated": true
        }}))));
        let row = vcx.debug_bounds("live-turn-mic-0").unwrap();
        let badge = vcx.debug_bounds("live-row-badge").unwrap();
        assert!(row.left() <= badge.left() && badge.right() <= row.right());
    }

    #[test]
    fn transcript_rows_keep_the_view_on_the_same_row_when_others_come_and_go() {
        use gpui::ListOffset;
        let turn = |id: &str, revision: u64| TranscriptRow {
            key: RowKey::Turn(id.into()),
            revision,
            naming: false,
            selected: false,
            hovered: false,
            status: None,
        };
        let list = ListState::new(0, ListAlignment::Top, px(100.));
        let rows: Vec<_> = ["a", "b", "c", "d", "e"]
            .into_iter()
            .map(|id| turn(id, 1))
            .collect();
        sync_rows(&list, &[], &rows);
        assert_eq!(list.item_count(), 5);
        list.scroll_to(ListOffset {
            item_ix: 2,
            offset_in_item: px(7.),
        });
        // The two oldest dropped, one row inserted after "c", two added at
        // the end, and "c" itself revised.
        let next = vec![
            turn("c", 2),
            turn("x", 1),
            turn("d", 1),
            turn("e", 1),
            turn("f", 1),
            turn("g", 1),
        ];
        sync_rows(&list, &rows, &next);
        assert_eq!(list.item_count(), 6);
        assert_eq!(
            list.logical_scroll_top(),
            ListOffset {
                item_ix: 0,
                offset_in_item: px(7.)
            },
            "still on c, at the same place"
        );
        // The row at the top dropped: the view starts at the row after it.
        sync_rows(&list, &next, &next[1..]);
        assert_eq!(list.item_count(), 5);
        assert_eq!(
            list.logical_scroll_top(),
            ListOffset {
                item_ix: 0,
                offset_in_item: px(0.)
            }
        );
    }

    /// Whether the top of the row with debug selector `row` (`live-turn-…`)
    /// is inside the transcript's view.
    fn in_view(cx: &mut gpui::VisualTestContext, row: &'static str) -> bool {
        let list = cx
            .debug_bounds("live-transcript")
            .expect("the transcript is shown");
        cx.debug_bounds(row)
            .is_some_and(|row| row.top() >= list.top() && row.top() < list.bottom())
    }

    #[gpui::test]
    fn both_panel_viewports_mount_scrollbars_and_dragging_resumes_follow_at_the_end(
        cx: &mut TestAppContext,
    ) {
        use gpui::{Modifiers, MouseButton, VisualTestContext, point};
        let h = Harness::new(cx);
        h.event(cx, recording("s1"));
        let vcx = &mut VisualTestContext::from_window(h.window.into(), cx);
        vcx.simulate_resize(size(px(420.), px(610.)));
        for number in 0..30 {
            h.event(
                vcx,
                live_event(live_turn(
                    &format!("mic-{}", number * 16000),
                    1,
                    "preview",
                    json!({}),
                )),
            );
        }
        let list = h
            .window
            .update(vcx, |panel, _, _| panel.transcript.clone())
            .unwrap();
        let bar = vcx
            .debug_bounds("scrollbar-overlay")
            .expect("transcript scrollbar");
        assert!(list.max_offset_for_scrollbar().y > px(0.));
        let x = bar.right() - px(7.);
        let bottom = point(x, bar.bottom() - px(9.));
        let center = point(x, bar.center().y);
        vcx.simulate_mouse_move(bottom, None, Modifiers::none());
        vcx.simulate_mouse_down(bottom, MouseButton::Left, Modifiers::none());
        assert!(list.is_scrollbar_dragging());
        vcx.simulate_mouse_move(center, MouseButton::Left, Modifiers::none());
        assert!(!list.is_following_tail());
        vcx.simulate_mouse_move(bottom, MouseButton::Left, Modifiers::none());
        vcx.simulate_mouse_up(bottom, MouseButton::Left, Modifiers::none());
        assert!(!list.is_scrollbar_dragging());
        assert!(list.is_following_tail());
        h.act(vcx, |panel, _, cx| {
            panel.tab = RecordingPanelTab::Details;
            cx.notify();
        });
        assert!(
            vcx.debug_bounds("scrollbar-overlay").is_some(),
            "Details scrollbar"
        );
    }

    #[gpui::test]
    fn live_transcript_follows_new_lines_and_holds_still_for_reading_and_naming(
        cx: &mut TestAppContext,
    ) {
        use crate::views::person_picker::TypedName;
        use gpui::{ScrollDelta, ScrollEvent, VisualTestContext, point};
        let h = Harness::new(cx);
        live_agent(&h, |_| Err(refusal("synthetic refusal (stale_revision)")));
        h.event(cx, recording("s1"));
        let mut vcx = VisualTestContext::from_window(h.window.into(), cx);
        vcx.simulate_resize(size(px(420.), px(900.)));
        let id = |n: u64| format!("mic-{}", 64_000 * (n + 1));
        let line = |n: u64| live_turn(&id(n), 1, "preview", json!({}));
        for n in 0..40 {
            h.event(&mut vcx, live_event(line(n)));
        }
        let list = h
            .window
            .update(&mut vcx, |v, _, _| v.transcript.clone())
            .unwrap();
        // The newest line is in view, and each new one comes into view.
        assert!(list.is_following_tail());
        assert!(in_view(&mut vcx, "live-turn-mic-2560000"));
        assert!(!in_view(&mut vcx, "live-turn-mic-64000"));
        h.event(&mut vcx, live_event(line(40)));
        assert!(in_view(&mut vcx, "live-turn-mic-2624000"));

        // Scrolled up to read earlier lines: new lines never move the view.
        let center = vcx.debug_bounds("live-transcript").unwrap().center();
        let wheel = |dy: f32| ScrollEvent {
            position: center,
            delta: ScrollDelta::Pixels(point(px(0.), px(dy))),
            ..Default::default()
        };
        vcx.simulate_event(wheel(300.));
        assert!(!list.is_following_tail());
        let tree = vcx.update(crate::ui::a11y_tree::draw_and_read);
        assert_eq!(
            tree.read(tree.find("live-show-latest")),
            ("Button", "Show latest")
        );
        let reading = list.logical_scroll_top();
        for n in 41..45 {
            h.event(&mut vcx, live_event(line(n)));
        }
        assert_eq!(list.logical_scroll_top(), reading);
        assert!(!in_view(&mut vcx, "live-turn-mic-2880000"));
        // Back at the bottom, it follows again.
        crate::ui::kit::dialog_gate::click(&mut vcx, "live-show-latest", Role::Button);
        h.event(&mut vcx, live_event(line(45)));
        assert!(list.is_following_tail());
        assert!(in_view(&mut vcx, "live-turn-mic-2944000"));

        // Naming a line in view: nothing scrolls while the picker is open,
        // and the row, the typed name and the focus stay.
        let named = id(44);
        assert!(in_view(&mut vcx, "live-turn-mic-2880000"));
        h.act(&mut vcx, |view, window, cx| {
            view.open_live_naming(&named, window, cx)
        });
        let picker = h
            .window
            .update(&mut vcx, |v, _, _| v.live_picker().cloned())
            .unwrap()
            .expect("the picker is open");
        h.act(&mut vcx, |_, window, cx| {
            picker.update(cx, |p, cx| p.set_query("Synthetic Newcomer", window, cx))
        });
        assert!(!list.is_following_tail());
        let held = list.logical_scroll_top();
        for n in 46..50 {
            h.event(&mut vcx, live_event(line(n)));
        }
        assert_eq!(list.logical_scroll_top(), held);
        assert!(in_view(&mut vcx, "live-turn-mic-2880000"));
        assert!(!in_view(&mut vcx, "live-turn-mic-3200000"));
        let still_open = |vcx: &mut VisualTestContext| {
            h.window
                .update(vcx, |v, window, cx| {
                    let open = v.live_picker().expect("the picker stays open");
                    assert_eq!(open, &picker);
                    assert_eq!(
                        open.read(cx).typed(),
                        TypedName::Offer("Synthetic Newcomer".into())
                    );
                    assert!(open.read(cx).search_focused(window, cx));
                })
                .unwrap()
        };
        still_open(&mut vcx);

        // Rows dropped above (a fresh snapshot without the oldest lines)
        // keep the row at the top and the picker's row: a name still goes to
        // exactly the line it was opened on.
        let top = |vcx: &mut VisualTestContext| {
            h.window
                .update(vcx, |v, _, _| {
                    v.transcript_rows[v.transcript.logical_scroll_top().item_ix]
                        .key
                        .clone()
                })
                .unwrap()
        };
        let first_shown = top(&mut vcx);
        let first_index = (0..50)
            .find(|index| first_shown == RowKey::Turn(id(*index)))
            .unwrap();
        let drop_before = first_index.min(10);
        assert!(
            drop_before > 0,
            "the fixture must drop rows above the visible anchor"
        );
        let survivors: Vec<Value> = (drop_before..50).map(&line).collect();
        h.agent.answer(move |call| match call {
            Call::RecordLive { session_id } => Ok(json!({
                "session_id": session_id,
                "identity": { "status": { "state": "ready" }, "epoch": 1 },
                "evidence": "ev1",
                "turns": survivors.clone()
            })),
            _ => Err(refusal("synthetic refusal (stale_revision)")),
        });
        h.event(&mut vcx, Event::Lagged { dropped: 1 });
        assert_eq!(
            turns(&h, &mut vcx).first().map(|t| t.turn_id.clone()),
            Some(id(drop_before)),
            "the oldest rows are gone"
        );
        assert_eq!(top(&mut vcx), first_shown, "the same row stays at the top");
        assert!(in_view(&mut vcx, "live-turn-mic-2880000"));
        still_open(&mut vcx);
        h.act(&mut vcx, |_, _, cx| {
            picker.update(cx, |p, cx| p.use_typed(cx))
        });
        assert!(matches!(
            h.agent.calls().last(),
            Some(Call::RecordLiveAttribute(p))
                if p.turn_id == named && p.person_ref.as_deref() == Some("Synthetic Newcomer")
        ));
        // The picker closed; it was following when it opened, so it follows
        // again.
        assert!(list.is_following_tail());
        h.event(&mut vcx, live_event(line(50)));
        assert!(in_view(&mut vcx, "live-turn-mic-3264000"));

        // A picker opened while reading earlier lines: closing it leaves the
        // view where it was.
        vcx.simulate_event(wheel(300.));
        let reading = list.logical_scroll_top();
        let RowKey::Turn(row) = top(&mut vcx) else {
            panic!("a live row at the top");
        };
        h.act(&mut vcx, |view, window, cx| {
            view.open_live_naming(&row, window, cx)
        });
        h.act(&mut vcx, |view, window, cx| {
            view.cancel_live_naming(window, cx)
        });
        assert!(!list.is_following_tail());
        assert_eq!(list.logical_scroll_top(), reading);
    }

    // ---- drafts, sessions, bounded rows, health and saving states ----

    fn recording_with(session: &str, change: impl FnOnce(&mut RecordingState)) -> Event {
        let Event::State(mut state) = recording(session) else {
            unreachable!()
        };
        change(state.recording.as_mut().unwrap());
        Event::State(state)
    }

    #[gpui::test]
    fn drafts_typed_while_a_request_is_in_flight_survive_its_answer(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        h.event(cx, recording("s1"));
        h.agent.answer(|call| match call {
            Call::RecordTag { .. } => Ok(json!({"session_id": "s1", "tags": ["kept", "alpha"]})),
            _ => Ok(json!({})),
        });
        // The note is sent; the user goes on typing before the answer.
        h.type_into(cx, |v| v.note.clone(), "Synthetic first note");
        h.window
            .update(cx, |view, window, cx| {
                view.submit_note(window, cx);
                view.note.update(cx, |input, cx| {
                    input.set_value("Synthetic second note", window, cx)
                });
            })
            .unwrap();
        cx.run_until_parked();
        assert!(matches!(
            h.status(cx, Control::Note),
            Some(ControlStatus::Done { .. })
        ));
        assert_eq!(h.value(cx, |v| v.note.clone()), "Synthetic second note");
        // The same for tags.
        h.type_into(cx, |v| v.tag.clone(), "alpha");
        h.window
            .update(cx, |view, window, cx| {
                view.submit_tags(window, cx);
                view.tag
                    .update(cx, |input, cx| input.set_value("beta", window, cx));
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(h.value(cx, |v| v.tag.clone()), "beta");
        let tags = h.window.update(cx, |v, _, cx| v.shown_tags(cx)).unwrap();
        assert_eq!(tags, ["kept", "alpha"]);
        // A draft left exactly as sent is cleared once accepted.
        h.act(cx, |view, window, cx| view.submit_note(window, cx));
        assert_eq!(h.value(cx, |v| v.note.clone()), "");
    }

    #[gpui::test]
    fn a_new_recording_resets_its_language_and_drafts(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        h.event(
            cx,
            recording_with("s1", |r| r.language = Some("pt-BR".into())),
        );
        let language = |cx: &mut TestAppContext| {
            h.window
                .update(cx, |panel, _, cx| panel.shown_language(cx).to_owned())
                .unwrap()
        };
        assert_eq!(language(cx), "pt-BR");
        h.event(cx, recording_with("s1", |r| r.language = Some("es".into())));
        assert_eq!(language(cx), "es");
        h.type_into(cx, |v| v.note.clone(), "Synthetic note");
        h.type_into(cx, |v| v.tag.clone(), "synthetic-tag");
        // The next recording starts from its own language and empty drafts.
        h.event(cx, recording_with("s2", |r| r.language = Some("en".into())));
        assert_eq!(language(cx), "en");
        assert_eq!(h.value(cx, |v| v.note.clone()), "");
        assert_eq!(h.value(cx, |v| v.tag.clone()), "");
        // Untouched, the field follows a change made elsewhere.
        h.event(cx, recording_with("s2", |r| r.language = Some("de".into())));
        assert_eq!(language(cx), "de");
    }

    #[test]
    fn meters_never_move_the_transcript_and_dropped_rows_never_return() {
        let mut model = PanelModel::default();
        let now = Instant::now();
        model.apply(&recording("s1"), now);
        let before = (model.revision(), model.rows_revision());
        for ms in [0, 50, 120] {
            model.apply(
                &Event::Meters {
                    me_dbfs: -10. - ms as f32 / 10.,
                    others_dbfs: -20.,
                },
                now + Duration::from_millis(ms),
            );
        }
        model.flush_meters(now + Duration::from_millis(400));
        assert_eq!((model.revision(), model.rows_revision()), before);
        // The agent keeps its newest turns only, and so does the panel.
        let turn = |n: usize| -> LiveTurn {
            serde_json::from_value(live_turn(
                &format!("mic-{}", 16_000 * (n + 1)),
                1,
                "preview",
                json!({}),
            ))
            .unwrap()
        };
        let max = live_turns_max();
        for n in 0..=max {
            assert!(model.upsert_turn(turn(n)));
        }
        assert_eq!(model.turns.len(), max);
        assert!(model.turn(&turn(0).turn_id).is_none());
        assert_eq!(model.turns[0].turn_id, turn(1).turn_id);
        // A late answer about the dropped row never brings it back.
        let mut late = turn(0);
        late.revision = 5;
        assert!(!model.replace_turn(late));
        assert!(model.turn(&turn(0).turn_id).is_none());
        // Capture warnings belong to the recording they were sent for.
        let disk_low = CaptureWarning::DiskLow {
            free_bytes: 900_000_000,
            headroom_s: 1_800.,
            stop_below_bytes: 500_000_000,
            stop_below_s: 600.,
        };
        assert!(!model.apply(
            &Event::CaptureWarning {
                session_id: "s0".into(),
                warning: disk_low.clone(),
            },
            now
        ));
        assert!(model.apply(
            &Event::CaptureWarning {
                session_id: "s1".into(),
                warning: disk_low.clone(),
            },
            now
        ));
        assert_eq!(model.capture_warnings().collect::<Vec<_>>(), [&disk_low]);
        // A state whose disk is fine again ends the disk warning.
        model.apply(
            &recording_with("s1", |r| {
                r.health.disk = Some(DiskHealth {
                    free_bytes: 50_000_000_000,
                    headroom_s: 90_000.,
                    level: DiskLevel::Ok,
                })
            }),
            now,
        );
        assert_eq!(model.capture_warnings().count(), 0);
    }

    #[gpui::test]
    fn a_naming_picker_on_a_row_that_leaves_the_preview_closes_without_naming(
        cx: &mut TestAppContext,
    ) {
        let h = Harness::new(cx);
        h.agent.answer(|call| match call {
            Call::RecordLive { session_id } => Ok(json!({
                "session_id": session_id,
                "identity": { "status": { "state": "ready" }, "epoch": 1 },
                "evidence": "ev1",
                "turns": []
            })),
            _ => Err(refusal("synthetic refusal (stale_turn)")),
        });
        h.event(cx, recording("s1"));
        let id = |n: usize| format!("mic-{}", 64_000 * (n + 1));
        let max = live_turns_max();
        for n in 0..max {
            h.daemon.update(cx, |d, cx| {
                d.received(live_event(live_turn(&id(n), 1, "preview", json!({}))), cx)
            });
        }
        cx.run_until_parked();
        let oldest = id(0);
        h.act(cx, |view, window, cx| {
            view.open_live_naming(&oldest, window, cx)
        });
        assert!(
            h.window
                .update(cx, |v, _, _| v.live_picker().is_some())
                .unwrap()
        );
        // One more line: the agent drops its oldest, the panel too.
        h.event(cx, live_event(live_turn(&id(max), 1, "preview", json!({}))));
        assert_eq!(turns(&h, cx).len(), max);
        assert!(
            h.window
                .update(cx, |v, _, _| v.live_picker().is_none())
                .unwrap()
        );
        let (gone, neighbour) = h
            .window
            .update(cx, |v, _, _| {
                (
                    v.live_status("s1", &id(0)).cloned(),
                    v.live_status("s1", &id(1)).cloned(),
                )
            })
            .unwrap();
        assert!(matches!(
            gone,
            Some(ControlStatus::Failed(text)) if text.contains("left the live preview")
        ));
        assert_eq!(neighbour, None, "never shown on another row");
        assert!(
            h.agent
                .calls()
                .iter()
                .all(|c| !matches!(c, Call::RecordLiveAttribute(_))),
            "nothing was named"
        );
    }

    /// The two tracks close their lines independently, so the agent
    /// publishes them out of time order, and `record.live` lists them in
    /// that order: the order the agent drops them in at its bound. After a
    /// resync the panel drops exactly the line the agent drops, still shows
    /// time order, and a picker on the earliest line (which the agent keeps)
    /// stays open and names that line as shown.
    #[gpui::test]
    fn after_a_resync_the_panel_drops_what_the_agent_drops(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        let max = live_turns_max();
        assert_eq!(max, 200);
        // Each system line is published before the mic line that started
        // 10 s earlier (the longer mic speech closed later).
        let id = |k: usize| {
            let pair = (k / 2) as u64 * 20 * 16_000;
            if k.is_multiple_of(2) {
                format!("system-{}", pair + 160_000)
            } else {
                format!("mic-{pair}")
            }
        };
        let published: Vec<Value> = (0..max)
            .map(|k| live_turn(&id(k), 1, "preview", json!({})))
            .collect();
        h.agent.answer(move |call| match call {
            Call::RecordLive { session_id } => Ok(json!({
                "session_id": session_id,
                "identity": { "status": { "state": "ready" }, "epoch": 1 },
                "evidence": "ev1",
                "turns": published.clone(),
            })),
            _ => Err(refusal("synthetic refusal (stale_turn)")),
        });
        h.event(cx, recording("s1"));
        h.event(cx, Event::Lagged { dropped: 3 });
        assert_eq!(h.agent.snapshots(), ["s1", "s1"]);
        let shown = turns(&h, cx);
        assert_eq!(shown.len(), max);
        assert_eq!(
            [shown[0].turn_id.as_str(), shown[1].turn_id.as_str()],
            ["mic-0", "system-160000"],
            "shown in time order"
        );
        h.act(cx, |view, window, cx| {
            view.open_live_naming("mic-0", window, cx)
        });

        // The next line: the agent drops its first publication, the system
        // line at 10 s, and keeps the mic line at 0 s.
        h.event(cx, live_event(live_turn(&id(max), 1, "preview", json!({}))));
        let shown = turns(&h, cx);
        let mut held: Vec<&str> = shown.iter().map(|t| t.turn_id.as_str()).collect();
        held.sort_unstable();
        let agent: Vec<String> = (1..=max).map(id).collect();
        let mut agent: Vec<&str> = agent.iter().map(String::as_str).collect();
        agent.sort_unstable();
        assert_eq!(held, agent);
        assert_eq!(shown[0].turn_id, "mic-0");
        assert!(shown.iter().all(|t| t.turn_id != "system-160000"));
        assert!(
            h.window
                .update(cx, |v, _, _| v.live_picker().is_some())
                .unwrap(),
            "the kept line's picker stays open"
        );
        assert_eq!(
            h.window
                .update(cx, |v, _, _| v.live_status("s1", "mic-0").cloned())
                .unwrap(),
            None
        );
        let picker = h
            .window
            .update(cx, |v, _, _| v.live_picker().cloned())
            .unwrap()
            .unwrap();
        h.act(cx, |_, window, cx| {
            picker.update(cx, |p, cx| p.set_query("Synthetic Newcomer", window, cx))
        });
        h.act(cx, |_, _, cx| picker.update(cx, |p, cx| p.use_typed(cx)));
        assert!(matches!(
            &h.agent.calls()[..],
            [Call::RecordLiveAttribute(p)] if p.action == LiveAction::Name
                && (p.session_id.as_str(), p.turn_id.as_str(), p.revision, p.epoch, p.evidence.as_str())
                    == ("s1", "mic-0", 1, 1, "ev1")
                && p.person_ref.as_deref() == Some("Synthetic Newcomer")
        ));
    }

    #[test]
    fn overlay_notice_and_panel_share_the_plain_warning() {
        let notice = overlay_notice();
        let platform = broscribe_platform_native::NativePlatform::host_platform();
        let title = if platform == broscribe_platform::HostPlatform::MacOs {
            "Recording Controls Can't Stay on Top"
        } else {
            "Recording controls can't stay on top"
        };
        assert_eq!(notice.title, title);
        assert_eq!(notice.body, overlay_warning());
        assert_eq!(
            notice.body,
            "Other apps can cover the recording controls, because they can't be kept on top. Recording isn't affected."
        );
    }

    #[test]
    fn recording_health_and_saving_states_are_told_apart() {
        let loading = PreviewState::Loading {
            model: Some("synthetic-asr".into()),
            elapsed_ms: 12_400,
        };
        let loading = preview_notice(&loading).unwrap();
        assert!(loading.contains("Loading the live transcription model"));
        assert!(!loading.contains("synthetic-asr") && !loading.contains("12"));
        assert_eq!(
            preview_notice(&PreviewState::Ready {
                model: "synthetic-asr".into()
            }),
            None
        );
        let gone = preview_notice(&PreviewState::Unavailable {
            reason: WorkerProblem::LoadTimeout,
        })
        .unwrap();
        assert!(gone.contains("took too long to load") && gone.contains("Recording is unaffected"));
        let off = identity_notice(&LiveIdentityState {
            status: LiveIdentityStatus::Disabled,
            epoch: 0,
        })
        .unwrap();
        assert!(off.contains("off in Settings") && off.contains("live transcript go on"));
        assert!(
            reader_notice(
                Some(&ReaderStatus::Degraded {
                    reason: broscribe_ipc::ReaderDegraded::Silent
                }),
                None,
                Some("Zoom")
            )
            .unwrap()
            .contains("can't read the Zoom window, so automatic stop uses the microphone only")
        );
        assert_eq!(reader_notice(None, None, None), None);
        assert_eq!(
            reader_notice(Some(&ReaderStatus::Reading), None, None),
            None
        );

        // The latest batch's diagnostics tell the causes of an unusable
        // call state apart; a whole, healthy batch adds nothing.
        let unusable = ReaderStatus::Degraded {
            reason: broscribe_ipc::ReaderDegraded::UnusableCallState,
        };
        let with =
            |health: ReaderHealth| reader_notice(Some(&unusable), Some(&health), None).unwrap();
        let plain = reader_notice(Some(&unusable), None, None).unwrap();
        assert_eq!(with(ReaderHealth::default()), plain);
        let capped = with(ReaderHealth {
            incomplete: Some(IncompleteReason::NodeCap),
            ..ReaderHealth::default()
        });
        let transient = with(ReaderHealth {
            incomplete: Some(IncompleteReason::Transient),
            consecutive_failures: 3,
            retry_after_s: Some(30.),
            ..ReaderHealth::default()
        });
        let manual = with(ReaderHealth {
            manual_accessibility: ManualAccessibility::Failed,
            ..ReaderHealth::default()
        });
        let distinct: BTreeSet<&String> = [&plain, &capped, &transient, &manual].into();
        assert_eq!(distinct.len(), 4, "{distinct:?}");
        assert!(
            transient.ends_with("The app is slow to answer. BroScribe keeps trying."),
            "{transient}"
        );
        // Diagnostics never turn a working or paused reader into a notice.
        let failing = ReaderHealth {
            incomplete: Some(IncompleteReason::Failed),
            ..ReaderHealth::default()
        };
        assert_eq!(
            reader_notice(Some(&ReaderStatus::Reading), Some(&failing), None),
            None
        );
        assert_ne!(
            preview_notice(&PreviewState::Unavailable {
                reason: WorkerProblem::ModelMissing
            }),
            preview_notice(&PreviewState::Unavailable {
                reason: WorkerProblem::LoadTimeout
            })
        );

        let (track, attention) = track_health(&TrackHealth {
            track: Track::Mic,
            state: TrackState::Waiting,
            dropped_frames: 480,
            failures: 2,
            last_error: Some("synthetic device unplugged".into()),
            retry_in_s: Some(4.),
        });
        assert!(attention);
        assert!(track.contains("Trying again.") && track.contains("480 audio frames"));
        assert!(
            track.contains("lost its device. Trying again") && !track.contains("unplugged"),
            "{track}"
        );
        assert!(!track_health(&TrackHealth::capturing(Track::System)).1);
        let (disk, attention) = disk_health(&DiskHealth {
            free_bytes: 2_500_000_000,
            headroom_s: 5_400.,
            level: DiskLevel::Warning,
        });
        assert!(attention);
        assert!(disk.contains("about 1 h 30 min") && disk.contains("before the disk fills"));

        let retrying = stopped_summary(&StoppedRecordingState::Retrying {
            attempts: 2,
            retry_in_s: 30,
            error: Some("worker crashed".into()),
        });
        // The job's error is the pipeline's text for the log: an unknown
        // one is left out, never shown (WRT-04).
        assert_eq!(retrying, "Retrying…");
        assert_eq!(
            stopped_summary(&StoppedRecordingState::Failed { error: None }),
            "Couldn't transcribe"
        );
        assert!(
            stopped_summary(&StoppedRecordingState::Queued { ahead: 3 }).contains("3 jobs ahead")
        );
        assert!(
            stopped_summary(&StoppedRecordingState::Running {
                fraction: Some(0.42)
            },)
            .contains("· 42%")
        );
        assert!(
            !stopped_summary(&StoppedRecordingState::Running { fraction: None }).contains('%'),
            "no invented progress"
        );
        assert_eq!(live_review_notice(0), None);
        assert_eq!(
            live_review_notice(2).as_deref(),
            Some("Open the meeting to check 2 names you gave while recording.")
        );
    }

    #[gpui::test]
    fn only_a_failed_save_is_retried_and_only_by_choice(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        let with_state = |state: Value| {
            daemon_state(
                None,
                json!([{ "session_id": "s1", "title": null, "tags": [],
                         "stopped_at": "2026-11-14T11:00:00Z", "state": state }]),
                json!([]),
            )
        };
        h.event(
            cx,
            with_state(json!({ "phase": "failed", "error": "worker crashed" })),
        );
        h.agent
            .answer(|_| Ok(json!({ "session_id": "s1", "job_id": "j-synthetic" })));
        h.act(cx, |view, window, cx| view.retry("s1".into(), window, cx));
        assert!(matches!(
            &h.agent.calls()[..],
            [Call::RecordRetry { session_id, .. }] if session_id == "s1"
        ));
        assert!(matches!(
            h.window
                .update(cx, |v, _, _| v
                    .session_status(Control::Retry, "s1")
                    .cloned())
                .unwrap(),
            Some(ControlStatus::Done { .. })
        ));
        // Running again: nothing to retry.
        h.event(cx, with_state(processing()));
        h.act(cx, |view, window, cx| view.retry("s1".into(), window, cx));
        assert_eq!(h.agent.calls().len(), 1);
    }

    /// WRT-04: a stopped recording that couldn't be saved says which of
    /// three it is, with its cause in words and never a code: an automatic
    /// retry (its attempt and countdown, no Retry), a manual one (Retry),
    /// and a damaged journal kept for repair (no Retry; the agent refuses
    /// it).
    #[gpui::test]
    fn a_failed_save_says_whether_it_retries_waits_for_retry_or_is_kept_for_repair(
        cx: &mut TestAppContext,
    ) {
        let h = Harness::new(cx);
        let with = |state: Value, issues: Value| {
            let Event::State(mut daemon) = daemon_state(
                None,
                json!([{ "session_id": "s1", "title": "Synthetic sync", "tags": [],
                         "stopped_at": "2026-11-14T11:00:00Z", "state": state }]),
                json!([]),
            ) else {
                unreachable!()
            };
            daemon.journal_issues = serde_json::from_value(issues).unwrap();
            Event::State(daemon)
        };
        let shown = |h: &Harness, cx: &mut TestAppContext| {
            cx.update_window(h.window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            let named = h
                .window
                .update(cx, |view, window, cx| {
                    crate::ui::spoken_log::while_building(|| Render::render(view, window, cx)).1
                })
                .unwrap();
            let find = |id: &str| {
                named
                    .iter()
                    .find(|(named, ..)| named == id)
                    .map(|(_, role, text)| (*role, text.clone()))
            };
            let retry = gpui::VisualTestContext::from_window(h.window.into(), cx)
                .debug_bounds("retry-s1")
                .is_some();
            (
                find("stopped-state-s1")
                    .or_else(|| find("stopped-state-s1-attempt-0"))
                    .or_else(|| find("stopped-state-s1-attempt-2")),
                find("retry-countdown-s1"),
                retry,
            )
        };

        // The agent tries again itself: attempt and countdown, no Retry.
        h.event(
            cx,
            with(
                json!({ "phase": "retrying", "attempts": 2, "retry_in_s": 30,
                        "error": "model missing: synthetic-asr" }),
                json!([]),
            ),
        );
        let (status, countdown, retry) = shown(&h, cx);
        assert_eq!(
            status,
            Some((
                Role::Status,
                "Couldn't transcribe. A model it needs isn't installed.".to_owned()
            ))
        );
        assert!(countdown.is_some_and(|(_, text)| text == "30 seconds"));
        assert!(!retry, "an automatic retry offers no Retry");

        // A journal the agent's next sweep recovers again (an issue that
        // may pass): its code is never shown.
        h.event(
            cx,
            with(
                json!({ "phase": "retrying", "attempts": 0, "retry_in_s": 60, "error": "io" }),
                json!([{ "session_id": "s1", "code": "io", "corrupt": false }]),
            ),
        );
        let (status, _, retry) = shown(&h, cx);
        let (_, text) = status.unwrap();
        assert_eq!(
            text,
            "Couldn't transcribe. BroScribe couldn't read or write a file."
        );
        assert!(
            !text.contains("io.") && !text.contains("attempt 0"),
            "{text}"
        );
        assert!(!retry);

        // Failed for good: Retry, which the agent is asked for.
        h.event(
            cx,
            with(
                json!({ "phase": "failed",
                        "error": "/synthetic/journal/s1/mic.wav: Input/output error (os error 5)" }),
                json!([]),
            ),
        );
        let (status, countdown, retry) = shown(&h, cx);
        assert_eq!(
            status,
            Some((Role::Alert, "Couldn't transcribe".to_owned()))
        );
        assert_eq!(countdown, None);
        assert!(retry, "a manual retry offers Retry");
        h.agent
            .answer(|_| Ok(json!({ "session_id": "s1", "job_id": "j-synthetic" })));
        h.act(cx, |view, window, cx| view.retry("s1".into(), window, cx));
        assert_eq!(h.agent.calls().len(), 1);

        // A damaged journal: kept for repair, no Retry, nothing asked.
        h.event(
            cx,
            with(
                json!({ "phase": "failed", "error": "schema" }),
                json!([{ "session_id": "s1", "code": "schema", "corrupt": true }]),
            ),
        );
        let (status, _, retry) = shown(&h, cx);
        let (role, text) = status.unwrap();
        assert_eq!(role, Role::Alert);
        assert_eq!(
            text,
            "Couldn't save the recording. Its files are damaged, so trying \
             again can't help. They're kept as they are for repair; discard them only if you \
             don't need this recording."
        );
        assert!(!retry, "a journal kept for repair offers no Retry");
        h.act(cx, |view, window, cx| view.retry("s1".into(), window, cx));
        assert_eq!(
            h.agent.calls().len(),
            1,
            "nothing asks the agent to retry it"
        );
    }

    #[gpui::test]
    fn the_clock_ticks_only_while_something_counts(cx: &mut TestAppContext) {
        let h = Harness::new(cx);
        let ticking = |h: &Harness, cx: &mut TestAppContext| {
            h.window.update(cx, |v, _, _| v.ticking()).unwrap()
        };
        assert!(!ticking(&h, cx), "idle");
        h.event(cx, recording("s1"));
        assert!(ticking(&h, cx), "recording");
        h.event(
            cx,
            recording_with("s1", |r| r.status = RecordingStatus::Paused),
        );
        assert!(!ticking(&h, cx), "paused");
        // An unsaved title keeps the panel open once the capture ends.
        h.type_into(cx, title_of("s1"), "Synthetic draft");
        h.event(
            cx,
            daemon_state(
                None,
                json!([{ "session_id": "s1", "title": null, "tags": [],
                         "stopped_at": "2026-11-14T11:00:00Z",
                         "state": { "phase": "retrying", "attempts": 1, "retry_in_s": 30 } }]),
                json!([]),
            ),
        );
        assert!(ticking(&h, cx), "a retry countdown");
        h.event(
            cx,
            daemon_state(None, json!([stopped_entry("s1", None)]), json!([])),
        );
        assert!(!ticking(&h, cx), "saving, nothing counts");
    }

    /// The active page uses the available height, including at the minimum size.
    #[gpui::test]
    fn a_short_panel_keeps_a_usable_live_viewport(cx: &mut TestAppContext) {
        use gpui::VisualTestContext;
        let h = Harness::new(cx);
        live_agent(&h, |_| Ok(json!({})));
        h.event(
            cx,
            recording_with("s1", |r| {
                r.reader = Some(ReaderStatus::Refused {
                    refusal: broscribe_readers::ReaderStartRefusal::Untrusted,
                });
                r.health.tracks = [Track::Mic, Track::System]
                    .map(|track| TrackHealth {
                        track,
                        state: TrackState::Capturing,
                        dropped_frames: 0,
                        failures: 0,
                        last_error: None,
                        retry_in_s: None,
                    })
                    .into();
            }),
        );
        h.event(
            cx,
            live_event(live_turn("mic-160000", 1, "preview", json!({}))),
        );
        let mut vcx = VisualTestContext::from_window(h.window.into(), cx);
        for dimensions in [(420., 712.), crate::ui_state::PANEL_MIN] {
            vcx.simulate_resize(size(px(dimensions.0), px(dimensions.1)));
            let viewport = vcx.debug_bounds("live-transcript").expect("live viewport");
            let header = vcx
                .debug_bounds("panel-header-scroll")
                .expect("header viewport");
            let tabs = vcx.debug_bounds("panel-tabs").expect("segmented tabs");
            assert!(header.bottom() <= tabs.top() && tabs.bottom() <= viewport.top());
            assert!(
                viewport.size.height >= px(48.),
                "Transcript {viewport:?}, header {header:?}, tabs {tabs:?}"
            );
            assert!(viewport.bottom() <= px(dimensions.1));
        }
        vcx.update(|_, cx| {
            crate::ui::apply_reading_size(broscribe_core::config::ReadingSize::Largest, cx);
        });
        vcx.simulate_resize(size(px(840.), px(712.)));
        let viewport = vcx
            .debug_bounds("live-transcript")
            .expect("large text live viewport");
        assert!(viewport.size.height >= px(72.) && viewport.bottom() <= px(712.));
    }

    #[test]
    fn default_panel_width_follows_all_reading_sizes() {
        use broscribe_core::config::ReadingSize;
        let widths = ReadingSize::ALL.map(|reading| panel_size(reading, Some(2000.)).0);
        assert_eq!(widths, [367.5, 420., 525., 630., 840.]);
        assert_eq!(panel_size(ReadingSize::Largest, Some(1000.)), (500., 712.));
        assert_eq!(
            panel_size(ReadingSize::Small, Some(640.)).0,
            crate::ui_state::PANEL_MIN.0
        );
    }

    #[gpui::test]
    fn active_panel_sections_follow_the_control_row_and_reset_for_the_next_session(
        cx: &mut TestAppContext,
    ) {
        use crate::ui::kit::dialog_gate::{click, draw};
        use gpui::Focusable as _;
        let h = Harness::new(cx);
        h.event(cx, recording("synthetic"));
        let vcx = &mut gpui::VisualTestContext::from_window(h.window.into(), cx);
        vcx.simulate_resize(size(px(420.), px(712.)));
        draw(vcx);
        let tree =
            |vcx: &mut gpui::VisualTestContext| vcx.update(crate::ui::a11y_tree::draw_and_read);
        let initial = tree(vcx);
        initial.assert_heading("panel-recording-heading", 2);
        assert_eq!(
            initial.read(initial.find("panel-upper")),
            ("Group", "Recording")
        );
        assert!(initial.all("panel-attention").is_empty());
        assert!(initial.all("panel-title-heading").is_empty());
        h.window
            .update(vcx, |view, _, cx| {
                view.overlay_note = Some(STANDARD_OVERLAY_NOTE.into());
                cx.notify();
            })
            .unwrap();
        assert!(tree(vcx).all("overlay-note").is_empty());
        let root = initial.find("recording-panel");
        let controls: Vec<_> = initial
            .subtree(root)
            .into_iter()
            .filter_map(|ix| {
                let node = &initial.nodes[ix];
                (node.role == "Button"
                    && [
                        "Pause",
                        "Bookmark",
                        "More recording actions",
                        "Stop and save",
                    ]
                    .contains(&node.name.as_str()))
                .then_some(node.name.as_str())
            })
            .collect();
        assert_eq!(
            controls,
            [
                "Pause",
                "Bookmark",
                "More recording actions",
                "Stop and save"
            ]
        );
        click(vcx, "recording-more-button", Role::Button);
        let more = tree(vcx);
        assert_eq!(more.read(more.find("panel-rename")), ("MenuItem", "Rename"));
        assert_eq!(
            more.read(more.find("panel-process")),
            ("MenuItem", "Process now")
        );
        assert_eq!(
            more.read(more.find("panel-process-caption")),
            (
                "Label",
                "Make a draft transcript of what's recorded so far. Recording continues."
            ),
        );
        assert_eq!(
            more.read(more.find("panel-discard")),
            ("MenuItem", "Discard")
        );
        click(vcx, "panel-rename", Role::MenuItem);
        vcx.update(|window, cx| window.simulate_next_frame(cx));
        draw(vcx);
        let details = tree(vcx);
        assert!(details.all("live-transcript").is_empty());
        assert_eq!(
            details.read(details.find("overlay-note")),
            ("Status", STANDARD_OVERLAY_NOTE)
        );
        assert!(
            details
                .subtree(details.find("recording-panel-page"))
                .contains(&details.find("overlay-note"))
        );
        let sections = [
            "panel-title-heading",
            "panel-language-heading",
            "panel-people-heading",
            "panel-tags-heading",
            "panel-auto-stop-heading",
            "panel-capture-heading",
            "panel-other-heading",
        ];
        let headings: Vec<_> = details
            .subtree(details.find("recording-panel-page"))
            .into_iter()
            .filter_map(|ix| details.nodes[ix].element_id.as_deref())
            .filter(|id| sections.contains(id))
            .collect();
        assert_eq!(headings, sections);
        for section in sections {
            details.assert_heading(section, 3);
        }
        vcx.update(|window, cx| {
            let panel = window.root::<RecordingPanel>().unwrap().unwrap();
            let input = panel.read(cx).title_input("synthetic").unwrap();
            assert!(input.read(cx).focus_handle(cx).is_focused(window));
            input.update(cx, |input, cx| {
                input.set_value("Synthetic draft", window, cx)
            });
        });
        draw(vcx);
        vcx.simulate_keystrokes("escape");
        vcx.run_until_parked();
        h.window
            .update(vcx, |view, window, cx| {
                let editor = &view.titles["synthetic"];
                assert!(!editor.revealed);
                assert_eq!(
                    editor.input.read(cx).value().as_str(),
                    editor.seen.as_deref().unwrap_or("")
                );
                assert!(view.more_focus.is_focused(window));
            })
            .unwrap();
        assert!(h.agent.calls().is_empty());
        h.daemon
            .update(vcx, |daemon, cx| daemon.received(recording("next"), cx));
        vcx.run_until_parked();
        assert!(!tree(vcx).all("live-transcript").is_empty());
    }

    #[gpui::test]
    fn stopped_rename_reveals_and_edits_only_its_own_row(cx: &mut TestAppContext) {
        use crate::ui::kit::dialog_gate::{click, draw};
        use gpui::Focusable as _;
        let h = Harness::new(cx);
        h.event(
            cx,
            daemon_state(
                None,
                json!([
                    stopped_entry("first", Some("Synthetic first")),
                    stopped_entry("second", Some("Synthetic second")),
                ]),
                json!([]),
            ),
        );
        h.agent.answer(|_| Ok(json!({"status": "saved"})));
        let vcx = &mut gpui::VisualTestContext::from_window(h.window.into(), cx);
        vcx.simulate_resize(size(px(480.), px(900.)));
        draw(vcx);
        assert!(
            !h.window
                .update(vcx, |view, _, _| view.titles["second"].revealed)
                .unwrap()
        );
        click(vcx, "rename-second", Role::Button);
        vcx.update(|window, cx| window.simulate_next_frame(cx));
        draw(vcx);
        vcx.simulate_keystrokes("escape");
        draw(vcx);
        h.window
            .update(vcx, |view, window, _| {
                let editor = &view.titles["second"];
                assert!(!editor.revealed);
                assert!(editor.rename_focus.is_focused(window));
            })
            .unwrap();
        click(vcx, "rename-second", Role::Button);
        vcx.update(|window, cx| window.simulate_next_frame(cx));
        draw(vcx);
        h.window
            .update(vcx, |view, window, cx| {
                let editor = &view.titles["second"];
                assert!(editor.revealed && !view.titles["first"].revealed);
                assert!(editor.input.read(cx).focus_handle(cx).is_focused(window));
                editor.input.update(cx, |input, cx| {
                    input.set_value("Synthetic renamed", window, cx)
                });
            })
            .unwrap();
        click(vcx, "panel-title-second", Role::Button);
        assert!(
            matches!(&h.agent.calls()[..], [Call::RecordTitle { session_id, title, .. }]
            if session_id == "second" && title == "Synthetic renamed")
        );
    }

    #[gpui::test]
    fn explicit_panel_focus_prioritizes_grace_then_pause_without_activating_automatically(
        cx: &mut TestAppContext,
    ) {
        let h = Harness::new(cx);
        for (event, urgent) in [
            (recording("synthetic"), false),
            (recording_auto("synthetic", pending_stop(1)), true),
        ] {
            h.event(cx, event);
            let vcx = &mut gpui::VisualTestContext::from_window(h.window.into(), cx);
            let expected = h
                .window
                .update(vcx, |panel, window, cx| {
                    window.blur(cx);
                    focus_from_command(window, PanelReason::Automatic, cx.weak_entity());
                    if urgent {
                        panel.urgent_control.clone()
                    } else {
                        panel.first_control.clone()
                    }
                })
                .unwrap();
            crate::ui::kit::dialog_gate::draw(vcx);
            vcx.update(|window, cx| assert!(window.focused(cx).is_none()));
            h.window
                .update(vcx, |_, window, cx| {
                    focus_from_command(window, PanelReason::Command, cx.weak_entity())
                })
                .unwrap();
            vcx.update(|window, cx| window.simulate_next_frame(cx));
            crate::ui::kit::dialog_gate::draw(vcx);
            vcx.update(|window, _| assert!(expected.is_focused(window)));
        }
    }

    /// UI34: a meter flush redraws the panel (its levels) but reuses the
    /// drawn transcript; a new or revised line and typing in a row's picker
    /// draw the transcript again.
    #[gpui::test]
    fn meters_redraw_the_panel_but_never_the_transcript(cx: &mut TestAppContext) {
        use gpui::VisualTestContext;
        let h = Harness::new(cx);
        live_agent(&h, |_| Ok(json!({})));
        h.event(cx, recording("s1"));
        let mut vcx = VisualTestContext::from_window(h.window.into(), cx);
        vcx.simulate_resize(size(px(420.), px(2000.)));
        let counts = |vcx: &mut VisualTestContext| {
            h.window
                .update(vcx, |v, _, cx| v.render_counts(cx))
                .unwrap()
        };
        let meters = h.daemon.read_with(&vcx, |d, _| d.meters().clone());
        let (panel, transcript) = counts(&mut vcx);
        assert!(transcript > 0, "the transcript was drawn");
        h.event(
            &mut vcx,
            Event::Meters {
                me_dbfs: -20.,
                others_dbfs: -30.,
            },
        );
        assert_eq!(meters.read_with(&vcx, |m, _| m.levels()), [-20, -30]);
        let after_meters = counts(&mut vcx);
        assert!(after_meters.0 > panel, "the levels were drawn");
        assert_eq!(after_meters.1, transcript, "the transcript was reused");

        // A new line draws the transcript again.
        h.event(
            &mut vcx,
            live_event(live_turn("mic-160000", 1, "preview", json!({}))),
        );
        let after_line = counts(&mut vcx);
        assert!(after_line.1 > after_meters.1);

        // Typing in a row's picker draws the transcript (the picker is in it).
        h.act(&mut vcx, |view, window, cx| {
            view.open_live_naming("mic-160000", window, cx)
        });
        let picker = h
            .window
            .update(&mut vcx, |v, _, _| v.live_picker().cloned())
            .unwrap()
            .expect("the picker is open");
        let opened = counts(&mut vcx);
        h.act(&mut vcx, |_, window, cx| {
            picker.update(cx, |p, cx| p.set_query("Synthetic", window, cx))
        });
        let typed = counts(&mut vcx);
        assert!(typed.1 > opened.1);

        // Meters still never redraw it, picker open or not.
        std::thread::sleep(Duration::from_millis(110));
        h.event(
            &mut vcx,
            Event::Meters {
                me_dbfs: -10.,
                others_dbfs: -40.,
            },
        );
        assert_eq!(meters.read_with(&vcx, |m, _| m.levels()), [-10, -40]);
        let last = counts(&mut vcx);
        assert!(last.0 > typed.0);
        assert_eq!(last.1, typed.1);
    }

    #[gpui::test]
    fn the_panel_names_idle_active_paused_and_stopped_text(cx: &mut TestAppContext) {
        let mut harness = Harness::new(cx);
        let named = |harness: &Harness, cx: &mut TestAppContext| {
            harness
                .window
                .update(cx, |view, window, cx| {
                    crate::ui::spoken_log::while_building(|| Render::render(view, window, cx)).1
                })
                .unwrap()
        };
        let assert_line = |lines: &[(String, Role, String)], id, role, text| {
            assert!(
                lines.iter().any(|(actual_id, actual_role, actual_text)| {
                    actual_id == id && *actual_role == role && actual_text == text
                }),
                "missing {id}: {lines:?}"
            );
        };

        cx.update_window(harness.window.into(), |_, window, cx| {
            crate::ui::a11y_tree::draw_and_read(window, cx)
                .assert_title("recording-controls-title");
        })
        .unwrap();
        assert_line(
            &named(&harness, cx),
            "visor-state",
            Role::Status,
            "Connecting…",
        );
        harness.event(cx, daemon_state(None, json!([]), json!([])));
        assert_line(
            &named(&harness, cx),
            "visor-state",
            Role::Status,
            "Not recording",
        );

        harness.event(
            cx,
            recording_with("s1", |recording| {
                recording.app = Some("Synthetic call".into());
            }),
        );
        let active = named(&harness, cx);
        assert_line(
            &active,
            "visor-context",
            Role::Label,
            "Synthetic call · Synthetic s1",
        );
        assert_line(&active, "visor-state", Role::Status, "Recording");
        assert!(active.iter().any(|(id, role, word)| id == "visor-elapsed"
            && *role == Role::Timer
            && word.contains("second")));

        harness.event(
            cx,
            recording_with("s1", |recording| {
                recording.status = RecordingStatus::Paused;
            }),
        );
        assert_line(&named(&harness, cx), "visor-state", Role::Status, "Paused");
        harness.event(cx, stopped("s1"));
        harness.reopen(cx);
        assert_line(
            &named(&harness, cx),
            "visor-state",
            Role::Status,
            "Transcribing…",
        );
        let mut queued = stopped_entry("s1", None);
        queued["state"] = json!({"phase": "queued", "ahead": 0});
        harness.event(cx, daemon_state(None, json!([queued]), json!([])));
        assert_line(
            &named(&harness, cx),
            "visor-state",
            Role::Status,
            "Queued · next",
        );
        harness.event(cx, saved("s1"));
        assert!(
            named(&harness, cx)
                .iter()
                .any(|(id, role, words)| id == "saved-recording-label-s1"
                    && *role == Role::Label
                    && words == "Untitled meeting")
        );
        assert_line(&named(&harness, cx), "visor-state", Role::Status, "Saved");
        harness.act(cx, |view, _, cx| {
            view.live_reviews
                .insert("m-synthetic".into(), LiveReviews::Count(2));
            cx.notify();
        });
        let tree = cx
            .update_window(harness.window.into(), |_, window, cx| {
                crate::ui::a11y_tree::draw_and_read(window, cx)
            })
            .unwrap();
        assert_eq!(tree.read(tree.find("saved-state-s1")), ("Status", "Saved"));
        assert_eq!(
            tree.read(tree.find("live-review-s1")),
            ("Status", "2 names to review")
        );
        assert_eq!(
            tree.read(tree.find("visor-state")),
            ("Status", "Saved · 2 names to review")
        );
        for (id, words) in [
            ("visor-open", "Open meeting"),
            ("open-meeting-m-synthetic", "Open meeting"),
            ("delete-meeting-m-synthetic", "Delete meeting"),
        ] {
            assert_eq!(tree.read(tree.find(id)), ("Button", words));
        }
    }

    #[gpui::test]
    fn idle_recording_controls_explain_the_update_hold(cx: &mut TestAppContext) {
        use crate::views::update::{RECORDING_HELD, UpdateNotices, parse_status};
        let h = Harness::new(cx);
        h.event(cx, daemon_state(None, json!([]), json!([])));
        let held = parse_status(
            &json!({"update": {"state":"idle"}, "agent":{"install":{"fence":4,"durable":true}}}),
        );
        cx.update(|cx| {
            UpdateNotices::global(cx).update(cx, |notices, cx| notices.set_update_state(&held, cx))
        });
        let mut visual = gpui::VisualTestContext::from_window(h.window.into(), cx);
        visual.simulate_resize(size(px(480.), px(712.)));
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        assert_eq!(
            tree.read(tree.find("panel-update-hold")),
            ("Status", RECORDING_HELD)
        );
        assert_eq!(
            tree.read(tree.find("panel-open-updates")),
            ("Button", "Open update settings")
        );
        crate::ui::kit::dialog_gate::click(&mut visual, "panel-start", Role::Button);
        assert!(h.agent.calls().is_empty());
    }

    #[gpui::test]
    fn idle_start_refusal_stays_with_start_and_reconnect_preserves_the_recording_draft(
        cx: &mut TestAppContext,
    ) {
        use crate::ui::kit::dialog_gate::{click, draw};
        let h = Harness::new(cx);
        h.event(cx, daemon_state(None, json!([]), json!([])));
        h.agent
            .answer(|_| Err(IpcError::new(IpcError::DISK_LOW, "synthetic raw error")));
        let vcx = &mut gpui::VisualTestContext::from_window(h.window.into(), cx);
        vcx.simulate_resize(size(px(480.), px(712.)));
        click(vcx, "panel-start", Role::Button);
        assert!(matches!(&h.agent.calls()[..], [Call::RecordStart(_)]));
        let tree =
            |vcx: &mut gpui::VisualTestContext| vcx.update(crate::ui::a11y_tree::draw_and_read);
        let refused = tree(vcx);
        assert_eq!(
            refused.read(refused.find("panel-start-recovery")),
            ("Button", "Open Storage settings")
        );
        assert!(refused.nodes.iter().any(|n| n.role == "Alert"
            && n.name.contains("500 MB")
            && !n.name.contains("synthetic raw")));
        live_agent(&h, |_| Ok(json!({})));
        let active = || {
            recording_with("s1", |recording| {
                recording.elapsed_s = 763.;
                recording.title = Some("Synthetic sync".into());
                recording.app = Some("Synthetic call".into());
            })
        };
        h.daemon.update(vcx, |d, cx| d.received(active(), cx));
        vcx.run_until_parked();
        let shown = tree(vcx);
        let visor = shown.subtree(shown.find("recording-visor"));
        let words: Vec<_> = visor
            .into_iter()
            .filter_map(|ix| {
                let node = &shown.nodes[ix];
                node.element_id
                    .as_deref()
                    .filter(|id| ["visor-state", "visor-elapsed", "visor-context"].contains(id))
            })
            .collect();
        assert_eq!(words, ["visor-state", "visor-elapsed", "visor-context"]);
        assert_eq!(
            shown.read(shown.find("visor-context")).1,
            "Synthetic call · Synthetic sync"
        );
        h.window
            .update(vcx, |view, window, cx| {
                view.note.update(cx, |input, cx| {
                    input.set_value("Synthetic draft", window, cx)
                });
            })
            .unwrap();
        h.daemon.update(vcx, |d, cx| d.lost(None, cx));
        vcx.run_until_parked();
        draw(vcx);
        let disconnected = tree(vcx);
        assert_eq!(
            disconnected.read(disconnected.find("visor-state")),
            ("Status", "Not connected")
        );
        assert_eq!(
            disconnected.read(disconnected.find("visor-context")).1,
            "Last known: recording · 12:43"
        );
        assert!(disconnected.all("visor-elapsed").is_empty());
        assert_eq!(
            disconnected.read(disconnected.find("meter-me")).1,
            "Me: not connected"
        );
        assert_eq!(
            disconnected.read(disconnected.find("meter-others")).1,
            "Others: not connected"
        );
        let calls = h.agent.calls().len();
        click(vcx, "panel-pause", Role::Button);
        assert_eq!(h.agent.calls().len(), calls);
        assert!(!h.window.update(vcx, |view, _, _| view.ticking()).unwrap());
        h.daemon.update(vcx, |d, cx| d.received(active(), cx));
        vcx.run_until_parked();
        assert_eq!(tree(vcx).read(tree(vcx).find("visor-state")).1, "Recording");
        h.window
            .update(vcx, |view, _, cx| {
                assert_eq!(view.note.read(cx).value().as_ref(), "Synthetic draft")
            })
            .unwrap();
    }

    #[gpui::test]
    fn consent_and_early_stop_dismissals_are_session_bound_and_settings_are_actionable(
        cx: &mut TestAppContext,
    ) {
        use crate::state::{AppEvent, SettingsPage};
        use crate::ui::kit::dialog_gate::{click, draw};
        let h = Harness::new(cx);
        let events = Rc::new(std::cell::RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&h.state, move |_, event: &AppEvent, _| {
                events.borrow_mut().push(event.clone())
            })
        });
        h.event(cx, recording("s1"));
        h.act(cx, |view, _, cx| {
            view.consent_reminder = true;
            cx.notify();
        });
        let vcx = &mut gpui::VisualTestContext::from_window(h.window.into(), cx);
        vcx.simulate_resize(size(px(500.), px(900.)));
        let tree =
            |vcx: &mut gpui::VisualTestContext| vcx.update(crate::ui::a11y_tree::draw_and_read);
        click(vcx, "consent-settings", Role::Link);
        assert!(
            events
                .borrow()
                .contains(&AppEvent::OpenSettings(Some(SettingsPage::Recording)))
        );
        click(vcx, "consent-reminder-dismiss", Role::Button);
        assert!(tree(vcx).all("consent-reminder").is_empty());
        h.daemon
            .update(vcx, |d, cx| d.received(recording("s2"), cx));
        vcx.run_until_parked();
        assert!(!tree(vcx).all("consent-reminder").is_empty());
        let mut stopped = stopped_entry("s1", None);
        stopped["stop_reason"] = json!("disk_low");
        h.daemon.update(vcx, |d, cx| {
            d.received(daemon_state(Some("s2"), json!([stopped]), json!([])), cx)
        });
        vcx.run_until_parked();
        let current = tree(vcx);
        assert_eq!(
            current.read(current.find("stop-reason-attention-s1")),
            (
                "Alert",
                broscribe_ipc::recording_words::stop_reason_text("disk_low")
            )
        );
        click(vcx, "stop-settings-attention-s1", Role::Button);
        assert!(
            events
                .borrow()
                .contains(&AppEvent::OpenSettings(Some(SettingsPage::Storage)))
        );
        click(vcx, "stop-dismiss-attention-s1", Role::Button);
        let current = tree(vcx);
        assert!(current.all("stop-reason-attention-s1").is_empty());
        h.window
            .update(vcx, |view, _, cx| {
                view.tab = RecordingPanelTab::Details;
                cx.notify();
            })
            .unwrap();
        draw(vcx);
        let details = tree(vcx);
        assert_eq!(
            details.read(details.find("stop-reason-row-s1")).1,
            broscribe_ipc::recording_words::stop_reason_text("disk_low")
        );
    }

    #[gpui::test]
    fn auto_stop_alert_stays_constant_while_timer_counts_and_announces_once(
        cx: &mut TestAppContext,
    ) {
        use broscribe_platform::AnnouncementPriority;
        use broscribe_platform::fakes::FakeAnnouncer;

        use crate::ui::{install_announcer, spoken_log};

        let announcer = Arc::new(FakeAnnouncer::new());
        cx.update(|cx| install_announcer(announcer.clone(), cx));
        let h = Harness::new(cx);
        let draw = |h: &Harness, cx: &mut TestAppContext| {
            cx.update_window(h.window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            cx.run_until_parked();
        };
        let auto_stop_event = |remaining_s: Option<u64>| {
            let mut ev = recording("s1");
            if let Event::State(state) = &mut ev
                && let Some(rec) = state.recording.as_mut()
            {
                rec.auto_stop = Some(AutoStopStatus {
                    enabled: true,
                    app: Some(AutoStopApp {
                        app_id: "zoom".into(),
                        app_name: "Zoom".into(),
                    }),
                    generation: 1,
                    reader_available: true,
                    pending: remaining_s.map(|r| broscribe_ipc::protocol::AutoStopPending {
                        reason: AutoStopReason::CallEnded,
                        remaining_s: r,
                    }),
                    candidates: Vec::new(),
                });
            }
            ev
        };

        // Initial active recording (no pending auto-stop).
        h.event(cx, auto_stop_event(None));
        draw(&h, cx);
        announcer.clear();

        // Pending auto-stop starts at 10 s: Alert announces once.
        h.event(cx, auto_stop_event(Some(10)));
        draw(&h, cx);
        let named_10 = h
            .window
            .update(cx, |view, window, cx| {
                spoken_log::while_building(|| Render::render(view, window, cx)).1
            })
            .unwrap();
        let alert_10 = named_10
            .iter()
            .find(|(id, _, _)| id == "auto-stop-pending")
            .cloned()
            .expect("auto-stop alert rendered");
        let timer_10 = named_10
            .iter()
            .find(|(id, _, _)| id == "auto-stop-countdown")
            .cloned()
            .expect("auto-stop countdown timer rendered");
        assert_eq!(alert_10.1, Role::Alert);
        assert_eq!(
            alert_10.2,
            "Zoom shows the call has ended. Saving automatically unless you keep recording."
        );
        assert_eq!(timer_10.1, Role::Timer);
        assert_eq!(timer_10.2, "10 seconds");
        assert_eq!(
            announcer.take(),
            vec![(alert_10.2.clone(), AnnouncementPriority::High)]
        );

        // Countdown ticks to 8 s and meters update: Alert label stays
        // identical, Timer value updates to "8 seconds", and nothing repeats.
        h.event(cx, auto_stop_event(Some(8)));
        h.event(
            cx,
            Event::Meters {
                me_dbfs: -22.0,
                others_dbfs: -31.0,
            },
        );
        draw(&h, cx);
        let named_8 = h
            .window
            .update(cx, |view, window, cx| {
                spoken_log::while_building(|| Render::render(view, window, cx)).1
            })
            .unwrap();
        let alert_8 = named_8
            .iter()
            .find(|(id, _, _)| id == "auto-stop-pending")
            .cloned()
            .expect("auto-stop alert rendered");
        let timer_8 = named_8
            .iter()
            .find(|(id, _, _)| id == "auto-stop-countdown")
            .cloned()
            .expect("auto-stop countdown timer rendered");
        assert_eq!(alert_8, alert_10);
        assert_eq!(timer_8.1, Role::Timer);
        assert_eq!(timer_8.2, "8 seconds");
        assert!(
            announcer.take().is_empty(),
            "countdown and meter updates must not re-announce the auto-stop alert"
        );
    }

    #[gpui::test]
    fn retry_status_changes_only_with_attempt_while_countdown_timer_ticks(cx: &mut TestAppContext) {
        use broscribe_platform::AnnouncementPriority;
        use broscribe_platform::fakes::FakeAnnouncer;

        use crate::ui::{install_announcer, spoken_log};

        let announcer = Arc::new(FakeAnnouncer::new());
        cx.update(|cx| install_announcer(announcer.clone(), cx));
        let h = Harness::new(cx);
        let retrying_event = |attempts: u32, retry_in_s: u64, error: &str| {
            daemon_state(
                None,
                json!([{
                    "session_id": "s1",
                    "title": "Synthetic sync",
                    "tags": [],
                    "stopped_at": "2026-11-14T11:00:00Z",
                    "state": {
                        "phase": "retrying",
                        "attempts": attempts,
                        "retry_in_s": retry_in_s,
                        "error": error
                    }
                }]),
                json!([]),
            )
        };

        let draw = |h: &Harness, cx: &mut TestAppContext| {
            cx.update_window(h.window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            cx.run_until_parked();
        };

        // Initial idle baseline.
        h.event(cx, daemon_state(None, json!([]), json!([])));
        draw(&h, cx);
        announcer.clear();

        // Attempt 1 with 12 s countdown: Status announces once.
        h.event(cx, retrying_event(1, 12, "worker crashed"));
        draw(&h, cx);
        let named_a1_12 = h
            .window
            .update(cx, |view, window, cx| {
                spoken_log::while_building(|| Render::render(view, window, cx)).1
            })
            .unwrap();
        let status_a1_12 = named_a1_12
            .iter()
            .find(|(id, _, _)| id == "stopped-state-s1-attempt-1")
            .cloned()
            .expect("retry status rendered");
        let timer_a1_12 = named_a1_12
            .iter()
            .find(|(id, _, _)| id == "retry-countdown-s1")
            .cloned()
            .expect("retry countdown timer rendered");
        assert_eq!(status_a1_12.1, Role::Status);
        assert_eq!(status_a1_12.2, "Couldn't transcribe.");
        assert_eq!(timer_a1_12.1, Role::Timer);
        assert_eq!(timer_a1_12.2, "12 seconds");
        assert_eq!(
            announcer
                .take()
                .into_iter()
                .filter(|(words, _)| words.starts_with("Couldn't"))
                .collect::<Vec<_>>(),
            vec![(status_a1_12.2.clone(), AnnouncementPriority::Medium)]
        );

        // Countdown ticks to 8 s on the same attempt: Status stays identical,
        // Timer updates, no new announcement.
        h.event(cx, retrying_event(1, 8, "worker crashed"));
        draw(&h, cx);
        let named_a1_8 = h
            .window
            .update(cx, |view, window, cx| {
                spoken_log::while_building(|| Render::render(view, window, cx)).1
            })
            .unwrap();
        let status_a1_8 = named_a1_8
            .iter()
            .find(|(id, _, _)| id == "stopped-state-s1-attempt-1")
            .cloned()
            .expect("retry status rendered");
        let timer_a1_8 = named_a1_8
            .iter()
            .find(|(id, _, _)| id == "retry-countdown-s1")
            .cloned()
            .expect("retry countdown timer rendered");
        assert_eq!(status_a1_8, status_a1_12);
        assert_eq!(timer_a1_8.1, Role::Timer);
        assert_eq!(timer_a1_8.2, "8 seconds");
        assert!(
            announcer.take().is_empty(),
            "countdown tick must not re-announce the retry status"
        );

        // Attempt 2 fails: Status changes and announces once.
        h.event(cx, retrying_event(2, 20, "worker crashed"));
        draw(&h, cx);
        let named_a2 = h
            .window
            .update(cx, |view, window, cx| {
                spoken_log::while_building(|| Render::render(view, window, cx)).1
            })
            .unwrap();
        let status_a2 = named_a2
            .iter()
            .find(|(id, _, _)| id == "stopped-state-s1-attempt-2")
            .cloned()
            .expect("retry status rendered");
        assert_eq!(status_a2.2, "Couldn't transcribe.");
        assert_eq!(
            announcer.take(),
            vec![(status_a2.2, AnnouncementPriority::Medium)]
        );
    }

    #[gpui::test]
    fn track_frames_and_disk_bytes_do_not_spam_live_announcements(cx: &mut TestAppContext) {
        use broscribe_platform::AnnouncementPriority;
        use broscribe_platform::fakes::FakeAnnouncer;

        use crate::ui::install_announcer;

        let announcer = Arc::new(FakeAnnouncer::new());
        cx.update(|cx| install_announcer(announcer.clone(), cx));
        let h = Harness::new(cx);
        let with_health = |dropped: u64, free_bytes: u64, disk_level: DiskLevel| {
            let mut ev = recording("s1");
            if let Event::State(state) = &mut ev
                && let Some(rec) = state.recording.as_mut()
            {
                rec.health.tracks = vec![TrackHealth {
                    track: Track::Mic,
                    state: TrackState::Capturing,
                    dropped_frames: dropped,
                    failures: 0,
                    last_error: None,
                    retry_in_s: None,
                }];
                rec.health.disk = Some(DiskHealth {
                    free_bytes,
                    headroom_s: 5400.0,
                    level: disk_level,
                });
            }
            ev
        };

        let draw = |h: &Harness, cx: &mut TestAppContext| {
            cx.update_window(h.window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            cx.run_until_parked();
        };

        // Baseline: Ok disk, 0 dropped frames.
        h.event(cx, with_health(0, 10_000_000_000, DiskLevel::Ok));
        draw(&h, cx);
        announcer.clear();

        // Dropped frames increment and disk free space ticks down while still Ok:
        // neither is live, so zero announcements.
        h.event(cx, with_health(120, 9_500_000_000, DiskLevel::Ok));
        draw(&h, cx);
        assert!(announcer.take().is_empty());

        // Disk transitions to Warning: Alert announces once.
        h.event(cx, with_health(240, 2_500_000_000, DiskLevel::Warning));
        draw(&h, cx);
        assert_eq!(
            announcer.take(),
            vec![(
                disk_alert_text(DiskLevel::Warning).unwrap().to_owned(),
                AnnouncementPriority::High,
            )]
        );

        // Disk free space ticks further while still Warning: no repeat alert.
        h.event(cx, with_health(360, 2_300_000_000, DiskLevel::Warning));
        draw(&h, cx);
        assert!(announcer.take().is_empty());

        // Disk transitions to Low: Alert announces once.
        h.event(cx, with_health(360, 400_000_000, DiskLevel::Low));
        draw(&h, cx);
        assert_eq!(
            announcer.take(),
            vec![(
                disk_alert_text(DiskLevel::Low).unwrap().to_owned(),
                AnnouncementPriority::High,
            )]
        );
    }

    #[gpui::test]
    fn separate_meter_names_report_sound_pause_retry_and_denied_access(cx: &mut TestAppContext) {
        use crate::ui::{a11y_tree, kit::dialog_gate::click};
        use broscribe_ipc::CapturePermission;
        let h = Harness::new(cx);
        let with_access = |mut event: Event, permission| {
            let Event::State(state) = &mut event else {
                unreachable!()
            };
            state.access.microphone = permission;
            event
        };
        let denied = with_access(recording("s1"), CapturePermission::Denied);
        h.event(cx, denied);
        h.event(
            cx,
            Event::Meters {
                me_dbfs: -20.,
                others_dbfs: -26.,
            },
        );
        let vcx = &mut gpui::VisualTestContext::from_window(h.window.into(), cx);
        vcx.simulate_resize(size(px(450.), px(900.)));
        let mut events = Vec::new();
        let state = h
            .window
            .update(vcx, |panel, _, _| panel.state.clone())
            .unwrap();
        let received = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let _subscription = vcx.update(|_, cx| {
            let received = received.clone();
            cx.subscribe(&state, move |_, event: &crate::state::AppEvent, _| {
                received.borrow_mut().push(event.clone())
            })
        });
        let tree = vcx.update(a11y_tree::draw_and_read);
        assert_eq!(
            tree.read(tree.find("audio-levels")),
            ("Group", "Audio levels")
        );
        assert_eq!(tree.read(tree.find("meter-me")), ("Meter", "Me: no access"));
        assert_eq!(
            tree.read(tree.find("meter-others")),
            ("Meter", "Others: recording, sound")
        );
        assert_eq!(
            tree.read(tree.find("capture-access-0")),
            (
                "Alert",
                "BroScribe Agent can't use the microphone, so Me is recording silence."
            )
        );
        click(vcx, "open-capture-privacy-0", Role::Button);
        events.append(&mut received.borrow_mut());
        assert_eq!(events, [crate::state::AppEvent::OpenMicrophonePrivacy]);
        h.event(
            vcx,
            with_access(
                recording_with("s1", |recording| {
                    recording.health.tracks = vec![TrackHealth {
                        track: Track::System,
                        state: TrackState::Waiting,
                        dropped_frames: 0,
                        failures: 1,
                        last_error: None,
                        retry_in_s: None,
                    }];
                }),
                CapturePermission::Granted,
            ),
        );
        let tree = vcx.update(a11y_tree::draw_and_read);
        assert!(tree.all("capture-access-0").is_empty());
        assert_eq!(
            tree.read(tree.find("meter-me")),
            ("Meter", "Me: recording, sound")
        );
        assert_eq!(
            tree.read(tree.find("meter-others")),
            ("Meter", "Others: retrying")
        );
        h.event(
            vcx,
            recording_with("s1", |recording| recording.status = RecordingStatus::Paused),
        );
        let tree = vcx.update(a11y_tree::draw_and_read);
        assert_eq!(tree.read(tree.find("meter-me")), ("Meter", "Me: paused"));
        assert_eq!(
            tree.read(tree.find("meter-others")),
            ("Meter", "Others: paused")
        );
    }

    /// The agent's sequence: the health reports Warning, then the one-off
    /// low-disk warning arrives, then the headroom changes, then the disk
    /// is Low. The latest health always shows (its numbers non-live), the
    /// warning adds no second alert, and Low still says the recording is
    /// stopping.
    #[gpui::test]
    fn a_low_disk_warning_never_hides_later_disk_health(cx: &mut TestAppContext) {
        use broscribe_platform::AnnouncementPriority;
        use broscribe_platform::fakes::FakeAnnouncer;

        use crate::ui::{install_announcer, spoken_log};

        let announcer = Arc::new(FakeAnnouncer::new());
        cx.update(|cx| install_announcer(announcer.clone(), cx));
        let h = Harness::new(cx);
        let disk = |free_bytes: u64, headroom_s: f64, level: DiskLevel| DiskHealth {
            free_bytes,
            headroom_s,
            level,
        };
        let with_disk = |disk: DiskHealth| recording_with("s1", |r| r.health.disk = Some(disk));
        let step = |h: &Harness, cx: &mut TestAppContext, event: Event| {
            h.event(cx, event);
            cx.update_window(h.window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            cx.run_until_parked();
            let named = h
                .window
                .update(cx, |view, window, cx| {
                    spoken_log::while_building(|| Render::render(view, window, cx)).1
                })
                .unwrap();
            let disk_nodes: Vec<(String, Role, String)> = named
                .into_iter()
                .filter(|(id, _, _)| id.starts_with("disk-") || id.starts_with("capture-warning"))
                .collect();
            (disk_nodes, announcer.take())
        };
        let warning_alert = disk_alert_text(DiskLevel::Warning).unwrap().to_owned();
        let low_alert = disk_alert_text(DiskLevel::Low).unwrap().to_owned();
        let node = |id: &str, role: Role, text: &str| (id.to_owned(), role, text.to_owned());

        step(
            &h,
            cx,
            with_disk(disk(40_000_000_000, 90_000., DiskLevel::Ok)),
        );
        // 1. The health reports Warning: one alert.
        let warned = disk(2_500_000_000, 3_000., DiskLevel::Warning);
        let (nodes, spoken) = step(&h, cx, with_disk(warned));
        assert_eq!(
            nodes,
            [
                node("disk-health-alert-0", Role::Alert, &warning_alert),
                node("disk-health-0", Role::Label, &spoken_disk_space(&warned)),
            ]
        );
        assert_eq!(
            spoken,
            [(warning_alert.clone(), AnnouncementPriority::High)]
        );

        // 2. The agent's warning: its thresholds join as a label, no alert.
        let (nodes, spoken) = step(
            &h,
            cx,
            Event::CaptureWarning {
                session_id: "s1".into(),
                warning: CaptureWarning::DiskLow {
                    free_bytes: 2_500_000_000,
                    headroom_s: 3_000.,
                    stop_below_bytes: 500_000_000,
                    stop_below_s: 600.,
                },
            },
        );
        let stop = disk_stop_text(500_000_000, 600., crate::ui::format::spoken_length);
        assert_eq!(
            nodes,
            [
                node("disk-health-alert-0", Role::Alert, &warning_alert),
                node("disk-health-0", Role::Label, &spoken_disk_space(&warned)),
                node("disk-stop-0", Role::Label, &stop),
            ]
        );
        assert!(spoken.is_empty(), "one warning, one alert: {spoken:?}");

        // 3. The headroom changes: the label follows, nothing is spoken.
        let lower = disk(1_200_000_000, 1_400., DiskLevel::Warning);
        let (nodes, spoken) = step(&h, cx, with_disk(lower));
        assert_eq!(
            nodes,
            [
                node("disk-health-alert-0", Role::Alert, &warning_alert),
                node("disk-health-0", Role::Label, &spoken_disk_space(&lower)),
                node("disk-stop-0", Role::Label, &stop),
            ]
        );
        assert!(spoken.is_empty(), "{spoken:?}");

        // 4. Low: the stopping alert speaks once.
        let low = disk(400_000_000, 200., DiskLevel::Low);
        let (nodes, spoken) = step(&h, cx, with_disk(low));
        assert_eq!(
            nodes,
            [
                node("disk-health-alert-0", Role::Alert, &low_alert),
                node("disk-health-0", Role::Label, &spoken_disk_space(&low)),
            ]
        );
        assert_eq!(spoken, [(low_alert, AnnouncementPriority::High)]);
    }

    /// The other order: the agent's one-off warning arrives while the last
    /// health still says Ok. It shows at once as the Warning alert, with
    /// its own numbers; the Warning health that follows keeps that node and
    /// its words; Low replaces them; a recovered Ok removes the alert.
    /// Exactly one Warning and one Low announcement, none for the numbers.
    #[gpui::test]
    fn a_low_disk_warning_before_its_health_speaks_once(cx: &mut TestAppContext) {
        use broscribe_platform::AnnouncementPriority;
        use broscribe_platform::fakes::FakeAnnouncer;

        use crate::ui::{install_announcer, spoken_log};

        let announcer = Arc::new(FakeAnnouncer::new());
        cx.update(|cx| install_announcer(announcer.clone(), cx));
        let h = Harness::new(cx);
        let disk = |free_bytes: u64, headroom_s: f64, level: DiskLevel| DiskHealth {
            free_bytes,
            headroom_s,
            level,
        };
        let with_disk = |disk: DiskHealth| recording_with("s1", |r| r.health.disk = Some(disk));
        let step = |h: &Harness, cx: &mut TestAppContext, event: Event| {
            h.event(cx, event);
            cx.update_window(h.window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            cx.run_until_parked();
            let named = h
                .window
                .update(cx, |view, window, cx| {
                    spoken_log::while_building(|| Render::render(view, window, cx)).1
                })
                .unwrap();
            let disk_nodes: Vec<(String, Role, String)> = named
                .into_iter()
                .filter(|(id, _, _)| id.starts_with("disk-") || id.starts_with("capture-warning"))
                .collect();
            (disk_nodes, announcer.take())
        };
        let warning_alert = disk_alert_text(DiskLevel::Warning).unwrap().to_owned();
        let low_alert = disk_alert_text(DiskLevel::Low).unwrap().to_owned();
        let node = |id: &str, role: Role, text: &str| (id.to_owned(), role, text.to_owned());
        let mut spoken_all = Vec::new();

        // Ok: the numbers only, nothing spoken.
        let ok = disk(40_000_000_000, 90_000., DiskLevel::Ok);
        h.event(cx, with_disk(ok));
        h.act(cx, |view, _, cx| {
            view.tab = RecordingPanelTab::Details;
            cx.notify();
        });
        let (nodes, spoken) = step(&h, cx, with_disk(ok));
        assert_eq!(
            nodes,
            [node("disk-health-0", Role::Label, &spoken_disk_space(&ok))]
        );
        assert!(spoken.iter().all(|(text, _)| !text.contains("Disk")));

        // 1. The agent's warning while the health still says Ok: the
        // Warning alert at once, the warning's own numbers, its thresholds.
        let (nodes, spoken) = step(
            &h,
            cx,
            Event::CaptureWarning {
                session_id: "s1".into(),
                warning: CaptureWarning::DiskLow {
                    free_bytes: 2_500_000_000,
                    headroom_s: 3_000.,
                    stop_below_bytes: 500_000_000,
                    stop_below_s: 600.,
                },
            },
        );
        let stop = disk_stop_text(500_000_000, 600., crate::ui::format::spoken_length);
        let warned = disk(2_500_000_000, 3_000., DiskLevel::Warning);
        assert_eq!(
            nodes,
            [
                node("disk-health-alert-0", Role::Alert, &warning_alert),
                node("disk-health-0", Role::Label, &spoken_disk_space(&warned)),
                node("disk-stop-0", Role::Label, &stop),
            ]
        );
        spoken_all.extend(spoken);

        // 2. The Warning health follows: same node, same words; the numbers
        // are the health's now.
        let health = disk(2_400_000_000, 2_900., DiskLevel::Warning);
        let (nodes, spoken) = step(&h, cx, with_disk(health));
        assert_eq!(
            nodes,
            [
                node("disk-health-alert-0", Role::Alert, &warning_alert),
                node("disk-health-0", Role::Label, &spoken_disk_space(&health)),
                node("disk-stop-0", Role::Label, &stop),
            ]
        );
        assert!(spoken.is_empty(), "one warning, one alert: {spoken:?}");

        // 3. Low: the stopping alert.
        let low = disk(400_000_000, 200., DiskLevel::Low);
        let (nodes, spoken) = step(&h, cx, with_disk(low));
        assert_eq!(
            nodes,
            [
                node("disk-health-alert-0", Role::Alert, &low_alert),
                node("disk-health-0", Role::Label, &spoken_disk_space(&low)),
            ]
        );
        spoken_all.extend(spoken);

        // 4. Recovered: the alert and the warning are gone.
        let recovered = disk(30_000_000_000, 80_000., DiskLevel::Ok);
        let (nodes, spoken) = step(&h, cx, with_disk(recovered));
        assert_eq!(
            nodes,
            [node(
                "disk-health-0",
                Role::Label,
                &spoken_disk_space(&recovered)
            )]
        );
        assert!(spoken.is_empty(), "{spoken:?}");

        assert_eq!(
            spoken_all,
            [
                (warning_alert, AnnouncementPriority::High),
                (low_alert, AnnouncementPriority::High),
            ]
        );
    }

    /// Missed live updates: the live notice keeps its words while the
    /// count beside it grows.
    #[gpui::test]
    fn missed_updates_speak_once_while_their_count_grows(cx: &mut TestAppContext) {
        use broscribe_platform::AnnouncementPriority;
        use broscribe_platform::fakes::FakeAnnouncer;

        use crate::ui::{install_announcer, spoken_log};

        let announcer = Arc::new(FakeAnnouncer::new());
        cx.update(|cx| install_announcer(announcer.clone(), cx));
        let h = Harness::new(cx);
        let step = |h: &Harness, cx: &mut TestAppContext, event: Event| {
            h.event(cx, event);
            cx.update_window(h.window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            cx.run_until_parked();
            let named = h
                .window
                .update(cx, |view, window, cx| {
                    spoken_log::while_building(|| Render::render(view, window, cx)).1
                })
                .unwrap();
            let nodes: Vec<(String, Role, String)> = named
                .into_iter()
                .filter(|(id, _, _)| id.starts_with("live-missed"))
                .collect();
            (nodes, announcer.take())
        };
        step(&h, cx, recording("s1"));
        let notice = missed_updates_notice().to_owned();
        let expected = |count: &str| {
            vec![
                ("live-missed".to_owned(), Role::Status, notice.clone()),
                (
                    "live-missed-count".to_owned(),
                    Role::Label,
                    count.to_owned(),
                ),
            ]
        };

        let (nodes, spoken) = step(&h, cx, Event::Lagged { dropped: 3 });
        assert_eq!(nodes, expected("3 updates missed"));
        assert_eq!(spoken, [(notice.clone(), AnnouncementPriority::Medium)]);
        let (nodes, spoken) = step(&h, cx, Event::Lagged { dropped: 9 });
        assert_eq!(nodes, expected("12 updates missed"));
        assert!(spoken.is_empty(), "{spoken:?}");
    }

    /// While a model loads, its live notice keeps the same words and only
    /// elapsed values in worker updates do not turn into frozen counters.
    #[gpui::test]
    fn model_loading_notices_stay_put_without_frozen_counters(cx: &mut TestAppContext) {
        use broscribe_platform::AnnouncementPriority;
        use broscribe_platform::fakes::FakeAnnouncer;

        use crate::ui::{install_announcer, spoken_log};

        let announcer = Arc::new(FakeAnnouncer::new());
        cx.update(|cx| install_announcer(announcer.clone(), cx));
        let h = Harness::new(cx);
        h.event(cx, recording("s1"));
        cx.update_window(h.window.into(), |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        cx.run_until_parked();
        announcer.clear();
        let loading = |h: &Harness, cx: &mut TestAppContext, elapsed_ms: u64| {
            h.event(
                cx,
                Event::Preview {
                    session_id: "s1".into(),
                    preview: broscribe_ipc::PreviewState::Loading {
                        model: Some("synthetic-asr".into()),
                        elapsed_ms,
                    },
                },
            );
            h.event(
                cx,
                Event::LiveIdentity {
                    session_id: "s1".into(),
                    identity: LiveIdentityState {
                        status: LiveIdentityStatus::Loading { elapsed_ms },
                        epoch: 1,
                    },
                },
            );
            cx.update_window(h.window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
            cx.run_until_parked();
            let named = h
                .window
                .update(cx, |view, window, cx| {
                    spoken_log::while_building(|| Render::render(view, window, cx)).1
                })
                .unwrap();
            let nodes: Vec<(String, Role, String)> = named
                .into_iter()
                .filter(|(id, _, _)| {
                    id.starts_with("live-preview") || id.starts_with("live-identity")
                })
                .collect();
            (nodes, announcer.take())
        };
        let preview = preview_notice(&broscribe_ipc::PreviewState::Loading {
            model: Some("synthetic-asr".into()),
            elapsed_ms: 0,
        })
        .unwrap();
        let identity = identity_notice(&LiveIdentityState {
            status: LiveIdentityStatus::Loading { elapsed_ms: 0 },
            epoch: 1,
        })
        .unwrap();
        let expected = || {
            vec![
                ("live-identity".to_owned(), Role::Status, identity.clone()),
                ("live-preview".to_owned(), Role::Status, preview.clone()),
            ]
        };

        let (nodes, spoken) = loading(&h, cx, 3_200);
        assert_eq!(nodes, expected());
        assert_eq!(
            spoken,
            [
                (preview.clone(), AnnouncementPriority::Medium),
                (identity.clone(), AnnouncementPriority::Medium),
            ]
        );
        let (nodes, spoken) = loading(&h, cx, 9_800);
        assert_eq!(nodes, expected());
        assert!(spoken.is_empty(), "{spoken:?}");
    }
}
