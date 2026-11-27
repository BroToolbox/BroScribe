//! The meeting view (design §4, plan §4.2; §8 items 4, 12, 14, 15, 18):
//! header (title with Rename, date, duration, participant chips, editable
//! tags, Export… and Delete meeting…), a
//! Transcript tab (`[hh:mm:ss] Name: text`; clicking a line plays from there;
//! the current line is highlighted while audio plays; each line's "Name…"
//! opens the shared [`PersonPicker`] for every line of its speaker or only
//! that line, an attribution overlay that never changes speakers), a
//! Speakers tab (voice samples through the shared [`SampleControls`](super::speaker_review::SampleControls), accept
//! / reject a suggestion, name or rename through the picker, "Can't tell"
//! to dismiss a speaker from review with Undo / Restore, and the result of
//! the speaker recheck a naming here started), an Overview with the
//! [`GeneratePanel`], summary, decisions and actions, and a Templates tab
//! ([`TemplatesTab`]). The header holds a playback bar and opens the Export
//! dialog ([`ExportForm`]). Rename
//! runs `title <id> --stdin` and Delete meeting… the shared
//! [`DeleteMeetingPrompt`] (`delete <id> --dry-run`, then `--yes`), both as
//! CLI children. A
//! picker's choice names what it was opened for; leaving the tab or the
//! meeting, or its target disappearing, cancels it.
//!
//! Execution paths (plan §8 item 14):
//! - reads and library writes (`ops::tag`, `ops::name_speaker` with the
//!   centroid fallback, `ops::reject_suggestion`, marking a vanished
//!   recording `missing`, enqueueing jobs) run on GPUI's background
//!   executor and deliver their result through a foreground task;
//! - naming queues a `confirm-samples` job, core requests the speaker
//!   recheck (`reidentify`) in the same write, and playback of formats the
//!   player cannot open queues a `playback-cache` job; all run in the queue
//!   worker (the agent's, or the `queue work` CLI child: no native model in
//!   the GUI process, item 18);
//! - the player lives on the main thread in [`AppState`]; a 4 Hz tick runs
//!   only while this view's audio plays.
//!
//! Mounting: create the view with [`MeetingView::new`] inside a window
//! (`cx.new(|cx| MeetingView::new(state, meeting_id, window, cx))`) and render
//! the entity as the main area's content; dropping the entity stops playback
//! it started.

mod header;
mod history;
mod overview;
mod saved_transcript;
pub(crate) mod speakers;

pub(crate) use header::RemoveMeetingTag;

gpui::actions!(meeting_tabs, [NextMeetingTab, PreviousMeetingTab]);
pub(crate) use saved_transcript::{NameTranscriptLine, PlayTranscriptLine};

use crate::ui::kit::{self, StatusKind, button};
use broscribe_platform::SystemSettingsOpener as _;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use broscribe_core::config::Thresholds;
use broscribe_core::identity;
use broscribe_core::index::Index;
use broscribe_core::learning::LearningRequest;
use broscribe_core::live_identity::{LiveOutcome, LiveReviewItem, live_review_items};
use broscribe_core::ops::{self, SampleClip, SegmentFingerprint, SpeakerFence, SpeakerState};
use broscribe_core::queue::{CONFIRM_KIND, JobId, JobState, PLAYBACK_KIND, Queue, VECTORS_KIND};
use broscribe_core::render::{
    NameStyle, PeopleLookup, enrichment_who, speaker_label, substitute_speakers,
};
use broscribe_core::schema::{
    ActionStatus, Assignment, Enrichment, GroupOrigin, Meeting, ProcessingStatus, RecordingState,
    ReviewNote, ReviewSource,
};
use broscribe_core::transcript::{
    self, TranscriptFilter, TranscriptLine, line_fingerprints, merge_lines,
};
use broscribe_core::{Error, Library, MeetingId, PersonId, Result, SpeakerId};
use broscribe_platform::PlayerState;
use broscribe_platform::player::{PLAYBACK_CANCELLED, PLAYBACK_NEEDS_CACHE, PLAYBACK_SUPERSEDED};
use broscribe_platform_native::NativePlatform;
use gpui::component::button::{Button, ButtonVariants as _};
use gpui::component::input::{Input, InputEvent, InputState};
use gpui::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui::component::slider::{Slider, SliderEvent, SliderState};
use gpui::component::tooltip::Tooltip;
use gpui::component::{ActiveTheme as _, Sizable as _, WindowExt as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, ClipboardItem, Context, Div, ElementId, Entity,
    Focusable as _, FontWeight, InteractiveElement as _, IntoElement, KeyDownEvent, ListState,
    ParentElement as _, Render, Role, ScrollHandle, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, Window, div, px, rems,
};
use serde_json::json;

use crate::generation::GenerationTarget;
use crate::jobs::{ChildKind, CliError, core_user_text, failure_text, meeting_user_text, sentence};
use crate::state::{AppState, Route};
use crate::ui::format::{length, spoken_length, spoken_position};
#[cfg(test)]
use crate::ui::format::{local_date_time, spoken_date_time};
use crate::ui::type_scale::{TextRole, TextRoleExt as _, tabular};
use crate::ui::{Spoken as _, format_clock, text_line};
use crate::views::delete_meeting::{DeleteEvent, DeleteMeetingPrompt};
use crate::views::export::{ExportEvent, ExportForm};
use crate::views::generate::{GenerateEvent, GeneratePanel, enrich_job_id, job_state};
use crate::views::people::{Clip, ClipSource, SamplePlayback};
use crate::views::person_picker::{PersonChoice, PersonPicker, PersonPickerEvent, VoiceTarget};
use crate::views::speaker_review::{SampleCursor, clip_of, set_review_dismissed};
use crate::views::templates::TemplatesTab;

/// Playback position updates while audio plays (4 Hz).
pub const PLAYBACK_TICK: Duration = Duration::from_millis(250);
/// A slider change not followed by a release (keyboard steps) seeks after this.
const SCRUB_SETTLE: Duration = Duration::from_millis(400);

// ---- pure helpers (tested) ----

/// Queue key of the `confirm-samples` job for one naming.
pub fn confirm_samples_key(
    meeting: &MeetingId,
    speaker: SpeakerId,
    person: &PersonId,
    model: &str,
) -> String {
    format!("{meeting}/{speaker}/{person}/{model}")
}

pub use broscribe_core::ops::{playback_cache_key, playback_cache_path};

/// The meeting's transcript lines as every front end shows them (the one
/// projection, [`broscribe_core::transcript`]).
fn shown_lines(meeting: &Meeting, people: &PeopleLookup) -> Vec<TranscriptLine> {
    merge_lines(&transcript::transcript(
        meeting,
        people,
        &TranscriptFilter::default(),
    ))
}

/// `transcript attribute <meeting> --fingerprints-stdin [--clear]` (the
/// runner adds `--library <lib> --json`); the fingerprints and the person go
/// on stdin ([`attribute_stdin`]), so a typed name never reaches argv.
pub fn attribute_args(meeting_id: &MeetingId, clear: bool) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "transcript".into(),
        "attribute".into(),
        meeting_id.as_str().into(),
        "--fingerprints-stdin".into(),
    ];
    if clear {
        args.push("--clear".into());
    }
    args
}

/// `{"fingerprints":[{"track","start","end","text_sha256"}],"person":"<ref>",
/// "fence":{"speaker_namespace","grouping_revision"}}`; no `person` key when
/// clearing. The fence is the speakers the line was read with: the CLI
/// refuses the line once they were reanalysed or regrouped.
pub fn attribute_stdin(
    fingerprints: &[SegmentFingerprint],
    person_ref: Option<&str>,
    fence: &SpeakerFence,
) -> Vec<u8> {
    let mut input = json!({ "fingerprints": fingerprints, "fence": fence });
    if let Some(person) = person_ref {
        input["person"] = person.into();
    }
    serde_json::to_vec(&input).expect("fingerprints serialize")
}

/// Sets (or, with `None`, removes) the person of the segments `fingerprints`
/// name — one transcript line — through `broscribe transcript attribute`,
/// which also refreshes the search vectors of the changed windows; returns
/// the person's display name. The CLI refuses (`library_busy`) while the
/// meeting is processed or enriched, (`invalid_input`) when the transcript
/// changed meanwhile, and (`speakers_regrouped` / `speakers_reanalysed`)
/// when the speakers changed since `fence`. Blocks: background executor
/// only.
pub fn attribute_line(
    lib: &Library,
    cli: &crate::jobs::CliRunner,
    meeting_id: &MeetingId,
    fingerprints: &[SegmentFingerprint],
    person_ref: Option<&str>,
    fence: &SpeakerFence,
) -> std::result::Result<Option<String>, CliError> {
    let out = cli.run_blocking_stdin(
        &attribute_args(meeting_id, person_ref.is_none()),
        crate::jobs::ChildKind::Command,
        attribute_stdin(fingerprints, person_ref, fence),
    )?;
    let Some(person) = out["person_id"].as_str() else {
        return Ok(None);
    };
    let person = PersonId::parse(person).map_err(|_| {
        CliError::new(
            "bad_output",
            "the CLI answered with a person id that couldn't be read",
        )
    })?;
    let people = lib.people_lookup().map_err(|e| CliError::from_core(&e))?;
    Ok(Some(person_name(&people, &person)))
}

/// What a refused line naming tells the user, by the error's `code`;
/// `cause` is the refusal in words ([`CliError::user_text`],
/// [`core_user_text`]), never the code.
pub fn attribution_refusal(code: &str, cause: &str) -> String {
    match code {
        "library_busy" => "This meeting is busy (still processing, or a summary is being \
                           generated); try again when it finishes."
            .to_owned(),
        "speakers_regrouped" | "speakers_reanalysed" => regrouped_refusal(code).to_owned(),
        // The CLI says why in words of its own.
        "invalid_input" if !cause.is_empty() => cause.to_owned(),
        _ => failure_text("Couldn't name the line.", cause, ""),
    }
}

/// What a write refused by the speaker fence ([`ops::check_speaker_fence`])
/// tells the user.
pub fn regrouped_refusal(code: &str) -> &'static str {
    match code {
        "speakers_reanalysed" => "This meeting was reanalysed; review its speakers again.",
        _ => "The speakers were regrouped; review again.",
    }
}

/// Index of the line playing at `position_s`: the last one starting at or
/// before it.
pub fn line_at(lines: &[TranscriptLine], position_s: f64) -> Option<usize> {
    lines
        .partition_point(|l| l.start <= position_s)
        .checked_sub(1)
}

#[derive(Clone, Debug)]
pub struct ParticipantChip {
    name: String,
    person: Option<PersonId>,
}

/// Named people first (in speaker order), then roster participants not already shown.
pub fn participant_chips(meeting: &Meeting, people: &PeopleLookup) -> Vec<ParticipantChip> {
    let mut ids = std::collections::HashSet::new();
    let mut chips = Vec::new();
    for speaker in ops::effective_speakers(meeting) {
        if let Some(person) = speaker.named_person()
            && ids.insert(person)
        {
            chips.push(ParticipantChip {
                name: person_name(people, person),
                person: Some(person.clone()),
            });
        }
    }
    for participant in &meeting.participants {
        let person = participant.person_id.as_ref();
        if person.is_some_and(|id| !ids.insert(id)) {
            continue;
        }
        let name = participant
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .or_else(|| person.map(|id| person_name(people, id)))
            .or_else(|| participant.email.clone());
        if let Some(name) = name
            && (person.is_some() || !chips.iter().any(|chip| chip.name == name))
        {
            chips.push(ParticipantChip {
                name,
                person: person.cloned(),
            });
        }
    }
    chips
}

fn person_name(people: &PeopleLookup, id: &PersonId) -> String {
    people.display_name(id).unwrap_or(id.as_str()).to_owned()
}

/// A person a speaker was auto-named or suggested as; accepting confirms it.
#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    pub person_id: PersonId,
    pub name: String,
    pub score: Option<f32>,
}

/// A merged group shown as one Speakers-tab row.
#[derive(Clone, Debug, PartialEq)]
pub struct MergedRow {
    /// `G2`, what Unmerge names.
    pub group_id: String,
    pub members: Vec<SpeakerId>,
    pub origin: GroupOrigin,
}

/// One row of the Speakers tab: a shown speaker (a merged group is one row,
/// its kept speaker's).
#[derive(Clone, Debug, PartialEq)]
pub struct SpeakerRow {
    pub id: SpeakerId,
    /// The named person, else `Speaker N`.
    pub label: String,
    pub assignment: Assignment,
    /// The group's total for a merged group.
    pub talk_s: f64,
    pub proposal: Option<Proposal>,
    pub live_source: Option<String>,
    pub identity_conflict: Option<String>,
    pub merged: Option<MergedRow>,
    /// Dismissed from review with "Can't tell" (`ops::review_dismissed`).
    pub review_dismissed: bool,
    /// Voice samples (`ops::sample_clips`), in rank order.
    pub samples: Vec<SampleClip>,
    /// First transcript segment of this shown speaker or one of its members.
    pub transcript_segment: Option<u32>,
}

pub(super) fn live_name_source(lines: usize) -> String {
    let lines = crate::ui::format::count(lines as u64, "line", "lines");
    format!("Named while recording on {lines}")
}

fn raw_label(id: SpeakerId) -> String {
    format!("Speaker {}", id.number())
}

pub fn speaker_rows(meeting: &Meeting, people: &PeopleLookup) -> Vec<SpeakerRow> {
    let mut rows: Vec<SpeakerRow> = ops::effective_speakers(meeting)
        .map(|s| {
            let state = SpeakerState::of(s);
            let proposal = match state.assignment {
                Assignment::Auto | Assignment::Suggested => {
                    state.person_id.as_ref().map(|p| Proposal {
                        person_id: p.clone(),
                        name: person_name(people, p),
                        score: if s.live_suggestion_lines.is_some() {
                            None
                        } else {
                            state.score
                        },
                    })
                }
                Assignment::Confirmed | Assignment::None => None,
            };
            let merged = ops::group_of(meeting, s.id).map(|group| MergedRow {
                group_id: group.id.clone(),
                members: group.members.clone(),
                origin: group.origin,
            });
            SpeakerRow {
                id: s.id,
                label: speaker_label(meeting, Some(s.id), people),
                assignment: state.assignment,
                talk_s: ops::effective_talk_s(meeting, s.id),
                proposal,
                live_source: s
                    .live_suggestion_lines
                    .or_else(|| s.live_promotion.as_ref().map(|p| p.line_count))
                    .map(live_name_source),
                identity_conflict: s.identity_conflict.as_ref().map(|conflict| {
                    let names = conflict
                        .lines
                        .iter()
                        .map(|line| &line.person_id)
                        .chain(conflict.candidate.iter())
                        .map(|id| (id.clone(), person_name(people, id)))
                        .collect();
                    conflict.message(s.id, &names)
                }),
                merged,
                review_dismissed: ops::review_dismissed(meeting, s.id),
                samples: ops::sample_clips(meeting, s.id),
                transcript_segment: ops::first_speaker_segment(meeting, s.id).map(|seg| seg.id),
            }
        })
        .collect();
    rows.sort_by(|left, right| right.talk_s.total_cmp(&left.talk_s));
    rows
}

/// A pair of shown speakers that
/// may be one voice ([`ops::duplicate_suggestions`]); a merge keeps `a`.
#[derive(Clone, Debug, PartialEq)]
pub struct DuplicateRow {
    pub a: SpeakerId,
    pub b: SpeakerId,
    pub score: f32,
    pub text: String,
}

/// The Speakers tab's grouping choices, computed with the voice model and
/// thresholds of the profile ([`grouping_view`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GroupingView {
    pub duplicates: Vec<DuplicateRow>,
    /// "Merge into…" for each shown speaker: the shown speakers on the same
    /// track (never the paired microphone) it may be merged into, in
    /// meeting order.
    pub merge_targets: HashMap<SpeakerId, Vec<SpeakerId>>,
}

/// The shown speakers `speaker`'s row may be merged into: same track, never
/// the paired microphone track, and compatible under the merge rule
/// (no two people proven different, no person one of them rejected).
/// "Not the same person" does not hide a target: merging overrides it.
pub fn merge_targets(
    meeting: &Meeting,
    speaker: SpeakerId,
    model: &str,
    proven: &BTreeMap<SpeakerId, PersonId>,
) -> Vec<SpeakerId> {
    let Some(own) = meeting.speaker(speaker) else {
        return Vec::new();
    };
    if meeting.paired_mic_track() == Some(own.track.as_str()) {
        return Vec::new();
    }
    let unit = |id: SpeakerId| -> Vec<SpeakerId> {
        match ops::group_of(meeting, id) {
            Some(g) => g.speakers().collect(),
            None => vec![id],
        }
    };
    let mine = unit(ops::effective_speaker(meeting, speaker));
    ops::effective_speakers(meeting)
        .filter(|t| t.track == own.track && !mine.contains(&t.id))
        .filter(|t| {
            let inputs: Option<Vec<_>> = mine
                .iter()
                .copied()
                .chain(unit(t.id))
                .map(|id| {
                    let s = meeting.speaker(id)?;
                    identity::merge_input(s, model, proven).ok()
                })
                .collect();
            inputs.is_some_and(|inputs| identity::compatible(&inputs, &[]))
        })
        .map(|t| t.id)
        .collect()
}

/// Possible duplicates and merge targets of `meeting` (reads the caption
/// samples that prove identities: background executor).
pub fn grouping_view(
    lib: &Library,
    meeting: &Meeting,
    people: &PeopleLookup,
    model: &str,
    thresholds: Option<&Thresholds>,
) -> Result<GroupingView> {
    let proven = ops::caption_proven(lib, meeting)?;
    let duplicates = ops::duplicate_suggestions(meeting, model, thresholds, &proven)?
        .into_iter()
        .map(|d| DuplicateRow {
            text: format!(
                "{} may be the same person as {} · voice {:.2}",
                speaker_label(meeting, Some(d.b), people),
                speaker_label(meeting, Some(d.a), people),
                d.score
            ),
            a: d.a,
            b: d.b,
            score: d.score,
        })
        .collect();
    let merge_targets = ops::effective_speakers(meeting)
        .map(|s| (s.id, merge_targets(meeting, s.id, model, &proven)))
        .collect();
    Ok(GroupingView {
        duplicates,
        merge_targets,
    })
}

/// A grouping decision made in the Speakers tab.
#[derive(Clone, Debug, PartialEq)]
pub enum GroupingChange {
    /// `speakers merge <meeting> <kept> <member>…`.
    Merge {
        kept: SpeakerId,
        members: Vec<SpeakerId>,
    },
    /// `speakers unmerge <meeting> <group>`.
    Unmerge { group: String },
    /// `speakers not-same <meeting> <a> <b>`.
    NotSame { a: SpeakerId, b: SpeakerId },
}

/// The notice for a committed grouping change.
pub fn grouping_report(change: &GroupingChange, out: &serde_json::Value) -> String {
    let ids = |v: &serde_json::Value| -> Vec<String> {
        v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|id| id.as_str())
            .filter_map(|id| SpeakerId::parse(id).ok())
            .map(raw_label)
            .collect()
    };
    match change {
        GroupingChange::Merge { .. } => {
            let group = &out["group"];
            let kept = group["kept"]
                .as_str()
                .and_then(|id| SpeakerId::parse(id).ok())
                .map(raw_label)
                .unwrap_or_default();
            let mut text = format!("Merged {} into {kept}.", ids(&group["members"]).join(", "));
            if out["kept_swapped"] == true {
                text.push_str(&format!(
                    " {kept} is kept: it is the confirmed speaker, or the one who talks most."
                ));
            }
            text
        }
        GroupingChange::Unmerge { .. } => {
            let mut speakers = vec![out["group"]["kept"].clone()];
            speakers.extend(
                out["group"]["members"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
            );
            format!(
                "{} show separately again; they won't be merged automatically again.",
                ids(&serde_json::Value::Array(speakers)).join(", ")
            )
        }
        GroupingChange::NotSame { .. } => format!(
            "{} won't be merged automatically or suggested as one speaker again.",
            ids(&out["pair"]).join(" and ")
        ),
    }
}

/// "Couldn't rename the meeting. {Cause}. Your title is still in the field."
/// (WRT-08): a refused rename keeps the draft.
pub fn rename_failure(cause: &str) -> String {
    failure_text(
        "Couldn't rename the meeting.",
        cause,
        "Your title is still in the field.",
    )
}

/// "Couldn't play the recording. {Cause}." (WRT-08, 3.4): the meeting's
/// playback failed.
pub fn playback_failure(cause: &str) -> String {
    failure_text("Couldn't play the recording.", cause, "")
}

/// The cause shown when no player is available.
const NO_PLAYER: &str = "Audio playback isn't available.";

/// What a refused grouping change tells the user, by the error's `code`;
/// `cause` is the refusal in words ([`CliError::user_text`]).
pub fn grouping_refusal(code: &str, cause: &str) -> String {
    match code {
        "speakers_regrouped" | "speakers_reanalysed" => regrouped_refusal(code).to_owned(),
        "library_busy" => "This meeting is busy (still processing, or another job is working \
                           on it); try again when it finishes."
            .to_owned(),
        _ => failure_text("Couldn't change the speakers.", cause, ""),
    }
}

/// The meeting's title as the view shows it.
fn shown_title(meeting: &Meeting) -> String {
    meeting
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or("Untitled meeting")
        .to_owned()
}

/// The Actions table's columns and fixed widths (`None`: takes the rest).
pub const ACTION_COLUMNS: [(&str, Option<f32>); 5] = [
    ("Done", Some(2.5)),
    ("Action", None),
    ("Owner", Some(9.)),
    ("Due", Some(7.)),
    ("Said at", Some(6.5)),
];

/// One row of the Actions table, as shown.
#[derive(Clone, Debug, PartialEq)]
pub struct ActionRow {
    pub id: String,
    pub stale: bool,
    pub dropped: bool,
    pub action: String,
    pub owner: String,
    pub due: Option<chrono::NaiveDate>,
    pub open: bool,
    pub at_s: f64,
    /// Done (the row offers Reopen; else Done).
    pub done: bool,
}

/// The enrichment's actions as table rows: names substituted through the
/// enrichment's line attribution; a status the latest summary no longer
/// produced is marked.
pub fn action_rows(meeting: &Meeting, people: &PeopleLookup) -> Vec<ActionRow> {
    let Some(enrichment) = &meeting.enrichment else {
        return Vec::new();
    };
    let lp = &enrichment.line_people;
    enrichment
        .actions
        .iter()
        .map(|action| ActionRow {
            id: action.id.clone(),
            stale: action.stale,
            dropped: action.status == ActionStatus::Dropped,
            action: substitute_speakers(meeting, lp, &action.text, people),
            owner: enrichment_who(
                meeting,
                lp,
                action.owner_speaker,
                action.owner_name.as_deref(),
                people,
            )
            .unwrap_or_default(),
            due: action.due,
            open: action.status == ActionStatus::Open,
            at_s: action.at_s,
            done: action.status == ActionStatus::Done,
        })
        .collect()
}

/// The line above a summary: visible text and accessible name. The model
/// is the one the provider reported; without a report it is the one
/// requested (marked so), or none at all (the provider's default). A run
/// with custom instructions says so; the accessible name reads them.
#[cfg(test)]
pub fn enrichment_credit(
    enrichment: &Enrichment,
    provider: &str,
    dates: (&str, &str),
) -> (String, String) {
    let (mut text, mut aria) = credit_text(
        "AI summary",
        provider,
        enrichment.model.as_deref(),
        enrichment.requested_model.as_deref(),
        enrichment.model_reported,
        dates,
    );
    if let Some(instructions) = enrichment
        .instructions
        .as_deref()
        .map(str::trim)
        .filter(|i| !i.is_empty())
    {
        text.push_str(" · custom instructions");
        aria.push_str(&format!(" Custom instructions: {instructions}"));
    }
    (text, aria)
}

/// The AI credit and its accessible name,
/// for any generated content (summary or template result).
pub fn credit_text(
    kind: &str,
    provider: &str,
    model: Option<&str>,
    requested: Option<&str>,
    model_reported: bool,
    (when, spoken): (&str, &str),
) -> (String, String) {
    match model {
        Some(model) if model_reported => {
            let asked = match requested {
                Some(req) if req != model => format!(" (requested: {req})"),
                Some(_) => String::new(),
                None => " for its default model".to_owned(),
            };
            (
                format!("{kind} · {provider} · {model} · {when}"),
                format!(
                    "{kind} by {provider} {spoken} using model {model}, as reported \
                     by the provider{asked}."
                ),
            )
        }
        Some(model) => (
            format!("{kind} · {provider} · {model} (requested) · {when}"),
            format!(
                "{kind} by {provider} {spoken}. Model {model} was requested; the \
                 provider did not report which version ran."
            ),
        ),
        None => (
            format!("{kind} · {provider} · provider default model · {when}"),
            format!(
                "{kind} by {provider} {spoken} using the provider's default model; \
                 the provider did not report which version ran."
            ),
        ),
    }
}

// ---- background work ----

/// Where the meeting's audio stands.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioSource {
    NoRecording,
    /// Removed by retention or by the user.
    Deleted,
    /// The referenced file is gone (`recording.state = missing`).
    Missing {
        path: Option<PathBuf>,
    },
    Available {
        path: PathBuf,
        sha256: String,
        /// The playback cache for formats the player cannot open.
        cache: PathBuf,
        cache_ready: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MeetingMenuState {
    pub(crate) find_matches: bool,
    audio: bool,
    playing: bool,
    has_lines: bool,
    summary: bool,
    pub(crate) undo: Option<&'static str>,
    pub(crate) redo: Option<&'static str>,
}

pub(crate) fn native_menu(state: Option<MeetingMenuState>) -> Vec<gpui::MenuItem> {
    // AppKit menu equivalents do not check GPUI's focused key context.
    // The menu actions have no bindings; the view owns the playback keys.
    use crate::menu::*;
    use gpui::MenuItem;
    let audio = state.is_some_and(|state| state.audio);
    let summary = state.is_some_and(|state| state.summary);
    vec![
        MenuItem::action("Rename", RenameMeeting).disabled(state.is_none()),
        MenuItem::action("Copy Transcript", CopyTranscript)
            .disabled(!state.is_some_and(|state| state.has_lines)),
        MenuItem::action("Show Recording in Finder", RevealRecording).disabled(!audio),
        MenuItem::separator(),
        MenuItem::action(
            if state.is_some_and(|state| state.playing) {
                "Pause"
            } else {
                "Play"
            },
            ToggleMeetingPlayback,
        )
        .disabled(!audio),
        MenuItem::action("Back 15 Seconds", BackFifteenSeconds).disabled(!audio),
        MenuItem::action("Forward 15 Seconds", ForwardFifteenSeconds).disabled(!audio),
        MenuItem::separator(),
        MenuItem::action(
            if summary {
                "Regenerate Summary…"
            } else {
                "Generate Summary…"
            },
            GenerateMeetingSummary,
        )
        .disabled(state.is_none()),
        MenuItem::action("Copy Summary", CopyMeetingSummary).disabled(!summary),
        MenuItem::separator(),
        MenuItem::action("Delete Meeting", DeleteMeeting).disabled(state.is_none()),
    ]
}

/// Everything the view shows, read off the main thread.
#[derive(Clone, Debug)]
pub struct MeetingData {
    pub meeting: Meeting,
    pub lines: Vec<TranscriptLine>,
    segment_indices: HashMap<u32, usize>,
    pub speakers: Vec<SpeakerRow>,
    speakers_without_evidence: Vec<SpeakerRow>,
    pub participants: Vec<ParticipantChip>,
    library_tags: Vec<String>,
    app_name: Option<String>,
    missing_folder: Option<PathBuf>,
    processing_job: Option<header::HeaderJob>,
    cache_ahead: Option<usize>,
    pub audio: AudioSource,
    pub people: PeopleLookup,
    /// The enrichment no longer matches the transcript or its attribution.
    pub enrichment_stale: bool,
    /// State of the meeting's enrichment job in the queue, if any.
    pub enrich_job: Option<JobState>,
    /// Possible duplicates and merge targets; empty until [`grouping_view`]
    /// fills them (the view does on every load).
    pub grouping: GroupingView,
    /// The names given during the recording that were not applied and wait
    /// for the user's review ([`live_review_items`]), in decision order.
    pub live_review: Vec<LiveReviewItem>,
}

/// Loads the meeting; an `available` recording whose file is gone is marked
/// `missing` under the library lock (plan §8 item 12), and a `missing` one
/// (referenced, copied or moved) whose files are back at their expected
/// paths with matching hashes is restored under the same snapshot check.
pub fn load_meeting_data(lib: &Library, id: &MeetingId, cache_dir: &Path) -> Result<MeetingData> {
    let meeting = ops::reconcile_recording(lib, id)?;
    let audio = match meeting.recording.as_ref().map(|r| r.state) {
        None => AudioSource::NoRecording,
        Some(RecordingState::Deleted) => AudioSource::Deleted,
        Some(RecordingState::Missing) => AudioSource::Missing {
            path: ops::expected_recording_path(lib, &meeting)?,
        },
        Some(RecordingState::Available) => match ops::recording_path(lib, &meeting)? {
            Some(path) => available(&meeting, path, cache_dir),
            None => AudioSource::NoRecording,
        },
    };
    let people = lib.people_lookup()?;
    let enrich_job = job_state(lib.root(), &enrich_job_id(id))?.map(|(state, _)| state);
    let missing_folder = if let AudioSource::Missing { path: Some(path) } = &audio {
        path.parent()
            .filter(|folder| folder.is_dir())
            .map(Path::to_path_buf)
    } else {
        None
    };
    let mut snapshot = None;
    if meeting.processing.status != ProcessingStatus::Done
        || matches!(
            audio,
            AudioSource::Available {
                cache_ready: false,
                ..
            }
        )
    {
        snapshot = Queue::open(lib.root())?.snapshot().ok();
    }
    let processing_job = snapshot
        .as_ref()
        .and_then(|queue| header::meeting_job(queue, &meeting));
    let cache_ahead = snapshot.as_ref().and_then(|queue| {
        let AudioSource::Available { sha256, .. } = &audio else {
            return None;
        };
        let job = JobId::for_key(PLAYBACK_KIND, &playback_cache_key(id, sha256));
        queue
            .jobs
            .iter()
            .find(|entry| entry.id == job && queue.is_waiting(entry))
            .map(|_| queue.jobs_ahead(&job))
    });
    let library_tags = broscribe_core::index::Index::open(lib.root())
        .and_then(|index| index.meeting_tags())
        .unwrap_or_default()
        .into_iter()
        .map(|(tag, _)| tag)
        .collect();
    let finished = meeting.processing.status == ProcessingStatus::Done;
    let playable = matches!(audio, AudioSource::Available { .. });
    let (speakers_without_evidence, speakers) = speaker_rows(&meeting, &people)
        .into_iter()
        .partition(|row| {
            finished
                && !row.review_dismissed
                && matches!(row.assignment, Assignment::None | Assignment::Suggested)
                && row.transcript_segment.is_none()
                && (!playable || row.samples.is_empty())
        });
    Ok(MeetingData {
        library_tags,
        app_name: meeting.app.clone(),
        missing_folder,
        processing_job,
        cache_ahead,
        lines: shown_lines(&meeting, &people),
        segment_indices: meeting
            .segments
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id, i))
            .collect(),
        speakers,
        speakers_without_evidence,
        participants: participant_chips(&meeting, &people),
        enrichment_stale: ops::enrichment_is_stale(&meeting),
        enrich_job,
        grouping: GroupingView::default(),
        live_review: live_review_items(&meeting),
        people,
        meeting,
        audio,
    })
}

fn available(meeting: &Meeting, path: PathBuf, cache_dir: &Path) -> AudioSource {
    let sha256 = meeting
        .recording
        .as_ref()
        .map(|r| r.sha256.clone())
        .unwrap_or_default();
    let cache = playback_cache_path(cache_dir, &sha256);
    let cache_ready = std::fs::metadata(&cache).is_ok_and(|m| m.is_file());
    AudioSource::Available {
        path,
        sha256,
        cache,
        cache_ready,
    }
}

/// Why the playback-cache job `job` failed, if it did: reported by the
/// worker, or found crashed out of attempts by the app's queue check.
pub fn cache_job_failure(jobs: &crate::jobs::JobsStatus, job: &JobId) -> Option<String> {
    jobs.recent_failures
        .iter()
        .rev()
        .find(|f| f.job_id == job.as_str())
        .map(|f| crate::jobs::playback_cache_failure(f.message.as_deref()))
}

/// Result of naming a speaker from the view.
#[derive(Clone, Debug, PartialEq)]
pub struct Naming {
    /// The speaker named: a merged group's kept speaker when the request
    /// named a member.
    pub speaker_id: SpeakerId,
    pub person_name: String,
    pub voiceprint_added: bool,
    /// The speaker recheck core requested with the naming (this meeting
    /// first, then the rest of the library), run by the queue worker.
    pub learning: LearningRequest,
    /// Members taken out of the group because their own evidence
    /// contradicts the name (core's messages).
    pub separated: Vec<String>,
}

impl Naming {
    /// What the naming did, said as it is: this speaker's name is saved,
    /// the others are only being checked ("S1: name saved as Ana Example.
    /// Checking other speakers, this meeting first.").
    pub fn message(&self, label: &str) -> String {
        let mut text = format!("{label}: name saved as {}.", self.person_name);
        for separated in &self.separated {
            text.push(' ');
            text.push_str(separated);
            text.push('.');
        }
        if !self.voiceprint_added {
            text.push_str(" No voice was added: the speaker has no audio.");
        }
        text.push_str(" Checking other speakers, this meeting first.");
        text
    }
}

/// Names a speaker (`person_ref`: person ID, email or name; a new name
/// creates the person) with the centroid fallback and, in the same
/// transaction, queues the `confirm-samples` job that adds sample-based
/// voiceprints later; core requests the speaker recheck in that write too.
/// Nobody else is named here: the recheck runs in the queue worker. A
/// merged member names its group's kept speaker. Refuses, before any
/// mutation, a picker opened before the meeting was reanalysed or its
/// speakers regrouped (`fence`, taken when it opened).
pub fn name_speaker_and_queue(
    lib: &Library,
    meeting_id: &MeetingId,
    speaker_id: SpeakerId,
    person_ref: &str,
    model: &str,
    fence: &SpeakerFence,
) -> Result<Naming> {
    name_with_history(
        lib, meeting_id, speaker_id, person_ref, model, None, fence, false,
    )
    .map(|(naming, _)| naming)
}

/// Confirms the proposal shown for the speaker, refused (nothing changed)
/// when it or the speakers' grouping (`fence`) changed since it was shown.
pub fn accept_proposal_and_queue(
    lib: &Library,
    meeting_id: &MeetingId,
    speaker_id: SpeakerId,
    person: &PersonId,
    model: &str,
    fence: &SpeakerFence,
) -> Result<Naming> {
    name_with_history(
        lib,
        meeting_id,
        speaker_id,
        person.as_str(),
        model,
        Some(person),
        fence,
        false,
    )
    .map(|(naming, _)| naming)
}

fn check_proposal(meeting: &Meeting, speaker_id: SpeakerId, expected: &PersonId) -> Result<()> {
    let current = meeting
        .speakers
        .iter()
        .find(|speaker| speaker.id == speaker_id)
        .and_then(|speaker| match speaker.assignment {
            Assignment::Auto => speaker.person_id.as_ref(),
            Assignment::Suggested => speaker
                .suggestion
                .as_ref()
                .map(|suggestion| &suggestion.person_id),
            _ => None,
        });
    if current != Some(expected) {
        return Err(Error::InvalidInput(
            "The suggestion changed; review it again.".into(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn name_with_history(
    lib: &Library,
    meeting_id: &MeetingId,
    speaker_id: SpeakerId,
    person_ref: &str,
    model: &str,
    proposal: Option<&PersonId>,
    fence: &SpeakerFence,
    capture: bool,
) -> Result<(Naming, Option<ops::ReviewEdit>)> {
    let (outcome, edit) = ops::with_index(lib, |tx, index| {
        let work = |tx: &mut broscribe_core::WriteTx<'_>, index: &mut Index| {
            let meeting = tx.library().load_meeting(meeting_id)?;
            ops::check_speaker_fence(&meeting, fence)?;
            if let Some(person) = proposal {
                check_proposal(&meeting, speaker_id, person)?;
            }
            let outcome = ops::name_speaker(
                tx,
                index,
                meeting_id,
                speaker_id,
                person_ref,
                Vec::new(),
                model,
            )?;
            // Samples come from the speaker actually named (a group's kept one).
            let named = outcome.speaker_id;
            let key = confirm_samples_key(meeting_id, named, &outcome.person_id, model);
            Queue::open(tx.root())?.enqueue(
                tx,
                CONFIRM_KIND,
                &key,
                json!({
                    "meeting_id": meeting_id,
                    "speaker_id": named.to_string(),
                    "person_id": outcome.person_id,
                    "model": model,
                }),
            )?;
            queue_vector_refresh(tx, meeting_id, &outcome.separated)?;
            Ok(outcome)
        };
        if capture {
            ops::record_review_edit(tx, index, model, work)
        } else {
            work(tx, index).map(|outcome| (outcome, None))
        }
    })?;
    let people = lib.people_lookup()?;
    Ok((
        Naming {
            speaker_id: outcome.speaker_id,
            person_name: person_name(&people, &outcome.person_id),
            voiceprint_added: outcome.voiceprint_added,
            learning: outcome.learning,
            separated: outcome
                .separated
                .iter()
                .map(|s| s.message.clone())
                .collect(),
        },
        edit,
    ))
}

/// Runs retention after an identity change. The change is already
/// committed, so a failure here is logged, never reported as its failure;
/// the next retention run (daemon maintenance) retries.
fn apply_retention(cli: &crate::jobs::CliRunner) {
    if let Err(e) = cli.run_blocking(
        &["retention".into(), "apply".into()],
        crate::jobs::ChildKind::Command,
    ) {
        tracing::warn!(code = %e.code, "retention after an identity change failed");
    }
}

/// Queues the meeting's `refresh-vectors` job (one per meeting, in the
/// identity write's transaction) when that write separated speakers from a
/// group: their lines now show as another speaker, so the transcript's
/// search windows changed and need vectors. Returns whether it was queued.
fn queue_vector_refresh(
    tx: &mut broscribe_core::WriteTx<'_>,
    meeting_id: &MeetingId,
    separated: &[ops::Separation],
) -> Result<bool> {
    if separated.is_empty() {
        return Ok(false);
    }
    Queue::open(tx.root())?.enqueue(
        tx,
        VECTORS_KIND,
        meeting_id.as_str(),
        json!({ "meeting_id": meeting_id }),
    )?;
    Ok(true)
}

/// Rejects the speaker's current suggestion or auto-name, refused (nothing
/// changed) when it or the speakers' grouping (`fence`) changed since it was
/// shown. Returns whether a `refresh-vectors` job was queued (the caller
/// kicks the worker).
#[allow(clippy::too_many_arguments)]
pub fn reject_proposal(
    lib: &Library,
    meeting_id: &MeetingId,
    speaker_id: SpeakerId,
    person: &PersonId,
    model: &str,
    thresholds: Option<&Thresholds>,
    fence: &SpeakerFence,
    cli: &crate::jobs::CliRunner,
) -> Result<bool> {
    let queued = ops::with_index(lib, |tx, index| {
        let meeting = tx.library().load_meeting(meeting_id)?;
        ops::check_speaker_fence(&meeting, fence)?;
        check_proposal(&meeting, speaker_id, person)?;
        let outcome = ops::reject_suggestion(
            tx,
            index,
            meeting_id,
            speaker_id,
            Some(person.as_str()),
            model,
            thresholds,
        )?;
        queue_vector_refresh(tx, meeting_id, &outcome.separated)
    })?;
    apply_retention(cli);
    Ok(queued)
}

/// Adds and removes tags; returns the meeting's tags afterwards.
pub fn edit_tags(
    lib: &Library,
    meeting_id: &MeetingId,
    add: &[String],
    remove: &[String],
) -> Result<Vec<String>> {
    ops::with_index(lib, |tx, index| {
        ops::tag(tx, index, meeting_id, add, remove)
    })
}

/// Tags typed in a tag field, comma-separated, trimmed, empty ones dropped.
/// Every entry is a literal tag to add (a typed `-x` adds `-x`); removing is
/// the chip's own action. Shared by the meeting header and the recording
/// panel.
pub fn typed_tags(input: &str) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for tag in input.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        if !tags.iter().any(|t| t == tag) {
            tags.push(tag.to_owned());
        }
    }
    tags
}

/// `title <meeting> --stdin` (the runner adds `--library <lib> --json`);
/// the title travels on stdin only.
pub fn title_args(meeting_id: &MeetingId) -> Vec<OsString> {
    vec!["title".into(), meeting_id.as_str().into(), "--stdin".into()]
}

/// Renames through the CLI; returns what to tell the user. Blocks:
/// background executor only.
pub fn rename_meeting(
    cli: &crate::jobs::CliRunner,
    meeting_id: &MeetingId,
    title: &str,
) -> std::result::Result<String, CliError> {
    let out = cli.run_blocking_stdin(
        &title_args(meeting_id),
        crate::jobs::ChildKind::Command,
        title.as_bytes().to_vec(),
    )?;
    Ok(if out["session_id"].is_string() {
        "Title saved. Your title is kept over calendar and window titles. The recording \
         shows it too."
            .to_owned()
    } else {
        "Title saved. Your title is kept over calendar and window titles.".to_owned()
    })
}

pub use broscribe_core::ops::enqueue_playback_cache;

// ---- view ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeetingTab {
    Overview,
    Transcript,
    Speakers,
    Templates,
}

const MEETING_TABS: [MeetingTab; 4] = [
    MeetingTab::Overview,
    MeetingTab::Transcript,
    MeetingTab::Speakers,
    MeetingTab::Templates,
];

impl MeetingTab {
    /// The tab's label, which also names its tab panel (MTG-53).
    pub fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Transcript => "Transcript",
            Self::Speakers => "Speakers",
            Self::Templates => "Templates",
        }
    }
}

/// Playback as this view drives it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlaybackStatus {
    /// The player holds this view's file (playing or paused).
    pub loaded: bool,
    pub playing: bool,
    /// Recording-relative seconds.
    pub position_s: f64,
    pub current_line: Option<usize>,
    /// Waiting for the playback cache; plays from here when it is ready.
    pub pending_start: Option<f64>,
    /// The queued `playback-cache` job.
    pub cache_job: Option<JobId>,
    /// The cache job failed (it is retried by the next worker run).
    pub cache_failed: Option<String>,
    pub error: Option<String>,
    /// The file is being opened off the UI loop; it plays once ready.
    pub preparing: bool,
    /// Playback reached the end of the recording.
    pub finished: bool,
}

/// Notes the user should check on the meeting (references enrichment or
/// reprocessing could not map), with or without an enrichment; `None` when
/// there are none.
pub fn review_notice(meeting: &Meeting) -> Option<&[ReviewNote]> {
    Some(meeting.review_notes.as_slice()).filter(|notes| !notes.is_empty())
}

/// One name given during the recording that waits for review, as shown:
/// who was named (`name`: `None` for a person since forgotten), where, and
/// why it was not applied; then its accessible name.
pub fn live_review_text(item: &LiveReviewItem, name: Option<&str>) -> (String, String) {
    let who = name.unwrap_or("someone no longer in your people");
    let span = format!("{}–{}", format_clock(item.from_s), format_clock(item.to_s));
    let why = match item.outcome {
        LiveOutcome::Partial => "It covers only part of these lines.",
        LiveOutcome::Conflicting => "You gave another name to the same lines.",
        _ if !item.mapped => "The final transcript has no speech there.",
        _ => "The final transcript has no matching line.",
    };
    let lines = match item.lines.len() {
        0 => String::new(),
        n => format!(" · {}", crate::ui::format::count(n as u64, "line", "lines")),
    };
    (
        format!("{who} · {span}{lines}"),
        format!("Check this name. {why}"),
    )
}

/// The review notes the banner lists: a live-preview note is left out while
/// the structured review below lists the same decisions.
fn speakers_to_name(data: &MeetingData) -> usize {
    data.speakers
        .iter()
        .filter(|row| !row.review_dismissed && row.assignment != Assignment::Confirmed)
        .count()
}

fn speakers_to_name_label(count: usize) -> String {
    if count == 1 {
        "Speakers, 1 needs a name".to_owned()
    } else {
        format!("Speakers, {count} need a name")
    }
}

fn initial_tab(data: &MeetingData) -> MeetingTab {
    let meeting = &data.meeting;
    if meeting.enrichment.is_some()
        || !meeting.review_notes.is_empty()
        || !data.live_review.is_empty()
        || speakers_to_name(data) > 0
        || !meeting.bookmarks.is_empty()
        || !meeting.notes.is_empty()
    {
        MeetingTab::Overview
    } else {
        MeetingTab::Transcript
    }
}

fn banner_notes<'a>(meeting: &'a Meeting, live_review: &[LiveReviewItem]) -> Vec<&'a ReviewNote> {
    review_notice(meeting)
        .into_iter()
        .flatten()
        .filter(|n| live_review.is_empty() || n.source != ReviewSource::LivePreview)
        .collect()
}

/// What a refused identity write tells the user (`failed`: "Couldn't name
/// the speaker."): a changed grouping or analysis asks for a fresh look,
/// anything else says why in words.
pub fn identity_failure(failed: &str, e: &Error) -> String {
    match e.code() {
        code @ ("speakers_regrouped" | "speakers_reanalysed") => regrouped_refusal(code).to_owned(),
        _ => failure_text(failed, &meeting_user_text(e), ""),
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Notice {
    Info(String),
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SpeakerControl {
    Row,
    Name,
    Accept,
    Reject,
    Review,
    More,
}

const SPEAKER_CONTROLS: [SpeakerControl; 5] = [
    SpeakerControl::Name,
    SpeakerControl::Accept,
    SpeakerControl::Reject,
    SpeakerControl::Review,
    SpeakerControl::More,
];

struct SpeakerFocusReturn {
    opener: gpui::FocusHandle,
    speaker: SpeakerId,
    control: SpeakerControl,
    neighbours: [Option<SpeakerId>; 2],
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum EditTarget {
    Title,
    Tags,
    Line(Vec<u32>),
    Action(String),
    LiveReview(u64),
    ReviewNote(String),
    Speaker(SpeakerId),
    Speakers,
    Processing,
}

/// "Dismissed — Undo" after "Can't tell" on the Speakers tab.
const UNDO_WINDOW: Duration =
    Duration::from_secs((broscribe_core::retention::DISMISS_GRACE.num_seconds() - 60) as u64);

struct UndoDismiss {
    speaker: SpeakerId,
    label: String,
    /// The speakers as read when dismissing.
    fence: SpeakerFence,
    /// The offer ends after [`UNDO_WINDOW`] unless a newer one replaced it.
    generation: u64,
}

enum Playable {
    File(PathBuf),
    NeedsCache,
    Nothing,
}

/// Which lines a choice from a transcript line's picker names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamingScope {
    /// Every line of the line's speaker: names the speaker
    /// ([`name_speaker_and_queue`]).
    SpeakerLines,
    /// Only this line: an attribution overlay ([`attribute_line`]).
    ThisLine,
}

/// What an open picker names.
#[derive(Clone, Debug, PartialEq)]
enum NamingTarget {
    /// A speaker, from the Speakers tab.
    Speaker(SpeakerId),
    /// A transcript line, from the Transcript tab.
    Line {
        speaker: Option<SpeakerId>,
        segment_ids: Vec<u32>,
        /// Taken when the picker opened; "Only this line" writes against
        /// them, so a changed transcript is refused, never renamed blindly.
        fingerprints: Vec<SegmentFingerprint>,
        scope: NamingScope,
        button_anchor: bool,
    },
}

/// The picker open on one speaker or line; a choice names that target.
struct NamingSession {
    target: NamingTarget,
    /// The meeting's speakers when the picker opened: a reanalysis may give
    /// the speaker number to another voice, a regrouping may show it as
    /// another speaker.
    fence: SpeakerFence,
    generation: u64,
    picker: Entity<PersonPicker>,
    speaker_opener: Option<gpui::FocusHandle>,
    _subscription: Subscription,
}

impl NamingSession {
    fn speaker(&self) -> Option<SpeakerId> {
        match &self.target {
            NamingTarget::Speaker(speaker) => Some(*speaker),
            NamingTarget::Line { speaker, .. } => *speaker,
        }
    }

    fn line(&self) -> Option<&[u32]> {
        match &self.target {
            NamingTarget::Line { segment_ids, .. } => Some(segment_ids),
            NamingTarget::Speaker(_) => None,
        }
    }
}

/// Key of a speaker's sample in [`MeetingView::samples`].
fn sample_key(speaker: SpeakerId) -> SharedString {
    format!("speaker-{speaker}").into()
}

/// The player key of a live decision's stretch of the recording.
fn live_review_key(decision: u64) -> SharedString {
    format!("live-review-{decision}").into()
}

/// The voice samples of the speaker shown as `speaker` (`ops::sample_clips`,
/// rank order); empty before the meeting loads.
fn speaker_clips(data: Option<&MeetingData>, speaker: SpeakerId) -> &[SampleClip] {
    data.and_then(|d| d.speakers.iter().find(|row| row.id == speaker))
        .map(|row| row.samples.as_slice())
        .unwrap_or_default()
}

fn samples_of(view: &mut MeetingView) -> &mut SamplePlayback {
    &mut view.samples
}

/// Whether the picker's target is still what the meeting shows: same
/// speakers (namespace and grouping), and its speaker or its line (same
/// segments) still there.
fn naming_still_valid(session: &NamingSession, data: &MeetingData) -> bool {
    session.fence == SpeakerFence::of(&data.meeting)
        && match &session.target {
            NamingTarget::Speaker(speaker) => data.speakers.iter().any(|row| row.id == *speaker),
            NamingTarget::Line { segment_ids, .. } => data
                .lines
                .iter()
                .any(|line| line.segment_ids == *segment_ids),
        }
}

pub struct MeetingView {
    state: Entity<AppState>,
    embedded: bool,
    setup_tips: [Option<bool>; 2],
    word_tip_model: Option<(String, Option<bool>)>,
    meeting_id: MeetingId,
    data: Option<MeetingData>,
    load_error: Option<String>,
    meeting_not_found: bool,
    header_narrow: bool,
    participants_expanded: bool,
    job_display: header::JobDisplay,
    load_generation: u64,
    seen_meetings_revision: u64,
    seen_people_revision: u64,
    seen_jobs_completed: u64,
    tab: MeetingTab,
    /// The player opens the recording itself (else the cache is used).
    native: bool,
    /// The recording (by SHA-256) the player refused to play faithfully
    /// ([`PLAYBACK_NEEDS_CACHE`]): it plays from the cache from then on.
    needs_cache: Option<String>,
    playback: PlaybackStatus,
    player_owner: Option<u64>,
    tick: Option<Task<()>>,
    ticking: bool,
    slider: Option<(Entity<SliderState>, f32, Subscription)>,
    scrub: Option<f64>,
    scrub_generation: u64,
    transcript_list: ListState,
    transcript_rows: Vec<gpui::FocusHandle>,
    transcript_entries: Vec<saved_transcript::Entry>,
    transcript_line_rows: Vec<Option<usize>>,
    transcript_speaker: String,
    timeline: Vec<saved_transcript::Marker>,
    live_review_segments: BTreeMap<u64, Vec<u32>>,
    transcript_rem: gpui::Pixels,
    transcript_row_builds: saved_transcript::RowBuildCount,
    following_transcript: bool,
    detail_focus: gpui::FocusHandle,
    transcript_cursor: Option<usize>,
    transcript_menu: Option<saved_transcript::LineMenu>,
    transcript_copy_status: Option<saved_transcript::CopyStatus>,
    library_match: Option<(u32, Vec<String>)>,
    pending_library_match: Option<bool>,
    pending_library_command: Option<crate::views::library::RowCommand>,
    find_input: Entity<InputState>,
    find_open: bool,
    find_hits: Vec<usize>,
    find_index: Option<usize>,
    find_query: String,
    find_debounce: Option<Task<()>>,
    restore_place: Option<crate::state::MeetingPlace>,
    choose_initial_tab: bool,
    new_review_notes: usize,
    review_announcement: usize,
    review_focus: gpui::FocusHandle,
    live_row_focus: HashMap<u64, gpui::FocusHandle>,
    live_first_focus: HashMap<u64, gpui::FocusHandle>,
    settled_live_focus: Option<(u64, Option<u64>)>,
    reviewed_line: Option<u32>,
    /// Summary, Decisions and Actions (one scroll area).
    enrichment_scroll: ScrollHandle,
    tag_input: Entity<InputState>,
    tag_open: bool,
    tag_choice: Option<usize>,
    add_tag_focus: gpui::FocusHandle,
    title_focus: gpui::FocusHandle,
    rename_form_focus: gpui::FocusHandle,
    rename_save_focus: gpui::FocusHandle,
    rename_opener: Option<gpui::FocusHandle>,
    tag_focus: HashMap<String, gpui::FocusHandle>,
    overview: overview::Overview,
    speaker_focus: HashMap<(SpeakerId, SpeakerControl), gpui::FocusHandle>,
    speaker_list_focus: gpui::FocusHandle,
    speaker_disclosure_focus: gpui::FocusHandle,
    speaker_focus_return: Option<SpeakerFocusReturn>,
    naming: Option<NamingSession>,
    /// Bumped by every naming start and end; a picker's choice only counts
    /// while its session is current.
    naming_generation: u64,
    /// The speaker row whose "Merge into…" list is open.
    selected_speaker: Option<SpeakerId>,
    show_speakers_without_evidence: bool,
    speaker_scroll: ScrollHandle,
    speaker_rows_bounds: ScrollHandle,
    speaker_menu: Option<speakers::OpenSpeakerMenu>,
    speaker_recheck_dismissed: Option<speakers::RecheckRevision>,
    /// A library write in flight and feedback retained beside each control.
    busy: Option<(EditTarget, &'static str)>,
    edit_status: HashMap<EditTarget, Notice>,
    last_edit: Option<EditTarget>,
    /// Undo offered for the last "Can't tell".
    undo: Option<UndoDismiss>,
    undo_edits: Vec<history::Step>,
    redo_edits: Vec<history::Step>,
    undo_generation: u64,
    /// Speaker samples (Speakers tab); one owner of the player at a time
    /// with the playback bar.
    samples: SamplePlayback,
    /// The speaker whose samples are current and their clip.
    sample_cursor: SampleCursor,
    generate: Entity<GeneratePanel>,
    templates: Entity<TemplatesTab>,
    /// The Export… form while open.
    export: Option<(Entity<ExportForm>, Subscription)>,
    export_focus: gpui::FocusHandle,
    export_result: Option<PathBuf>,
    /// The title field while renaming.
    rename: Option<(Entity<InputState>, Subscription)>,
    /// The "Delete meeting" confirmation while open.
    delete: Option<(Entity<DeleteMeetingPrompt>, Subscription)>,
    delete_focus: gpui::FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl MeetingView {
    pub(crate) fn detail_focus(&self) -> &gpui::FocusHandle {
        self.active_transcript_row()
            .filter(|_| self.tab == MeetingTab::Transcript)
            .map_or(&self.detail_focus, |ix| &self.transcript_rows[ix])
    }

    pub(crate) fn window_title(&self) -> String {
        self.data.as_ref().map_or_else(
            || "Untitled meeting".to_owned(),
            |data| shown_title(&data.meeting),
        )
    }

    pub(crate) fn embedded(mut self) -> Self {
        self.embedded = true;
        self
    }

    pub fn new(
        state: Entity<AppState>,
        meeting_id: MeetingId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let tag_input = cx.new(|cx| InputState::new(window, cx).placeholder("Add tag"));
        let generate = cx.new(|cx| {
            GeneratePanel::new(
                state.clone(),
                meeting_id.clone(),
                GenerationTarget::Summary,
                window,
                cx,
            )
        });
        let templates = cx.new(|_| TemplatesTab::new(state.clone(), meeting_id.clone()));
        let find_input = cx.new(|cx| InputState::new(window, cx).placeholder("Find in transcript"));
        let shown_token = state.update(cx, |s, _| {
            s.meeting_view_shown(meeting_id.clone(), window.window_handle())
        });
        let restore_place = state.read(cx).meeting_place(&meeting_id);
        let subscriptions = vec![
            cx.subscribe_in(&find_input, window, |this, _, event, _, cx| match event {
                InputEvent::Change => this.schedule_find(cx),
                InputEvent::PressEnter { shift, .. } => this.step_find(!shift, cx),
                _ => {}
            }),
            cx.observe_in(&state, window, |this, _, window, cx| {
                this.state_changed(window, cx)
            }),
            cx.subscribe_in(
                &tag_input,
                window,
                |this, _, ev: &InputEvent, window, cx| match ev {
                    InputEvent::PressEnter { .. } => this.submit_tag(window, cx),
                    InputEvent::Change => {
                        this.tag_choice = None;
                        cx.notify();
                    }
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &generate,
                window,
                |this, _, ev: &GenerateEvent, window, cx| match ev {
                    GenerateEvent::Finished => this.reload(window, cx),
                    GenerateEvent::CopySummary => this.copy_summary(cx),
                    GenerateEvent::CatalogChanged => {}
                },
            ),
            cx.subscribe_in(
                &templates,
                window,
                |this, _, ev: &GenerateEvent, window, cx| match ev {
                    GenerateEvent::Finished => this.reload(window, cx),
                    GenerateEvent::CopySummary | GenerateEvent::CatalogChanged => {}
                },
            ),
            // Playback this view started ends with it; the app no longer
            // counts it as shown.
            cx.on_release(move |this, cx| {
                let place = crate::state::MeetingPlace {
                    tab: this.tab,
                    playback_s: this.playback.position_s,
                    transcript: this.transcript_list.logical_scroll_top(),
                    overview: this.enrichment_scroll.offset(),
                };
                this.stop_owned_playback(cx);
                this.samples.stop(&this.state, cx);
                this.state.update(cx, |s, _| {
                    s.meeting_view_hidden(shown_token);
                    if this.data.is_some() {
                        s.remember_meeting_place(this.meeting_id.clone(), place);
                    }
                });
            }),
        ];
        let (meetings_revision, people_revision, completed) = {
            let s = state.read(cx);
            (
                s.meetings_revision(),
                s.people_revision(),
                s.jobs().completed,
            )
        };
        let mut this = Self {
            state,
            embedded: false,
            setup_tips: [None; 2],
            word_tip_model: None,
            meeting_id,
            data: None,
            load_error: None,
            meeting_not_found: false,
            header_narrow: window.viewport_size().width < window.rem_size() * 40.,
            participants_expanded: false,
            job_display: header::JobDisplay::default(),
            load_generation: 0,
            seen_meetings_revision: meetings_revision,
            seen_people_revision: people_revision,
            seen_jobs_completed: completed,
            tab: restore_place
                .as_ref()
                .map_or(MeetingTab::Transcript, |place| place.tab),
            native: false,
            needs_cache: None,
            playback: PlaybackStatus::default(),
            player_owner: None,
            tick: None,
            ticking: false,
            slider: None,
            scrub: None,
            scrub_generation: 0,
            transcript_list: Self::new_transcript_list(cx),
            transcript_rows: Vec::new(),
            transcript_entries: Vec::new(),
            transcript_line_rows: Vec::new(),
            transcript_speaker: String::new(),
            timeline: Vec::new(),
            live_review_segments: BTreeMap::new(),
            transcript_rem: window.rem_size(),
            transcript_row_builds: saved_transcript::RowBuildCount::default(),
            following_transcript: true,
            transcript_menu: None,
            transcript_copy_status: None,
            detail_focus: cx.focus_handle(),
            transcript_cursor: None,
            library_match: None,
            pending_library_match: None,
            pending_library_command: None,
            find_input,
            find_open: false,
            find_hits: Vec::new(),
            find_index: None,
            find_query: String::new(),
            find_debounce: None,
            choose_initial_tab: restore_place.is_none(),
            restore_place,
            new_review_notes: 0,
            review_announcement: 0,
            review_focus: cx.focus_handle(),
            live_row_focus: HashMap::new(),
            live_first_focus: HashMap::new(),
            settled_live_focus: None,
            reviewed_line: None,
            enrichment_scroll: ScrollHandle::new(),
            tag_input,
            tag_open: false,
            tag_choice: None,
            add_tag_focus: cx.focus_handle(),
            title_focus: cx.focus_handle(),
            rename_form_focus: cx.focus_handle(),
            rename_save_focus: cx.focus_handle(),
            rename_opener: None,
            tag_focus: HashMap::new(),
            overview: overview::Overview::default(),
            speaker_focus: HashMap::new(),
            speaker_list_focus: cx.focus_handle(),
            speaker_disclosure_focus: cx.focus_handle(),
            speaker_focus_return: None,
            naming: None,
            naming_generation: 0,
            selected_speaker: None,
            show_speakers_without_evidence: false,
            speaker_scroll: ScrollHandle::new(),
            speaker_rows_bounds: ScrollHandle::new(),
            speaker_menu: None,
            speaker_recheck_dismissed: None,
            busy: None,
            edit_status: HashMap::new(),
            last_edit: None,
            samples: SamplePlayback::default(),
            sample_cursor: SampleCursor::default(),
            undo: None,
            undo_edits: Vec::new(),
            redo_edits: Vec::new(),
            undo_generation: 0,
            generate,
            templates,
            export: None,
            export_focus: cx.focus_handle(),
            export_result: None,
            rename: None,
            delete: None,
            delete_focus: cx.focus_handle(),
            _subscriptions: subscriptions,
        };
        this.reload(window, cx);
        this
    }

    pub fn meeting_id(&self) -> &MeetingId {
        &self.meeting_id
    }

    pub fn data(&self) -> Option<&MeetingData> {
        self.data.as_ref()
    }

    pub(crate) fn dialog_open(&self) -> bool {
        self.export.is_some() || self.delete.is_some()
    }

    pub(crate) fn menu_state_for_focus(
        &self,
        window: &Window,
        cx: &App,
    ) -> Option<MeetingMenuState> {
        let mut state = self.menu_state()?;
        let focused =
            |input: &Entity<InputState>| input.read(cx).focus_handle(cx).is_focused(window);
        if self
            .rename
            .as_ref()
            .is_some_and(|(input, _)| focused(input))
            || (self.tag_open && focused(&self.tag_input))
            || (self.find_open && focused(&self.find_input))
        {
            state.undo = Some("Typing");
            state.redo = Some("Typing");
        } else if !self.detail_focus.contains_focused(window, cx) {
            state.undo = None;
            state.redo = None;
        }
        Some(state)
    }

    pub(crate) fn menu_state(&self) -> Option<MeetingMenuState> {
        let data = self.data.as_ref()?;
        Some(MeetingMenuState {
            find_matches: self.find_open && !self.find_hits.is_empty(),
            audio: matches!(data.audio, AudioSource::Available { .. }),
            playing: self.playback.playing,
            has_lines: !data.lines.is_empty(),
            undo: self.undo_edits.last().map(|step| step.name),
            redo: self.redo_edits.last().map(|step| step.name),
            summary: data.meeting.enrichment.is_some(),
        })
    }

    pub(crate) fn reveal_recording(&self, cx: &App) {
        if let Some(MeetingData {
            audio: AudioSource::Available { path, .. },
            ..
        }) = &self.data
        {
            super::export::reveal_file(path, cx);
        }
    }

    pub(crate) fn seek_relative(
        &mut self,
        seconds: f64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(data) = &self.data
            && matches!(data.audio, AudioSource::Available { .. })
        {
            self.seek(
                (self.playback.position_s + seconds).clamp(0., data.meeting.duration_s.max(0.)),
                window,
                cx,
            );
        }
    }

    pub(crate) fn show_summary_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_tab(MeetingTab::Overview, cx);
        self.generate
            .update(cx, |panel, cx| panel.show_options(window, cx));
    }

    pub(crate) fn copy_summary(&mut self, cx: &mut Context<Self>) {
        use broscribe_core::export::{ExportFormat, ExportSection, render_export};
        if let Some(data) = &self.data
            && data.meeting.enrichment.is_some()
            && let Ok(doc) = render_export(
                &data.meeting,
                &data.people,
                &[
                    ExportSection::Summary,
                    ExportSection::Decisions,
                    ExportSection::Actions,
                ],
                ExportFormat::Markdown,
                true,
            )
        {
            cx.write_to_clipboard(ClipboardItem::new_string(doc.content));
            self.generate
                .update(cx, |panel, cx| panel.summary_copied(cx));
        }
    }

    pub fn playback(&self) -> &PlaybackStatus {
        &self.playback
    }

    pub fn tab(&self) -> MeetingTab {
        self.tab
    }

    fn cycle_tab(&mut self, forward: bool, cx: &mut Context<Self>) {
        let index = MEETING_TABS
            .iter()
            .position(|tab| *tab == self.tab)
            .unwrap();
        let step = if forward { 1 } else { MEETING_TABS.len() - 1 };
        self.set_tab(MEETING_TABS[(index + step) % MEETING_TABS.len()], cx);
    }

    pub fn set_tab(&mut self, tab: MeetingTab, cx: &mut Context<Self>) {
        self.choose_initial_tab = false;
        if self.tab != tab {
            if self.tab == MeetingTab::Overview {
                self.new_review_notes = 0;
            }
            self.tab = tab;
            self.speaker_menu = None;
            // Pickers live on the tab they were opened from.
            self.end_naming(cx);
            if self.samples.active().is_some() {
                self.samples.stop(&self.state, cx);
            }
            let summary = tab == MeetingTab::Overview;
            self.generate.update(cx, |g, cx| {
                g.set_visible(summary, cx);
                if summary {
                    g.ensure_loaded(cx);
                }
            });
            self.templates
                .update(cx, |t, cx| t.set_visible(tab == MeetingTab::Templates, cx));
            cx.notify();
        }
    }

    /// The generation panel of the enrichment tabs.
    #[cfg(test)]
    pub(crate) fn generate_panel(&self) -> &Entity<GeneratePanel> {
        &self.generate
    }

    /// The scroll position of the Summary, Decisions and Actions tabs.
    #[cfg(test)]
    pub(crate) fn enrichment_scroll(&self) -> &ScrollHandle {
        &self.enrichment_scroll
    }

    /// The Templates tab.
    #[cfg(test)]
    pub(crate) fn templates_tab(&self) -> &Entity<TemplatesTab> {
        &self.templates
    }

    /// The Export… form, while open.
    pub fn export_form(&self) -> Option<&Entity<ExportForm>> {
        self.export.as_ref().map(|(form, _)| form)
    }

    pub(crate) fn present_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let opener = if self.header_narrow {
            self.delete_focus.clone()
        } else {
            self.export_focus.clone()
        };
        self.present_export_from(opener, window, cx);
    }

    fn present_export_from(
        &mut self,
        opener: gpui::FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.export.is_some()
            || self.delete.is_some()
            || window.root::<gpui::component::Root>().flatten().is_none()
            || window.has_active_dialog(cx)
        {
            return;
        }
        let Some(data) = &self.data else { return };
        self.export_result = None;
        let title = shown_title(&data.meeting);
        let state = self.state.clone();
        let meeting = data.meeting.clone();
        let form = cx.new(|_| ExportForm::new(state, meeting));
        let close_opener = opener.clone();
        let sub = cx.subscribe_in(
            &form,
            window,
            move |view, _, ev: &ExportEvent, window, cx| {
                if let ExportEvent::Saved(path) = ev {
                    view.export_result = Some(path.clone());
                }
                view.export = None;
                window.close_dialog(cx);
                close_opener.focus(window, cx);
                cx.notify();
            },
        );
        self.export = Some((form.clone(), sub));
        let meeting = cx.weak_entity();
        let cancel = form.downgrade();
        window.open_dialog(cx, move |dialog, window, cx| {
            // BroScribe's dialogs stand on the popover ground, not the desk.
            let dialog = dialog.bg(cx.theme().popover);
            let meeting = meeting.clone();
            let cancel = cancel.clone();
            let opener = opener.clone();
            dialog
                .width(px(440.))
                .p_6()
                .entrance(gpui::component::dialog::DialogEntrance::None)
                .overlay_closable(false)
                .on_ok(|_, _, _| false)
                .on_cancel(move |_, _, cx| {
                    if cancel.upgrade().is_some_and(|form| form.read(cx).running()) {
                        return false;
                    }
                    let _ = meeting.update(cx, |view, cx| {
                        view.export = None;
                        cx.notify();
                    });
                    true
                })
                .on_close(move |_, window, cx| opener.focus(window, cx))
                .title(
                    crate::ui::heading("export-heading", "Export meeting").a11y_synthetic_children(
                        {
                            let title = title.clone();
                            move |builder| {
                                if let Some(dialog) = builder.ancestor_node(Role::Dialog) {
                                    dialog.set_label("Export meeting");
                                    dialog.set_description(title.clone());
                                }
                            }
                        },
                    ),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .max_h(window.viewport_size().height * 0.72)
                        .child(text_line(
                            "export-meeting-description",
                            Role::Label,
                            title.clone(),
                        ))
                        .child(form.clone()),
                )
        });
        cx.notify();
    }

    // ---- loading ----

    /// Re-reads the meeting on the background executor. Older loads still in
    /// flight are discarded.
    pub fn reload(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.load_generation += 1;
        let generation = self.load_generation;
        // The speaker may change: a voice result in flight is dropped; the
        // scores shown stay until the reload completes.
        if let Some(session) = &self.naming {
            session
                .picker
                .update(cx, |picker, _| picker.begin_voice_reload());
        }
        let (lib, cache_dir, data_dir) = {
            let s = self.state.read(cx);
            (
                s.library().clone(),
                s.paths().cache_dir.clone(),
                s.paths().data_dir.clone(),
            )
        };
        let (model, thresholds) = self.model_and_thresholds(cx);
        let id = self.meeting_id.clone();
        let task = cx.background_executor().spawn(async move {
            let mut data = load_meeting_data(&lib, &id, &cache_dir)?;
            if let Some(app) = &data.meeting.app {
                data.app_name = broscribe_core::detected_apps::names(&data_dir)
                    .remove(&app.to_ascii_lowercase())
                    .or_else(|| Some(app.clone()));
            }
            data.grouping = grouping_view(
                &lib,
                &data.meeting,
                &data.people,
                &model,
                thresholds.as_ref(),
            )?;
            Ok::<_, Error>(data)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |view, window, cx| {
                view.apply_load(generation, result, window, cx)
            })
            .ok();
        })
        .detach();
    }

    fn apply_load(
        &mut self,
        generation: u64,
        result: Result<MeetingData>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if generation != self.load_generation {
            return;
        }
        let data = match result {
            Ok(data) => data,
            Err(e) => {
                self.meeting_not_found = matches!(e, Error::NotFound(_));
                self.load_error = Some(match e {
                    Error::NotFound(_) => "This meeting no longer exists.".to_owned(),
                    e => {
                        tracing::warn!(code = e.code(), "reading the meeting");
                        failure_text("Couldn't open this meeting.", &meeting_user_text(&e), "")
                    }
                });
                self.stop_owned_playback(cx);
                self.samples.stop(&self.state, cx);
                self.sample_cursor = SampleCursor::default();
                self.end_naming(cx);
                self.sync_transcript_rows(Some(&[]), Vec::new(), window, cx);
                self.data = None;
                cx.notify();
                return;
            }
        };
        let first_load = self.data.is_none();
        self.load_error = None;
        self.meeting_not_found = false;
        if self
            .naming
            .as_ref()
            .is_some_and(|n| !naming_still_valid(n, &data))
        {
            self.end_naming(cx);
        }
        // Every reload recomputes the closest voices from the speaker as read now.
        if let Some(session) = &self.naming {
            let voice = session
                .speaker()
                .and_then(|s| VoiceTarget::of(&data.meeting, s))
                .map(std::sync::Arc::new);
            session
                .picker
                .update(cx, |picker, cx| picker.set_voice_target(voice, cx));
        }
        self.overview.update(&data);
        self.generate.update(cx, |g, cx| {
            g.set_summary(data.meeting.enrichment.as_ref(), cx);
            g.set_can_restore(data.meeting.previous_enrichment.is_some(), cx);
            g.set_meeting(
                data.meeting.enrichment.as_ref().map(|e| e.created_at),
                data.enrichment_stale,
                data.enrich_job,
                cx,
            )
        });
        let (meeting, people) = (data.meeting.clone(), data.people.clone());
        self.templates
            .update(cx, |t, cx| t.set_meeting(meeting, people, cx));
        self.native = match &data.audio {
            AudioSource::Available { path, sha256, .. }
                if data
                    .meeting
                    .recording
                    .as_ref()
                    .is_none_or(|r| r.sources.is_empty())
                    && self.needs_cache.as_deref() != Some(sha256.as_str()) =>
            {
                self.can_play_natively(path, cx)
            }
            _ => false,
        };
        let playable = matches!(data.audio, AudioSource::Available { .. });
        if let AudioSource::Available {
            cache_ready: true, ..
        } = data.audio
        {
            self.playback.cache_job = None;
            self.playback.cache_failed = None;
        }
        if !playable {
            self.stop_owned_playback(cx);
            self.samples.stop(&self.state, cx);
            self.playback.pending_start = None;
        }
        // The current sample follows its clip; a clip that is no longer a
        // candidate stops (its text would describe another clip).
        if let Some(key) = self.sample_cursor.current_key().cloned() {
            let clips = data
                .speakers
                .iter()
                .find(|row| sample_key(row.id) == key)
                .map(|row| row.samples.as_slice());
            if !self.sample_cursor.reconcile(clips) && self.samples.active() == Some(&key) {
                self.samples.stop(&self.state, cx);
            }
        }
        if let Some(old) = &self.data {
            let previous = banner_notes(&old.meeting, &old.live_review);
            self.new_review_notes = banner_notes(&data.meeting, &data.live_review)
                .into_iter()
                .filter(|note| !previous.contains(note))
                .count();
            if self.new_review_notes > 0 {
                self.review_announcement += 1;
            }
        }
        if self.choose_initial_tab {
            self.set_tab(initial_tab(&data), cx);
        }
        self.sync_transcript_rows(
            Some(&data.lines),
            saved_transcript::markers(&data.meeting),
            window,
            cx,
        );
        self.live_review_segments = data
            .live_review
            .iter()
            .map(|item| {
                let segments = data
                    .meeting
                    .segments
                    .iter()
                    .filter(|segment| {
                        item.lines.iter().any(|line| {
                            line.track == segment.track
                                && line.start == segment.start
                                && line.end == segment.end
                                && *line == ops::segment_fingerprint(segment)
                        })
                    })
                    .map(|segment| segment.id)
                    .collect();
                (item.decision, segments)
            })
            .collect();
        if let Some(place) = self.restore_place.take() {
            self.playback.position_s = place.playback_s.min(data.meeting.duration_s);
            self.transcript_list.scroll_to(place.transcript);
            self.enrichment_scroll.set_offset(place.overview);
            if self.tab == MeetingTab::Overview {
                self.generate.update(cx, |panel, cx| {
                    panel.set_visible(true, cx);
                    panel.ensure_loaded(cx);
                });
            }
            self.templates.update(cx, |panel, cx| {
                panel.set_visible(self.tab == MeetingTab::Templates, cx)
            });
        }
        self.ensure_slider(data.meeting.duration_s, window, cx);
        self.playback.current_line = line_at(&data.lines, self.playback.position_s);
        self.tag_focus
            .retain(|tag, _| data.meeting.effective_tags().contains(tag));
        for tag in data.meeting.effective_tags() {
            self.tag_focus
                .entry(tag)
                .or_insert_with(|| cx.focus_handle());
        }
        self.sync_speaker_focus(&data, window, cx);
        let review_pending = self.settled_live_focus.take();
        for item in &data.live_review {
            self.live_row_focus
                .entry(item.decision)
                .or_insert_with(|| cx.focus_handle());
            self.live_first_focus
                .entry(item.decision)
                .or_insert_with(|| cx.focus_handle());
        }
        if let Some((decision, next)) = review_pending
            && !data
                .live_review
                .iter()
                .any(|item| item.decision == decision)
            && self.tab == MeetingTab::Overview
            && self
                .live_row_focus
                .get(&decision)
                .is_some_and(|row| row.contains_focused(window, cx))
        {
            next.filter(|id| data.live_review.iter().any(|item| item.decision == *id))
                .and_then(|id| self.live_first_focus.get(&id))
                .unwrap_or(&self.review_focus)
                .focus(window, cx);
        }
        self.live_row_focus
            .retain(|id, _| data.live_review.iter().any(|item| item.decision == *id));
        self.live_first_focus
            .retain(|id, _| data.live_review.iter().any(|item| item.decision == *id));
        self.edit_status.retain(|target, _| match target {
            EditTarget::Line(ids) => data.lines.iter().any(|line| &line.segment_ids == ids),
            _ => true,
        });
        self.data = Some(data);
        if self.find_open {
            self.update_find(cx);
        }
        if let Some(focus) = self.pending_library_match.take() {
            self.reveal_library_match(focus, window, cx);
        }
        if let Some(command) = self.pending_library_command.take() {
            self.open_library_command(command, window, cx);
        }
        // A meeting can receive focus while it is still loading. Transfer
        // that focus when its transcript arrives if the user has stayed there.
        if first_load && self.detail_focus.is_focused(window) {
            self.detail_focus().focus(window, cx);
        }
        if let Some(start) = self.playback.pending_start
            && let Playable::File(path) = self.playable()
        {
            self.playback.pending_start = None;
            self.start_playback(path, start, false, window, cx);
        }
        cx.notify();
    }

    pub(crate) fn open_library_command(
        &mut self,
        command: crate::views::library::RowCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::views::library::RowCommand;
        if self.data.is_none() {
            self.pending_library_command = Some(command);
            return;
        }
        match command {
            RowCommand::Rename => self.start_rename(window, cx),
            RowCommand::AddTag => self.open_tag(window, cx),
            RowCommand::Export => {
                let opener = window
                    .focused(cx)
                    .unwrap_or_else(|| self.export_focus.clone());
                self.present_export_from(opener, window, cx);
            }
            RowCommand::Delete => {
                let opener = window
                    .focused(cx)
                    .unwrap_or_else(|| self.delete_focus.clone());
                self.present_delete_from(opener, window, cx);
            }
        }
    }

    pub(crate) fn open_library_match(
        &mut self,
        segment: u32,
        snippet: &str,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.library_match = Some((
            segment,
            snippet
                .split("**")
                .skip(1)
                .step_by(2)
                .map(str::to_owned)
                .collect(),
        ));
        self.set_tab(MeetingTab::Transcript, cx);
        if self.data.is_some() {
            self.reveal_library_match(focus, window, cx);
        } else {
            self.pending_library_match = Some(focus);
        }
    }

    fn reveal_library_match(&mut self, focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((segment, _)) = self.library_match {
            self.reveal_transcript_segment(segment, focus, window, cx);
        }
    }

    fn reveal_transcript_segment(
        &mut self,
        segment: u32,
        focus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.data.as_ref().and_then(|data| {
            data.lines
                .iter()
                .position(|line| line.segment_ids.contains(&segment))
        }) else {
            if focus {
                self.detail_focus.focus(window, cx);
            }
            return;
        };
        if self.transcript_row(index).is_none() {
            self.set_transcript_speaker("", window, cx);
        }
        let Some(row) = self.transcript_row(index) else {
            return;
        };
        self.transcript_cursor = Some(row);
        self.following_transcript = false;
        self.transcript_list.scroll_to(gpui::ListOffset {
            item_ix: row,
            offset_in_item: px(0.),
        });
        if focus {
            self.transcript_rows[row].focus(window, cx);
        }
        cx.notify();
    }

    fn state_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.state.read(cx).route(), Route::Library { selected: Some(id) } if *id == self.meeting_id)
        {
            self.end_naming(cx);
        }
        let (meetings, people, completed, failure) = {
            let s = self.state.read(cx);
            let failure = self
                .playback
                .cache_job
                .as_ref()
                .and_then(|job| cache_job_failure(s.jobs(), job));
            (
                s.meetings_revision(),
                s.people_revision(),
                s.jobs().completed,
                failure,
            )
        };
        let mut reload =
            meetings != self.seen_meetings_revision || people != self.seen_people_revision;
        self.seen_meetings_revision = meetings;
        self.seen_people_revision = people;
        let display = self.job_display(cx);
        let job_transition = display.state_changed(&self.job_display);
        if display != self.job_display {
            self.job_display = display;
            cx.notify();
        }
        if completed != self.seen_jobs_completed {
            self.seen_jobs_completed = completed;
            // A finished job may have been our playback cache.
            reload |= self.playback.cache_job.is_some()
                || self.data.as_ref().is_some_and(|d| {
                    d.processing_job.is_some()
                        || d.meeting.processing.status != ProcessingStatus::Done
                });
        }
        if failure != self.playback.cache_failed && failure.is_some() {
            self.playback.cache_failed = failure;
            self.playback.pending_start = None;
            cx.notify();
        }
        if reload || job_transition {
            self.reload(window, cx);
        }
    }

    // ---- playback ----

    fn ensure_player(&self, cx: &mut Context<Self>) {
        self.state.update(cx, |s, _| {
            if s.player_mut().is_none() {
                s.set_player(NativePlatform::audio_player());
            }
        });
    }

    fn can_play_natively(&self, path: &Path, cx: &mut Context<Self>) -> bool {
        self.ensure_player(cx);
        self.state.update(cx, |s, _| {
            s.player_mut().is_some_and(|p| p.can_play_natively(path))
        })
    }

    fn playable(&self) -> Playable {
        match self.data.as_ref().map(|d| &d.audio) {
            Some(AudioSource::Available {
                path,
                cache,
                cache_ready,
                ..
            }) => {
                if self.native {
                    Playable::File(path.clone())
                } else if *cache_ready {
                    Playable::File(cache.clone())
                } else {
                    Playable::NeedsCache
                }
            }
            _ => Playable::Nothing,
        }
    }

    fn duration(&self) -> f64 {
        self.data
            .as_ref()
            .map_or(0.0, |d| d.meeting.duration_s.max(0.0))
    }

    /// Plays from `start_s` (seconds into the recording). Formats the player
    /// cannot open wait for the playback cache, queued on first use.
    pub fn play_from(&mut self, start_s: f64, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.playable(), Playable::Nothing) {
            self.following_transcript = true;
            if self.tab == MeetingTab::Transcript
                && let Some(ix) = self.data.as_ref().and_then(|d| line_at(&d.lines, start_s))
            {
                self.reveal_transcript_line(ix);
            }
        }
        self.play_at(start_s, false, window, cx);
    }

    /// [`Self::play_from`], paused there once it is open when `paused`.
    fn play_at(&mut self, start_s: f64, paused: bool, window: &mut Window, cx: &mut Context<Self>) {
        // The bar takes the player back from a speaker sample.
        self.stop_sample(cx);
        let duration = self.duration();
        let start = if duration > 0.0 {
            start_s.clamp(0.0, duration)
        } else {
            start_s.max(0.0)
        };
        match self.playable() {
            Playable::File(path) => self.start_playback(path, start, paused, window, cx),
            Playable::NeedsCache => {
                self.playback.pending_start = Some(start);
                self.set_position(start, false, window, cx);
                self.request_cache(window, cx);
                cx.notify();
            }
            Playable::Nothing => {}
        }
    }

    /// Opens `path` without blocking the UI loop: the player issues a
    /// request here (no file I/O), the file is checked and probed on the
    /// background executor (bounded by the player's own timeout), and the
    /// result plays here, unless a newer request (another start, a stop, a
    /// sample taking the player) superseded it, which ends it silently.
    fn start_playback(
        &mut self,
        path: PathBuf,
        start: f64,
        paused: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.ensure_player(cx);
        let (owner, preparation) = self.state.update(cx, |s, _| {
            let owner = s.claim_player();
            let preparation = match s.player_owned(owner) {
                Some(player) => player.prepare(&path).map_err(|e| core_user_text(&e)),
                None => Err(NO_PLAYER.to_owned()),
            };
            (owner, preparation)
        });
        self.player_owner = Some(owner);
        let preparation = match preparation {
            Ok(preparation) => preparation,
            Err(e) => {
                self.playback.loaded = false;
                self.playback.playing = false;
                self.playback.preparing = false;
                self.playback.error = Some(e);
                cx.notify();
                return;
            }
        };
        self.playback.preparing = true;
        self.playback.finished = false;
        self.playback.error = None;
        self.set_position(start, false, window, cx);
        cx.notify();
        // The original recording (not the cache): a refusal that needs the
        // cache falls back to it once.
        let original = self.native;
        let run = self
            .state
            .read(cx)
            .cli()
            .spawn(cx.background_executor(), async move { preparation.run() });
        cx.spawn_in(window, async move |this, cx| {
            let prepared = run.await;
            this.update_in(cx, |view, window, cx| {
                // A newer request owns the player now: this one is over.
                if view.player_owner != Some(owner) {
                    return;
                }
                view.playback.preparing = false;
                let started = prepared.and_then(|prepared| {
                    view.state.update(cx, |s, _| {
                        let rate = s.playback_rate;
                        match s.player_owned(owner) {
                            Some(player) => {
                                player.set_rate(rate)?;
                                player.play_prepared(prepared, start, None)?;
                                if paused {
                                    player.pause();
                                }
                                Ok(())
                            }
                            None => Err(Error::Service {
                                code: PLAYBACK_SUPERSEDED,
                                message: "the player was taken over".to_owned(),
                            }),
                        }
                    })
                });
                match started {
                    Ok(()) => {
                        view.playback.loaded = true;
                        view.playback.playing = !paused;
                        view.set_position(start, false, window, cx);
                        if paused {
                            view.stop_tick();
                        } else {
                            view.start_tick(window, cx);
                        }
                    }
                    Err(e) if matches!(e.code(), PLAYBACK_CANCELLED | PLAYBACK_SUPERSEDED) => {}
                    // The player cannot play this file faithfully (unreadable
                    // here, or tracks of different lengths): play the
                    // playable cache instead, made on first use, as for a
                    // format it never opens.
                    Err(e) if original && e.code() == PLAYBACK_NEEDS_CACHE => {
                        view.stop_owned_playback(cx);
                        view.native = false;
                        view.needs_cache = view.data.as_ref().and_then(|d| match &d.audio {
                            AudioSource::Available { sha256, .. } => Some(sha256.clone()),
                            _ => None,
                        });
                        view.play_at(start, paused, window, cx);
                    }
                    Err(e) => {
                        view.playback.loaded = false;
                        view.playback.playing = false;
                        view.playback.cache_failed = Some(core_user_text(&e));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn set_playback_rate(&mut self, rate: f32, cx: &mut Context<Self>) {
        let changed = self.state.update(cx, |state, _| {
            if let Some(player) = self
                .player_owner
                .and_then(|owner| state.player_owned(owner))
            {
                player.set_rate(rate)?;
            }
            state.playback_rate = rate;
            Ok::<_, broscribe_platform::Error>(())
        });
        self.playback.error = changed.err().map(|e| core_user_text(&e));
        cx.notify();
    }

    /// Play / pause.
    pub fn toggle_playback(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.playback.loaded && self.playback.playing {
            self.state.update(cx, |s, _| {
                if let Some(p) = self.player_owner.and_then(|owner| s.player_owned(owner)) {
                    p.pause();
                }
            });
            self.playback.playing = false;
            self.stop_tick();
            cx.notify();
        } else if self.playback.loaded {
            self.stop_sample(cx);
            let resumed = self.state.update(cx, |s, _| {
                match self.player_owner.and_then(|owner| s.player_owned(owner)) {
                    Some(p) => p.resume().map_err(|e| core_user_text(&e)),
                    None => Err(NO_PLAYER.to_owned()),
                }
            });
            match resumed {
                Ok(()) => {
                    self.playback.playing = true;
                    self.playback.finished = false;
                    self.following_transcript = true;
                    if let Some(ix) = self.playback.current_line {
                        self.reveal_transcript_line(ix);
                    }
                    self.start_tick(window, cx);
                }
                Err(e) => {
                    self.playback.loaded = false;
                    self.playback.error = Some(e);
                }
            }
            cx.notify();
        } else if self.playback.pending_start.take().is_some() {
            // Leave the cache job running. Its completion no longer starts audio.
            cx.notify();
        } else if self.playback.preparing {
            // Pressed again while the file is still opening: cancel it.
            self.stop_owned_playback(cx);
            cx.notify();
        } else {
            let duration = self.duration();
            let mut start = self
                .focused_playback_position(window)
                .unwrap_or(self.playback.position_s);
            if duration > 0.0 && start >= duration - 0.5 {
                start = 0.0;
            }
            self.play_from(start, window, cx);
        }
    }

    /// Moves the position; keeps playing or paused as before.
    pub fn seek(&mut self, position_s: f64, window: &mut Window, cx: &mut Context<Self>) {
        if self.playback.loaded || self.playback.preparing {
            let paused = self.playback.loaded && !self.playback.playing;
            self.play_at(position_s, paused, window, cx);
        } else {
            self.set_position(position_s.max(0.0), false, window, cx);
        }
        cx.notify();
    }

    /// Stops this view's playback, and cancels a file still being opened
    /// (the player's `stop` ends a pending preparation without waiting).
    fn stop_owned_playback(&mut self, cx: &mut App) {
        // Only this view's own playback or pending opening: a player another
        // owner took meanwhile is left alone.
        if let Some(owner) = self.player_owner {
            self.state.update(cx, |s, _| s.release_player(owner));
        }
        self.playback.loaded = false;
        self.playback.playing = false;
        self.playback.preparing = false;
        self.playback.finished = false;
        self.player_owner = None;
        self.tick = None;
        self.ticking = false;
    }

    /// Stops the speaker sample, if one is active.
    fn stop_sample(&mut self, cx: &mut Context<Self>) {
        if self.samples.active().is_some() {
            self.samples.stop(&self.state, cx);
            cx.notify();
        }
    }

    /// The recording speaker samples play from; `None` without audio.
    fn sample_source(&self) -> Option<ClipSource> {
        let data = self.data.as_ref()?;
        matches!(data.audio, AudioSource::Available { .. }).then(|| ClipSource::Meeting {
            meeting_id: self.meeting_id.clone(),
            sha256: data.meeting.recording.as_ref().map(|r| r.sha256.clone()),
        })
    }

    /// The clip "Play sample" plays for `speaker`: their current sample
    /// (the longest until stepped); `None` without audio or speech.
    pub fn speaker_sample(&self, speaker: SpeakerId) -> Option<Clip> {
        let source = self.sample_source()?;
        let clips = speaker_clips(self.data.as_ref(), speaker);
        let ix = self.sample_cursor.index(&sample_key(speaker), clips.len());
        clips.get(ix).map(|clip| clip_of(&source, clip))
    }

    /// Plays (or stops) `speaker`'s current sample. Starting one pauses the
    /// playback bar at its position: the player has one owner at a time.
    pub fn toggle_sample(&mut self, speaker: SpeakerId, cx: &mut Context<Self>) {
        let key = sample_key(speaker);
        if self.samples.is_playing(&key) {
            self.samples.stop(&self.state, cx);
            cx.notify();
            return;
        }
        let Some(clip) = self.speaker_sample(speaker) else {
            return;
        };
        self.sample_cursor
            .select(&key, speaker_clips(self.data.as_ref(), speaker));
        self.play_sample(key, clip, cx);
    }

    /// Steps `speaker` to their next (or previous) sample and plays it.
    pub fn step_sample(&mut self, speaker: SpeakerId, forward: bool, cx: &mut Context<Self>) {
        let Some(source) = self.sample_source() else {
            return;
        };
        let key = sample_key(speaker);
        let clips = speaker_clips(self.data.as_ref(), speaker);
        let Some(ix) = self.sample_cursor.step(&key, clips, forward) else {
            return;
        };
        let clip = clip_of(&source, &clips[ix]);
        self.play_sample(key, clip, cx);
    }

    fn play_sample(&mut self, key: SharedString, clip: Clip, cx: &mut Context<Self>) {
        if self.playback.loaded || self.playback.preparing || self.playback.pending_start.is_some()
        {
            self.stop_owned_playback(cx);
            self.playback.pending_start = None;
        }
        self.ensure_player(cx);
        let state = self.state.clone();
        self.samples.play(key, clip, &state, cx, samples_of);
    }

    /// Whether `speaker`'s sample plays.
    pub fn sample_playing(&self, speaker: SpeakerId) -> bool {
        self.samples.is_playing(&sample_key(speaker))
    }

    fn stop_tick(&mut self) {
        self.tick = None;
        self.ticking = false;
    }

    fn start_tick(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.ticking {
            return;
        }
        self.ticking = true;
        self.tick = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(PLAYBACK_TICK).await;
                let keep = this
                    .update_in(cx, |view, window, cx| view.on_tick(window, cx))
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        }));
    }

    /// One 4 Hz poll; `false` ends the tick.
    fn on_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.ticking || !self.playback.loaded || !self.playback.playing {
            self.ticking = false;
            return false;
        }
        let polled = self.state.update(cx, |s, _| {
            self.player_owner
                .and_then(|owner| s.player_owned(owner))
                .map(|p| p.poll())
        });
        let before_s = self.playback.position_s as u64;
        let before_line = self.playback.current_line;
        let keep = match polled {
            // Still opening: keep following it like playback.
            Some(PlayerState::Playing { position_s } | PlayerState::Loading { position_s }) => {
                self.set_position(position_s, true, window, cx);
                true
            }
            Some(PlayerState::Paused { position_s }) => {
                self.set_position(position_s, true, window, cx);
                self.playback.playing = false;
                false
            }
            Some(PlayerState::Finished { position_s }) => {
                self.set_position(position_s, true, window, cx);
                self.playback.playing = false;
                self.playback.loaded = false;
                self.playback.finished = true;
                false
            }
            Some(PlayerState::Failed { message }) => {
                self.playback.error = Some(message);
                self.playback.playing = false;
                self.playback.loaded = false;
                false
            }
            Some(PlayerState::Stopped) | None => {
                self.playback.playing = false;
                self.playback.loaded = false;
                false
            }
        };
        if !keep {
            self.ticking = false;
        }
        if !keep
            || self.playback.position_s as u64 != before_s
            || self.playback.current_line != before_line
        {
            cx.notify();
        }
        keep
    }

    fn set_position(
        &mut self,
        position_s: f64,
        follow: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.playback.position_s = position_s;
        let line = self
            .data
            .as_ref()
            .and_then(|d| line_at(&d.lines, position_s));
        if line != self.playback.current_line {
            self.playback.current_line = line;
            if follow
                && self.following_transcript
                && self.naming.is_none()
                && !self.find_input.read(cx).focus_handle(cx).is_focused(window)
                && self.tab == MeetingTab::Transcript
                && let Some(ix) = line
            {
                self.reveal_transcript_line(ix);
            }
        }
        if self.scrub.is_none()
            && let Some((slider, ..)) = &self.slider
        {
            slider.update(cx, |s, cx| s.set_value(position_s as f32, window, cx));
        }
    }

    fn ensure_slider(&mut self, duration_s: f64, window: &mut Window, cx: &mut Context<Self>) {
        let max = if duration_s > 0.0 {
            duration_s as f32
        } else {
            1.0
        };
        if self.slider.as_ref().is_some_and(|(_, m, _)| *m == max) {
            return;
        }
        let position = (self.playback.position_s as f32).min(max);
        let slider = cx.new(|_| {
            SliderState::new()
                .min(0.0)
                .max(max)
                .step(1.0)
                .default_value(position)
        });
        let sub = cx.subscribe_in(
            &slider,
            window,
            |this, _, ev: &SliderEvent, window, cx| match ev {
                SliderEvent::Change(v) => this.scrub_to(f64::from(v.start()), window, cx),
                SliderEvent::Release(v) => {
                    this.scrub_generation += 1;
                    this.scrub = None;
                    this.seek(f64::from(v.start()), window, cx);
                }
            },
        );
        self.slider = Some((slider, max, sub));
    }

    /// A slider move: drags end with a release; keyboard steps only change,
    /// so a change that is not followed by another one seeks shortly after.
    fn scrub_to(&mut self, position_s: f64, window: &Window, cx: &mut Context<Self>) {
        self.scrub = Some(position_s);
        self.scrub_generation += 1;
        let generation = self.scrub_generation;
        cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(SCRUB_SETTLE).await;
            this.update_in(cx, |view, window, cx| {
                if view.scrub_generation == generation
                    && let Some(position) = view.scrub.take()
                {
                    view.seek(position, window, cx);
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn request_cache(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some(AudioSource::Available { sha256, .. }) = self.data.as_ref().map(|d| &d.audio)
        else {
            return;
        };
        let sha256 = sha256.clone();
        let lib = self.state.read(cx).library().clone();
        let id = self.meeting_id.clone();
        self.playback.cache_failed = None;
        let task = cx
            .background_executor()
            .spawn(async move { enqueue_playback_cache(&lib, &id, &sha256) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |view, window, cx| {
                match result {
                    Ok(job) => {
                        view.state.update(cx, |s, cx| {
                            s.clear_playback_job_failure(&job);
                            s.kick_jobs(cx);
                        });
                        view.playback.cache_job = Some(job);
                        // The cache may already exist (made by another run).
                        view.reload(window, cx);
                    }
                    Err(e) => {
                        view.playback.pending_start = None;
                        tracing::warn!(code = e.code(), "queueing the playback cache");
                        view.playback.error = Some(core_user_text(&e));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---- library writes ----

    /// Displays the status of a serialized write only at its own control.
    fn edit_line(
        &self,
        target: &EditTarget,
        id: impl Into<ElementId>,
        cx: &Context<Self>,
    ) -> Option<Stateful<Div>> {
        let (kind, words) = if self
            .busy
            .as_ref()
            .is_some_and(|(active, _)| active == target)
        {
            (StatusKind::Pending, "Saving…")
        } else {
            match self.edit_status.get(target)? {
                Notice::Info(text) => (StatusKind::Done, text.as_str()),
                Notice::Error(text) => (StatusKind::Failed, text.as_str()),
            }
        };
        Some(kit::status_line(id, kind, words.to_owned(), cx))
    }

    fn write_refused_while_busy(&mut self, target: &EditTarget, cx: &mut Context<Self>) -> bool {
        let Some((active, wait)) = &self.busy else {
            return false;
        };
        if active != target {
            self.edit_status
                .insert(target.clone(), Notice::Error((*wait).to_owned()));
            self.last_edit = Some(target.clone());
            cx.notify();
        }
        true
    }

    /// Runs one library edit, reports beside its control, then reloads.
    #[allow(clippy::too_many_arguments)]
    fn run_write_then<T: Send + 'static>(
        &mut self,
        target: EditTarget,
        wait: &'static str,
        window: &Window,
        cx: &mut Context<Self>,
        work: impl FnOnce(&Library) -> Result<T> + Send + 'static,
        finish: impl FnOnce(&mut Self, Result<T>, &mut Window, &mut Context<Self>) -> Notice + 'static,
    ) {
        if self.write_refused_while_busy(&target, cx) {
            return;
        }
        let speaker_focus = match &target {
            EditTarget::Speaker(speaker) => self.speaker_focus_return(*speaker, window, cx),
            _ => None,
        };
        self.busy = Some((target.clone(), wait));
        self.edit_status.remove(&target);
        self.undo = None;
        cx.notify();
        let lib = self.state.read(cx).library().clone();
        let task = cx.background_executor().spawn(async move { work(&lib) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            this.update_in(cx, |view, window, cx| {
                view.busy = None;
                let notice = finish(view, result, window, cx);
                if matches!(notice, Notice::Info(_)) {
                    view.speaker_focus_return = speaker_focus;
                }
                view.last_edit = Some(target.clone());
                if let (EditTarget::LiveReview(decision), Notice::Info(_)) = (&target, &notice) {
                    let next = view.data.as_ref().and_then(|data| {
                        let current = data
                            .live_review
                            .iter()
                            .position(|item| item.decision == *decision)?;
                        data.live_review.get(current + 1).map(|item| item.decision)
                    });
                    view.settled_live_focus = Some((*decision, next));
                }
                view.edit_status.insert(target, notice);
                view.state.update(cx, |s, cx| s.refresh_review_count(cx));
                view.reload(window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    #[cfg(test)]
    fn last_notice(&self) -> Option<&Notice> {
        self.edit_status.get(self.last_edit.as_ref()?)
    }

    fn speaker_focus_return(
        &self,
        speaker: SpeakerId,
        window: &Window,
        cx: &App,
    ) -> Option<SpeakerFocusReturn> {
        if self.tab != MeetingTab::Speakers {
            return None;
        }
        let rows = &self.data.as_ref()?.speakers;
        let index = rows.iter().position(|row| row.id == speaker)?;
        let neighbours = [
            rows.get(index + 1).map(|row| row.id),
            index.checked_sub(1).map(|index| rows[index].id),
        ];
        let opener = window.focused(cx)?;
        let control = SPEAKER_CONTROLS
            .into_iter()
            .find(|control| self.speaker_focus.get(&(speaker, *control)) == Some(&opener))
            .or_else(|| (opener == self.speaker_list_focus).then_some(SpeakerControl::Row))
            .or_else(|| {
                let naming_speaker = self.naming.as_ref().and_then(NamingSession::speaker);
                (naming_speaker == Some(speaker)).then_some(SpeakerControl::Name)
            })?;
        Some(SpeakerFocusReturn {
            opener,
            speaker,
            control,
            neighbours,
        })
    }

    fn sync_speaker_focus(
        &mut self,
        data: &MeetingData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.speaker_focus
            .retain(|(id, _), _| data.speakers.iter().any(|row| row.id == *id));
        for row in &data.speakers {
            for control in SPEAKER_CONTROLS {
                self.speaker_focus
                    .entry((row.id, control))
                    .or_insert_with(|| cx.focus_handle());
            }
        }
        if !data
            .speakers
            .iter()
            .any(|row| Some(row.id) == self.selected_speaker)
        {
            self.selected_speaker = self
                .speaker_focus_return
                .as_ref()
                .into_iter()
                .flat_map(|restore| restore.neighbours.into_iter().flatten())
                .find(|id| data.speakers.iter().any(|row| row.id == *id))
                .or_else(|| data.speakers.first().map(|row| row.id));
        }
        if self.speaker_menu.as_ref().is_some_and(|menu| {
            menu.fence != SpeakerFence::of(&data.meeting)
                || !data.speakers.iter().any(|row| row.id == menu.speaker)
        }) {
            self.close_speaker_menu(window, cx);
        }
        let Some(restore) = self.speaker_focus_return.take() else {
            return;
        };
        if self.tab != MeetingTab::Speakers || !restore.opener.is_focused(window) {
            return;
        }
        let target = if let Some(row) = data.speakers.iter().find(|row| row.id == restore.speaker) {
            let present = match restore.control {
                SpeakerControl::Accept | SpeakerControl::Reject => row.proposal.is_some(),
                SpeakerControl::Review => {
                    row.review_dismissed || row.assignment == Assignment::None
                }
                SpeakerControl::More => self.speaker_has_menu(row, data),
                SpeakerControl::Row | SpeakerControl::Name => true,
            };
            let control = if present {
                restore.control
            } else {
                SpeakerControl::Name
            };
            if control == SpeakerControl::Row {
                None
            } else {
                self.speaker_focus.get(&(row.id, control))
            }
        } else {
            None
        };
        let fallback = if data.speakers.is_empty() && !data.speakers_without_evidence.is_empty() {
            &self.speaker_disclosure_focus
        } else {
            &self.speaker_list_focus
        };
        target.unwrap_or(fallback).focus(window, cx);
    }

    fn named_speaker_message(&self, naming: &Naming) -> String {
        naming.message(&format!(
            "{} in “{}”",
            raw_label(naming.speaker_id),
            self.window_title()
        ))
    }

    fn model_and_thresholds(&self, cx: &App) -> (String, Option<Thresholds>) {
        let config = self.state.read(cx).config();
        let model = config.profile.speaker_embedding.clone();
        let thresholds = config.thresholds(&model).copied();
        (model, thresholds)
    }

    /// Names (or renames) the speaker of naming session `generation`; the
    /// picker closes once that write succeeds and stays open on failure.
    /// Opened from transcript line `line` (its segments), a failure shows
    /// under that line instead of the banner.
    #[allow(clippy::too_many_arguments)]
    fn name_speaker(
        &mut self,
        speaker_id: SpeakerId,
        choice: PersonChoice,
        fence: SpeakerFence,
        generation: u64,
        line: Option<Vec<u32>>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let model = self.model_and_thresholds(cx).0;
        let id = self.meeting_id.clone();
        let target = line
            .clone()
            .map_or(EditTarget::Speaker(speaker_id), EditTarget::Line);
        let history_target = target.clone();
        self.run_write_then(
            target,
            "Wait until the speaker is named.",
            window,
            cx,
            move |lib| {
                name_with_history(
                    lib,
                    &id,
                    speaker_id,
                    choice.person_ref(),
                    &model,
                    None,
                    &fence,
                    true,
                )
            },
            move |view, result, window, cx| {
                let (naming, edit) = match result {
                    Ok(naming) => naming,
                    Err(e) if line.is_some() => {
                        return Notice::Error(attribution_refusal(
                            e.code(),
                            &meeting_user_text(&e),
                        ));
                    }
                    Err(e) => {
                        return Notice::Error(identity_failure("Couldn't name the speaker.", &e));
                    }
                };
                if view.naming_is(generation) {
                    view.dismiss_naming(window, cx);
                }
                view.remember_identity(history_target, "Identification", edit);
                let message = view.named_speaker_message(&naming);
                view.state
                    .update(cx, |s, cx| s.recheck_requested(naming.learning, cx));
                Notice::Info(message)
            },
        );
    }

    /// The fence of the speakers the view shows now, if loaded.
    fn shown_fence(&self) -> Option<SpeakerFence> {
        self.data.as_ref().map(|d| SpeakerFence::of(&d.meeting))
    }

    pub fn accept_proposal(
        &mut self,
        speaker_id: SpeakerId,
        person: PersonId,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(fence) = self.shown_fence() else {
            return;
        };
        let model = self.model_and_thresholds(cx).0;
        let id = self.meeting_id.clone();
        self.run_write_then(
            EditTarget::Speaker(speaker_id),
            "Wait until the suggestion is accepted.",
            window,
            cx,
            move |lib| {
                name_with_history(
                    lib,
                    &id,
                    speaker_id,
                    person.as_str(),
                    &model,
                    Some(&person),
                    &fence,
                    true,
                )
            },
            move |view, result, _, cx| match result {
                Ok((naming, edit)) => {
                    view.remember_identity(EditTarget::Speaker(speaker_id), "Identification", edit);
                    let message = view.named_speaker_message(&naming);
                    view.state
                        .update(cx, |s, cx| s.recheck_requested(naming.learning, cx));
                    Notice::Info(message)
                }
                Err(e) => Notice::Error(identity_failure("Couldn't accept the suggestion.", &e)),
            },
        );
    }

    pub fn reject_proposal(
        &mut self,
        speaker_id: SpeakerId,
        person: PersonId,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(fence) = self.shown_fence() else {
            return;
        };
        let Some(proposal) = self.data.as_ref().and_then(|data| {
            data.speakers
                .iter()
                .find(|row| row.id == speaker_id)?
                .proposal
                .as_ref()
                .filter(|proposal| proposal.person_id == person)
        }) else {
            return;
        };
        let rejected = format!(
            "{}: {} rejected. BroScribe won't suggest them for this speaker again.",
            raw_label(speaker_id),
            proposal.name
        );
        let (model, thresholds) = self.model_and_thresholds(cx);
        let id = self.meeting_id.clone();
        let cli = self.state.read(cx).cli().clone();
        self.run_write_then(
            EditTarget::Speaker(speaker_id),
            "Wait until the suggestion is rejected.",
            window,
            cx,
            move |lib| {
                reject_proposal(
                    lib,
                    &id,
                    speaker_id,
                    &person,
                    &model,
                    thresholds.as_ref(),
                    &fence,
                    &cli,
                )
            },
            move |view, result, _, cx| match result {
                Ok(queued) => {
                    if queued {
                        view.state.update(cx, |s, cx| s.kick_jobs(cx));
                    }
                    Notice::Info(rejected)
                }
                Err(e) => Notice::Error(identity_failure("Couldn't reject the suggestion.", &e)),
            },
        );
    }

    /// "Can't tell" (`dismissed`) or Restore for the speaker shown as
    /// `speaker`, against the speakers the view shows now; a dismissal offers
    /// Undo for [`UNDO_WINDOW`].
    pub fn set_dismissed(
        &mut self,
        speaker: SpeakerId,
        dismissed: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some((fence, label)) = self.data.as_ref().map(|d| {
            (
                SpeakerFence::of(&d.meeting),
                speaker_label(&d.meeting, Some(speaker), &d.people),
            )
        }) else {
            return;
        };
        self.write_dismissed(speaker, label, fence, dismissed, window, cx);
    }

    /// Undoes the last "Can't tell" while it is offered.
    pub fn undo_dismiss(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        if let Some(undo) = self.undo.take() {
            if self
                .undo_edits
                .last()
                .is_some_and(|step| step.target == EditTarget::Speaker(undo.speaker))
            {
                self.replay_edit(false, window, cx);
            } else {
                self.write_dismissed(undo.speaker, undo.label, undo.fence, false, window, cx);
            }
        }
    }

    fn write_dismissed(
        &mut self,
        speaker: SpeakerId,
        label: String,
        fence: SpeakerFence,
        dismissed: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.meeting_id.clone();
        let (action, failed) = if dismissed {
            (
                "Wait until the speaker is dismissed.",
                "Couldn't dismiss the speaker from review.",
            )
        } else {
            (
                "Wait until the speaker is restored.",
                "Couldn't restore the speaker to review.",
            )
        };
        let undo_fence = fence.clone();
        self.run_reversible_write(
            EditTarget::Speaker(speaker),
            history::Scope::Dismiss {
                speaker,
                fence: fence.clone(),
            },
            if dismissed { "Can’t Tell" } else { "Restore" },
            action,
            window,
            cx,
            move |lib| set_review_dismissed(lib, &id, speaker, &fence, dismissed),
            move |view, result, _, cx| match result {
                Ok(outcome) if outcome.dismissed => {
                    let words = format!("{label}: dismissed.");
                    view.offer_undo(outcome.speaker_id, label, undo_fence, cx);
                    Notice::Info(words)
                }
                Ok(_) => Notice::Info(format!("{label}: back in the review queue.")),
                Err(e) => Notice::Error(identity_failure(failed, &e)),
            },
        );
    }

    fn offer_undo(
        &mut self,
        speaker: SpeakerId,
        label: String,
        fence: SpeakerFence,
        cx: &mut Context<Self>,
    ) {
        self.undo_generation += 1;
        let generation = self.undo_generation;
        self.undo = Some(UndoDismiss {
            speaker,
            label,
            fence,
            generation,
        });
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(UNDO_WINDOW).await;
            this.update(cx, |view, cx| {
                if view
                    .undo
                    .as_ref()
                    .is_some_and(|u| u.generation == generation)
                {
                    if let Some(undo) = view.undo.take() {
                        view.undo_edits
                            .retain(|edit| edit.target != EditTarget::Speaker(undo.speaker));
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Merges, unmerges or separates speakers against the
    /// speakers the view shows now; the result or the refusal becomes the
    /// notice.
    pub fn change_grouping(
        &mut self,
        change: GroupingChange,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(fence) = self.shown_fence() else {
            return;
        };
        let target = match &change {
            GroupingChange::Merge { members, .. } => members.first().copied(),
            GroupingChange::Unmerge { group } => self.data.as_ref().and_then(|data| {
                data.speakers
                    .iter()
                    .find(|row| row.merged.as_ref().is_some_and(|m| &m.group_id == group))
                    .map(|row| row.id)
            }),
            GroupingChange::NotSame { a, .. } => Some(*a),
        }
        .map_or(EditTarget::Speakers, EditTarget::Speaker);
        // A picker open on a speaker of this meeting no longer matches.
        self.end_naming(cx);
        let id = self.meeting_id.clone();
        let (model, thresholds) = self.model_and_thresholds(cx);
        let name = match change {
            GroupingChange::Merge { .. } => "Speaker Merge",
            GroupingChange::Unmerge { .. } => "Speaker Separation",
            GroupingChange::NotSame { .. } => "Different Speakers",
        };
        let history_target = target.clone();
        self.run_write_then(
            target,
            "Wait until the speakers are updated.",
            window,
            cx,
            move |lib| {
                ops::with_index(lib, |tx, index| {
                    ops::record_review_edit(tx, index, &model, |tx, index| {
                        let (value, refresh) = match &change {
                            GroupingChange::Merge { kept, members } => {
                                let outcome = ops::group_speakers(
                                    tx,
                                    index,
                                    &id,
                                    *kept,
                                    members,
                                    GroupOrigin::User,
                                    &fence,
                                    &model,
                                    thresholds.as_ref(),
                                )?;
                                (json!(&outcome), outcome.changed)
                            }
                            GroupingChange::Unmerge { group } => (
                                json!(ops::ungroup_speakers(tx, index, &id, group, &fence)?),
                                true,
                            ),
                            GroupingChange::NotSame { a, b } => (
                                json!(ops::mark_not_same(tx, index, &id, *a, *b, &fence)?),
                                false,
                            ),
                        };
                        if refresh {
                            Queue::open(tx.root())?.enqueue(
                                tx,
                                VECTORS_KIND,
                                id.as_str(),
                                json!({ "meeting_id": id }),
                            )?;
                        }
                        Ok((grouping_report(&change, &value), refresh))
                    })
                })
            },
            move |view, result, _, cx| match result {
                Ok(((text, refresh), edit)) => {
                    view.remember_identity(history_target, name, edit);
                    if refresh {
                        view.state.update(cx, |state, cx| state.kick_jobs(cx));
                    }
                    Notice::Info(text)
                }
                Err(e) => Notice::Error(grouping_refusal(e.code(), &meeting_user_text(&e))),
            },
        );
    }

    pub fn add_tags(&mut self, tags: Vec<String>, window: &Window, cx: &mut Context<Self>) {
        self.write_tags(tags, Vec::new(), None, window, cx);
    }

    fn write_tags(
        &mut self,
        add: Vec<String>,
        remove: Vec<String>,
        draft: Option<String>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if add.is_empty() && remove.is_empty() {
            return;
        }
        let adding = remove.is_empty();
        let id = self.meeting_id.clone();
        let focus = remove.first().and_then(|tag| {
            let from = self.tag_focus.get(tag)?;
            if !from.is_focused(window) {
                return None;
            }
            let tags = self.data.as_ref()?.meeting.effective_tags();
            let next = tags.iter().position(|value| value == tag)? + 1;
            Some((from.clone(), tags.get(next).cloned()))
        });
        let (wait, failed, done) = if adding {
            (
                "Wait until the tag is added.",
                "Couldn't add the tag.",
                "Tags saved.",
            )
        } else {
            (
                "Wait until the tag is removed.",
                "Couldn't remove the tag.",
                "Tag removed.",
            )
        };
        self.run_reversible_write(
            EditTarget::Tags,
            history::Scope::Tags,
            if adding { "Add Tag" } else { "Remove Tag" },
            wait,
            window,
            cx,
            move |lib| edit_tags(lib, &id, &add, &remove),
            move |view, result, window, cx| match result {
                Ok(tags) => {
                    if draft.is_some_and(|sent| view.tag_input.read(cx).value().as_str() == sent) {
                        view.tag_input
                            .update(cx, |input, cx| input.set_value("", window, cx));
                    }
                    if let Some((from, next)) = focus
                        && from.is_focused(window)
                    {
                        if let Some(target) = next
                            .filter(|tag| tags.contains(tag))
                            .and_then(|tag| view.tag_focus.get(&tag))
                        {
                            target.focus(window, cx);
                        } else if view.tag_open {
                            view.tag_input
                                .update(cx, |input, cx| input.focus(window, cx));
                        } else {
                            view.add_tag_focus.focus(window, cx);
                        }
                    }
                    Notice::Info(done.to_owned())
                }
                Err(e) => Notice::Error(failure_text(failed, &meeting_user_text(&e), "")),
            },
        );
    }

    pub fn remove_tag(&mut self, tag: String, window: &Window, cx: &mut Context<Self>) {
        self.write_tags(Vec::new(), vec![tag], None, window, cx);
    }

    fn set_action_status(
        &mut self,
        action: String,
        status: ActionStatus,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.meeting_id.clone();
        self.run_reversible_write(
            EditTarget::Action(action.clone()),
            history::Scope::Action(action.clone()),
            match status {
                ActionStatus::Done => "Mark Done",
                ActionStatus::Dropped => "Drop Action",
                ActionStatus::Open => "Reopen Action",
            },
            "Wait until the action item is saved.",
            window,
            cx,
            move |lib| {
                ops::with_index(lib, |tx, index| {
                    ops::set_action_status(tx, index, &id, &action, status)
                })
            },
            move |_, result, _, _| match result {
                Ok(_) => Notice::Info(
                    match status {
                        ActionStatus::Done => "Marked done.",
                        ActionStatus::Dropped => "Dropped.",
                        ActionStatus::Open => "Reopened.",
                    }
                    .to_owned(),
                ),
                Err(e) => Notice::Error(failure_text(
                    "Couldn't update the action.",
                    &meeting_user_text(&e),
                    "",
                )),
            },
        );
    }

    fn open_tag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tag_open {
            self.edit_status.remove(&EditTarget::Tags);
            self.tag_open = true;
        }
        self.tag_input
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn close_tag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tag_open = false;
        self.tag_choice = None;
        self.tag_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.add_tag_focus.focus(window, cx);
        cx.notify();
    }

    fn tag_suggestions(&self, cx: &App) -> Vec<&str> {
        use broscribe_core::schema::fold_tag;
        let value = self.tag_input.read(cx).value();
        let prefix = fold_tag(value.rsplit(',').next().unwrap_or_default());
        let Some(data) = &self.data else {
            return Vec::new();
        };
        let current = data.meeting.effective_tags();
        data.library_tags
            .iter()
            .filter(|tag| {
                fold_tag(tag).starts_with(&prefix)
                    && !current
                        .iter()
                        .any(|existing| fold_tag(existing) == fold_tag(tag))
            })
            .take(8)
            .map(String::as_str)
            .collect()
    }

    fn choose_tag(&mut self, suggestion: &str, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.tag_input.read(cx).value();
        let draft = value.rsplit_once(',').map_or_else(
            || suggestion.to_owned(),
            |(prefix, _)| format!("{prefix}, {suggestion}"),
        );
        self.tag_input
            .update(cx, |input, cx| input.set_value(draft, window, cx));
        self.tag_input
            .update(cx, |input, cx| input.focus(window, cx));
        self.submit_tag(window, cx);
    }

    fn submit_tag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.write_refused_while_busy(&EditTarget::Tags, cx) {
            return;
        }
        let value = self.tag_input.read(cx).value().to_string();
        let mut tags = typed_tags(&value);
        if let Some(suggestion) = self
            .tag_choice
            .take()
            .and_then(|index| self.tag_suggestions(cx).get(index).copied())
        {
            if !value.trim_end().ends_with(',') {
                tags.pop();
            }
            tags.push(suggestion.to_owned());
        }
        if let Some(data) = &self.data {
            use broscribe_core::schema::fold_tag;
            for tag in &mut tags {
                if let Some(existing) = data
                    .library_tags
                    .iter()
                    .find(|existing| fold_tag(existing) == fold_tag(tag))
                {
                    tag.clone_from(existing);
                }
            }
        }
        self.write_tags(tags, Vec::new(), Some(value), window, cx);
    }

    /// "Rename": a title field prefilled with the shown title.
    pub fn start_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start_rename_from(self.title_focus.clone(), window, cx);
    }

    fn start_rename_from(
        &mut self,
        opener: gpui::FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((input, _)) = &self.rename {
            input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        self.edit_status.remove(&EditTarget::Title);
        let current = self.window_title();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Meeting title")
                .default_value(current)
        });
        input.update(cx, |input, cx| input.focus(window, cx));
        let subscription = cx.subscribe_in(
            &input,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } => this.submit_rename(window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            },
        );
        self.rename = Some((input, subscription));
        self.rename_opener = Some(opener);
        cx.notify();
    }

    pub fn cancel_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_rename(true, window, cx);
    }

    fn finish_rename(&mut self, restore: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.rename = None;
        if let Some(opener) = self.rename_opener.take()
            && restore
        {
            opener.focus(window, cx);
        }
        cx.notify();
    }

    fn cancel_transient(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.rename.is_some() && self.rename_form_focus.contains_focused(window, cx) {
            self.cancel_rename(window, cx);
        } else if self.tag_open && self.tag_input.read(cx).focus_handle(cx).is_focused(window) {
            self.close_tag(window, cx);
        } else if self.find_open
            && self.tab == MeetingTab::Transcript
            && self.naming.is_none()
            && self.transcript_menu.is_none()
        {
            self.close_find(window, cx);
        } else {
            return;
        }
        cx.stop_propagation();
    }

    /// The title typed in the open rename field, when it can be saved.
    fn rename_draft(&self, cx: &App) -> Option<String> {
        let (input, _) = self.rename.as_ref()?;
        let title = input.read(cx).value().trim().to_owned();
        (!title.is_empty() && title != self.window_title()).then_some(title)
    }

    /// Saves the typed title through `title <id> --stdin`; the field closes
    /// only once it is saved, so a refusal keeps the draft.
    pub fn submit_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(title) = self.rename_draft(cx) else {
            return;
        };
        let input = self.rename.as_ref().map(|(input, _)| input.entity_id());
        let sent = title.clone();
        let id = self.meeting_id.clone();
        let cli = self.state.read(cx).cli().clone();
        self.run_reversible_write(
            EditTarget::Title,
            history::Scope::Title,
            "Rename",
            "Wait until the title is saved.",
            window,
            cx,
            move |_| Ok(rename_meeting(&cli, &id, &title)),
            move |view, result, window, cx| match result {
                Ok(Ok(text)) => {
                    if view.rename.as_ref().is_some_and(|(current, _)| {
                        Some(current.entity_id()) == input
                            && current.read(cx).value().trim() == sent
                    }) {
                        let restore = view.rename_form_focus.contains_focused(window, cx);
                        view.finish_rename(restore, window, cx);
                    }
                    Notice::Info(text)
                }
                Ok(Err(e)) => Notice::Error(rename_failure(&e.user_text())),
                Err(e) => Notice::Error(rename_failure(&meeting_user_text(&e))),
            },
        );
    }

    /// "Delete meeting": opens the confirmation, which first asks the CLI
    /// what deleting would remove.
    pub fn open_delete(&mut self, cx: &mut Context<Self>) {
        if self.delete.is_some() {
            return;
        }
        let title = self.window_title();
        let state = self.state.clone();
        let id = self.meeting_id.clone();
        let prompt = cx.new(|cx| DeleteMeetingPrompt::new(state, id, title, cx));
        let subscription = cx.subscribe(&prompt, |this, _, ev: &DeleteEvent, cx| {
            this.delete = None;
            match ev {
                DeleteEvent::Deleted { .. } => {
                    this.stop_owned_playback(cx);
                    if !this.embedded {
                        this.state
                            .update(cx, |state, cx| state.navigate(Route::library(), cx));
                    }
                }
                DeleteEvent::Cancelled => {}
            }
            cx.notify();
        });
        self.delete = Some((prompt, subscription));
        cx.notify();
    }

    pub(crate) fn present_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.present_delete_from(self.delete_focus.clone(), window, cx);
    }

    fn present_delete_from(
        &mut self,
        opener: gpui::FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.delete.is_none() && self.export.is_none() && !window.has_active_dialog(cx) {
            self.open_delete(cx);
            if let Some((prompt, _)) = &self.delete {
                DeleteMeetingPrompt::present(prompt, opener, window, cx);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn delete_prompt(&self) -> Option<&Entity<DeleteMeetingPrompt>> {
        self.delete.as_ref().map(|(prompt, _)| prompt)
    }

    /// Opens the person picker for `speaker_id` (Speakers tab).
    pub fn start_naming(
        &mut self,
        speaker_id: SpeakerId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self
            .data
            .as_ref()
            .and_then(|d| d.speakers.iter().find(|row| row.id == speaker_id))
        else {
            return;
        };
        let label = format!("Name for {}", row.label);
        self.open_picker(NamingTarget::Speaker(speaker_id), label, window, cx);
    }

    /// Opens the person picker for transcript line `ix`, with the segment
    /// fingerprints taken now. The scope starts at "Every line of Speaker N"
    /// (only "Only this line" when the line has no speaker).
    pub fn start_line_naming(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.open_line_naming(ix, None, false, window, cx);
    }

    fn open_line_naming(
        &mut self,
        ix: usize,
        scope: Option<NamingScope>,
        button_anchor: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(data) = self.data.as_ref() else {
            return;
        };
        let Some(line) = data.lines.get(ix) else {
            return;
        };
        let Some(fingerprints) = line_fingerprints(&data.meeting, &line.segment_ids) else {
            return;
        };
        let label = format!("Name for the line at {}", spoken_position(line.start));
        let target = NamingTarget::Line {
            speaker: line.speaker,
            segment_ids: line.segment_ids.clone(),
            fingerprints,
            scope: if line.speaker.is_some() {
                scope.unwrap_or(NamingScope::SpeakerLines)
            } else {
                NamingScope::ThisLine
            },
            button_anchor,
        };
        self.edit_status
            .remove(&EditTarget::Line(line.segment_ids.clone()));
        self.select_transcript_row(ix, window, cx);
        self.open_picker(target, label, window, cx);
    }

    fn open_picker(
        &mut self,
        target: NamingTarget,
        label: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(data) = self.data.as_ref() else {
            return;
        };
        let fence = SpeakerFence::of(&data.meeting);
        let voice = match &target {
            NamingTarget::Speaker(speaker) => Some(*speaker),
            NamingTarget::Line { speaker, .. } => *speaker,
        }
        .and_then(|speaker| VoiceTarget::of(&data.meeting, speaker))
        .map(std::sync::Arc::new);
        self.naming_generation += 1;
        let generation = self.naming_generation;
        let state = self.state.clone();
        let line = matches!(target, NamingTarget::Line { .. });
        let speaker_opener = matches!(target, NamingTarget::Speaker(_))
            .then(|| window.focused(cx))
            .flatten();
        let picker = cx.new(|cx| {
            let mut picker = PersonPicker::new_unfocused(state, label, window, cx);
            if line {
                picker.set_cancel_button(false);
            } else {
                picker.focus_search(window, cx);
            }
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
                        this.dismiss_naming(window, cx);
                    }
                }
            },
        );
        self.naming = Some(NamingSession {
            target,
            fence,
            generation,
            picker,
            speaker_opener,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// The open picker, if any.
    pub fn picker(&self) -> Option<&Entity<PersonPicker>> {
        self.naming.as_ref().map(|n| &n.picker)
    }

    /// The scope of the open line picker, if one is open.
    pub fn naming_scope(&self) -> Option<NamingScope> {
        match self.naming.as_ref().map(|n| &n.target) {
            Some(NamingTarget::Line { scope, .. }) => Some(*scope),
            _ => None,
        }
    }

    /// Switches the open line picker's scope; "Every line of Speaker N" only
    /// when the line has a speaker.
    pub fn set_naming_scope(&mut self, new: NamingScope, cx: &mut Context<Self>) {
        if let Some(NamingTarget::Line { speaker, scope, .. }) =
            self.naming.as_mut().map(|n| &mut n.target)
            && *scope != new
            && (new == NamingScope::ThisLine || speaker.is_some())
        {
            *scope = new;
            cx.notify();
        }
    }

    #[cfg(test)]
    fn line_error(&self) -> Option<(Vec<u32>, String)> {
        let target = self.last_edit.as_ref()?;
        match (target, self.edit_status.get(target)?) {
            (EditTarget::Line(ids), Notice::Error(text)) => Some((ids.clone(), text.clone())),
            _ => None,
        }
    }

    fn naming_is(&self, generation: u64) -> bool {
        self.naming
            .as_ref()
            .is_some_and(|n| n.generation == generation)
    }

    /// A choice from the picker of session `generation`: names what the
    /// picker was opened for while the session, the route, the tab and the
    /// meeting's speakers or lines are unchanged.
    fn choose(
        &mut self,
        generation: u64,
        choice: PersonChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.naming.as_ref().filter(|n| n.generation == generation) else {
            return;
        };
        let tab = match session.target {
            NamingTarget::Speaker(_) => MeetingTab::Speakers,
            NamingTarget::Line { .. } => MeetingTab::Transcript,
        };
        let current = matches!(self.state.read(cx).route(), Route::Library { selected: Some(id) } if *id == self.meeting_id)
            && self.tab == tab
            && self
                .data
                .as_ref()
                .is_some_and(|d| naming_still_valid(session, d));
        if !current {
            self.end_naming(cx);
            return;
        }
        let fence = session.fence.clone();
        match session.target.clone() {
            NamingTarget::Speaker(speaker) => {
                self.name_speaker(speaker, choice, fence, generation, None, window, cx)
            }
            NamingTarget::Line {
                speaker: Some(speaker),
                segment_ids,
                scope: NamingScope::SpeakerLines,
                ..
            } => self.name_speaker(
                speaker,
                choice,
                fence,
                generation,
                Some(segment_ids),
                window,
                cx,
            ),
            NamingTarget::Line {
                segment_ids,
                fingerprints,
                ..
            } => self.attribute(
                segment_ids,
                fingerprints,
                Some(choice),
                Some(generation),
                fence,
                window,
                cx,
            ),
        }
    }

    /// The live decision `decision` awaiting review, as loaded.
    fn live_review_item(&self, decision: u64) -> Option<&LiveReviewItem> {
        self.data
            .as_ref()?
            .live_review
            .iter()
            .find(|item| item.decision == decision)
    }

    /// Plays (or stops) the stretch of the recording a live decision
    /// covers; the playback bar pauses meanwhile (one player owner).
    pub fn toggle_live_review_audio(&mut self, decision: u64, cx: &mut Context<Self>) {
        let key = live_review_key(decision);
        if self.samples.is_playing(&key) {
            self.samples.stop(&self.state, cx);
            cx.notify();
            return;
        }
        let (Some(item), Some(source)) = (self.live_review_item(decision), self.sample_source())
        else {
            return;
        };
        let clip = Clip {
            source,
            start_s: item.from_s,
            end_s: item.to_s,
        };
        self.play_sample(key, clip, cx);
    }

    /// Shows the transcript at the first line a live decision touches.
    pub fn show_live_review(&mut self, decision: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some((ix, segment)) = self
            .live_review_segments
            .get(&decision)
            .and_then(|segments| {
                self.data
                    .as_ref()?
                    .lines
                    .iter()
                    .enumerate()
                    .find_map(|(ix, line)| {
                        line.segment_ids
                            .iter()
                            .find(|id| segments.contains(id))
                            .map(|id| (ix, *id))
                    })
            })
        else {
            return;
        };
        self.set_tab(MeetingTab::Transcript, cx);
        self.following_transcript = false;
        self.reviewed_line = Some(segment);
        self.select_transcript_row(ix, window, cx);
    }

    /// The user's explicit correction of the lines a live decision touches:
    /// names them as said during the recording, through the CLI against the
    /// speakers shown, like any line correction; that settles the decision.
    pub fn apply_live_review(&mut self, decision: u64, window: &Window, cx: &mut Context<Self>) {
        let Some((item, fence)) = self.data.as_ref().and_then(|d| {
            let item = d.live_review.iter().find(|i| i.decision == decision)?;
            Some((item.clone(), SpeakerFence::of(&d.meeting)))
        }) else {
            return;
        };
        let Some(person) = item.person_id.clone() else {
            return;
        };
        if item.lines.is_empty() {
            return;
        }
        let id = self.meeting_id.clone();
        let cli = self.state.read(cx).cli().clone();
        let count = item.lines.len();
        self.run_write_then(
            EditTarget::LiveReview(decision),
            "Wait until the lines are corrected.",
            window,
            cx,
            move |lib| {
                Ok(attribute_line(
                    lib,
                    &cli,
                    &id,
                    &item.lines,
                    Some(person.as_str()),
                    &fence,
                ))
            },
            move |_, result, _, _| {
                let lines = crate::ui::format::plural(count as u64, "line", "lines");
                let corrected = crate::ui::format::plural(count as u64, "line was", "lines were");
                match result {
                    Ok(Ok(Some(name))) => {
                        Notice::Info(format!("The {lines} now name {name}, as you said."))
                    }
                    Ok(Ok(None)) => Notice::Info(format!("The {corrected} corrected.")),
                    Ok(Err(e)) => Notice::Error(attribution_refusal(&e.code, &e.user_text())),
                    Err(e) => Notice::Error(attribution_refusal(e.code(), &meeting_user_text(&e))),
                }
            },
        );
    }

    /// "Leave as transcribed": settles a live decision without changing any
    /// line ([`ops::dismiss_live_review`]), fenced by the item exactly as
    /// shown (`decision` and `review_key`): a decision that changed since is
    /// refused and the meeting reloads.
    pub fn leave_live_review(&mut self, decision: u64, window: &Window, cx: &mut Context<Self>) {
        let Some(key) = self
            .live_review_item(decision)
            .map(|i| i.review_key.clone())
        else {
            return;
        };
        let id = self.meeting_id.clone();
        self.run_write_then(
            EditTarget::LiveReview(decision),
            "Wait until the name is reviewed.",
            window,
            cx,
            move |lib| {
                ops::with_index(lib, |tx, index| {
                    ops::dismiss_live_review(tx, index, &id, decision, &key)
                })
            },
            |_, result, _, _| match result {
                Ok(outcome) if !outcome.changed => {
                    Notice::Info("This name was already settled.".to_owned())
                }
                Ok(_) => Notice::Info("Left as transcribed.".to_owned()),
                Err(e) if e.code() == ops::STALE_REVIEW => Notice::Error(
                    "This name changed since it was shown; check it again.".to_owned(),
                ),
                Err(e) => Notice::Error(attribution_refusal(e.code(), &meeting_user_text(&e))),
            },
        );
    }

    /// "Use the speaker's name" on corrected line `ix`: removes its overlay.
    pub fn clear_line(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(line) = self.data.as_ref().and_then(|d| d.lines.get(ix)) else {
            return;
        };
        if line.line_person.is_none() {
            return;
        }
        let segment_ids = line.segment_ids.clone();
        let Some((fingerprints, fence)) = self.data.as_ref().and_then(|d| {
            Some((
                line_fingerprints(&d.meeting, &segment_ids)?,
                SpeakerFence::of(&d.meeting),
            ))
        }) else {
            return;
        };
        self.dismiss_line_naming(window, cx);
        self.attribute(segment_ids, fingerprints, None, None, fence, window, cx);
    }

    /// Writes (or with `None` removes) one line's person through the CLI
    /// (off the UI thread) against the speakers `fence` the line was read
    /// with, then reloads; a refusal shows under the line and keeps its
    /// picker open.
    #[allow(clippy::too_many_arguments)]
    fn attribute(
        &mut self,
        segment_ids: Vec<u32>,
        fingerprints: Vec<SegmentFingerprint>,
        choice: Option<PersonChoice>,
        generation: Option<u64>,
        fence: SpeakerFence,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.meeting_id.clone();
        let cli = self.state.read(cx).cli().clone();
        self.run_reversible_write(
            EditTarget::Line(segment_ids.clone()),
            history::Scope::Line {
                fingerprints: fingerprints.clone(),
                fence: fence.clone(),
            },
            "Line Name",
            "Wait until the line is named.",
            window,
            cx,
            move |lib| {
                Ok(attribute_line(
                    lib,
                    &cli,
                    &id,
                    &fingerprints,
                    choice.as_ref().map(PersonChoice::person_ref),
                    &fence,
                ))
            },
            move |view, result, window, cx| match result {
                Ok(Ok(name)) => {
                    if generation.is_some_and(|g| view.naming_is(g)) {
                        view.dismiss_line_naming(window, cx);
                    }
                    Notice::Info(match name {
                        Some(name) => format!("Named {name} for this line only."),
                        None => "This line uses its speaker's name again.".to_owned(),
                    })
                }
                Ok(Err(e)) => Notice::Error(attribution_refusal(&e.code, &e.user_text())),
                Err(e) => Notice::Error(attribution_refusal(e.code(), &meeting_user_text(&e))),
            },
        );
    }

    fn dismiss_naming(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tab == MeetingTab::Speakers
            && let Some(session) = self.naming.as_ref()
            && matches!(session.target, NamingTarget::Speaker(_))
        {
            let focus = session
                .speaker_opener
                .clone()
                .unwrap_or_else(|| self.speaker_list_focus.clone());
            self.end_naming(cx);
            focus.focus(window, cx);
        } else {
            self.dismiss_line_naming(window, cx);
        }
    }

    /// Closes the picker; later choices from it are ignored.
    fn end_naming(&mut self, cx: &mut Context<Self>) {
        if self.naming.take().is_some() {
            self.naming_generation += 1;
            cx.notify();
        }
    }

    // ---- rendering ----

    fn chip(&self, text: impl Into<SharedString>, cx: &Context<Self>) -> Div {
        crate::ui::kit::badge(crate::ui::kit::BadgeTone::Neutral, text, cx)
    }

    /// The names given during the recording that were not applied: each
    /// with who and where, why, Play (that stretch of the recording), Show
    /// in the transcript, and the user's explicit decision: name the lines
    /// it touches as said (a line correction, `set_utterance_people`), or
    /// leave them as transcribed (`dismiss_live_review`, fenced by the item
    /// as shown, no line changed). Either settles it; nothing is applied
    /// without one.
    fn live_review_panel(&self, data: &MeetingData, cx: &Context<Self>) -> Option<Stateful<Div>> {
        if data.live_review.is_empty() {
            return None;
        }
        let playable = matches!(data.audio, AudioSource::Available { .. });
        let mut panel = div()
            .id("live-review")
            .role(Role::List)
            .aria_label("Line names to check")
            .flex()
            .flex_col()
            .gap_2()
            .child(crate::ui::heading(
                "live-review-title",
                "Line names to check",
            ))
            .child(
                text_line(
                    "live-review-caption",
                    Role::Label,
                    "Check these names from the recording and choose how to label each line.",
                )
                .text_role(TextRole::Caption),
            );
        for (ix, item) in data.live_review.iter().enumerate() {
            let name = item
                .person_id
                .as_ref()
                .and_then(|p| data.people.display_name(p));
            let (text, aria) = live_review_text(item, name);
            let decision = item.decision;
            let key = live_review_key(decision);
            let mut actions = div().flex().flex_wrap().gap_2();
            if playable {
                let playing = self.samples.is_playing(&key);
                actions = actions.child(
                    button(
                        ("live-review-play", ix),
                        if playing { "Stop" } else { "Play" },
                        true,
                        cx,
                        move |this, _, cx| this.toggle_live_review_audio(decision, cx),
                    )
                    .track_focus(&self.live_first_focus[&decision])
                    .accessibility_label(if playing {
                        "Stop this stretch of the recording".to_owned()
                    } else {
                        format!(
                            "Play from {} to {} of the recording",
                            spoken_position(item.from_s),
                            spoken_position(item.to_s)
                        )
                    }),
                );
            }
            if !item.lines.is_empty() {
                actions = actions.child(
                    button(
                        ("live-review-show", ix),
                        "Show lines",
                        true,
                        cx,
                        move |this, window, cx| this.show_live_review(decision, window, cx),
                    )
                    .when(!playable, |button| {
                        button.track_focus(&self.live_first_focus[&decision])
                    }),
                );
            }
            if let (false, Some(name)) = (item.lines.is_empty(), name) {
                actions = actions.child(
                    button(
                        ("live-review-apply", ix),
                        format!("Name as {name}"),
                        true,
                        cx,
                        move |this, window, cx| this.apply_live_review(decision, window, cx),
                    )
                    .accessibility_label(format!(
                        "Attribute the {} it touches to {name}",
                        crate::ui::format::plural(item.lines.len() as u64, "line", "lines")
                    )),
                );
            }
            // Settles the decision without touching any line (also one with
            // no final line to correct).
            actions = actions.child(
                button(
                    ("live-review-keep", ix),
                    "Leave as transcribed",
                    true,
                    cx,
                    move |this, window, cx| this.leave_live_review(decision, window, cx),
                )
                .when(!playable && item.lines.is_empty(), |button| {
                    button.track_focus(&self.live_first_focus[&decision])
                }),
            );
            panel = panel.child(
                div()
                    .id(("live-review-item", ix))
                    .role(Role::ListItem)
                    .track_focus(&self.live_row_focus[&decision].clone().tab_index(-1))
                    .aria_label(text)
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        text_line(("live-review-reason", ix), Role::Note, aria)
                            .text_role(TextRole::Caption),
                    )
                    .child(actions)
                    .children(self.edit_line(
                        &EditTarget::LiveReview(decision),
                        ("live-review-status", ix),
                        cx,
                    )),
            );
        }
        Some(panel)
    }

    #[cfg(test)]
    pub fn credit_line(&self, cx: &App) -> Option<(String, String)> {
        let result = self.data.as_ref()?.meeting.enrichment.as_ref()?;
        let provider = self.state.read(cx).provider_name(&result.provider)?;
        Some(enrichment_credit(
            result,
            provider,
            (
                &local_date_time(&result.created_at),
                &spoken_date_time(&result.created_at),
            ),
        ))
    }

    fn overview_review(&self, data: &MeetingData, cx: &Context<Self>) -> Option<Stateful<Div>> {
        let notes = banner_notes(&data.meeting, &data.live_review);
        let count = speakers_to_name(data);
        let settled = self
            .edit_status
            .keys()
            .filter_map(|target| match target {
                EditTarget::LiveReview(decision)
                    if !data
                        .live_review
                        .iter()
                        .any(|item| &item.decision == decision) =>
                {
                    Some((*decision, target))
                }
                _ => None,
            })
            .collect::<BTreeMap<_, _>>();
        if notes.is_empty() && data.live_review.is_empty() && count == 0 && settled.is_empty() {
            return None;
        }
        Some(
            div()
                .id("overview-review")
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    crate::ui::heading("meeting-needs-review", "Needs review")
                        .track_focus(&self.review_focus.clone().tab_index(-1)),
                )
                .when(count > 0, |section| {
                    section.child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(text_line(
                                "overview-speakers-to-name",
                                Role::Label,
                                if count == 1 {
                                    "1 speaker needs a name".to_owned()
                                } else {
                                    format!("{count} speakers need a name")
                                },
                            ))
                            .child(button(
                                "overview-name-speakers",
                                "Name speakers",
                                true,
                                cx,
                                |this, _, cx| {
                                    this.set_tab(MeetingTab::Speakers, cx);
                                },
                            )),
                    )
                })
                .when(self.new_review_notes > 0, |section| {
                    section.child(kit::status_line(
                        ("overview-new-review-notes", self.review_announcement),
                        StatusKind::Info,
                        crate::ui::format::count(
                            self.new_review_notes as u64,
                            "new review note",
                            "new review notes",
                        ),
                        cx,
                    ))
                })
                .children(notes.into_iter().filter_map(|note| {
                    let index = data
                        .meeting
                        .review_notes
                        .iter()
                        .position(|n| std::ptr::eq(n, note))?;
                    let revision = ops::review_note_revision(note);
                    let target = EditTarget::ReviewNote(revision.clone());
                    Some(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_start()
                                    .gap_2()
                                    .child(kit::glyph(
                                        kit::StatusKind::Attention.glyph_path(),
                                        cx.theme().warning,
                                    ))
                                    .child(
                                        text_line(
                                            ("overview-review-note", index),
                                            Role::Note,
                                            note.text.clone(),
                                        )
                                        .flex_1(),
                                    )
                                    .child(
                                        button(
                                            ("check-review-note", index),
                                            "Mark as checked",
                                            self.busy.is_none(),
                                            cx,
                                            move |this, window, cx| {
                                                this.check_review_note(
                                                    index + 1,
                                                    revision.clone(),
                                                    window,
                                                    cx,
                                                )
                                            },
                                        )
                                        .ghost()
                                        .small(),
                                    ),
                            )
                            .children(self.edit_line(
                                &target,
                                ("check-review-note-status", index),
                                cx,
                            )),
                    )
                }))
                .children(self.live_review_panel(data, cx))
                .children(settled.into_iter().filter_map(|(id, target)| {
                    self.edit_line(target, ("settled-live-review", id), cx)
                })),
        )
    }

    fn check_review_note(
        &mut self,
        position: usize,
        revision: String,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.meeting_id.clone();
        self.run_write_then(
            EditTarget::ReviewNote(revision.clone()),
            "Wait until the review note is saved.",
            window,
            cx,
            move |lib| {
                ops::with_index(lib, |tx, index| {
                    ops::dismiss_review_note(tx, index, &id, position, &revision)
                })
            },
            |this, result, window, cx| match result {
                Ok(()) => {
                    if this.data.as_ref().is_some_and(|data| {
                        data.meeting.review_notes.len() > 1
                            || speakers_to_name(data) > 0
                            || !data.live_review.is_empty()
                    }) {
                        this.review_focus.focus(window, cx);
                    } else {
                        this.detail_focus.focus(window, cx);
                    }
                    Notice::Info("Marked as checked.".into())
                }
                Err(error) => Notice::Error(failure_text(
                    "Couldn't check the note.",
                    &meeting_user_text(&error),
                    "",
                )),
            },
        );
    }

    /// The Actions tab: a table (Status, Action, Owner, Due, At) whose
    /// rows keep Done / Reopen.
    /// The speaker recheck a naming in this meeting started: still checking
    /// (this meeting first), then this meeting's committed changes, then the
    /// other meetings', with links to them. Read from the recheck's job, so
    /// it survives leaving and reopening the meeting.
    fn meeting_tabs(&self, data: &MeetingData, cx: &Context<Self>) -> impl IntoElement {
        use gpui::base::TestSupportExt as _;

        let selected = MEETING_TABS
            .iter()
            .position(|tab| *tab == self.tab)
            .unwrap();
        let count = speakers_to_name(data);
        let view = cx.weak_entity();
        let labels = MEETING_TABS.map(|tab| {
            let show_count = tab == MeetingTab::Speakers && count > 0;
            let label = tab.label();
            let name = if show_count {
                speakers_to_name_label(count).into()
            } else {
                SharedString::from(label)
            };
            let content = div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .id(format!("meeting-tab-label-{label}"))
                        .test_support()
                        .child(label),
                )
                .when(show_count, |el| {
                    el.child(
                        kit::badge(kit::BadgeTone::Outline, count.to_string(), cx)
                            .id("meeting-tab-speaker-count")
                            .test_support(),
                    )
                });
            (content, name)
        });
        let tabs = kit::tabs_with_content(
            "meeting-tabs",
            "Meeting sections",
            labels,
            selected,
            cx.listener(|this, index, _, cx| {
                if let Some(tab) = MEETING_TABS.get(*index) {
                    this.set_tab(*tab, cx);
                }
            }),
            cx,
        );
        div()
            .id("meeting-tab-strip")
            .test_support()
            .flex()
            .items_start()
            .gap_1()
            .mx_4()
            .child(tabs.flex_1().min_w_0())
            .when(self.header_narrow, |bar| {
                bar.child(
                    Button::new("meeting-tabs-more")
                        .accessibility_label("More meeting sections")
                        .icon(gpui::assets::IconName::Ellipsis)
                        .ghost()
                        .xsmall()
                        .tab_stop(false)
                        .dropdown_menu(move |menu, _, _| {
                            MEETING_TABS.into_iter().fold(menu, |menu, tab| {
                                let view = view.clone();
                                menu.item(PopupMenuItem::new(tab.label()).on_click(
                                    move |_, _, cx| {
                                        let _ = view.update(cx, |view, cx| view.set_tab(tab, cx));
                                    },
                                ))
                            })
                        }),
                )
            })
    }

    /// The selected tab's content, in a panel named after the tab (MTG-53).
    fn tab_panel(&self, body: AnyElement) -> Stateful<Div> {
        div()
            .id("meeting-tab-panel")
            .role(Role::TabPanel)
            .aria_label(self.tab.label())
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .child(body)
    }
}

impl MeetingView {
    fn setup_tip(&self, index: usize, cx: &Context<Self>) -> Div {
        use crate::state::{AppEvent, SettingsPage};
        let (id, text, label, page) = if index == 0 {
            (
                "setup-words-tip",
                "Names or terms transcribed wrong? Add them so the speech model listens for them.",
                "Add words…",
                SettingsPage::Transcription,
            )
        } else {
            (
                "setup-calendars-tip",
                "Fill participants from your calendar",
                "Connect calendars…",
                SettingsPage::Calendars,
            )
        };
        kit::banner(id, kit::StatusKind::Pending, Role::Note, text, cx).child(
            super::settings::row()
                .child(
                    Button::new((id, 0usize))
                        .label(label)
                        .outline()
                        .tab_stop(true)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.state.update(cx, |_, cx| {
                                cx.emit(if index == 0 {
                                    AppEvent::OpenWordList
                                } else {
                                    AppEvent::OpenSettings(Some(page))
                                })
                            })
                        })),
                )
                .child(
                    Button::new((id, 1usize))
                        .label("Dismiss")
                        .accessibility_label(if index == 0 {
                            "Dismiss word list tip"
                        } else {
                            "Dismiss calendar tip"
                        })
                        .ghost()
                        .tab_stop(true)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.setup_tips[index] = Some(false);
                            cx.notify();
                        })),
                ),
        )
    }
}

impl Render for MeetingView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        // The landmark says which meeting it is (MTG-53).
        let landmark = match &self.data {
            Some(data) if self.embedded => shown_title(&data.meeting),
            Some(data) => format!("Meeting: {}", shown_title(&data.meeting)),
            None => "Untitled meeting".to_owned(),
        };
        let root = div()
            .id("meeting-view")
            .role(if self.embedded {
                Role::Region
            } else {
                Role::Main
            })
            .track_focus(&self.detail_focus)
            .aria_label(landmark)
            .key_context("MeetingView")
            .on_action(cx.listener(|this, _: &NextMeetingTab, _, cx| this.cycle_tab(true, cx)))
            .on_action(cx.listener(|this, _: &PreviousMeetingTab, _, cx| this.cycle_tab(false, cx)))
            .on_action(
                cx.listener(|this, _: &gpui::component::input::Undo, window, cx| {
                    this.replay_edit(false, window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &gpui::component::input::Redo, window, cx| {
                    this.replay_edit(true, window, cx);
                }),
            )
            .on_action(cx.listener(
                |this, _: &crate::menu::ToggleMeetingPlaybackShortcut, window, cx| {
                    this.toggle_playback(window, cx);
                },
            ))
            .on_action(cx.listener(
                |this, _: &crate::menu::BackFifteenSecondsShortcut, window, cx| {
                    this.seek_relative(-15., window, cx);
                },
            ))
            .on_action(cx.listener(
                |this, _: &crate::menu::ForwardFifteenSecondsShortcut, window, cx| {
                    this.seek_relative(15., window, cx);
                },
            ))
            .capture_action(cx.listener(
                |this, _: &crate::ui::keymap::CancelTransient, window, cx| {
                    this.cancel_transient(window, cx);
                },
            ))
            .capture_action(
                cx.listener(|this, _: &gpui::component::input::Escape, window, cx| {
                    this.cancel_transient(window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &gpui::base::actions::Cancel, window, cx| {
                    this.cancel_transient(window, cx);
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" && !event.keystroke.modifiers.modified() {
                    this.cancel_transient(window, cx);
                }
            }))
            .flex()
            .flex_col()
            .flex_1()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground);
        let Some(data) = self.data.as_ref() else {
            return root.child(self.load_status(cx));
        };
        if self.transcript_rem != window.rem_size() {
            self.transcript_rem = window.rem_size();
            self.transcript_list
                .remeasure_items(0..self.transcript_rows.len());
        }
        let state = self.state.read(cx);
        let data_dir = &state.paths().data_dir;
        let transcript = self.tab == MeetingTab::Transcript;
        let setup_finished = state
            .config()
            .setup
            .completed_steps
            .iter()
            .any(|step| step == super::setup::ASSISTANT_DONE);
        if setup_finished
            && transcript
            && self.setup_tips[0].is_none()
            && !data.lines.is_empty()
            && data.meeting.processing.status == ProcessingStatus::Done
        {
            let asr = &state.config().profile.asr;
            match &self.word_tip_model {
                Some((model, result)) if model == asr => {
                    if *result == Some(true) {
                        self.setup_tips[0] =
                            Some(crate::ui::once::claim(data_dir, "setup-words-tip"));
                    }
                }
                _ => {
                    let asr = asr.clone();
                    self.word_tip_model = Some((asr.clone(), None));
                    let task = state.run_cli(
                        super::settings::args(["models", "list"]),
                        ChildKind::Command,
                        cx,
                    );
                    cx.spawn(async move |this, cx| {
                        let accepts_words = task
                            .await
                            .ok()
                            .map(|reply| super::settings::parse_models_list(&reply))
                            .is_some_and(|list| {
                                list.rows
                                    .iter()
                                    .any(|row| row.id == asr && row.accepts_hotwords == Some(true))
                            });
                        let _ = this.update(cx, |this, cx| {
                            if this
                                .word_tip_model
                                .as_ref()
                                .is_some_and(|(model, _)| model == &asr)
                            {
                                this.word_tip_model = Some((asr, Some(accepts_words)));
                                cx.notify();
                            }
                        });
                    })
                    .detach();
                }
            }
        }
        if setup_finished
            && self.setup_tips[1].is_none()
            && data.participants.is_empty()
            && broscribe_platform_native::NativePlatform::system_settings().host_platform()
                == broscribe_platform::HostPlatform::MacOs
        {
            self.setup_tips[1] = Some(crate::ui::once::claim(data_dir, "setup-calendars-tip"));
        }
        let body = match self.tab {
            MeetingTab::Transcript => self.transcript(data, cx),
            MeetingTab::Speakers => self.speakers(data, window, cx),
            MeetingTab::Overview => self.enrichment(data, window, cx),
            MeetingTab::Templates => self.templates.clone().into_any_element(),
        };
        let root = root
            .when(transcript && self.setup_tips[0] == Some(true), |root| {
                root.child(self.setup_tip(0, cx))
            })
            .when(self.setup_tips[1] == Some(true), |root| {
                root.child(self.setup_tip(1, cx))
            });
        // The header sits on the desk; the sections and their content are
        // one card below it, its edge in line with the title.
        let card = kit::desk_card("meeting-card", cx)
            .flex_1()
            .min_h_0()
            .mx_4()
            .mb_4()
            .overflow_hidden()
            .child(self.meeting_tabs(data, cx))
            .child(
                self.tab_panel(
                    div()
                        .flex()
                        .flex_col()
                        .min_h_0()
                        .flex_1()
                        .children(self.processing_notice(data, cx))
                        .child(body)
                        .into_any_element(),
                ),
            )
            .map(|card| {
                #[cfg(test)]
                let card = gpui::base::TestSupportExt::test_support(card);
                card
            });
        root.child(self.header(data, window, cx)).child(card)
    }
}

#[cfg(test)]
mod tests {
    use broscribe_core::schema::{
        Centroid, Handling, Participant, Person, Recording, Segment, Source, Speaker,
    };
    use chrono::DateTime;
    use gpui::{TestAppContext, WindowHandle};

    use super::*;
    use crate::views::person_picker::fixtures::{self, Synthetic};

    const MODEL: &str = "test-embedder";

    fn sid(n: u32) -> SpeakerId {
        SpeakerId::new(n).unwrap()
    }

    fn seg(id: u32, speaker: u32, start: f64, text: &str) -> Segment {
        Segment {
            speech_overlap: Vec::new(),
            id,
            track: "t0".into(),
            speaker: Some(sid(speaker)),
            start,
            end: start + 2.0,
            text: text.into(),
            words: Vec::new(),
        }
    }

    fn meeting() -> Meeting {
        let started = DateTime::parse_from_rfc3339("2026-11-02T10:00:00Z").unwrap();
        let mut m = Meeting::new(
            MeetingId::parse("m-0000a1").unwrap(),
            Source::Import,
            started,
        );
        m.title = Some("Sync".into());
        m.duration_s = 60.0;
        m.processing.status = ProcessingStatus::Done;
        let mut s1 = Speaker::new(sid(1), "t0");
        s1.talk_s = 20.0;
        s1.centroid = Some(Centroid::new(MODEL, &[1.0, 0.0, 0.0, 0.0], 3));
        m.speakers = vec![s1, Speaker::new(sid(2), "t0")];
        m.segments = vec![
            seg(0, 1, 0.0, "hello"),
            seg(1, 1, 2.5, " there "),
            seg(2, 2, 6.0, "hi"),
            seg(3, 1, 9.0, ""),
            seg(4, 1, 12.0, "bye"),
        ];
        m
    }

    #[test]
    fn transcript_merges_turns_and_finds_the_current_line() {
        let lines = shown_lines(&meeting(), &PeopleLookup::default());
        let shown: Vec<String> = lines.iter().map(|l| l.display(NameStyle::Plain)).collect();
        assert_eq!(
            shown,
            [
                "[00:00:00] Speaker 1: hello there",
                "[00:00:06] Speaker 2: hi",
                "[00:00:12] Speaker 1: bye",
            ]
        );
        assert_eq!(line_at(&lines, -1.0), None);
        assert_eq!(line_at(&lines, 0.0), Some(0));
        assert_eq!(line_at(&lines, 5.9), Some(0));
        assert_eq!(line_at(&lines, 6.0), Some(1));
        assert_eq!(line_at(&lines, 100.0), Some(2));
    }

    #[test]
    fn rows_and_chips_show_names_and_proposals() {
        let ana = Person::new(PersonId::parse("p-a1a1a1").unwrap(), "Ana");
        let people = PeopleLookup::new([ana.clone()]);
        let mut m = meeting();
        m.speakers[0].person_id = Some(ana.id.clone());
        m.speakers[0].assignment = Assignment::Auto;
        m.speakers[0].score = Some(0.8);
        m.participants = vec![
            Participant {
                person_id: Some(ana.id.clone()),
                name: Some("Ana".into()),
                ..Participant::default()
            },
            Participant {
                email: Some("bia@example.com".into()),
                ..Participant::default()
            },
        ];
        assert_eq!(
            participant_chips(&m, &people)
                .into_iter()
                .map(|chip| chip.name)
                .collect::<Vec<_>>(),
            ["Ana", "bia@example.com"]
        );
        let rows = speaker_rows(&m, &people);
        assert_eq!(rows[0].label, "Ana");
        assert_eq!(rows[0].assignment, Assignment::Auto);
        assert_eq!(rows[0].proposal.as_ref().unwrap().person_id, ana.id);
        assert_eq!(rows[1].label, "Speaker 2");
        assert_eq!(rows[1].assignment, Assignment::None);
        assert!(rows[1].proposal.is_none());
    }

    fn library_with(m: &Meeting) -> (tempfile::TempDir, Library) {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path().join("lib")).unwrap();
        lib.write(|tx| {
            tx.create_meeting_dir(m)?;
            tx.save_meeting(m)
        })
        .unwrap();
        (dir, lib)
    }

    #[test]
    fn naming_applies_the_centroid_and_queues_confirmation() {
        let m = meeting();
        let (_dir, lib) = library_with(&m);
        let naming =
            name_speaker_and_queue(&lib, &m.id, sid(1), "Ana", MODEL, &SpeakerFence::of(&m))
                .unwrap();
        assert_eq!(naming.person_name, "Ana");
        assert!(naming.voiceprint_added);

        let saved = lib.load_meeting(&m.id).unwrap();
        let speaker = saved.speaker(sid(1)).unwrap();
        assert_eq!(speaker.assignment, Assignment::Confirmed);
        let person = speaker.person_id.clone().unwrap();

        let jobs = Queue::open(lib.root()).unwrap().list(CONFIRM_KIND).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].key,
            confirm_samples_key(&m.id, sid(1), &person, MODEL)
        );
        assert_eq!(jobs[0].input["speaker_id"], "S1");
        assert_eq!(jobs[0].input["person_id"], person.as_str());
        // Core requested the recheck of other speakers, this meeting first;
        // nobody else was named by the naming itself.
        let recheck = Queue::open(lib.root())
            .unwrap()
            .get(&naming.learning.job_id)
            .unwrap();
        assert_eq!(recheck.kind, broscribe_core::learning::REIDENTIFY_KIND);
        assert_eq!(naming.learning.source_meeting.as_ref(), Some(&m.id));
        assert!(
            saved
                .speakers
                .iter()
                .filter(|s| s.id != sid(1))
                .all(|s| s.assignment != Assignment::Confirmed)
        );
    }

    #[test]
    fn vanished_recording_is_marked_missing() {
        let mut m = meeting();
        m.recording = Some(Recording {
            state: RecordingState::Available,
            handling: Handling::Copy,
            path: "audio.m4a".into(),
            original_path: None,
            sha256: "ab".repeat(32),
            source_sha256: None,
            sources: Vec::new(),
            bytes: 10,
            retention_override: None,
            delete_after: None,
        });
        let (dir, lib) = library_with(&m);
        let data = load_meeting_data(&lib, &m.id, dir.path()).unwrap();
        assert!(matches!(data.audio, AudioSource::Missing { .. }));
        let saved = lib.load_meeting(&m.id).unwrap();
        assert_eq!(saved.recording.unwrap().state, RecordingState::Missing);
    }

    #[test]
    fn available_recording_reports_its_cache() {
        let mut m = meeting();
        let sha = "cd".repeat(32);
        m.recording = Some(Recording {
            state: RecordingState::Available,
            handling: Handling::Copy,
            path: "audio.ogg".into(),
            original_path: None,
            sha256: sha.clone(),
            source_sha256: None,
            sources: Vec::new(),
            bytes: 10,
            retention_override: None,
            delete_after: None,
        });
        let (dir, lib) = library_with(&m);
        let audio_path = lib.meeting_dir(&m.id).unwrap().join("audio.ogg");
        std::fs::write(&audio_path, b"ogg").unwrap();
        let cache_dir = dir.path().join("cache");
        let data = load_meeting_data(&lib, &m.id, &cache_dir).unwrap();
        assert_eq!(
            data.audio,
            AudioSource::Available {
                path: audio_path.clone(),
                sha256: sha.clone(),
                cache: playback_cache_path(&cache_dir, &sha),
                cache_ready: false,
            }
        );
        let job = enqueue_playback_cache(&lib, &m.id, &sha).unwrap();
        assert_eq!(
            job,
            JobId::for_key(PLAYBACK_KIND, &playback_cache_key(&m.id, &sha))
        );
        std::fs::create_dir_all(cache_dir.join("playback")).unwrap();
        std::fs::write(playback_cache_path(&cache_dir, &sha), b"wav").unwrap();
        let data = load_meeting_data(&lib, &m.id, &cache_dir).unwrap();
        assert!(matches!(
            data.audio,
            AudioSource::Available {
                cache_ready: true,
                ..
            }
        ));
    }

    #[test]
    fn restored_reference_requires_matching_content_and_updates_index() {
        let mut m = meeting();
        let source = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(source.path(), b"synthetic recording").unwrap();
        m.recording = Some(Recording {
            state: RecordingState::Missing,
            handling: Handling::Reference,
            path: source.path().into(),
            original_path: None,
            sha256: broscribe_core::schema::sha256_hex(b"synthetic recording"),
            bytes: 19,
            source_sha256: None,
            sources: Vec::new(),
            retention_override: None,
            delete_after: None,
        });
        let (dir, lib) = library_with(&m);
        std::fs::write(source.path(), b"different").unwrap();
        assert!(matches!(
            load_meeting_data(&lib, &m.id, dir.path()).unwrap().audio,
            AudioSource::Missing { .. }
        ));
        std::fs::write(source.path(), b"synthetic recording").unwrap();
        assert!(matches!(
            load_meeting_data(&lib, &m.id, dir.path()).unwrap().audio,
            AudioSource::Available { .. }
        ));
        assert_eq!(
            lib.load_meeting(&m.id).unwrap().recording.unwrap().state,
            RecordingState::Available
        );
        assert_eq!(
            broscribe_core::index::Index::open_readonly(lib.root())
                .unwrap()
                .search_meetings(&Default::default())
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn crashed_out_playback_cache_job_reports_a_failure() {
        use broscribe_platform::fakes::FakeProcessProbe;
        let m = meeting();
        let (_dir, lib) = library_with(&m);
        let sha = "ef".repeat(32);
        let job = enqueue_playback_cache(&lib, &m.id, &sha).unwrap();
        let queue = Queue::open(lib.root()).unwrap();
        // Every worker dies holding the lease until no attempt is left.
        for pid in 1..=broscribe_core::queue::MAX_ATTEMPTS {
            drop(
                queue
                    .claim(&lib, &job, &FakeProcessProbe::new(pid))
                    .unwrap()
                    .unwrap(),
            );
        }
        let mut jobs = crate::jobs::JobsStatus::default();
        assert_eq!(cache_job_failure(&jobs, &job), None);
        let check = crate::jobs::has_pending_jobs(
            lib.root(),
            &FakeProcessProbe::new(900),
            broscribe_core::schema::now(),
        )
        .unwrap();
        for failed in check.crashed {
            jobs.record_failure(failed);
        }
        // The view explains the failure instead of waiting forever.
        assert_eq!(
            cache_job_failure(&jobs, &job).as_deref(),
            Some("The background worker stopped unexpectedly.")
        );
    }

    #[test]
    fn playback_cache_failure_hides_private_job_text() {
        let mut jobs = crate::jobs::JobsStatus::default();
        let job = JobId::for_key("playback-cache", "synthetic");
        for (stored, words) in [
            (
                "decode error: /synthetic/private/recording.m4a",
                "BroScribe couldn't read its library data.",
            ),
            (
                "unexpected output from /synthetic/private/ffmpeg",
                "The preparation job failed.",
            ),
        ] {
            jobs.record_failure(crate::jobs::FailedJob {
                job_id: job.to_string(),
                kind: None,
                meeting_id: None,
                message: Some(stored.into()),
            });
            assert_eq!(cache_job_failure(&jobs, &job).as_deref(), Some(words));
        }
    }

    #[test]
    fn returned_owned_recording_is_restored_after_hash_check() {
        for handling in [Handling::Copy, Handling::Move] {
            let mut m = meeting();
            m.recording = Some(Recording {
                state: RecordingState::Available,
                handling,
                path: "audio.m4a".into(),
                original_path: None,
                sha256: broscribe_core::schema::sha256_hex(b"synthetic recording"),
                bytes: 19,
                source_sha256: None,
                sources: Vec::new(),
                retention_override: None,
                delete_after: None,
            });
            let (dir, lib) = library_with(&m);
            // Opened while the file is absent: marked missing.
            assert!(matches!(
                load_meeting_data(&lib, &m.id, dir.path()).unwrap().audio,
                AudioSource::Missing { .. }
            ));
            let audio = lib.meeting_dir(&m.id).unwrap().join("audio.m4a");
            std::fs::write(&audio, b"different").unwrap();
            assert!(matches!(
                load_meeting_data(&lib, &m.id, dir.path()).unwrap().audio,
                AudioSource::Missing { .. }
            ));
            std::fs::write(&audio, b"synthetic recording").unwrap();
            match load_meeting_data(&lib, &m.id, dir.path()).unwrap().audio {
                AudioSource::Available { path, .. } => assert_eq!(path, audio),
                other => panic!("{handling:?}: expected available, got {other:?}"),
            }
            assert_eq!(
                lib.load_meeting(&m.id).unwrap().recording.unwrap().state,
                RecordingState::Available
            );
        }
    }

    #[test]
    fn stale_proposal_cannot_accept_or_reject_a_different_person() {
        let mut m = meeting();
        let shown = PersonId::random();
        let changed = PersonId::random();
        m.speakers[0].assignment = Assignment::Auto;
        m.speakers[0].person_id = Some(changed.clone());
        let (_dir, lib) = library_with(&m);
        let fence = SpeakerFence::of(&m);
        assert!(accept_proposal_and_queue(&lib, &m.id, sid(1), &shown, MODEL, &fence).is_err());
        let cli = crate::test_support::runner("/unused".into(), lib.root().into());
        assert!(reject_proposal(&lib, &m.id, sid(1), &shown, MODEL, None, &fence, &cli).is_err());
        assert_eq!(
            lib.load_meeting(&m.id).unwrap().speakers[0].person_id,
            Some(changed)
        );
    }

    #[test]
    fn enrichment_substitutes_names_and_action_status_persists() {
        let mut m = meeting();
        let person = Person::new(PersonId::random(), "Synthetic Alex");
        m.speakers[0].assignment = Assignment::Confirmed;
        m.speakers[0].person_id = Some(person.id.clone());
        m.enrichment = Some(
            serde_json::from_value(json!({
                "provider":"synthetic", "created_at":"2026-11-14T00:00:00Z",
                "summary":"{{S1}} led.", "decisions":[{"id":"d1","text":"{{S1}} agreed."}],
                "actions":[{"id":"a1","text":"{{S1}} ships."}]
            }))
            .unwrap(),
        );
        let (dir, lib) = library_with(&m);
        lib.write(|tx| tx.save_person(&person)).unwrap();
        let data = load_meeting_data(&lib, &m.id, dir.path()).unwrap();
        let enrichment = data.meeting.enrichment.as_ref().unwrap();
        assert_eq!(
            substitute_speakers(
                &data.meeting,
                &enrichment.line_people,
                &enrichment.summary,
                &data.people
            ),
            "Synthetic Alex led."
        );
        let rows = action_rows(&data.meeting, &data.people);
        assert!(!rows[0].stale);
        assert!(!rows[0].dropped);
        assert!(!rows[0].done);
        ops::with_index(&lib, |tx, index| {
            ops::set_action_status(tx, index, &m.id, "a1", ActionStatus::Done)
        })
        .unwrap();
        let data = load_meeting_data(&lib, &m.id, dir.path()).unwrap();
        // The Actions tab's table: Status, Action, Owner, Due, At.
        assert_eq!(
            action_rows(&data.meeting, &data.people),
            [ActionRow {
                id: "a1".into(),
                stale: false,
                dropped: false,
                action: "Synthetic Alex ships.".into(),
                owner: String::new(),
                due: None,
                open: false,
                at_s: 0.0,
                done: true,
            }]
        );
        assert_eq!(
            ACTION_COLUMNS.map(|(name, _)| name),
            ["Done", "Action", "Owner", "Due", "Said at"]
        );
        assert_eq!(
            data.meeting.enrichment.unwrap().actions[0].status,
            ActionStatus::Done
        );
    }

    #[test]
    fn credit_line_names_reported_requested_or_default_model() {
        let enrichment = |extra: serde_json::Value| -> Enrichment {
            let mut v =
                json!({"provider": "synthetic-agent", "created_at": "2026-11-14T09:30:00Z"});
            v.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            serde_json::from_value(v).unwrap()
        };
        let when = ("2026-11-14 09:30", "Saturday 14 November at 09:30");
        // The provider reported what the alias resolved to.
        let (text, aria) = enrichment_credit(
            &enrichment(json!({
                "model": "synthetic-large-2", "requested_model": "large", "model_reported": true
            })),
            "Synthetic Agent",
            when,
        );
        assert_eq!(
            text,
            "AI summary · Synthetic Agent · synthetic-large-2 · 2026-11-14 09:30"
        );
        assert!(
            aria.contains("Saturday 14 November at 09:30 using model synthetic-large-2, as reported by the provider")
                && aria.contains("requested: large"),
            "{aria}"
        );
        // Reported for the provider default.
        let (text, aria) = enrichment_credit(
            &enrichment(json!({"model": "synthetic-large-2", "model_reported": true})),
            "Synthetic Agent",
            when,
        );
        assert_eq!(
            text,
            "AI summary · Synthetic Agent · synthetic-large-2 · 2026-11-14 09:30"
        );
        assert!(aria.ends_with("for its default model."), "{aria}");
        // Not reported: the requested model, marked as such (also how a
        // record from before reported models reads).
        let (text, aria) = enrichment_credit(
            &enrichment(json!({"model": "large"})),
            "synthetic-agent",
            when,
        );
        assert_eq!(
            text,
            "AI summary · synthetic-agent · large (requested) · 2026-11-14 09:30"
        );
        assert!(aria.contains("did not report which version ran"), "{aria}");
        // Neither reported nor requested: the provider default.
        let (text, aria) = enrichment_credit(&enrichment(json!({})), "Synthetic Agent", when);
        assert_eq!(
            text,
            "AI summary · Synthetic Agent · provider default model · 2026-11-14 09:30"
        );
        assert!(
            aria.contains("using the provider's default model"),
            "{aria}"
        );
    }

    /// The header shows the start in local time, as the Library groups and
    /// shows it: one instant stored with different offsets reads the same,
    /// while the same wall-clock time at another offset (another instant)
    /// does not. Its accessible label says what each fact is.
    #[test]
    fn header_shows_one_local_start_whatever_offset_it_was_stored_with() {
        let facts = |started_at: &str| {
            let mut m = meeting();
            m.started_at = DateTime::parse_from_rfc3339(started_at).unwrap();
            header::header_facts(&m, None, &AudioSource::NoRecording)
        };
        let utc = facts("2026-11-14T12:00:00Z");
        assert_eq!(facts("2026-11-14T09:00:00-03:00"), utc);
        assert_eq!(facts("2026-11-14T13:00:00+01:00"), utc);
        assert_ne!(facts("2026-11-14T12:00:00-03:00").0, utc.0);

        let (text, label) = utc;
        assert!(
            text.ends_with(" · 1 min · Imported · No recording"),
            "{text}"
        );
        assert!(label.starts_with("Started "), "{label}");
        assert!(label.contains(" at "), "{label}");
        assert!(
            label.ends_with(", 1 minute long, Imported, No recording"),
            "{label}"
        );
        // An unfinished meeting says so, visibly and in the label.
        let mut analyzed = meeting();
        analyzed.processing.status = ProcessingStatus::Analyzed;
        let (text, label) = header::header_facts(&analyzed, None, &AudioSource::NoRecording);
        assert!(text.ends_with(" · Identifying speakers…"), "{text}");
        assert!(label.ends_with(", Identifying speakers…"), "{label}");
    }

    #[test]
    fn review_notice_lists_meeting_notes_with_or_without_enrichment() {
        use broscribe_core::schema::ReviewSource;
        let mut m = meeting();
        assert!(review_notice(&m).is_none());
        // Reprocessing without an enrichment (providers off) still warns.
        assert!(m.enrichment.is_none());
        m.set_review_notes(
            ReviewSource::Reprocess,
            ["old S9: confirmed but matches no single new speaker".to_owned()],
        );
        let texts = |m: &Meeting| -> Vec<String> {
            review_notice(m)
                .unwrap()
                .iter()
                .map(|n| n.text.clone())
                .collect()
        };
        assert_eq!(
            texts(&m),
            ["old S9: confirmed but matches no single new speaker"]
        );
        m.enrichment = Some(
            serde_json::from_value(json!({
                "provider":"synthetic", "created_at":"2026-11-14T00:00:00Z", "summary":"x"
            }))
            .unwrap(),
        );
        m.set_review_notes(ReviewSource::Enrichment, ["Check S2's action.".to_owned()]);
        assert_eq!(
            texts(&m),
            [
                "old S9: confirmed but matches no single new speaker",
                "Check S2's action."
            ]
        );
    }

    #[test]
    fn rejecting_a_shown_proposal_triggers_retention_child() {
        let mut m = meeting();
        let person = Person::new(PersonId::random(), "Synthetic Alex");
        m.speakers[0].assignment = Assignment::Auto;
        m.speakers[0].person_id = Some(person.id.clone());
        let (dir, lib) = library_with(&m);
        lib.write(|tx| tx.save_person(&person)).unwrap();
        let marker = dir.path().join("retention-called");
        let executable = crate::test_support::write_cli(
            &dir.path().join("retention-cli"),
            &format!(
                "[ \"$4 $5\" = 'retention apply' ] || exit 9\ntouch '{}'\nprintf '{{}}'\n",
                marker.display()
            ),
            &format!(
                "if((fx_words 4,5) -cne 'retention apply'){{exit 9}};fx_touch {};fx_print '{{}}'",
                crate::test_support::ps_path(&marker)
            ),
        );
        let cli = crate::test_support::runner(executable, lib.root().into());
        let fence = SpeakerFence::of(&m);
        reject_proposal(&lib, &m.id, sid(1), &person.id, MODEL, None, &fence, &cli).unwrap();
        assert!(marker.exists());
        assert!(
            lib.load_meeting(&m.id).unwrap().speakers[0]
                .rejected_person_ids
                .contains(&person.id)
        );
    }

    #[test]
    fn reject_succeeds_when_retention_afterwards_fails() {
        let mut m = meeting();
        let person = Person::new(PersonId::random(), "Synthetic Alex");
        m.speakers[0].assignment = Assignment::Auto;
        m.speakers[0].person_id = Some(person.id.clone());
        let (dir, lib) = library_with(&m);
        lib.write(|tx| tx.save_person(&person)).unwrap();
        let cli = crate::test_support::runner(dir.path().join("missing-cli"), lib.root().into());
        let fence = SpeakerFence::of(&m);
        reject_proposal(&lib, &m.id, sid(1), &person.id, MODEL, None, &fence, &cli).unwrap();
        assert!(
            lib.load_meeting(&m.id).unwrap().speakers[0]
                .rejected_person_ids
                .contains(&person.id)
        );
    }

    fn open_meeting(
        cx: &mut TestAppContext,
    ) -> (
        WindowHandle<MeetingView>,
        Entity<AppState>,
        Library,
        Synthetic,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (app, lib, s) = fixtures::state(cx, dir.path());
        let id = s.meeting.clone();
        app.update(cx, |st, cx| st.navigate(Route::meeting(id.clone()), cx));
        let window = cx.add_window({
            let app = app.clone();
            move |window, cx| MeetingView::new(app, id, window, cx)
        });
        cx.run_until_parked();
        window
            .update(cx, |view, _, cx| view.set_tab(MeetingTab::Speakers, cx))
            .unwrap();
        (window, app, lib, s, dir)
    }

    /// Opens the picker on speaker `n` and lets it load.
    fn start(
        window: &WindowHandle<MeetingView>,
        cx: &mut TestAppContext,
        n: u32,
    ) -> Entity<PersonPicker> {
        let picker = window
            .update(cx, |view, window, cx| {
                view.start_naming(sid(n), window, cx);
                view.picker().unwrap().clone()
            })
            .unwrap();
        cx.run_until_parked();
        picker
    }

    /// Searches `query` and clicks the candidate `id`.
    fn click(
        window: &WindowHandle<MeetingView>,
        cx: &mut TestAppContext,
        picker: &Entity<PersonPicker>,
        query: &str,
        id: &PersonId,
    ) {
        window
            .update(cx, |_, window, cx| {
                picker.update(cx, |p, cx| {
                    p.set_query(query, window, cx);
                    let ix = p.candidates().position(|c| c.id == *id).unwrap();
                    p.pick(ix, cx);
                })
            })
            .unwrap();
        cx.run_until_parked();
    }

    fn named(lib: &Library, s: &Synthetic, n: u32) -> (Assignment, Option<PersonId>) {
        let m = lib.load_meeting(&s.meeting).unwrap();
        let speaker = m.speaker(sid(n)).unwrap();
        (speaker.assignment, speaker.person_id.clone())
    }

    /// WRT-04: a damaged meeting says so in words. Neither the Alert nor
    /// the log line names its folder (named after its title) or its file.
    #[gpui::test]
    fn a_damaged_meeting_shows_and_logs_no_path(cx: &mut TestAppContext) {
        let (window, _app, lib, s, _dir) = open_meeting(cx);
        let folder = lib.meeting_dir(&s.meeting).unwrap();
        let name = folder.file_name().unwrap().to_str().unwrap().to_owned();
        std::fs::write(folder.join("meeting.json"), b"{ not json").unwrap();
        let (log, _guard) = crate::test_support::LogCapture::start();
        window
            .update(cx, |view, window, cx| view.reload(window, cx))
            .unwrap();
        cx.run_until_parked();
        let shown = window
            .update(cx, |view, _, _| view.load_error.clone())
            .unwrap()
            .expect("the failed read is shown");
        assert_eq!(
            shown,
            "Couldn't open this meeting. BroScribe couldn't read this meeting's data."
        );
        let logged = log.text();
        assert!(
            logged.contains("reading the meeting") && logged.contains("code=\"schema\""),
            "{logged}"
        );
        let root = lib.root().to_str().unwrap().to_owned();
        for private in [name.as_str(), root.as_str(), "meeting.json"] {
            assert!(!shown.contains(private), "{private}: {shown}");
            assert!(!logged.contains(private), "{private}: {logged}");
        }
    }

    #[gpui::test]
    fn picker_names_the_opened_speaker_by_id(cx: &mut TestAppContext) {
        let (window, _app, lib, s, _dir) = open_meeting(cx);
        let before = fixtures::people_count(&lib);
        // Email search tells the two "Ana Silva"s apart.
        let picker = start(&window, cx, 1);
        click(&window, cx, &picker, "ana@example", &s.ana);
        assert_eq!(
            named(&lib, &s, 1),
            (Assignment::Confirmed, Some(s.ana.clone()))
        );
        window
            .update(cx, |view, _, _| assert!(view.picker().is_none()))
            .unwrap();
        // A caption placeholder waiting for review and the unnamed self are
        // selectable; naming confirms the placeholder's identity.
        let picker = start(&window, cx, 3);
        click(&window, cx, &picker, "caption", &s.placeholder);
        assert_eq!(
            named(&lib, &s, 3),
            (Assignment::Confirmed, Some(s.placeholder.clone()))
        );
        assert!(!lib.load_person(&s.placeholder).unwrap().needs_review);
        let picker = start(&window, cx, 2);
        click(&window, cx, &picker, "me", &s.me);
        assert_eq!(
            named(&lib, &s, 2),
            (Assignment::Confirmed, Some(s.me.clone()))
        );
        assert_eq!(fixtures::people_count(&lib), before);
        // Free text only through the explicit action, with the backend's
        // resolve-or-create semantics.
        let picker = start(&window, cx, 2);
        window
            .update(cx, |_, window, cx| {
                picker.update(cx, |p, cx| {
                    p.set_query("Bea Nova", window, cx);
                    p.use_typed(cx);
                })
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(fixtures::people_count(&lib), before + 1);
        let (assignment, person) = named(&lib, &s, 2);
        assert_eq!(assignment, Assignment::Confirmed);
        assert_eq!(lib.load_person(&person.unwrap()).unwrap().name, "Bea Nova");
        assert!(matches!(notice(&window, cx), Notice::Info(text)
            if text.contains("Speaker ") && text.contains(" in “Synthetic sync”: name saved as")));
    }

    #[gpui::test]
    fn closed_or_off_route_pickers_name_nobody(cx: &mut TestAppContext) {
        let (window, app, lib, s, _dir) = open_meeting(cx);
        let tab_left = start(&window, cx, 1);
        window
            .update(cx, |view, _, cx| {
                view.set_tab(MeetingTab::Transcript, cx);
                assert!(view.picker().is_none());
                view.set_tab(MeetingTab::Speakers, cx);
            })
            .unwrap();
        let cancelled = start(&window, cx, 1);
        cancelled.update(cx, |p, cx| p.cancel(cx));
        cx.run_until_parked();
        let route_left = start(&window, cx, 3);
        app.update(cx, |st, cx| st.navigate(Route::library(), cx));
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| assert!(view.picker().is_none()))
            .unwrap();
        for old in [&tab_left, &cancelled, &route_left] {
            old.update(cx, |p, cx| p.pick(0, cx));
        }
        cx.run_until_parked();
        assert_eq!(named(&lib, &s, 1), (Assignment::None, None));
        assert_eq!(named(&lib, &s, 3), (Assignment::None, None));
    }

    #[gpui::test]
    fn removed_person_fails_closed_without_a_name_fallback(cx: &mut TestAppContext) {
        let (window, _app, lib, s, _dir) = open_meeting(cx);
        let picker = start(&window, cx, 1);
        // Deleted (or merged away) after the picker cached it; another
        // registered person still has the same display name.
        lib.write(|tx| tx.delete_person_dir(&s.ana)).unwrap();
        let before = fixtures::people_count(&lib);
        click(&window, cx, &picker, "ana@example", &s.ana);
        assert_eq!(named(&lib, &s, 1), (Assignment::None, None));
        assert_eq!(fixtures::people_count(&lib), before);
        window
            .update(cx, |view, _, _| {
                assert!(matches!(view.last_notice(), Some(Notice::Error(_))));
                assert!(view.picker().is_some());
            })
            .unwrap();
        let err = name_speaker_and_queue(
            &lib,
            &s.meeting,
            sid(1),
            PersonChoice::Existing(s.ana.clone()).person_ref(),
            MODEL,
            &SpeakerFence::of(&lib.load_meeting(&s.meeting).unwrap()),
        )
        .unwrap_err();
        assert!(matches!(err, Error::NotFound(_)));
    }

    #[gpui::test]
    fn cached_picker_cannot_name_a_reanalysed_speaker(cx: &mut TestAppContext) {
        let (window, _app, lib, s, _dir) = open_meeting(cx);
        let picker = start(&window, cx, 1);
        let before = fixtures::people_count(&lib);
        // No notification: the view still shows its cached, old namespace.
        lib.write(|tx| {
            let mut meeting = tx.library().load_meeting(&s.meeting)?;
            meeting.speaker_namespace = "replacement-analysis".into();
            tx.save_meeting(&meeting)
        })
        .unwrap();
        window
            .update(cx, |_, window, cx| {
                picker.update(cx, |p, cx| {
                    p.set_query("Dora New Example", window, cx);
                    p.use_typed(cx);
                });
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(named(&lib, &s, 1), (Assignment::None, None));
        assert_eq!(fixtures::people_count(&lib), before);
        window
            .update(cx, |view, _, _| {
                assert!(matches!(view.last_notice(), Some(Notice::Error(_))));
            })
            .unwrap();
    }

    // ---- per-line attribution ----

    fn overlay_entry(
        segment: &Segment,
        person: &PersonId,
    ) -> broscribe_core::schema::UtterancePerson {
        let f = ops::segment_fingerprint(segment);
        broscribe_core::schema::UtterancePerson {
            track: f.track,
            start: f.start,
            end: f.end,
            text_sha256: f.text_sha256,
            person_id: person.clone(),
            at: DateTime::parse_from_rfc3339("2026-11-15T00:00:00Z").unwrap(),
            live_decision: None,
        }
    }

    #[test]
    fn transcript_splits_where_a_line_correction_starts_and_ends() {
        let mut m = meeting();
        let lines = shown_lines(&m, &PeopleLookup::default());
        let ids: Vec<&[u32]> = lines.iter().map(|l| l.segment_ids.as_slice()).collect();
        assert_eq!(ids, [&[0, 1][..], &[2], &[4]]);
        assert!(lines.iter().all(|l| l.line_person.is_none()));

        let bia = Person::new(PersonId::random(), "Bia Example");
        m.utterance_people = vec![overlay_entry(&m.segments[1], &bia.id)];
        let lines = shown_lines(&m, &PeopleLookup::new([bia.clone()]));
        let shown: Vec<(Vec<u32>, String, bool)> = lines
            .iter()
            .map(|l| {
                (
                    l.segment_ids.clone(),
                    l.display(NameStyle::Plain),
                    l.line_person.is_some(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                (vec![0], "[00:00:00] Speaker 1: hello".to_owned(), false),
                (vec![1], "[00:00:02] Bia Example: there".to_owned(), true),
                (vec![2], "[00:00:06] Speaker 2: hi".to_owned(), false),
                (vec![4], "[00:00:12] Speaker 1: bye".to_owned(), false),
            ]
        );
        assert_eq!(
            line_fingerprints(&m, &[0, 1]).unwrap(),
            [
                ops::segment_fingerprint(&m.segments[0]),
                ops::segment_fingerprint(&m.segments[1])
            ]
        );
        assert_eq!(line_fingerprints(&m, &[0, 99]), None);
    }

    /// A synthetic `broscribe`: argv to `args.log`, stdin to `stdin.log` (one
    /// line per call), then prints `reply.json`. It writes nothing: tests
    /// apply the real command's effect themselves where they need it.
    pub(super) fn synthetic_cli(dir: &Path) -> PathBuf {
        use crate::test_support::{ps_path, write_cli};
        let d = dir.display();
        let stdin_log = ps_path(&dir.join("stdin.log"));
        write_cli(
            &dir.join("synthetic-cli"),
            &format!(
                "printf '%s\\n' \"$*\" >> '{d}/args.log'\n\
                 cat >> '{d}/stdin.log'\nprintf '\\n' >> '{d}/stdin.log'\n\
                 cat '{d}/reply.json'\n"
            ),
            &format!(
                "fx_log_args {};fx_append_stdin {stdin_log};fx_append {stdin_log} ([string][char]10);fx_cat {}",
                ps_path(&dir.join("args.log")),
                ps_path(&dir.join("reply.json")),
            ),
        )
    }

    pub(super) fn reply(dir: &Path, value: serde_json::Value) {
        std::fs::write(dir.join("reply.json"), value.to_string()).unwrap();
    }

    /// `(argv, stdin JSON)` of every CLI call so far.
    fn calls(dir: &Path) -> Vec<(String, serde_json::Value)> {
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap_or_default();
        let args = read("args.log");
        let stdin = read("stdin.log");
        args.lines()
            .zip(stdin.lines())
            .map(|(a, s)| (a.to_owned(), serde_json::from_str(s).unwrap_or_default()))
            .collect()
    }

    /// The argv and stdin `transcript attribute` must get for `segments`:
    /// the person (never in argv) on stdin, `--clear` without one.
    fn attribute_call(
        lib: &Library,
        m: &Meeting,
        person: Option<&str>,
        segments: &[u32],
    ) -> (String, serde_json::Value) {
        let fingerprints: Vec<serde_json::Value> = segments
            .iter()
            .map(|id| {
                let s = m.segments.iter().find(|s| s.id == *id).unwrap();
                json!({
                    "track": s.track, "start": s.start, "end": s.end,
                    "text_sha256": broscribe_core::schema::sha256_hex(s.text.as_bytes()),
                })
            })
            .collect();
        let mut argv = format!(
            "--library {} --json transcript attribute {} --fingerprints-stdin",
            lib.root().display(),
            m.id
        );
        let mut stdin = json!({ "fingerprints": fingerprints, "fence": SpeakerFence::of(m) });
        match person {
            Some(p) => stdin["person"] = p.into(),
            None => argv.push_str(" --clear"),
        }
        (argv, stdin)
    }

    /// Opens the meeting after `edit` changed it on disk (index rebuilt), on
    /// the Transcript tab, with [`synthetic_cli`].
    pub(super) fn open_edited(
        cx: &mut TestAppContext,
        edit: impl FnOnce(&mut Meeting, &Synthetic),
    ) -> (
        WindowHandle<MeetingView>,
        Entity<AppState>,
        Library,
        Synthetic,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (app, lib, s) = fixtures::state_with_cli(cx, dir.path(), synthetic_cli(dir.path()));
        lib.write(|tx| {
            let mut m = tx.library().load_meeting(&s.meeting)?;
            edit(&mut m, &s);
            tx.save_meeting(&m)?;
            broscribe_core::index::Index::rebuild(tx)?;
            Ok(())
        })
        .unwrap();
        let id = s.meeting.clone();
        app.update(cx, |st, cx| st.navigate(Route::meeting(id.clone()), cx));
        let window = cx.add_window({
            let app = app.clone();
            move |window, cx| {
                let mut view = MeetingView::new(app, id, window, cx);
                view.set_tab(MeetingTab::Transcript, cx);
                view
            }
        });
        cx.run_until_parked();
        (window, app, lib, s, dir)
    }

    /// What `transcript attribute` would have written: `person` on
    /// `segments` (`None` clears them), saved without notifying the view.
    fn apply_overlay(lib: &Library, s: &Synthetic, segments: &[u32], person: Option<&PersonId>) {
        lib.write(|tx| {
            let mut m = tx.library().load_meeting(&s.meeting)?;
            let prints: Vec<_> = m
                .segments
                .iter()
                .filter(|seg| segments.contains(&seg.id))
                .map(ops::segment_fingerprint)
                .collect();
            m.utterance_people.retain(|u| {
                !prints.iter().any(|f| {
                    f.track == u.track
                        && f.start == u.start
                        && f.end == u.end
                        && f.text_sha256 == u.text_sha256
                })
            });
            if let Some(person) = person {
                let added: Vec<_> = m
                    .segments
                    .iter()
                    .filter(|seg| segments.contains(&seg.id))
                    .map(|seg| overlay_entry(seg, person))
                    .collect();
                m.utterance_people.extend(added);
            }
            tx.save_meeting(&m)
        })
        .unwrap();
    }

    /// Speaker 1 says segments 1 and 4 back to back (one line).
    fn merged(m: &mut Meeting, _: &Synthetic) {
        m.segments.push(Segment {
            speech_overlap: Vec::new(),
            id: 4,
            track: "t0".into(),
            speaker: Some(sid(1)),
            start: 8.5,
            end: 9.5,
            text: "more from one".into(),
            words: Vec::new(),
        });
    }

    fn draw(window: &WindowHandle<MeetingView>, cx: &mut TestAppContext) {
        cx.update_window((*window).into(), |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
    }

    /// Opens the picker on transcript line `ix` and lets it load.
    fn start_line(
        window: &WindowHandle<MeetingView>,
        cx: &mut TestAppContext,
        ix: usize,
    ) -> Entity<PersonPicker> {
        let picker = window
            .update(cx, |view, window, cx| {
                view.start_line_naming(ix, window, cx);
                view.picker().unwrap().clone()
            })
            .unwrap();
        cx.run_until_parked();
        draw(window, cx);
        picker
    }

    fn this_line(window: &WindowHandle<MeetingView>, cx: &mut TestAppContext) {
        window
            .update(cx, |view, _, cx| {
                view.set_naming_scope(NamingScope::ThisLine, cx);
                assert_eq!(view.naming_scope(), Some(NamingScope::ThisLine));
            })
            .unwrap();
    }

    fn confirm_jobs(lib: &Library) -> usize {
        Queue::open(lib.root())
            .unwrap()
            .list(CONFIRM_KIND)
            .unwrap()
            .len()
    }

    #[gpui::test]
    fn line_naming_scopes_take_their_paths(cx: &mut TestAppContext) {
        let (window, _app, lib, s, dir) = open_edited(cx, merged);
        window
            .update(cx, |view, _, _| {
                let ids: Vec<Vec<u32>> = view
                    .data()
                    .unwrap()
                    .lines
                    .iter()
                    .map(|l| l.segment_ids.clone())
                    .collect();
                assert_eq!(ids, [vec![1, 4], vec![2], vec![3]]);
            })
            .unwrap();
        draw(&window, cx);

        // "Every line of Speaker 1" (the default) names the speaker.
        let picker = start_line(&window, cx, 0);
        window
            .update(cx, |view, _, _| {
                assert_eq!(view.naming_scope(), Some(NamingScope::SpeakerLines))
            })
            .unwrap();
        click(&window, cx, &picker, "ana@example", &s.ana);
        assert_eq!(
            named(&lib, &s, 1),
            (Assignment::Confirmed, Some(s.ana.clone()))
        );
        assert!(
            lib.load_meeting(&s.meeting)
                .unwrap()
                .utterance_people
                .is_empty()
        );
        assert_eq!(confirm_jobs(&lib), 1);

        // "Only this line" runs `transcript attribute` for every merged
        // segment (fingerprints taken when the picker opened, on stdin), then
        // reloads; speakers, samples and the queue are left alone.
        let prints_before = std::fs::read(lib.person_dir(&s.ana).join("voiceprints.json")).ok();
        let before = lib.load_meeting(&s.meeting).unwrap();
        let picker = start_line(&window, cx, 0);
        this_line(&window, cx);
        reply(
            dir.path(),
            json!({"meeting_id": s.meeting, "segments": [1, 4], "person_id": s.placeholder}),
        );
        apply_overlay(&lib, &s, &[1, 4], Some(&s.placeholder));
        click(&window, cx, &picker, "caption", &s.placeholder);
        assert_eq!(
            calls(dir.path()),
            [attribute_call(
                &lib,
                &before,
                Some(s.placeholder.as_str()),
                &[1, 4]
            )]
        );
        assert_eq!(
            named(&lib, &s, 1),
            (Assignment::Confirmed, Some(s.ana.clone()))
        );
        assert_eq!(confirm_jobs(&lib), 1);
        assert_eq!(
            std::fs::read(lib.person_dir(&s.ana).join("voiceprints.json")).ok(),
            prints_before
        );
        window
            .update(cx, |view, _, _| {
                assert!(view.picker().is_none());
                assert!(view.line_error().is_none());
                assert_eq!(
                    view.last_notice(),
                    Some(&Notice::Info(
                        "Named Caption Person for this line only.".into()
                    ))
                );
                let line = &view.data().unwrap().lines[0];
                assert_eq!(line.segment_ids, [1, 4]);
                assert_eq!(line.line_person.as_ref(), Some(&s.placeholder));
                assert_eq!(line.label, "Caption Person");
            })
            .unwrap();
        draw(&window, cx);

        // A suggested speaker's line: only the line goes to the CLI.
        let picker = start_line(&window, cx, 1);
        this_line(&window, cx);
        reply(
            dir.path(),
            json!({"meeting_id": s.meeting, "segments": [2], "person_id": s.ana}),
        );
        click(&window, cx, &picker, "ana@example", &s.ana);
        assert_eq!(
            calls(dir.path())[1],
            attribute_call(&lib, &before, Some(s.ana.as_str()), &[2])
        );
        let m = lib.load_meeting(&s.meeting).unwrap();
        assert_eq!(m.speaker(sid(2)).unwrap().assignment, Assignment::Suggested);

        // A typed name goes on stdin like an ID, never in argv.
        let picker = start_line(&window, cx, 2);
        this_line(&window, cx);
        reply(
            dir.path(),
            json!({"meeting_id": s.meeting, "segments": [3], "person_id": s.jose}),
        );
        window
            .update(cx, |_, window, cx| {
                picker.update(cx, |p, cx| {
                    p.set_query("Dora Typed Example", window, cx);
                    p.use_typed(cx);
                })
            })
            .unwrap();
        cx.run_until_parked();
        let (argv, stdin) = calls(dir.path()).remove(2);
        assert_eq!(
            (argv.clone(), stdin),
            attribute_call(&lib, &before, Some("Dora Typed Example"), &[3])
        );
        assert!(!argv.contains("Dora"), "{argv}");
    }

    #[gpui::test]
    fn corrected_line_shows_its_person_and_clears(cx: &mut TestAppContext) {
        let (window, _app, lib, s, dir) = open_edited(cx, |m, s| {
            let entry = overlay_entry(&m.segments[1], &s.ana);
            m.utterance_people.push(entry);
        });
        draw(&window, cx);
        let before = lib.load_meeting(&s.meeting).unwrap();
        reply(
            dir.path(),
            json!({"meeting_id": s.meeting, "segments": [2], "person_id": null}),
        );
        apply_overlay(&lib, &s, &[2], None);
        window
            .update(cx, |view, window, cx| {
                let lines = &view.data().unwrap().lines;
                assert_eq!(lines[1].line_person.as_ref(), Some(&s.ana));
                assert_eq!(lines[1].label, "Ana Silva");
                assert!(lines[0].line_person.is_none());
                // Lines without a correction have nothing to clear.
                view.clear_line(0, window, cx);
                assert!(view.busy.is_none());
                view.clear_line(1, window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            calls(dir.path()),
            [attribute_call(&lib, &before, None, &[2])]
        );
        window
            .update(cx, |view, _, _| {
                let line = &view.data().unwrap().lines[1];
                assert!(line.line_person.is_none());
                assert_eq!(line.label, "Speaker 2");
                assert!(view.line_error().is_none());
            })
            .unwrap();
        draw(&window, cx);
    }

    #[gpui::test]
    fn line_refusals_show_under_the_line(cx: &mut TestAppContext) {
        let (window, _app, lib, s, dir) = open_edited(cx, merged);
        // Stale: the transcript changed after the picker opened.
        let picker = start_line(&window, cx, 0);
        this_line(&window, cx);
        reply(
            dir.path(),
            json!({"error": {"code": "invalid_input",
                             "message": "invalid input: the transcript changed; reload"}}),
        );
        click(&window, cx, &picker, "ana@example", &s.ana);
        assert_eq!(calls(dir.path()).len(), 1);
        assert!(
            lib.load_meeting(&s.meeting)
                .unwrap()
                .utterance_people
                .is_empty()
        );
        window
            .update(cx, |view, _, _| {
                let (ids, text) = view.line_error().unwrap();
                assert_eq!(ids, [1, 4]);
                assert_eq!(text, "The transcript changed; reload.");
                assert!(view.picker().is_some());
            })
            .unwrap();
        draw(&window, cx);

        // Busy: a summary of the meeting is being generated.
        reply(
            dir.path(),
            json!({"error": {"code": "library_busy",
                             "message": "library busy: an enrichment of it is queued"}}),
        );
        let picker = start_line(&window, cx, 1);
        this_line(&window, cx);
        click(&window, cx, &picker, "ana@example", &s.ana);
        assert_eq!(calls(dir.path()).len(), 2);
        window
            .update(cx, |view, _, _| {
                let (ids, text) = view.line_error().unwrap();
                assert_eq!(ids, [2]);
                assert!(text.contains("busy"), "{text}");
                // The picker stays open to try again.
                assert!(view.picker().is_some());
            })
            .unwrap();
        draw(&window, cx);
    }

    // ---- speaker samples ----

    /// The seeded meeting with a natively playable recording on disk.
    pub(super) fn with_recording(m: &mut Meeting, _: &Synthetic) {
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
    }

    fn player_state(app: &Entity<AppState>, cx: &mut TestAppContext) -> PlayerState {
        app.update(cx, |s, _| s.player_mut().unwrap().poll())
    }

    #[gpui::test]
    fn speaker_sample_plays_its_range_and_hands_off_with_the_bar(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (app, lib, s) = fixtures::state(cx, dir.path());
        lib.write(|tx| {
            let mut m = tx.library().load_meeting(&s.meeting)?;
            with_recording(&mut m, &s);
            tx.save_meeting(&m)
        })
        .unwrap();
        std::fs::write(
            lib.meeting_dir(&s.meeting).unwrap().join("audio.m4a"),
            b"synthetic",
        )
        .unwrap();
        app.update(cx, |st, _| {
            st.set_player(Box::new(broscribe_platform::fakes::FakeAudioPlayer::new(
                60.0,
            )))
        });
        let id = s.meeting.clone();
        app.update(cx, |st, cx| st.navigate(Route::meeting(id.clone()), cx));
        let window = cx.add_window({
            let app = app.clone();
            move |window, cx| MeetingView::new(app, id, window, cx)
        });
        cx.run_until_parked();
        window
            .update(cx, |view, window, cx| {
                view.set_tab(MeetingTab::Speakers, cx);
                // Speaker 1 speaks 5–8 s: that range is the sample.
                assert_eq!(
                    view.speaker_sample(sid(1)).map(|c| (c.start_s, c.end_s)),
                    Some((5.0, 8.0))
                );
                // The bar plays first: the file opens off the UI loop.
                view.play_from(20.0, window, cx);
                assert!(view.playback().preparing && !view.playback().loaded);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| {
                assert!(view.playback().loaded && !view.playback().preparing);
            })
            .unwrap();
        draw(&window, cx);
        assert_eq!(
            player_state(&app, cx),
            PlayerState::Playing { position_s: 20.0 }
        );
        // Starting the sample pauses the bar at its position.
        window
            .update(cx, |view, _, cx| view.toggle_sample(sid(1), cx))
            .unwrap();
        cx.run_until_parked();
        draw(&window, cx);
        window
            .update(cx, |view, _, _| {
                assert!(view.sample_playing(sid(1)));
                assert!(!view.playback().loaded && !view.playback().playing);
                assert_eq!(view.playback().position_s, 20.0);
            })
            .unwrap();
        assert_eq!(
            player_state(&app, cx),
            PlayerState::Playing { position_s: 5.0 }
        );
        // It stops at the end of its range.
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(3_100));
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| assert!(!view.sample_playing(sid(1))))
            .unwrap();
        app.update(cx, |s, _| assert!(!s.player_mut().unwrap().is_playing()));

        // The bar's Play stops a playing sample and takes the player back.
        window
            .update(cx, |view, _, cx| view.toggle_sample(sid(2), cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |view, window, cx| {
                assert!(view.sample_playing(sid(2)));
                view.toggle_playback(window, cx);
                assert!(!view.sample_playing(sid(2)));
                assert!(view.playback().preparing);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| {
                assert!(view.playback().loaded && view.playback().playing);
            })
            .unwrap();
        assert_eq!(
            player_state(&app, cx),
            PlayerState::Playing { position_s: 20.0 }
        );
        // "Stop" stops the sample itself.
        window
            .update(cx, |view, _, cx| view.toggle_sample(sid(3), cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |view, _, cx| {
                assert!(view.sample_playing(sid(3)));
                view.toggle_sample(sid(3), cx);
                assert!(!view.sample_playing(sid(3)));
            })
            .unwrap();
        app.update(cx, |s, _| assert!(!s.player_mut().unwrap().is_playing()));
    }

    #[gpui::test]
    fn a_reload_follows_the_playing_speaker_sample_or_stops_it(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (app, lib, s) = fixtures::state(cx, dir.path());
        let edit = |change: &dyn Fn(&mut Meeting)| {
            lib.write(|tx| {
                let mut m = tx.library().load_meeting(&s.meeting)?;
                change(&mut m);
                tx.save_meeting(&m)
            })
            .unwrap();
        };
        edit(&|m| {
            with_recording(m, &s);
            m.segments.push(seg(10, 1, 20.0, "a second line"));
        });
        std::fs::write(
            lib.meeting_dir(&s.meeting).unwrap().join("audio.m4a"),
            b"synthetic",
        )
        .unwrap();
        app.update(cx, |st, _| {
            st.set_player(Box::new(broscribe_platform::fakes::FakeAudioPlayer::new(
                60.0,
            )))
        });
        let id = s.meeting.clone();
        app.update(cx, |st, cx| st.navigate(Route::meeting(id.clone()), cx));
        let window = cx.add_window({
            let app = app.clone();
            move |window, cx| MeetingView::new(app, id, window, cx)
        });
        cx.run_until_parked();
        let reload = |cx: &mut TestAppContext| {
            window
                .update(cx, |view, window, cx| view.reload(window, cx))
                .unwrap();
            cx.run_until_parked();
        };
        let current = |cx: &mut TestAppContext| {
            window
                .update(cx, |view, _, _| {
                    let clips = speaker_clips(view.data.as_ref(), sid(1));
                    let key = sample_key(sid(1));
                    let ix = view.sample_cursor.index(&key, clips.len());
                    (
                        view.sample_playing(sid(1)),
                        view.sample_cursor.is_current(&key).then(|| {
                            let label = crate::views::speaker_review::sample_label;
                            (label(ix, clips.len()), clips[ix].start)
                        }),
                    )
                })
                .unwrap()
        };
        window
            .update(cx, |view, _, cx| {
                view.set_tab(MeetingTab::Speakers, cx);
                view.toggle_sample(sid(1), cx);
                view.step_sample(sid(1), true, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(current(cx), (true, Some(("Sample 2 of 2".into(), 20.0))));
        // Moved: a longer line ranks first; the cursor follows the clip.
        edit(&|m| {
            let mut long = seg(11, 1, 40.0, "a long line");
            long.end = 50.0;
            m.segments.push(long);
        });
        reload(cx);
        assert_eq!(current(cx), (true, Some(("Sample 3 of 3".into(), 20.0))));
        assert_eq!(
            player_state(&app, cx),
            PlayerState::Playing { position_s: 20.0 }
        );
        // Removed: the playing line is no longer a candidate; it stops.
        edit(&|m| m.segments.retain(|seg| seg.id != 10));
        reload(cx);
        assert_eq!(current(cx), (false, None));
        app.update(cx, |s, _| assert!(!s.player_mut().unwrap().is_playing()));
    }

    #[gpui::test]
    fn a_recording_the_player_cannot_play_faithfully_plays_from_the_cache(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (app, lib, s) = fixtures::state(cx, dir.path());
        lib.write(|tx| {
            let mut m = tx.library().load_meeting(&s.meeting)?;
            with_recording(&mut m, &s);
            tx.save_meeting(&m)
        })
        .unwrap();
        let original = lib.meeting_dir(&s.meeting).unwrap().join("audio.m4a");
        std::fs::write(&original, b"synthetic").unwrap();
        // A multi-track recording the native player would cut short.
        let mut player = broscribe_platform::fakes::FakeAudioPlayer::new(60.0);
        player.needs_cache = vec![original];
        app.update(cx, |st, _| st.set_player(Box::new(player)));
        let id = s.meeting.clone();
        app.update(cx, |st, cx| st.navigate(Route::meeting(id.clone()), cx));
        let window = cx.add_window({
            let app = app.clone();
            move |window, cx| MeetingView::new(app, id, window, cx)
        });
        cx.run_until_parked();
        window
            .update(cx, |view, window, cx| view.play_from(20.0, window, cx))
            .unwrap();
        cx.run_until_parked();
        // Never shown as playing or as a plain error: it waits for the cache.
        let cache = window
            .update(cx, |view, _, _| {
                let playback = view.playback();
                assert!(!playback.loaded && !playback.playing && !playback.preparing);
                assert_eq!(playback.error, None);
                assert_eq!(playback.pending_start, Some(20.0));
                match &view.data().unwrap().audio {
                    AudioSource::Available { cache, .. } => cache.clone(),
                    other => panic!("audio: {other:?}"),
                }
            })
            .unwrap();
        // The cache is made (as the CLI job would); the reload plays it.
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, b"synthetic wav").unwrap();
        window
            .update(cx, |view, window, cx| view.reload(window, cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| {
                assert!(view.playback().loaded && view.playback().playing);
                assert_eq!(view.playback().pending_start, None);
            })
            .unwrap();
        assert_eq!(
            player_state(&app, cx),
            PlayerState::Playing { position_s: 20.0 }
        );
    }

    #[gpui::test]
    fn speaker_sample_is_disabled_without_audio(cx: &mut TestAppContext) {
        let (window, app, _lib, _s, _dir) = open_meeting(cx);
        app.update(cx, |st, _| {
            st.set_player(Box::new(broscribe_platform::fakes::FakeAudioPlayer::new(
                60.0,
            )))
        });
        draw(&window, cx);
        window
            .update(cx, |view, _, cx| {
                assert_eq!(view.speaker_sample(sid(1)), None);
                view.toggle_sample(sid(1), cx);
                assert!(!view.sample_playing(sid(1)));
                assert!(view.samples.active().is_none());
            })
            .unwrap();
        cx.run_until_parked();
        app.update(cx, |s, _| assert!(!s.player_mut().unwrap().is_playing()));
    }

    fn row_dismissed(window: &WindowHandle<MeetingView>, cx: &mut TestAppContext, n: u32) -> bool {
        window
            .update(cx, |view, _, _| {
                let data = view.data().unwrap();
                let row = data.speakers.iter().find(|r| r.id == sid(n)).unwrap();
                assert_eq!(
                    row.review_dismissed,
                    ops::review_dismissed(&data.meeting, sid(n))
                );
                row.review_dismissed
            })
            .unwrap()
    }

    #[gpui::test]
    fn speakers_tab_cant_tell_offers_undo_and_restore(cx: &mut TestAppContext) {
        let (window, _app, lib, s, _dir) = open_meeting(cx);
        let label = window
            .update(cx, |view, _, _| {
                view.data().unwrap().speakers[1].label.clone()
            })
            .unwrap();
        let dismiss = |cx: &mut TestAppContext, n: u32, dismissed: bool| {
            window
                .update(cx, |view, window, cx| {
                    view.set_dismissed(sid(n), dismissed, window, cx)
                })
                .unwrap();
            cx.run_until_parked();
            draw(&window, cx);
        };
        let flag = |n: u32| {
            lib.load_meeting(&s.meeting)
                .unwrap()
                .speaker(sid(n))
                .unwrap()
                .review_dismissed_at
                .is_some()
        };
        // "Can't tell" on the suggested speaker: dismissed, nothing else.
        dismiss(cx, 2, true);
        assert!(flag(2));
        assert!(row_dismissed(&window, cx, 2));
        assert_eq!(named(&lib, &s, 2).0, Assignment::Suggested);
        assert_eq!(
            notice(&window, cx),
            Notice::Info(format!("{label}: dismissed."))
        );
        // Undo restores it.
        window
            .update(cx, |view, window, cx| {
                assert_eq!(view.undo.as_ref().map(|u| u.speaker), Some(sid(2)));
                view.undo_dismiss(window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert!(!flag(2));
        assert!(!row_dismissed(&window, cx, 2));
        assert_eq!(notice(&window, cx), Notice::Info("Undone.".into()));
        // Restore on the dismissed chip's row.
        dismiss(cx, 2, true);
        dismiss(cx, 2, false);
        assert!(!flag(2));
        window
            .update(cx, |view, _, _| assert!(view.undo.is_none()))
            .unwrap();

        // A reanalysed meeting refuses; nothing is written.
        lib.write(|tx| {
            let mut m = tx.library().load_meeting(&s.meeting)?;
            m.speaker_namespace = "replacement-analysis".into();
            tx.save_meeting(&m)
        })
        .unwrap();
        dismiss(cx, 1, true);
        assert!(!flag(1));
        assert_eq!(
            notice(&window, cx),
            Notice::Error(regrouped_refusal("speakers_reanalysed").into())
        );
    }

    fn closest_voices(picker: &Entity<PersonPicker>, cx: &mut TestAppContext) -> Vec<PersonId> {
        picker.read_with(cx, |p, _| {
            p.closest_voices().map(|c| c.id.clone()).collect()
        })
    }

    #[gpui::test]
    fn name_pickers_list_the_speakers_closest_voices(cx: &mut TestAppContext) {
        let (window, app, lib, s, _dir) =
            open_edited(cx, |m, _| fixtures::set_voice(m, 1, &fixtures::voice(1.0)));
        fixtures::enroll(&lib, &s.ana, &fixtures::voice(1.0));
        fixtures::enroll(&lib, &s.jose, &fixtures::voice(0.6));
        // Transcript "Name…" on Speaker 1's line.
        let picker = start_line(&window, cx, 0);
        assert_eq!(closest_voices(&picker, cx), [s.ana.clone(), s.jose.clone()]);
        // A reload of the meeting recomputes from the speaker as read now.
        lib.write(|tx| {
            let mut m = tx.library().load_meeting(&s.meeting)?;
            fixtures::set_voice(&mut m, 1, &fixtures::voice(0.6));
            tx.save_meeting(&m)
        })
        .unwrap();
        app.update(cx, |st, cx| {
            st.library_changed(
                &crate::watch::Changes {
                    index: true,
                    ..Default::default()
                },
                cx,
            )
        });
        cx.run_until_parked();
        assert_eq!(closest_voices(&picker, cx), [s.jose.clone(), s.ana.clone()]);
        // Speakers tab "Name"; a speaker without a voice lists none.
        window
            .update(cx, |view, _, cx| view.set_tab(MeetingTab::Speakers, cx))
            .unwrap();
        let picker = start(&window, cx, 1);
        assert_eq!(closest_voices(&picker, cx), [s.jose.clone(), s.ana.clone()]);
        let picker = start(&window, cx, 2);
        assert!(closest_voices(&picker, cx).is_empty());
    }

    fn changed(app: &Entity<AppState>, cx: &mut TestAppContext, people: bool) {
        app.update(cx, |st, cx| {
            st.library_changed(
                &crate::watch::Changes {
                    index: true,
                    people,
                    ..Default::default()
                },
                cx,
            )
        });
    }

    #[gpui::test]
    fn unrelated_reloads_keep_closest_voices_and_the_highlight(cx: &mut TestAppContext) {
        let (window, app, lib, s, _dir) =
            open_edited(cx, |m, _| fixtures::set_voice(m, 1, &fixtures::voice(1.0)));
        fixtures::enroll(&lib, &s.ana, &fixtures::voice(1.0));
        fixtures::enroll(&lib, &s.jose, &fixtures::voice(0.6));
        // More people than the picker shows at once.
        lib.write(|tx| {
            for i in 0..6 {
                tx.save_person(&Person::new(
                    PersonId::random(),
                    format!("Extra Person {i}"),
                ))?;
            }
            broscribe_core::index::Index::rebuild(tx)
        })
        .unwrap();
        assert!(fixtures::people_count(&lib) > crate::views::person_picker::MAX_RESULTS);
        let picker = start_line(&window, cx, 0);
        assert_eq!(closest_voices(&picker, cx), [s.ana.clone(), s.jose.clone()]);
        let (generation, jose) = picker.update(cx, |p, cx| {
            p.move_highlight(true, cx);
            p.move_highlight(true, cx);
            assert_eq!(p.highlighted(), Some(1));
            assert_eq!(
                p.candidates().count(),
                crate::views::person_picker::MAX_RESULTS
            );
            (p.voice_generation(), p.voice_of(&s.jose).cloned().unwrap())
        });
        // The meeting reloads unchanged: nothing is recomputed or moved.
        changed(&app, cx, false);
        cx.run_until_parked();
        picker.read_with(cx, |p, _| {
            assert_eq!(p.highlighted(), Some(1));
            assert_eq!(p.candidates().nth(1).unwrap().id, s.jose);
            assert_eq!(p.voice_of(&s.jose), Some(&jose));
            assert_eq!(
                p.voice_generation(),
                generation + 1,
                "only the in-flight drop at reload start, no new request"
            );
        });
        assert_eq!(closest_voices(&picker, cx), [s.ana.clone(), s.jose.clone()]);
    }

    #[gpui::test]
    fn a_changed_speaker_drops_voices_in_flight(cx: &mut TestAppContext) {
        let (window, app, lib, s, _dir) = open_edited(cx, |m, s| {
            fixtures::set_voice(m, 1, &fixtures::voice(1.0));
            let speaker = m.speaker_mut(sid(1)).unwrap();
            speaker.assignment = Assignment::Suggested;
            speaker.suggestion = Some(broscribe_core::schema::Suggestion {
                person_id: s.ana.clone(),
                score: 1.0,
            });
        });
        fixtures::enroll(&lib, &s.ana, &fixtures::voice(1.0));
        fixtures::enroll(&lib, &s.jose, &fixtures::voice(0.6));
        // Open the line picker without letting its request finish.
        let picker = window
            .update(cx, |view, window, cx| {
                view.start_line_naming(0, window, cx);
                view.picker().unwrap().clone()
            })
            .unwrap();
        let in_flight = picker.read_with(cx, |p, _| p.voice_generation());
        // Forgetting Ana's voice clears the speaker's suggestion of her.
        ops::with_index(&lib, |tx, index| {
            ops::forget_voice(tx, index, s.ana.as_str())
        })
        .unwrap();
        changed(&app, cx, true);
        picker.update(cx, |p, cx| {
            let old = ops::VoiceMatch {
                person_id: s.ana.clone(),
                score: 1.0,
                likely: false,
                current_suggestion: true,
            };
            assert!(!p.apply_voices(in_flight, Ok(vec![old]), cx));
            assert!(p.voice_of(&s.ana).is_none());
        });
        cx.run_until_parked();
        assert_eq!(closest_voices(&picker, cx), std::slice::from_ref(&s.jose));
        let now = lib.load_meeting(&s.meeting).unwrap();
        assert!(now.speaker(sid(1)).unwrap().suggestion.is_none());
        picker.read_with(cx, |p, _| {
            assert!(p.voice_of(&s.ana).is_none());
            assert_eq!(
                p.voice_target().and_then(|t| t.speaker()),
                now.speaker(sid(1)),
                "recomputed for the speaker as reloaded"
            );
            assert!(!p.voice_of(&s.jose).unwrap().current_suggestion);
        });
    }

    // ---- speaker groups ----

    /// S3 (5 s, one line right after S1's "bye") merged into S1.
    fn grouped(origin: GroupOrigin) -> Meeting {
        let mut m = meeting();
        let mut s3 = Speaker::new(sid(3), "t0");
        s3.talk_s = 5.0;
        m.speakers.push(s3);
        m.segments.push(seg(5, 3, 13.0, "and more"));
        m.speaker_groups
            .push(broscribe_core::schema::SpeakerGroup::new(
                sid(1),
                [sid(3)],
                origin,
            ));
        m
    }

    fn refresh_jobs(lib: &Library) -> Vec<(String, serde_json::Value)> {
        Queue::open(lib.root())
            .unwrap()
            .list(VECTORS_KIND)
            .unwrap()
            .into_iter()
            .map(|j| (j.key, j.input))
            .collect()
    }

    #[test]
    fn vectors_are_refreshed_exactly_when_members_are_separated() {
        let ana = Person::new(PersonId::random(), "Synthetic Ana");
        let bia = Person::new(PersonId::random(), "Synthetic Bia");
        let cli = |dir: &tempfile::TempDir, lib: &Library| {
            crate::test_support::runner(dir.path().join("missing-cli"), lib.root().into())
        };
        // S3, merged into S1, was marked not to be Ana.
        let mut m = grouped(GroupOrigin::User);
        m.speaker_mut(sid(3)).unwrap().rejected_person_ids = vec![ana.id.clone()];

        // Naming the group Bia keeps it whole: no refresh.
        let (_dir, lib) = library_with(&m);
        lib.write(|tx| tx.save_person(&bia)).unwrap();
        let fence = SpeakerFence::of(&m);
        let naming =
            name_speaker_and_queue(&lib, &m.id, sid(3), bia.id.as_str(), MODEL, &fence).unwrap();
        assert_eq!(naming.speaker_id, sid(1), "the member names its group");
        assert!(naming.separated.is_empty());
        assert!(refresh_jobs(&lib).is_empty());
        assert!(ops::is_grouped_member(
            &lib.load_meeting(&m.id).unwrap(),
            sid(3)
        ));

        // Naming it Ana separates S3: one refresh job for the meeting, in
        // the same write; a second separating write reuses it.
        let (_dir, lib) = library_with(&m);
        lib.write(|tx| tx.save_person(&ana)).unwrap();
        let naming =
            name_speaker_and_queue(&lib, &m.id, sid(1), ana.id.as_str(), MODEL, &fence).unwrap();
        assert_eq!(naming.separated.len(), 1, "{naming:?}");
        assert!(!ops::is_grouped_member(
            &lib.load_meeting(&m.id).unwrap(),
            sid(3)
        ));
        let expected = vec![(m.id.to_string(), json!({ "meeting_id": m.id }))];
        assert_eq!(refresh_jobs(&lib), expected);
        lib.write(|tx| {
            queue_vector_refresh(
                tx,
                &m.id,
                &[ops::Separation {
                    speaker_id: sid(3),
                    kept: sid(1),
                    reason: ops::SeparationReason::Rejected,
                    person_id: ana.id.clone(),
                    message: String::new(),
                }],
            )
        })
        .unwrap();
        assert_eq!(refresh_jobs(&lib), expected);

        // A rejection that separates nobody queues nothing.
        let mut auto = grouped(GroupOrigin::User);
        auto.speakers[0].assignment = Assignment::Auto;
        auto.speakers[0].person_id = Some(bia.id.clone());
        let (dir, lib) = library_with(&auto);
        lib.write(|tx| tx.save_person(&bia)).unwrap();
        let queued = reject_proposal(
            &lib,
            &auto.id,
            sid(1),
            &bia.id,
            MODEL,
            None,
            &SpeakerFence::of(&auto),
            &cli(&dir, &lib),
        )
        .unwrap();
        assert!(!queued);
        assert!(refresh_jobs(&lib).is_empty());
    }

    #[test]
    fn merged_speakers_show_as_one_row_and_one_voice() {
        let people = PeopleLookup::default();
        let m = grouped(GroupOrigin::Auto { score: 0.93 });
        let lines = shown_lines(&m, &people);
        let shown: Vec<String> = lines.iter().map(|l| l.display(NameStyle::Plain)).collect();
        assert_eq!(
            shown,
            [
                "[00:00:00] Speaker 1: hello there",
                "[00:00:06] Speaker 2: hi",
                "[00:00:12] Speaker 1: bye and more",
            ]
        );
        assert_eq!(lines[2].speaker, Some(sid(1)));
        assert_eq!(lines[2].segment_ids, [4, 5]);

        let rows = speaker_rows(&m, &people);
        assert_eq!(rows.len(), 2, "the member has no row of its own");
        assert_eq!(rows[0].label, "Speaker 1");
        assert_eq!(
            rows[0].merged.as_ref().unwrap().origin,
            GroupOrigin::Auto { score: 0.93 }
        );
        assert_eq!(rows[0].talk_s, 25.0);
        assert_eq!(rows[0].merged.as_ref().unwrap().group_id, "G1");
        assert_eq!(rows[1].label, "Speaker 2");
        let user = speaker_rows(&grouped(GroupOrigin::User), &people);
        assert_eq!(user[0].merged.as_ref().unwrap().origin, GroupOrigin::User);

        let none = BTreeMap::new();
        assert_eq!(merge_targets(&m, sid(2), MODEL, &none), [sid(1)]);
        assert_eq!(merge_targets(&m, sid(1), MODEL, &none), [sid(2)]);
        // A member moves with its group.
        assert_eq!(merge_targets(&m, sid(3), MODEL, &none), [sid(2)]);

        // Two people proven different are never offered; a member's frozen
        // name is not a participant.
        let ana = Person::new(PersonId::parse("p-a1a1a1").unwrap(), "Ana");
        let bia = Person::new(PersonId::parse("p-b2b2b2").unwrap(), "Bia");
        let cai = Person::new(PersonId::parse("p-c3c3c3").unwrap(), "Cai");
        let people = PeopleLookup::new([ana.clone(), bia.clone(), cai.clone()]);
        let mut m = m;
        for (n, p) in [(1, &ana), (2, &bia)] {
            let s = m.speaker_mut(sid(n)).unwrap();
            s.assignment = Assignment::Confirmed;
            s.person_id = Some(p.id.clone());
        }
        let s3 = m.speaker_mut(sid(3)).unwrap();
        s3.assignment = Assignment::Auto;
        s3.person_id = Some(cai.id.clone());
        assert!(merge_targets(&m, sid(2), MODEL, &none).is_empty());
        assert_eq!(
            participant_chips(&m, &people)
                .into_iter()
                .map(|chip| chip.name)
                .collect::<Vec<_>>(),
            ["Ana", "Bia"]
        );
        assert_eq!(speaker_rows(&m, &people)[0].label, "Ana");
    }

    #[test]
    fn grouping_changes_report_the_saved_result() {
        let merge = GroupingChange::Merge {
            kept: sid(2),
            members: vec![sid(9)],
        };
        let unmerge = GroupingChange::Unmerge { group: "G2".into() };
        let not_same = GroupingChange::NotSame {
            a: sid(2),
            b: sid(9),
        };
        let swapped = json!({ "group": { "kept": "S9", "members": ["S2"] }, "kept_swapped": true });
        assert_eq!(
            grouping_report(&merge, &swapped),
            "Merged Speaker 2 into Speaker 9. Speaker 9 is kept: it is the confirmed speaker, \
             or the one who talks most."
        );
        let removed = json!({ "group": { "kept": "S2", "members": ["S9"] } });
        assert_eq!(
            grouping_report(&unmerge, &removed),
            "Speaker 2, Speaker 9 show separately again; they won't be merged automatically \
             again."
        );
        assert_eq!(
            grouping_report(&not_same, &json!({ "pair": ["S2", "S9"] })),
            "Speaker 2 and Speaker 9 won't be merged automatically or suggested as one speaker \
             again."
        );
        assert_eq!(
            grouping_refusal("speakers_regrouped", "the speakers of m were regrouped"),
            "The speakers were regrouped; review again."
        );
        assert_eq!(
            grouping_refusal(
                "invalid_input",
                &CliError::new(
                    "invalid_input",
                    "invalid input: S2 and S9 are on different tracks."
                )
                .user_text()
            ),
            "Couldn't change the speakers. S2 and S9 are on different tracks."
        );
    }

    pub(super) fn speakers_tab(window: &WindowHandle<MeetingView>, cx: &mut TestAppContext) {
        window
            .update(cx, |view, window, cx| {
                view.set_tab(MeetingTab::Speakers, cx);
                view.reload(window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        draw(window, cx);
    }

    fn change(window: &WindowHandle<MeetingView>, cx: &mut TestAppContext, c: GroupingChange) {
        window
            .update(cx, |view, window, cx| view.change_grouping(c, window, cx))
            .unwrap();
        cx.run_until_parked();
    }

    fn notice(window: &WindowHandle<MeetingView>, cx: &mut TestAppContext) -> Notice {
        window
            .update(cx, |view, _, _| view.last_notice().cloned().unwrap())
            .unwrap()
    }

    #[gpui::test]
    fn speakers_tab_can_undo_and_redo_a_group(cx: &mut TestAppContext) {
        let (window, app, lib, s, _dir) = open_edited(cx, |m, _| {
            fixtures::set_voice(m, 1, &fixtures::voice(1.0));
            fixtures::set_voice(m, 3, &fixtures::voice(0.86));
        });
        app.update(cx, |st, cx| {
            let mut config = st.config().clone();
            config.identity.insert(
                fixtures::voice_model(),
                Thresholds {
                    merge: Some(0.5),
                    auto_merge: Some(0.95),
                    ..Default::default()
                },
            );
            st.set_config(config, cx);
        });
        speakers_tab(&window, cx);
        window
            .update(cx, |view, _, _| {
                let grouping = &view.data().unwrap().grouping;
                assert_eq!(grouping.duplicates.len(), 1);
                let d = &grouping.duplicates[0];
                assert_eq!((d.a, d.b), (sid(1), sid(3)));
                assert_eq!(
                    d.text,
                    "Speaker 3 may be the same person as Speaker 1 · voice 0.86"
                );
                assert_eq!(grouping.merge_targets[&sid(2)], [sid(1), sid(3)]);
            })
            .unwrap();
        let original = lib.load_meeting(&s.meeting).unwrap();
        let fence = SpeakerFence::of(&original);
        change(
            &window,
            cx,
            GroupingChange::Merge {
                kept: sid(1),
                members: vec![sid(3)],
            },
        );
        assert_eq!(
            notice(&window, cx),
            Notice::Info("Merged Speaker 3 into Speaker 1.".into())
        );
        let grouped = lib.load_meeting(&s.meeting).unwrap();
        assert_ne!(SpeakerFence::of(&grouped), fence);
        assert_eq!(grouped.speaker_groups[0].members, [sid(3)]);

        for (redo, expected) in [(false, &original), (true, &grouped)] {
            window
                .update(cx, |view, window, cx| view.replay_edit(redo, window, cx))
                .unwrap();
            cx.run_until_parked();
            let current = lib.load_meeting(&s.meeting).unwrap();
            assert_eq!(current.speaker_groups, expected.speaker_groups);
            assert_eq!(current.not_same, expected.not_same);
            assert_eq!(current.segments, original.segments);
        }
    }

    #[gpui::test]
    fn a_stale_line_naming_after_regrouping_changes_nothing(cx: &mut TestAppContext) {
        let (window, _app, lib, s, dir) = open_edited(cx, |m, _| {
            m.speaker_groups
                .push(broscribe_core::schema::SpeakerGroup::new(
                    sid(1),
                    [sid(3)],
                    GroupOrigin::User,
                ));
        });
        window
            .update(cx, |view, _, _| {
                let lines = &view.data().unwrap().lines;
                assert_eq!(lines[2].label, "Speaker 1", "S3's line shows its group");
                assert_eq!(lines[2].speaker, Some(sid(1)));
            })
            .unwrap();
        let people_before = lib.list_people().unwrap();
        let picker = start_line(&window, cx, 2);
        // Unmerged elsewhere; the view has not reloaded.
        lib.write(|tx| {
            let mut m = tx.library().load_meeting(&s.meeting)?;
            m.speaker_groups.clear();
            tx.save_meeting(&m)
        })
        .unwrap();
        click(&window, cx, &picker, "ana@example", &s.ana);
        assert_eq!(named(&lib, &s, 1), (Assignment::None, None));
        assert_eq!(named(&lib, &s, 3), (Assignment::None, None));
        assert_eq!(
            lib.list_people().unwrap(),
            people_before,
            "no person changed"
        );
        assert!(
            lib.load_voiceprints(&s.ana).unwrap().embeddings.is_empty(),
            "no sample learned"
        );
        assert_eq!(confirm_jobs(&lib), 0);
        assert!(calls(dir.path()).is_empty());
        window
            .update(cx, |view, _, _| {
                assert_eq!(
                    view.line_error(),
                    Some((
                        vec![3],
                        "The speakers were regrouped; review again.".to_owned()
                    ))
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn a_stale_this_line_choice_carries_the_fence_it_was_read_with(cx: &mut TestAppContext) {
        let (window, _app, lib, s, dir) = open_edited(cx, |m, _| {
            m.speaker_groups
                .push(broscribe_core::schema::SpeakerGroup::new(
                    sid(1),
                    [sid(3)],
                    GroupOrigin::User,
                ));
        });
        let seen = lib.load_meeting(&s.meeting).unwrap();
        let picker = start_line(&window, cx, 2);
        this_line(&window, cx);
        // Unmerged elsewhere; the view has not reloaded.
        lib.write(|tx| {
            let mut m = tx.library().load_meeting(&s.meeting)?;
            m.speaker_groups.clear();
            tx.save_meeting(&m)
        })
        .unwrap();
        reply(
            dir.path(),
            json!({ "error": { "code": "speakers_regrouped", "message": "the speakers were regrouped; review again" } }),
        );
        click(&window, cx, &picker, "ana@example", &s.ana);
        // The CLI got the view's (old) speakers; it refuses them under the
        // lock (tested in the CLI), so nothing is written.
        let (_, stdin) = calls(dir.path()).pop().unwrap();
        assert_eq!(
            stdin["fence"],
            serde_json::to_value(SpeakerFence::of(&seen)).unwrap()
        );
        assert_ne!(
            stdin["fence"],
            serde_json::to_value(SpeakerFence::of(&lib.load_meeting(&s.meeting).unwrap())).unwrap()
        );
        assert!(
            lib.load_meeting(&s.meeting)
                .unwrap()
                .utterance_people
                .is_empty()
        );
        window
            .update(cx, |view, _, _| {
                assert_eq!(
                    view.line_error(),
                    Some((
                        vec![3],
                        "The speakers were regrouped; review again.".to_owned()
                    ))
                );
                // The reload after the refusal shows the new grouping, which
                // cancels the picker opened on the old one.
                assert!(view.picker().is_none());
            })
            .unwrap();
    }

    #[gpui::test]
    fn regrouping_cancels_an_open_picker(cx: &mut TestAppContext) {
        let (window, _app, lib, s, _dir) = open_meeting(cx);
        let picker = start(&window, cx, 1);
        lib.write(|tx| {
            let mut m = tx.library().load_meeting(&s.meeting)?;
            m.speaker_groups
                .push(broscribe_core::schema::SpeakerGroup::new(
                    sid(1),
                    [sid(3)],
                    GroupOrigin::User,
                ));
            tx.save_meeting(&m)
        })
        .unwrap();
        window
            .update(cx, |view, window, cx| view.reload(window, cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| assert!(view.picker().is_none()))
            .unwrap();
        picker.update(cx, |p, cx| p.pick(0, cx));
        cx.run_until_parked();
        assert_eq!(named(&lib, &s, 1), (Assignment::None, None));
    }

    /// A synthetic `broscribe` listing one usable provider and no models.
    pub(super) fn listing_cli(dir: &Path) -> PathBuf {
        use crate::test_support::{ps_str, write_cli};
        let providers = r#"{"providers":[{"id":"agent-a","name":"Synthetic Agent","purpose":"llm","runtime":"agent-cli","enabled":true,"consented":true}]}"#;
        let models = r#"{"provider":"agent-a","source":"none","models":[]}"#;
        write_cli(
            &dir.join("synthetic-cli"),
            &format!(
                "case \"$4 $5\" in\n\
                 'providers list') printf '{providers}' ;;\n\
                 'providers models') printf '{models}' ;;\n\
                 *) printf '{{}}' ;;\n\
                 esac\n"
            ),
            &format!(
                "$w=fx_words 4,5;\
                 if($w -ceq 'providers list'){{fx_print {}}}\
                 elseif($w -ceq 'providers models'){{fx_print {}}}\
                 else{{fx_print '{{}}'}}",
                ps_str(providers),
                ps_str(models),
            ),
        )
    }

    /// The seeded meeting with ~60 decisions and a ~200-line `meeting-notes`
    /// result, open in a 900×500 window.
    fn long_content_window(
        cx: &mut TestAppContext,
        dir: &Path,
    ) -> (Entity<MeetingView>, gpui::AnyWindowHandle) {
        let (app, lib, s) = fixtures::state_with_cli(cx, dir, listing_cli(dir));
        let mut m = lib.load_meeting(&s.meeting).unwrap();
        let decisions: Vec<serde_json::Value> = (0..60)
            .map(|i| json!({"id": format!("d{i}"), "text": format!("Synthetic decision {i}"), "at_s": i as f64}))
            .collect();
        m.enrichment = Some(
            serde_json::from_value(json!({
                "provider": "agent-a", "created_at": "2026-11-15T00:00:00Z",
                "summary": "Synthetic summary.", "decisions": decisions
            }))
            .unwrap(),
        );
        let markdown: String = (0..200)
            .map(|i| format!("- Synthetic note line {i}\n"))
            .collect();
        m.template_results = vec![
            serde_json::from_value(json!({
                "template_id": "meeting-notes", "template_name": "Meeting notes",
                "instructions_sha256": "00", "provider": "agent-a",
                "created_at": "2026-11-15T00:00:00Z", "markdown": markdown
            }))
            .unwrap(),
        ];
        lib.write(|tx| tx.save_meeting(&m)).unwrap();
        app.update(cx, |st, cx| st.navigate(Route::meeting(s.meeting), cx));
        let (main, window) = meeting_test_shell(cx, app, gpui::size(px(900.), px(500.)));
        let view = main.read_with(cx, |m, _| m.meeting_view().unwrap().clone());
        (view, window)
    }

    pub(super) fn meeting_test_shell(
        cx: &mut TestAppContext,
        app: Entity<AppState>,
        size: gpui::Size<gpui::Pixels>,
    ) -> (Entity<crate::ui::MainWindow>, gpui::AnyWindowHandle) {
        let mut config = app.read_with(cx, |st, _| st.config().clone());
        config
            .setup
            .completed_steps
            .push(crate::views::setup::ASSISTANT_DONE.into());
        app.update(cx, |st, cx| st.set_config(config, cx));
        let deps = crate::views::setup::SetupDeps {
            mic: std::sync::Arc::new(broscribe_platform::fakes::FakeMicCapture::new(16_000)),
            system: std::sync::Arc::new(broscribe_platform::fakes::FakeSystemInfo::default()),
        };
        let main = std::rc::Rc::new(std::cell::RefCell::new(None));
        let window = cx.open_window(size, {
            let app = app.clone();
            let main = main.clone();
            move |window, cx| {
                let view = cx.new(|cx| crate::ui::MainWindow::new(app, deps, window, cx));
                *main.borrow_mut() = Some(view.clone());
                gpui::component::Root::new(view, window, cx)
            }
        });
        cx.run_until_parked();
        let main: Entity<crate::ui::MainWindow> = main.borrow().clone().unwrap();
        (main, window.into())
    }

    fn draw_settled(window: gpui::AnyWindowHandle, cx: &mut TestAppContext) {
        cx.update_window(window, |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        cx.run_until_parked();
    }

    #[gpui::test]
    fn a_library_match_selects_and_scrolls_to_its_segment(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (app, lib, fixture) = fixtures::state_with_cli(cx, dir.path(), listing_cli(dir.path()));
        let mut meeting = lib.load_meeting(&fixture.meeting).unwrap();
        meeting.segments = (0..60)
            .map(|id| Segment {
                speech_overlap: Vec::new(),
                id,
                track: "mic".into(),
                speaker: Some(sid(1 + id % 2)),
                start: f64::from(id) * 10.,
                end: f64::from(id) * 10. + 1.,
                text: format!("Synthetic orbit line {id}"),
                words: vec![],
            })
            .collect();
        meeting.duration_s = 600.;
        lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
        cx.update(crate::ui::theme::init);
        let window = cx.add_window(|window, cx| MeetingView::new(app, fixture.meeting, window, cx));
        window
            .update(cx, |view, window, cx| {
                view.open_library_match(52, "Synthetic **orbit** line", true, window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        draw_settled(window.into(), cx);
        draw_settled(window.into(), cx);
        window
            .update(cx, |view, _, _| {
                assert_eq!(view.tab(), MeetingTab::Transcript);
                let line = view.transcript_cursor.unwrap();
                assert_eq!(line, 52);
                assert!(
                    view.data.as_ref().unwrap().lines[line]
                        .segment_ids
                        .contains(&52)
                );
                assert_eq!(view.library_match.as_ref().unwrap().1, ["orbit"]);
                assert!(view.transcript_list.logical_scroll_top().item_ix > 0);
                let row = view.transcript_list.bounds_for_item(line).unwrap();
                let viewport = view.transcript_list.viewport_bounds();
                assert!(
                    row.bottom() > viewport.top() && row.top() < viewport.bottom(),
                    "matched row is outside the viewport: {row:?} {viewport:?}"
                );
                assert!(!view.playback.playing);
            })
            .unwrap();
    }

    /// The container `handle` tracks is bounded by the 500 px window, its
    /// content is taller, and a wheel scroll over it moves it.
    fn assert_scrolls(
        window: gpui::AnyWindowHandle,
        handle: &ScrollHandle,
        cx: &mut TestAppContext,
    ) {
        draw_settled(window, cx);
        let bounds = handle.bounds();
        assert!(
            bounds.size.height > px(0.) && bounds.bottom() <= px(500.),
            "the scroll area is not bounded by the window: {bounds:?}"
        );
        assert!(
            handle.max_offset().y > px(0.),
            "the content fits: nothing to scroll ({:?})",
            handle.max_offset()
        );
        assert_eq!(handle.offset().y, px(0.));
        let mut vcx = gpui::VisualTestContext::from_window(window, cx);
        vcx.simulate_event(gpui::ScrollEvent {
            position: bounds.center(),
            delta: gpui::ScrollDelta::Pixels(gpui::point(px(0.), px(-240.))),
            ..Default::default()
        });
        draw_settled(window, cx);
        assert!(
            handle.offset().y < px(0.),
            "the wheel did not scroll: {:?}",
            handle.offset()
        );
    }

    #[gpui::test]
    fn long_decisions_scroll_and_the_panel_and_export_still_work(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, window) = long_content_window(cx, dir.path());
        let handle = view.update(cx, |view, cx| {
            view.set_tab(MeetingTab::Overview, cx);
            view.enrichment_scroll().clone()
        });
        cx.run_until_parked();
        assert_scrolls(window, &handle, cx);

        // The Generate panel at the top of the scroll area still lists and
        // takes a provider; the Export… form opens under the header.
        let panel = cx
            .update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.present_export(window, cx);
                    view.generate_panel().clone()
                })
            })
            .unwrap();
        panel.update(cx, |p, cx| p.choose_provider("agent-a".into(), cx));
        cx.run_until_parked();
        panel.read_with(cx, |p, _| {
            assert!(p.providers().is_some_and(|r| r.is_ok()));
            assert!(p.can_generate());
        });
        draw_settled(window, cx);
        view.read_with(cx, |view, cx| {
            let form = view.export_form().unwrap().read(cx);
            assert!(form.chosen().len() >= 3, "{:?}", form.chosen());
        });
        // With the form open in this short window the tab keeps a usable,
        // window-bounded scroll area; the form gives way and scrolls.
        let bounds = handle.bounds();
        assert!(
            bounds.bottom() <= px(500.) && bounds.size.height >= px(120.),
            "{bounds:?}"
        );
        assert!(handle.max_offset().y > px(0.));
    }

    #[gpui::test]
    fn a_long_template_result_scrolls(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (view, window) = long_content_window(cx, dir.path());
        let tab = view.update(cx, |view, cx| {
            view.set_tab(MeetingTab::Templates, cx);
            view.templates_tab().clone()
        });
        cx.run_until_parked();
        draw_settled(window, cx);
        let handle = tab.read_with(cx, |t, _| t.scroll_handle().clone());
        assert_scrolls(window, &handle, cx);
        // The template's panel inside the scroll area still works.
        let panel = tab.read_with(cx, |t, _| t.panel("meeting-notes").unwrap().clone());
        panel.update(cx, |p, cx| p.ensure_loaded(cx));
        cx.run_until_parked();
        panel.read_with(cx, |p, _| assert!(p.can_generate()));
    }

    fn log(dir: &Path, name: &str) -> String {
        std::fs::read_to_string(dir.join(name)).unwrap_or_default()
    }

    fn type_title(window: &WindowHandle<MeetingView>, cx: &mut TestAppContext, text: &'static str) {
        window
            .update(cx, |view, window, cx| {
                if view.rename.is_none() {
                    view.start_rename(window, cx);
                }
                let input = view.rename.as_ref().unwrap().0.clone();
                input.update(cx, |i, cx| i.set_value(text, window, cx));
            })
            .unwrap();
    }

    #[gpui::test]
    fn rename_sends_the_title_on_stdin_and_keeps_the_draft_when_refused(cx: &mut TestAppContext) {
        let (window, _app, lib, s, dir) = open_edited(cx, |_, _| {});
        reply(
            dir.path(),
            json!({"error": {"code": "library_busy", "message": "library busy: synthetic"}}),
        );
        type_title(&window, cx, "Synthetic private title");
        window
            .update(cx, |view, window, cx| view.submit_rename(window, cx))
            .unwrap();
        cx.run_until_parked();
        let argv = log(dir.path(), "args.log");
        assert_eq!(
            argv.trim(),
            format!(
                "--library {} --json title {} --stdin",
                lib.root().display(),
                s.meeting
            )
        );
        assert!(!argv.contains("private"), "the title never reaches argv");
        assert_eq!(
            log(dir.path(), "stdin.log").trim(),
            "Synthetic private title"
        );
        window
            .update(cx, |view, _, _| {
                assert!(view.rename.is_some(), "a refusal keeps the draft");
                assert!(matches!(view.last_notice(), Some(Notice::Error(t))
                        if t.starts_with("Couldn't rename the meeting. ")
                            && t.ends_with("Your title is still in the field.")));
            })
            .unwrap();

        reply(
            dir.path(),
            json!({"meeting_id": s.meeting, "title": "Synthetic private title", "session_id": "s-synthetic"}),
        );
        window
            .update(cx, |view, window, cx| view.submit_rename(window, cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| {
                assert!(view.rename.is_none());
                assert!(
                    matches!(view.last_notice(), Some(Notice::Info(t)) if t.contains("The recording shows it too"))
                );
            })
            .unwrap();
        // An unchanged or empty title is not sent.
        type_title(&window, cx, "   ");
        window
            .update(cx, |view, window, cx| view.submit_rename(window, cx))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(log(dir.path(), "args.log").lines().count(), 2);
    }

    #[gpui::test]
    fn delete_meeting_shows_the_plan_and_only_confirm_deletes(cx: &mut TestAppContext) {
        use crate::views::delete_meeting::DeletePhase;
        let (window, app, lib, s, dir) = open_edited(cx, |_, _| {});
        let plan = json!({
            "meeting_id": s.meeting, "dry_run": true, "owned_audio": [],
            "referenced_originals_kept": ["/synthetic/original.wav"], "moved_original": false,
            "voice_samples": 2, "people_affected": 1, "playback_caches": 0,
            "jobs_cancelled": 0, "state": "planned", "cleanup_failures": []
        });
        reply(dir.path(), plan.clone());
        window
            .update(cx, |view, _, cx| view.open_delete(cx))
            .unwrap();
        cx.run_until_parked();
        let prompt = window
            .update(cx, |view, _, _| view.delete_prompt().cloned())
            .unwrap()
            .unwrap();
        let phase = prompt.read_with(cx, |p, _| p.phase().clone());
        let DeletePhase::Planned(consequences) = phase else {
            panic!("a plan: {phase:?}");
        };
        assert_eq!(consequences.warning, None);
        assert!(
            consequences
                .goes
                .join(" ")
                .contains("Removes 2 voice samples")
        );
        assert!(
            consequences
                .stays
                .join(" ")
                .contains("The people and their other voice samples stay")
        );
        assert!(
            consequences
                .stays
                .join(" ")
                .contains("Keeps 1 original file outside the library")
        );
        // Cancel: nothing but the dry run ran.
        prompt.update(cx, |p, cx| p.cancel(cx));
        cx.run_until_parked();
        assert!(
            window
                .update(cx, |view, _, _| view.delete_prompt().is_none())
                .unwrap()
        );
        let root = lib.root().display().to_string();
        assert_eq!(
            log(dir.path(), "args.log").trim(),
            format!("--library {root} --json delete {} --dry-run", s.meeting)
        );
        // Confirmed: `--yes`, then the library is shown.
        window
            .update(cx, |view, _, cx| view.open_delete(cx))
            .unwrap();
        cx.run_until_parked();
        let prompt = window
            .update(cx, |view, _, _| view.delete_prompt().cloned())
            .unwrap()
            .unwrap();
        let mut deleted = plan;
        deleted["dry_run"] = json!(false);
        deleted["state"] = json!("deleted");
        reply(dir.path(), deleted);
        prompt.update(cx, |p, cx| p.confirm(cx));
        cx.run_until_parked();
        assert_eq!(
            log(dir.path(), "args.log").lines().last().unwrap(),
            format!("--library {root} --json delete {} --yes", s.meeting)
        );
        assert_eq!(
            app.read_with(cx, |st, _| st.route().clone()),
            Route::library()
        );
    }

    #[gpui::test]
    fn live_names_not_applied_are_reviewed_with_audio_and_an_explicit_correction(
        cx: &mut TestAppContext,
    ) {
        use broscribe_core::live_identity::{LiveAction, LiveAttribution, LiveRange};
        let (window, _app, lib, s, dir) = open_edited(cx, |m, s| {
            let seg = m.segments.iter().find(|seg| seg.id == 1).unwrap().clone();
            let mid = (seg.start + seg.end) / 2.0;
            m.live_attributions.push(LiveAttribution {
                turn_id: "synthetic-turn".into(),
                decision: 7,
                action: LiveAction::Name,
                person_id: Some(s.ana.clone()),
                range: LiveRange {
                    track: seg.track.clone(),
                    from_sample: (seg.start * 16_000.0) as u64,
                    to_sample: (mid * 16_000.0) as u64,
                    from: None,
                    to: None,
                },
                from_s: Some(seg.start),
                to_s: Some(mid),
                supported: vec![[seg.start, mid]],
                outcome: LiveOutcome::Partial,
                at: broscribe_core::schema::now(),
                user_override: false,
                forget_fence: None,
            });
            m.set_review_notes(
                ReviewSource::LivePreview,
                ["Live preview: synthetic note".to_owned()],
            );
        });
        let m = lib.load_meeting(&s.meeting).unwrap();
        window
            .update(cx, |view, _, _| {
                let data = view.data().unwrap();
                let [item] = data.live_review.as_slice() else {
                    panic!("one decision to review: {:?}", data.live_review);
                };
                let seg = m.segments.iter().find(|seg| seg.id == 1).unwrap();
                assert_eq!(item.lines, [ops::segment_fingerprint(seg)]);
                let name = data.people.display_name(&s.ana);
                let (text, aria) = live_review_text(item, name);
                assert!(aria.contains("covers only part"), "{aria}");
                assert!(text.contains(name.unwrap()), "{text}");
                // The structured list replaces the decision's plain note.
                assert!(banner_notes(&data.meeting, &data.live_review).is_empty());
            })
            .unwrap();
        cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
            .unwrap();
        window
            .update(cx, |view, _, cx| view.set_tab(MeetingTab::Overview, cx))
            .unwrap();
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        assert_eq!(
            tree.read(tree.find("live-review")),
            ("List", "Line names to check")
        );
        assert_eq!(
            tree.read(tree.find("live-review-show-0")),
            ("Button", "Show lines")
        );
        window
            .update(cx, |view, window, cx| {
                view.show_live_review(7, window, cx);
                assert_eq!(view.tab, MeetingTab::Transcript);
                assert_eq!(view.reviewed_line, Some(1));
                assert!(view.detail_focus().is_focused(window));
            })
            .unwrap();
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        let marker = tree.find("line-live-name-7");
        assert_eq!(tree.read(marker).0, "Note");
        assert!(
            tree.read(marker)
                .1
                .contains("while recording · not applied")
        );
        assert_eq!(
            tree.read(tree.find("line-leave-live-name-7")).1,
            "Leave as transcribed"
        );
        // Naming the lines as said is an explicit line correction.
        reply(dir.path(), json!({ "person_id": s.ana }));
        window
            .update(cx, |view, window, cx| view.apply_live_review(7, window, cx))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(
            calls(dir.path()).last().unwrap(),
            &attribute_call(&lib, &m, Some(s.ana.as_str()), &[1])
        );
        window
            .update(cx, |view, _, _| match view.last_notice() {
                Some(Notice::Info(text)) => assert!(text.contains("now name"), "{text}"),
                other => panic!("expected the correction's notice, got {other:?}"),
            })
            .unwrap();
        // The synthetic CLI wrote nothing, so the decision still waits.
        // Leaving the lines as transcribed settles it without any line
        // change (in-process, fenced by the item as shown).
        window
            .update(cx, |view, window, cx| {
                assert_eq!(view.data().unwrap().live_review.len(), 1);
                view.leave_live_review(7, window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |view, _, _| {
                assert!(view.data().unwrap().live_review.is_empty(), "settled");
                match view.last_notice() {
                    Some(Notice::Info(text)) => {
                        assert!(text.starts_with("Left as transcribed"), "{text}")
                    }
                    other => panic!("expected the settled notice, got {other:?}"),
                }
            })
            .unwrap();
        let after = lib.load_meeting(&s.meeting).unwrap();
        assert!(after.live_attributions.iter().all(|a| a.user_override));
        assert_eq!(
            after.utterance_people, m.utterance_people,
            "no line changed"
        );
    }

    // ---- named text (U08) ----

    /// The nodes one render of the meeting view names (nested views, such
    /// as the picker, the generation controls and the Templates tab, render
    /// on their own).
    pub(super) fn spoken_nodes(
        window: &WindowHandle<MeetingView>,
        cx: &mut TestAppContext,
    ) -> Vec<crate::ui::spoken_log::Spoken> {
        window
            .update(cx, |view, window, cx| {
                crate::ui::spoken_log::while_building(|| Render::render(view, window, cx)).1
            })
            .unwrap()
    }

    /// A meeting with two participants, a tag, and line 2 named Ana Silva
    /// for that line only.
    fn header_meeting(m: &mut Meeting, s: &Synthetic) {
        m.participants = vec![
            Participant {
                name: Some("Bia Example".into()),
                ..Participant::default()
            },
            Participant {
                email: Some("carla@example.com".into()),
                ..Participant::default()
            },
        ];
        m.tags = vec!["synthetic-tag".into()];
        let entry = overlay_entry(&m.segments[1], &s.ana);
        m.utterance_people.push(entry);
    }

    /// MTG-51, MTG-53, MTG-60, MTG-61, INS-01: the landmark and the tab
    /// panel say what they hold, participants are list items, a tag is read
    /// from its own node, the "corrected" marker and the line picker's voice
    /// note are their own nodes, and every header and transcript text is
    /// read.
    #[gpui::test]
    fn header_and_transcript_texts_are_named_nodes(cx: &mut TestAppContext) {
        use crate::ui::a11y_name;

        let (window, _app, _lib, _s, _dir) = open_edited(cx, header_meeting);
        window
            .update(cx, |view, window, cx| {
                view.set_tab(MeetingTab::Transcript, cx);
                let root = Render::render(view, window, cx).into_element();
                assert_eq!(
                    a11y_name(&root),
                    Some((
                        "meeting-view".to_owned(),
                        Role::Main,
                        "Meeting: Synthetic sync".to_owned()
                    ))
                );
                for tab in [
                    MeetingTab::Transcript,
                    MeetingTab::Speakers,
                    MeetingTab::Overview,
                    MeetingTab::Templates,
                ] {
                    view.tab = tab;
                    assert_eq!(
                        a11y_name(&view.tab_panel(div().into_any_element())),
                        Some((
                            "meeting-tab-panel".to_owned(),
                            Role::TabPanel,
                            tab.label().to_owned()
                        ))
                    );
                }
                view.tab = MeetingTab::Transcript;
            })
            .unwrap();
        draw(&window, cx);

        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        for (id, role, name) in [
            ("meeting-title", "Heading", "Synthetic sync"),
            ("tag-0", "Label", "Tag: synthetic-tag"),
            ("participant-0", "Label", "Bia Example"),
            ("participant-1", "Label", "carla@example.com"),
            ("line-item-0", "ListItem", "Speaker 1 at 5 seconds: line 1"),
            (
                "line-item-1",
                "ListItem",
                "Ana Silva at 10 seconds, corrected: line 2",
            ),
            ("line-item-2", "ListItem", "Speaker 3 at 15 seconds: line 3"),
            ("line-corrected-1", "Label", "corrected"),
        ] {
            assert_eq!(tree.read(tree.find(id)), (role, name));
        }

        // The line picker notes whose voice the matches use.
        start_line(&window, &mut visual, 1);
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        assert_eq!(
            tree.read(tree.find("line-voice-note-1")),
            ("Note", "Closest voices use Speaker 2's voice.")
        );
    }

    #[gpui::test]
    fn saved_transcript_clock_column_and_reading_text_scale_together(cx: &mut TestAppContext) {
        let (handle, _, _, _, _dir) = open_edited(cx, |meeting, _| {
            meeting.duration_s = 7200.;
            meeting.segments[0].start = 3723.;
            meeting.segments[0].end = 3724.;
            meeting.segments.sort_by(|a, b| a.start.total_cmp(&b.start));
        });
        let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
        for width in [620., 1100.] {
            visual.simulate_resize(gpui::size(px(width), px(800.)));
            visual.update(crate::ui::a11y_tree::draw_and_read);
            let first = meeting_control_bounds(&mut visual, "line-time-0");
            let hour = meeting_control_bounds(&mut visual, "line-time-2");
            assert_eq!(first.size.width, px(68.));
            assert_eq!(first.size.width, hour.size.width);
            assert_eq!(first.left(), hour.left());
            let words = meeting_control_bounds(&mut visual, "line-words-0");
            assert_eq!(words.size.height, px(22.));
            let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
            let (role, label) = tree.read(tree.find("line-item-2"));
            assert_eq!(role, "ListItem");
            assert!(label.contains("1 hour 2 minutes 3 seconds"), "{label}");
            assert_eq!(tree.read(tree.find("meeting-title")).0, "Heading");
        }
    }

    #[gpui::test]
    fn saved_transcript_name_action_reveals_on_hover_without_moving_words(cx: &mut TestAppContext) {
        let (handle, _, _, _, _dir) = open_edited(cx, |_, _| {});
        let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
        let action_visible = |visual: &mut gpui::VisualTestContext, id: &str| {
            visual.update(|window, cx| {
                window.draw(cx).clear(cx);
                gpui::base::test_support::snapshots(window)
                    .iter()
                    .any(|node| {
                        node.visible()
                            && node
                                .path()
                                .last()
                                .is_some_and(|part| part.to_string() == id)
                    })
            })
        };
        assert!(!action_visible(&mut visual, "line-actions-1"));
        let words = meeting_control_bounds(&mut visual, "line-words-1");
        visual.simulate_mouse_move(words.center(), None, Default::default());
        assert!(action_visible(&mut visual, "line-actions-1"));
        assert!(!action_visible(&mut visual, "line-actions-2"));
        assert_eq!(meeting_control_bounds(&mut visual, "line-words-1"), words);
        click_meeting_control(&mut visual, "line-name-1");
        handle
            .update(&mut visual, |view, window, cx| {
                assert!(view.picker().unwrap().read(cx).search_focused(window, cx));
                assert_eq!(view.transcript_cursor, Some(1));
            })
            .unwrap();
    }

    fn two_hour_transcript(m: &mut Meeting, _: &Synthetic) {
        m.duration_s = 7200.;
        m.segments = (0..1500)
            .map(|ix| seg(ix, ix % 3 + 1, f64::from(ix) * 4.8, "An invented turn."))
            .collect();
    }

    #[gpui::test]
    fn saved_transcript_roves_with_one_tab_stop_and_bounds_steady_state_work(
        cx: &mut TestAppContext,
    ) {
        let (handle, _, _, _, _dir) = open_edited(cx, two_hour_transcript);
        let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
        handle
            .update(&mut visual, |view, window, cx| {
                view.select_transcript_row(0, window, cx);
                view.transcript_row_builds.reset();
            })
            .unwrap();
        visual.simulate_keystrokes("down down pagedown");
        handle
            .update(&mut visual, |view, window, _| {
                let ix = view.transcript_cursor.unwrap();
                assert!(ix > 2);
                assert!(view.transcript_rows[ix].is_focused(window));
            })
            .unwrap();
        visual.simulate_keystrokes("end");
        visual.update(crate::ui::a11y_tree::draw_and_read);
        handle
            .update(&mut visual, |view, window, cx| {
                assert_eq!(view.transcript_cursor, Some(1499));
                assert!(view.transcript_rows[1499].is_focused(window));
                window.focus_next(cx);
                assert!(
                    !view
                        .transcript_rows
                        .iter()
                        .any(|row| row.is_focused(window))
                );
                window.focus_prev(cx);
                assert!(view.transcript_rows[1499].is_focused(window));
            })
            .unwrap();
        visual.simulate_keystrokes("home");
        handle
            .update(&mut visual, |view, window, cx| {
                assert_eq!(view.transcript_cursor, Some(0));
                view.transcript_list.scroll_to_reveal_item(1000);
                cx.notify();
                assert!(view.transcript_rows[0].is_focused(window));
            })
            .unwrap();
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        assert_eq!(tree.read(tree.find("line-item-0")).0, "ListItem");
        assert!(tree.all("line-item-500").is_empty());
        handle
            .update(&mut visual, |view, _, _| {
                assert!(
                    view.transcript_row_builds.get() < 250,
                    "{} row builds",
                    view.transcript_row_builds.get()
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn saved_transcript_follow_leaves_reading_position_and_returns_to_playing_line(
        cx: &mut TestAppContext,
    ) {
        use gpui::{ScrollDelta, ScrollEvent, point};
        let (handle, _, _, _, _dir) = open_edited(cx, two_hour_transcript);
        let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
        visual.update(crate::ui::a11y_tree::draw_and_read);
        handle
            .update(&mut visual, |view, window, cx| {
                view.playback.playing = true;
                view.playback.loaded = true;
                view.set_position(1200., true, window, cx);
                cx.notify();
            })
            .unwrap();
        let list = handle
            .update(&mut visual, |view, _, _| view.transcript_list.clone())
            .unwrap();
        visual.update(crate::ui::a11y_tree::draw_and_read);
        visual.simulate_event(ScrollEvent {
            position: list.viewport_bounds().center(),
            delta: ScrollDelta::Pixels(point(px(0.), px(500.))),
            ..Default::default()
        });
        let reading = list.logical_scroll_top().item_ix;
        handle
            .update(&mut visual, |view, window, cx| {
                assert!(!view.following_transcript);
                view.set_position(4500., true, window, cx);
                cx.notify();
            })
            .unwrap();
        assert_eq!(reading, list.logical_scroll_top().item_ix);
        visual
            .update(crate::ui::a11y_tree::draw_and_read)
            .find("transcript-back-current");
        click_meeting_control(&mut visual, "transcript-back-current");
        handle
            .update(&mut visual, |view, window, cx| {
                assert!(view.following_transcript);
                let playing = view.playback.current_line.unwrap();
                assert_eq!(view.transcript_cursor, Some(playing));
                assert!(
                    view.transcript_rows[playing].is_focused(window),
                    "focus after returning: {:?}",
                    window.focused(cx)
                );
                assert_eq!(list.item_is_below_viewport(playing), Some(false));
                view.open_find(window, cx);
                let top = list.logical_scroll_top().item_ix;
                view.set_position(6000., true, window, cx);
                assert_eq!(top, list.logical_scroll_top().item_ix);
                view.close_find(window, cx);
                view.start_line_naming(playing, window, cx);
                let top = list.logical_scroll_top().item_ix;
                view.set_position(7000., true, window, cx);
                assert_eq!(top, list.logical_scroll_top().item_ix);
            })
            .unwrap();
    }

    #[gpui::test]
    fn saved_transcript_find_wraps_folds_and_escape_focuses_match(cx: &mut TestAppContext) {
        let (handle, _, _, _, _dir) = open_edited(cx, |m, _| {
            m.segments[0].text = "The Café opens.".into();
            m.segments[2].text = "Find the cafe\u{301}.".into();
        });
        let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
        visual.update(crate::ui::a11y_tree::draw_and_read);
        let tools = visual.debug_bounds("transcript-tools").unwrap();
        let rows = visual.debug_bounds("transcript-list").unwrap();
        assert!(tools.bottom() <= rows.top());
        handle
            .update(&mut visual, |view, window, cx| {
                view.open_find(window, cx);
                assert!(view.find_input.focus_handle(cx).is_focused(window));
                view.find_input
                    .update(cx, |input, cx| input.replace_all("CAFE", window, cx));
            })
            .unwrap();
        visual.executor().advance_clock(Duration::from_millis(150));
        visual.run_until_parked();
        let status = |visual: &mut gpui::VisualTestContext| {
            let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
            tree.read(tree.find("find-transcript-status")).1.to_owned()
        };
        assert_eq!(status(&mut visual), "1 of 2");
        visual.simulate_keystrokes("shift-enter");
        assert_eq!(status(&mut visual), "2 of 2");
        visual.simulate_keystrokes("enter");
        assert_eq!(status(&mut visual), "1 of 2");
        visual.simulate_keystrokes("shift-enter escape");
        handle
            .update(&mut visual, |view, window, _| {
                assert_eq!(view.transcript_cursor, Some(2));
                assert!(view.transcript_rows[2].is_focused(window));
                assert!(view.find_query.is_empty() && view.find_hits.is_empty() && !view.find_open);
            })
            .unwrap();
        let text = "The cafe\u{301} opens";
        let ranges = broscribe_core::names::folded_match_ranges(text, "cafe");
        assert_eq!(
            ranges
                .iter()
                .map(|range| &text[range.clone()])
                .collect::<Vec<_>>(),
            ["cafe\u{301}"]
        );
    }

    #[gpui::test]
    fn saved_transcript_menu_copy_and_scope_share_the_selected_line(cx: &mut TestAppContext) {
        use gpui::component::input::Copy;
        let (handle, _, _, _, _dir) = open_edited(cx, header_meeting);
        let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
        handle
            .update(&mut visual, |view, window, cx| {
                let keys = crate::ui::keymap::key_bindings_for(
                    broscribe_platform::HostPlatform::Windows,
                    cx,
                );
                cx.bind_keys(keys);
                view.select_transcript_row(1, window, cx);
            })
            .unwrap();
        visual.simulate_keystrokes("shift-f10");
        let commands = |visual: &mut gpui::VisualTestContext| {
            visual
                .update(crate::ui::a11y_tree::draw_and_read)
                .nodes
                .into_iter()
                .filter(|node| node.role == "MenuItem" && !node.name.is_empty())
                .map(|node| node.name)
                .collect::<Vec<_>>()
        };
        let expected = [
            "Name speaker…",
            "Name this line only…",
            "Use Speaker 2's name again",
            "Copy line",
        ];
        assert_eq!(commands(&mut visual), expected);
        visual.simulate_keystrokes("escape");
        let row = meeting_control_bounds(&mut visual, "line-item-1").center();
        visual.simulate_mouse_down(row, gpui::MouseButton::Right, Default::default());
        assert_eq!(commands(&mut visual), expected);
        visual.simulate_keystrokes("escape");
        handle
            .update(&mut visual, |view, window, cx| {
                assert!(view.transcript_rows[1].is_focused(window));
                window.dispatch_action(Box::new(Copy), cx);
            })
            .unwrap();
        assert_eq!(
            visual.read_from_clipboard().unwrap().text().unwrap(),
            "0:10 Ana Silva: line 2"
        );
        visual
            .update(crate::ui::a11y_tree::draw_and_read)
            .find("transcript-copy-status");
        handle
            .update(&mut visual, |view, _, cx| view.copy_transcript(cx))
            .unwrap();
        assert!(
            visual
                .read_from_clipboard()
                .unwrap()
                .text()
                .unwrap()
                .contains("[00:00:10] Ana Silva: line 2")
        );
        visual.simulate_keystrokes("n");
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        assert_eq!(
            tree.read(tree.find("line-scope-1")),
            ("RadioGroup", "Name which lines")
        );
        assert!(tree.all("person-picker-cancel").is_empty());
        handle
            .update(&mut visual, |view, window, cx| {
                assert!(view.picker().unwrap().read(cx).search_focused(window, cx));
                assert_eq!(view.naming_scope(), Some(NamingScope::SpeakerLines));
            })
            .unwrap();
        visual.update(|window, cx| {
            window.focus_prev(cx);
            window.focus_prev(cx);
            window.draw(cx).clear(cx);
        });
        visual.simulate_keystrokes("right");
        handle
            .update(&mut visual, |view, _, _| {
                assert_eq!(view.naming_scope(), Some(NamingScope::ThisLine));
            })
            .unwrap();
        visual.simulate_click(gpui::point(px(2.), px(2.)), Default::default());
        handle
            .update(&mut visual, |view, window, _| {
                assert!(view.picker().is_none());
                assert!(view.transcript_rows[1].is_focused(window));
            })
            .unwrap();
    }

    fn reference_audio(path: &Path, meeting: &mut Meeting, fixture: &Synthetic) {
        with_recording(meeting, fixture);
        let rec = meeting.recording.as_mut().unwrap();
        rec.handling = Handling::Reference;
        rec.path = path.to_path_buf();
        rec.sha256 = broscribe_core::schema::sha256_hex(b"synthetic");
    }

    pub(super) fn click_meeting_control(visual: &mut gpui::VisualTestContext, id: &str) {
        kit::dialog_gate::click(visual, id, Role::Button);
    }

    pub(super) fn meeting_control_bounds(
        visual: &mut gpui::VisualTestContext,
        id: &str,
    ) -> gpui::Bounds<gpui::Pixels> {
        visual.update(|window, _| {
            gpui::base::test_support::snapshots(window)
                .into_iter()
                .find(|node| {
                    node.path()
                        .last()
                        .is_some_and(|part| part.to_string() == id)
                })
                .unwrap_or_else(|| panic!("missing {id} bounds"))
                .bounds()
        })
    }

    fn focus_meeting_control(visual: &mut gpui::VisualTestContext, id: &str) {
        visual.update(|window, cx| {
            for _ in 0..48 {
                window.focus_next(cx);
                window.draw(cx).clear(cx);
                if gpui::base::test_support::snapshots(window)
                    .iter()
                    .any(|node| {
                        node.path()
                            .last()
                            .is_some_and(|part| part.to_string() == id)
                            && node.focused() == Some(true)
                    })
                {
                    return;
                }
            }
            panic!("{id} was never focused");
        });
    }

    #[gpui::test]
    fn meeting_tab_labels_and_speaker_badge_fit_at_largest(cx: &mut TestAppContext) {
        use gpui::{Bounds, Pixels};

        let (_, app, _, _, _dir) = open_edited(cx, |_, _| {});
        let (_, window) = meeting_test_shell(cx, app, gpui::size(px(800.), px(720.)));
        let mut visual = gpui::VisualTestContext::from_window(window, cx);
        visual.update(|_, cx| {
            crate::ui::apply_reading_size(broscribe_core::config::ReadingSize::Largest, cx);
        });
        crate::ui::kit::dialog_gate::draw(&mut visual);
        let transcript = visual.update(|window, _| {
            let nodes = gpui::base::test_support::snapshots(window);
            let find_id = |id: &str| {
                nodes
                    .iter()
                    .find(|node| {
                        node.path()
                            .last()
                            .is_some_and(|part| part.to_string() == id)
                    })
                    .unwrap_or_else(|| panic!("missing {id}"))
            };
            let list = find_id("meeting-tab-strip").bounds();
            let mut transcript = None;
            for tab in MEETING_TABS {
                let node = nodes
                    .iter()
                    .find(|node| {
                        node.role() == Some(Role::Tab)
                            && node.label().is_some_and(|name| {
                                name == tab.label()
                                    || (tab == MeetingTab::Speakers
                                        && name.starts_with("Speakers,"))
                            })
                    })
                    .unwrap();
                let bounds = node.bounds();
                let label = find_id(&format!("meeting-tab-label-{}", tab.label())).bounds();
                let within = |inner: Bounds<Pixels>, outer: Bounds<Pixels>| {
                    inner.left() >= outer.left()
                        && inner.right() <= outer.right()
                        && inner.top() >= outer.top()
                        && inner.bottom() <= outer.bottom()
                };
                assert!(within(bounds, list), "{tab:?} {bounds:?} outside {list:?}");
                assert!(
                    within(label, bounds),
                    "{tab:?} {label:?} outside {bounds:?}"
                );
                assert!(bounds.size.height > label.size.height);
                if tab == MeetingTab::Speakers {
                    let badge = find_id("meeting-tab-speaker-count").bounds();
                    assert!(within(badge, bounds), "{badge:?} outside {bounds:?}");
                    assert!(label.right() <= badge.left());
                }
                if tab == MeetingTab::Transcript {
                    transcript = Some(bounds.center());
                }
            }
            transcript.unwrap()
        });
        visual.simulate_click(transcript, Default::default());
        crate::ui::kit::dialog_gate::draw(&mut visual);
        focus_meeting_control(&mut visual, "tab-1");
        visual.simulate_keystrokes("right");
        visual.update(|window, cx| {
            window.draw(cx).clear(cx);
            assert!(
                gpui::base::test_support::snapshots(window)
                    .iter()
                    .any(|node| {
                        node.role() == Some(Role::Tab)
                            && node
                                .label()
                                .is_some_and(|name| name.starts_with("Speakers,"))
                            && node.focused() == Some(true)
                    })
            );
        });
        visual.simulate_keystrokes("enter");
        visual.simulate_event(gpui::KeyUpEvent {
            keystroke: gpui::Keystroke::parse("enter").unwrap(),
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
                                .is_some_and(|name| name.starts_with("Speakers,"))
                            && node.selected() == Some(true)
                    })
            );
        });
    }

    #[gpui::test]
    fn playback_speed_and_timeline_share_the_toolbar_at_largest(cx: &mut TestAppContext) {
        let audio = tempfile::tempdir().unwrap();
        let path = audio.path().join("invented.m4a");
        std::fs::write(&path, b"synthetic").unwrap();
        let (_, app, _, _, _dir) = open_edited(cx, |meeting, fixture| {
            reference_audio(&path, meeting, fixture);
        });
        let (_, window) = meeting_test_shell(cx, app, gpui::size(px(1120.), px(720.)));
        let mut visual = gpui::VisualTestContext::from_window(window, cx);
        visual.update(|_, cx| {
            crate::ui::apply_reading_size(broscribe_core::config::ReadingSize::Largest, cx);
        });
        crate::ui::kit::dialog_gate::draw(&mut visual);
        visual.update(|window, _| {
            let nodes = gpui::base::test_support::snapshots(window);
            let bounds = |label| {
                nodes
                    .iter()
                    .find(|node| node.label() == Some(label))
                    .unwrap_or_else(|| panic!("{label} was not laid out"))
                    .bounds()
            };
            let speed = bounds("Playback speed");
            let slider = bounds("Playback position");
            let play = bounds("Play");
            assert!(play.right() <= speed.left(), "{play:?}, {speed:?}");
            assert!(speed.right() <= slider.left(), "{speed:?}, {slider:?}");
            assert!(speed.top() <= slider.center().y && slider.center().y <= speed.bottom());
        });
    }

    #[gpui::test]
    fn meeting_header_more_transport_and_participants_fit_actual_width(cx: &mut TestAppContext) {
        let audio = tempfile::tempdir().unwrap();
        let path = audio.path().join("synthetic.m4a");
        std::fs::write(&path, b"synthetic").unwrap();
        let (_, app, _, fixture, _dir) = open_edited(cx, |m, s| {
            reference_audio(&path, m, s);
            m.participants = (0..8)
                .map(|ix| Participant {
                    name: Some(format!("Person Example {ix}")),
                    person_id: (ix == 0).then(|| s.ana.clone()),
                    ..Default::default()
                })
                .collect();
        });
        let (shell, window) = meeting_test_shell(cx, app.clone(), gpui::size(px(1400.), px(800.)));
        let view = shell.read_with(cx, |shell, _| shell.meeting_view().unwrap().clone());
        let mut visual = gpui::VisualTestContext::from_window(window, cx);
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        let header = tree.subtree(tree.find("meeting-header"));
        assert!(header.contains(&tree.find("playback-bar")));
        assert!(header.contains(&tree.find("export-open")));
        assert_eq!(tree.all("playback-bar").len(), 1);
        assert_eq!(
            tree.read(tree.find("participants-more")),
            ("Button", "Show 2 more participants")
        );
        visual.update(|window, cx| {
            let keys = crate::ui::keymap::key_bindings(cx);
            cx.bind_keys(keys);
            view.update(cx, |view, cx| {
                view.set_tab(MeetingTab::Overview, cx);
                view.detail_focus.focus(window, cx);
            });
            window.draw(cx).clear(cx);
        });
        visual.simulate_keystrokes("secondary-f");
        visual.update(|window, cx| {
            let view = view.read(cx);
            assert_eq!(view.tab, MeetingTab::Transcript);
            assert!(view.find_open && view.find_input.focus_handle(cx).is_focused(window));
            assert!(!view.playback.playing);
        });
        visual.simulate_keystrokes("escape");
        click_meeting_control(&mut visual, "meeting-more");
        let menu = |visual: &mut gpui::VisualTestContext| {
            visual
                .update(crate::ui::a11y_tree::draw_and_read)
                .nodes
                .into_iter()
                .filter(|node| node.role == "MenuItem" && !node.name.is_empty())
                .map(|node| node.name)
                .collect::<Vec<_>>()
        };
        let reveal = NativePlatform::host_platform().reveal_object_label("recording");
        assert_eq!(
            menu(&mut visual),
            [
                "Rename",
                "Copy transcript",
                reveal.as_str(),
                "Delete meeting"
            ]
        );
        visual.simulate_keystrokes("escape");
        // At the largest supported text size, a pane wider than 640 px
        // can still need the compact header.
        visual.simulate_resize(gpui::size(px(880.), px(1000.)));
        let reading_size = |app: &mut AppState, size, cx: &mut Context<AppState>| {
            let mut config = app.config().clone();
            config.appearance.reading_size = size;
            app.set_config(config, cx);
        };
        use broscribe_core::config::ReadingSize;
        app.update(&mut visual, |app, cx| {
            reading_size(app, ReadingSize::Largest, cx)
        });
        visual.update(crate::ui::a11y_tree::draw_and_read);
        visual.run_until_parked();
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        assert!(tree.all("export-open").is_empty());
        assert!(tree.all("playback-back").is_empty() && tree.all("playback-forward").is_empty());
        assert_eq!(
            tree.read(tree.find("participants-more")),
            ("Button", "Show 5 more participants")
        );
        app.update(&mut visual, |app, cx| {
            reading_size(app, ReadingSize::Standard, cx)
        });
        visual.simulate_resize(gpui::size(px(630.), px(800.)));
        visual.update(crate::ui::a11y_tree::draw_and_read);
        visual.run_until_parked();
        click_meeting_control(&mut visual, "meeting-more");
        assert_eq!(
            menu(&mut visual).first().map(String::as_str),
            Some("Export…")
        );
        visual.simulate_keystrokes("up enter");
        visual.run_until_parked();
        assert!(view.read_with(&visual, |view, _| view.delete_prompt().is_some()));
        visual.simulate_keystrokes("escape");
        visual.run_until_parked();
        visual.update(crate::ui::a11y_tree::draw_and_read);
        click_meeting_control(&mut visual, "participants-more");
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        assert_eq!(
            tree.read(tree.find("participant-7")),
            ("Label", "Person Example 7")
        );
        click_meeting_control(&mut visual, "participant-0");
        assert_eq!(
            app.read_with(&visual, |state, _| state.route().clone()),
            Route::People {
                person: Some(fixture.ana.clone())
            }
        );
        assert!(
            visual
                .update(crate::ui::a11y_tree::draw_and_read)
                .nodes
                .iter()
                .any(|node| node.role == "Heading" && node.name == "Ana Silva")
        );
        click_meeting_control(&mut visual, "history-back");
        assert_eq!(
            app.read_with(&visual, |state, _| state.route().clone()),
            Route::meeting(fixture.meeting)
        );
    }

    #[gpui::test]
    fn meeting_header_facts_effective_tags_and_remove_follow_library_files(
        cx: &mut TestAppContext,
    ) {
        let (handle, _, lib, fixture, _dir) = open_edited(cx, |m, _| {
            m.app = Some("zoom".into());
            m.duration_s = 4355.;
            m.source = Source::Recording;
            m.tags = vec!["manual".into(), "shared".into()];
            m.tags_removed = vec!["hidden".into()];
            m.enrichment = Some(serde_json::from_value(json!({
                "provider": "fake", "created_at": "2026-11-15T00:00:00Z", "summary": "Synthetic.",
                "tags": ["hidden", "suggested", "shared"],
            })).unwrap());
            m.languages = [("en", 0., 10.), ("pt-BR", 10., 20.), ("en", 20., 30.)]
                .into_iter()
                .map(
                    |(lang, from_s, to_s)| broscribe_core::schema::LanguageSpan {
                        lang: lang.into(),
                        from_s,
                        to_s,
                    },
                )
                .collect();
            m.language_effective = Some("en".into());
        });
        let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        let facts = tree.read(tree.find("meeting-facts")).1;
        for word in [
            "local time",
            "1 hour",
            "Zoom",
            "Recorded",
            "No recording",
            "English",
            "Portuguese (Brazil)",
        ] {
            assert!(facts.contains(word), "{facts}");
        }
        assert_eq!(facts.matches("English").count(), 1);
        assert_eq!(
            tree.read(tree.find("meeting-detected-language")),
            ("Label", "Detected language: English")
        );
        assert!(tree.all("audio-none").is_empty());
        let names: Vec<_> = tree
            .nodes
            .iter()
            .filter(|node| node.name.starts_with("Tag: "))
            .map(|node| node.name.as_str())
            .collect();
        assert_eq!(names, ["Tag: manual", "Tag: shared", "Tag: suggested"]);
        let remove = tree
            .nodes
            .iter()
            .find(|node| node.name == "Remove tag suggested")
            .unwrap();
        assert_eq!(remove.role, "Button");
        let id = remove.element_id.clone().unwrap();
        let id = match id.as_str() {
            "tag-remove-2" => "tag-remove-2",
            other => panic!("unexpected chip order {other}"),
        };
        let bounds = meeting_control_bounds(&mut visual, id);
        assert!(
            bounds.size.width <= px(24.) && bounds.size.height <= px(24.),
            "{bounds:?}"
        );
        visual.update(|_, cx| {
            let keys =
                crate::ui::keymap::key_bindings_for(broscribe_platform::HostPlatform::Windows, cx);
            cx.bind_keys(keys);
        });
        focus_meeting_control(&mut visual, id);
        visual.simulate_keystrokes("delete");
        visual.run_until_parked();
        let saved = lib.load_meeting(&fixture.meeting).unwrap();
        assert_eq!(saved.effective_tags(), ["manual", "shared"]);
        assert!(saved.tags_removed.contains(&"suggested".to_owned()));
        let after = visual.update(crate::ui::a11y_tree::draw_and_read);
        assert!(
            after
                .nodes
                .iter()
                .all(|node| node.name != "Remove tag suggested")
        );
    }

    #[gpui::test]
    fn meeting_header_processing_and_load_states_offer_bound_recovery(cx: &mut TestAppContext) {
        let audio = tempfile::tempdir().unwrap();
        let path = audio.path().join("synthetic.m4a");
        std::fs::write(&path, b"synthetic").unwrap();
        let (handle, app, lib, fixture, dir) = open_edited(cx, |m, s| {
            reference_audio(&path, m, s);
            m.processing.status = ProcessingStatus::Failed;
            m.processing.error = Some("decode error: corrupted audio".into());
        });
        let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
        let report = |visual: &mut gpui::VisualTestContext, id: &str| {
            let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
            let (role, name) = tree.read(tree.find(id));
            (role.to_owned(), name.to_owned())
        };
        let (role, message) = report(&mut visual, "processing-failed");
        assert_eq!(role, "Alert");
        assert!(
            message.starts_with("Couldn't transcribe this meeting."),
            "{message}"
        );
        assert!(message.len() > "Couldn't transcribe this meeting.".len());
        let queue = Queue::open(lib.root()).unwrap();
        let job = lib
            .write(|tx| {
                queue.enqueue(tx, "synthetic-first", "ahead", json!({}))?;
                queue.enqueue(
                    tx,
                    broscribe_core::queue::REPROCESS_KIND,
                    fixture.meeting.as_str(),
                    json!({"meeting_id": fixture.meeting}),
                )
            })
            .unwrap();
        reply(dir.path(), json!({"queued": true, "job_id": job}));
        click_meeting_control(&mut visual, "processing-retry");
        assert!(calls(dir.path()).iter().any(|call| {
            call.0
                .contains(&format!("reprocess {} --queue", fixture.meeting))
        }));
        assert_eq!(
            report(&mut visual, "meeting-processing").1,
            "Queued · 1 job ahead"
        );
        app.update(&mut visual, |app, cx| {
            app.daemon_progress(
                &broscribe_ipc::JobProgress {
                    job_id: job.as_str().into(),
                    kind: broscribe_core::queue::REPROCESS_KIND.into(),
                    meeting_id: Some(fixture.meeting.clone()),
                    phase: "progress".into(),
                    fraction: Some(0.42),
                    message: None,
                },
                cx,
            )
        });
        assert_eq!(
            report(&mut visual, "meeting-processing").1,
            "Transcribing · 42%"
        );
        for (status, expected) in [
            (ProcessingStatus::Analyzed, "Identifying speakers…"),
            (
                ProcessingStatus::Provisional,
                "Draft transcript · the final transcript replaces it when it’s ready",
            ),
        ] {
            let mut meeting = lib.load_meeting(&fixture.meeting).unwrap();
            meeting.processing.status = status;
            lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
            handle
                .update(&mut visual, |view, window, cx| {
                    view.set_tab(MeetingTab::Overview, cx);
                    view.reload(window, cx);
                })
                .unwrap();
            visual.run_until_parked();
            assert_eq!(report(&mut visual, "meeting-processing").1, expected);
        }

        handle
            .update(&mut visual, |view, window, cx| {
                view.apply_load(
                    view.load_generation,
                    Err(Error::Schema("synthetic damaged file".into())),
                    window,
                    cx,
                );
            })
            .unwrap();
        let (role, message) = report(&mut visual, "meeting-status");
        assert_eq!(role, "Alert");
        assert!(message.starts_with("Couldn't open this meeting."));
        click_meeting_control(&mut visual, "meeting-load-retry");
        handle
            .update(&mut visual, |view, window, cx| {
                assert!(view.data.is_some());
                view.apply_load(
                    view.load_generation,
                    Err(Error::NotFound("synthetic".into())),
                    window,
                    cx,
                );
            })
            .unwrap();
        assert_eq!(
            report(&mut visual, "meeting-status"),
            (
                "Status".to_owned(),
                "This meeting no longer exists.".to_owned()
            )
        );
        assert!(
            visual
                .update(crate::ui::a11y_tree::draw_and_read)
                .all("meeting-load-retry")
                .is_empty()
        );
        click_meeting_control(&mut visual, "meeting-load-back");
        assert_eq!(
            app.read_with(&visual, |app, _| app.route().clone()),
            Route::library()
        );
    }

    #[gpui::test]
    fn meeting_header_rechecks_the_missing_reference_and_keeps_deleted_audio_in_facts(
        cx: &mut TestAppContext,
    ) {
        let audio = tempfile::tempdir().unwrap();
        let path = audio.path().join("synthetic.m4a");
        let (handle, _, lib, fixture, _dir) = open_edited(cx, |m, s| reference_audio(&path, m, s));
        let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        let (role, alert) = tree.read(tree.find("audio-missing"));
        assert_eq!(role, "Alert");
        assert!(alert.contains("BroScribe expects it at ") && alert.contains("synthetic.m4a"));
        assert_eq!(
            tree.read(tree.find("recording-show-folder")).1,
            NativePlatform::host_platform().reveal_object_label("folder")
        );
        assert!(tree.all("playback-bar").is_empty());
        std::fs::write(&path, b"synthetic").unwrap();
        click_meeting_control(&mut visual, "recording-check-again");
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        assert!(tree.all("audio-missing").is_empty());
        tree.find("playback-bar");
        let mut meeting = lib.load_meeting(&fixture.meeting).unwrap();
        assert_eq!(
            meeting.recording.as_ref().unwrap().state,
            RecordingState::Available
        );
        meeting.recording.as_mut().unwrap().state = RecordingState::Deleted;
        lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
        handle
            .update(&mut visual, |view, window, cx| view.reload(window, cx))
            .unwrap();
        visual.run_until_parked();
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        assert!(
            tree.read(tree.find("meeting-facts"))
                .1
                .contains("Audio deleted")
        );
        assert_eq!(
            tree.read(tree.find("audio-deleted-note")),
            ("Note", "The transcript and speakers remain.")
        );
        assert!(tree.all("audio-deleted").is_empty());
    }

    #[gpui::test]
    fn overview_can_mark_a_review_note_checked(cx: &mut TestAppContext) {
        let (handle, _, lib, fixture, _dir) = open_edited(cx, |meeting, _| {
            meeting.set_review_notes(
                ReviewSource::Reprocess,
                ["Check the synthetic timing.".into()],
            );
        });
        let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
        handle
            .update(&mut visual, |view, _, cx| {
                view.set_tab(MeetingTab::Overview, cx)
            })
            .unwrap();
        visual.update(crate::ui::a11y_tree::draw_and_read);
        click_meeting_control(&mut visual, "check-review-note-0");
        assert!(
            lib.load_meeting(&fixture.meeting)
                .unwrap()
                .review_notes
                .is_empty()
        );
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        assert!(tree.all("check-review-note-0").is_empty());
    }

    #[gpui::test]
    fn speaker_filter_preserves_original_transcript_indices(cx: &mut TestAppContext) {
        let (handle, _, _, _, _dir) = open_edited(cx, |m, _| {
            m.segments = (0..4)
                .map(|i| broscribe_core::schema::Segment {
                    speech_overlap: Vec::new(),
                    id: i + 100,
                    track: "mixed".into(),
                    speaker: Some(sid(i % 2 + 1)),
                    start: f64::from(10 * i),
                    end: f64::from(10 * i + 5),
                    text: format!("Synthetic words {}", i + 1),
                    words: Vec::new(),
                })
                .collect();
        });
        let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
        handle
            .update(&mut visual, |view, window, cx| {
                let key = view.data.as_ref().unwrap().lines[1]
                    .person_id
                    .as_ref()
                    .map_or_else(
                        || {
                            view.data.as_ref().unwrap().lines[1]
                                .speaker
                                .unwrap()
                                .to_string()
                        },
                        ToString::to_string,
                    );
                view.set_transcript_speaker(&key, window, cx);
                assert_eq!(view.transcript_line_rows, [None, Some(0), None, Some(1)]);
                view.select_transcript_row(3, window, cx);
                assert_eq!(view.focused_playback_position(window), Some(30.));
            })
            .unwrap();
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        assert!(
            tree.read(tree.find("transcript-speaker-status"))
                .1
                .contains("Showing 2 of 4 lines")
        );
        click_meeting_control(&mut visual, "transcript-show-all");
        handle
            .update(&mut visual, |view, _, _| {
                assert_eq!(
                    view.transcript_line_rows,
                    [Some(0), Some(1), Some(2), Some(3)]
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn clicking_words_in_a_long_turn_seeks_to_their_segment(cx: &mut TestAppContext) {
        let audio = tempfile::tempdir().unwrap();
        let path = audio.path().join("synthetic.m4a");
        std::fs::write(&path, b"synthetic").unwrap();
        let (handle, app, _, _, _dir) = open_edited(cx, |m, s| {
            reference_audio(&path, m, s);
            m.duration_s = 240.;
            m.segments = [
                (0, 0., 91., "First."),
                (
                    1,
                    91.,
                    180.,
                    "Second segment begins here and continues until the end.",
                ),
            ]
            .into_iter()
            .map(|(id, start, end, text)| broscribe_core::schema::Segment {
                speech_overlap: Vec::new(),
                id,
                track: "mixed".into(),
                speaker: Some(sid(1)),
                start,
                end,
                text: text.into(),
                words: Vec::new(),
            })
            .collect();
        });
        let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
        visual.simulate_resize(gpui::size(px(1000.), px(800.)));
        app.update(&mut visual, |app, _| {
            app.set_player(Box::new(broscribe_platform::fakes::FakeAudioPlayer::new(
                240.,
            )))
        });
        handle
            .update(&mut visual, |view, _, _| view.native = true)
            .unwrap();
        visual.update(crate::ui::a11y_tree::draw_and_read);
        let bounds = meeting_control_bounds(&mut visual, "line-words-0");
        visual.simulate_click(
            gpui::point(bounds.left() + px(105.), bounds.center().y),
            Default::default(),
        );
        visual.run_until_parked();
        handle
            .update(&mut visual, |view, _, _| {
                assert_eq!(view.playback.position_s, 91.);
                assert_eq!(view.data.as_ref().unwrap().lines[0].segment_ids, [0, 1]);
            })
            .unwrap();
    }

    #[gpui::test]
    fn meeting_transport_shortcuts_and_scrub_preview_use_the_same_player(cx: &mut TestAppContext) {
        let audio = tempfile::tempdir().unwrap();
        let path = audio.path().join("synthetic.m4a");
        std::fs::write(&path, b"synthetic").unwrap();
        let (handle, app, _, _, _dir) = open_edited(cx, |m, s| {
            reference_audio(&path, m, s);
            m.bookmarks.push(broscribe_core::schema::Bookmark {
                at_s: 15.,
                label: None,
            });
            m.notes.push(broscribe_core::schema::Note {
                at_s: 45.,
                text: "Invented transcript note".into(),
            });
        });
        let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
        app.update(&mut visual, |app, _| {
            app.set_player(Box::new(broscribe_platform::fakes::FakeAudioPlayer::new(
                60.,
            )))
        });
        handle
            .update(&mut visual, |view, window, cx| {
                view.native = true;
                let keys = crate::ui::keymap::key_bindings_for(
                    broscribe_platform::HostPlatform::Windows,
                    cx,
                );
                cx.bind_keys(keys);
                view.select_transcript_row(1, window, cx);
            })
            .unwrap();
        visual.update(crate::ui::a11y_tree::draw_and_read);
        visual.simulate_keystrokes("space");
        assert_eq!(
            player_state(&app, &mut visual),
            PlayerState::Playing { position_s: 10. }
        );
        visual.simulate_keystrokes("shift-right");
        assert_eq!(
            player_state(&app, &mut visual),
            PlayerState::Playing { position_s: 25. }
        );
        visual.simulate_keystrokes("space shift-left");
        assert_eq!(
            player_state(&app, &mut visual),
            PlayerState::Paused { position_s: 10. }
        );

        let bounds = meeting_control_bounds(&mut visual, "slider-bar-container");
        for (index, fraction) in [(0, 0.25), (1, 0.75)] {
            let mark = meeting_control_bounds(&mut visual, &format!("scrub-marker-{index}"));
            assert_eq!(mark.size.width, px(2.));
            let expected = bounds.left() + bounds.size.width * fraction;
            assert!(
                (mark.left() - expected).abs() < px(10.),
                "{mark:?} within {bounds:?}"
            );
        }
        let position = gpui::point(bounds.left() + bounds.size.width * 0.75, bounds.center().y);
        visual.simulate_mouse_down(position, gpui::MouseButton::Left, Default::default());
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        let readout = tree.read(tree.find("playback-position")).1.to_owned();
        assert!(readout.starts_with("45 seconds of "), "{readout}");
        handle
            .update(&mut visual, |view, _, _| {
                assert!(!view.ticking);
                assert_eq!(view.playback.position_s, 10.);
            })
            .unwrap();
        visual.simulate_mouse_up(position, gpui::MouseButton::Left, Default::default());
        assert_eq!(
            player_state(&app, &mut visual),
            PlayerState::Paused { position_s: 45. }
        );

        handle
            .update(&mut visual, |view, _, cx| {
                view.set_tab(MeetingTab::Speakers, cx)
            })
            .unwrap();
        visual.update(crate::ui::a11y_tree::draw_and_read);
        focus_meeting_control(&mut visual, "sample-play-speaker-S1");
        let keystroke = gpui::Keystroke::parse("space").unwrap();
        visual.simulate_event(KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        visual.simulate_event(gpui::KeyUpEvent { keystroke });
        visual.run_until_parked();
        assert_eq!(
            player_state(&app, &mut visual),
            PlayerState::Playing { position_s: 5. }
        );
    }

    #[gpui::test]
    fn meeting_transport_cancel_leaves_preparation_running_and_failures_can_retry(
        cx: &mut TestAppContext,
    ) {
        let audio = tempfile::tempdir().unwrap();
        let path = audio.path().join("synthetic.m4a");
        std::fs::write(&path, b"synthetic").unwrap();
        let (handle, app, lib, fixture, _dir) =
            open_edited(cx, |m, s| reference_audio(&path, m, s));
        let queue = Queue::open(lib.root()).unwrap();
        lib.write(|tx| {
            queue.enqueue(tx, "synthetic-first", "ahead", json!({}))?;
            Ok(())
        })
        .unwrap();
        let mut visual = gpui::VisualTestContext::from_window(handle.into(), cx);
        let mut player = broscribe_platform::fakes::FakeAudioPlayer::new(60.);
        player.native_extensions.clear();
        app.update(&mut visual, |app, _| app.set_player(Box::new(player)));
        handle
            .update(&mut visual, |view, _, cx| {
                view.native = false;
                cx.notify();
            })
            .unwrap();
        visual.update(crate::ui::a11y_tree::draw_and_read);
        click_meeting_control(&mut visual, "play-pause");
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        assert_eq!(tree.read(tree.find("play-pause")), ("Button", "Cancel"));
        assert_eq!(
            tree.read(tree.find("playback-state")).1,
            "Preparing audio for playback · 1 job ahead"
        );
        let job = handle
            .update(&mut visual, |view, _, _| {
                view.playback.cache_job.clone().unwrap()
            })
            .unwrap();
        let mut progress = broscribe_ipc::JobProgress {
            job_id: job.as_str().into(),
            kind: PLAYBACK_KIND.into(),
            meeting_id: Some(fixture.meeting),
            phase: "started".into(),
            fraction: None,
            message: None,
        };
        app.update(&mut visual, |app, cx| app.daemon_progress(&progress, cx));
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        assert_eq!(
            tree.read(tree.find("playback-state")).1,
            "Preparing audio for playback…"
        );
        click_meeting_control(&mut visual, "play-pause");
        handle
            .update(&mut visual, |view, _, _| {
                assert_eq!(view.playback.pending_start, None);
                assert_eq!(view.playback.cache_job.as_ref(), Some(&job));
                assert!(!view.ticking);
            })
            .unwrap();
        assert!(queue.get(&job).is_ok());
        progress.phase = "failed".into();
        progress.message = Some("decode error: /synthetic/private/recording.m4a".into());
        app.update(&mut visual, |app, cx| app.daemon_progress(&progress, cx));
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        assert_eq!(
            tree.read(tree.find("audio-unsupported")),
            (
                "Alert",
                "Couldn't prepare this recording for playback. BroScribe couldn't read its library data."
            )
        );
        click_meeting_control(&mut visual, "audio-cache-retry");
        handle
            .update(&mut visual, |view, _, cx| {
                assert!(view.playback.pending_start.is_some());
                view.playback.pending_start = None;
                view.playback.cache_failed = None;
                view.playback.error = Some("the recording could not be opened".into());
                view.native = true;
                cx.notify();
            })
            .unwrap();
        app.update(&mut visual, |state, _| {
            state.set_player(Box::new(broscribe_platform::fakes::FakeAudioPlayer::new(
                60.,
            )));
        });
        let tree = visual.update(crate::ui::a11y_tree::draw_and_read);
        assert_eq!(
            tree.read(tree.find("audio-error")),
            (
                "Alert",
                "Couldn't play the recording. The recording could not be opened."
            )
        );
        click_meeting_control(&mut visual, "audio-playback-retry");
        assert!(matches!(
            player_state(&app, &mut visual),
            PlayerState::Playing { .. }
        ));
    }

    #[gpui::test]
    fn setup_word_tip_waits_for_a_supported_model_and_final_transcript(cx: &mut TestAppContext) {
        let (window, app, _, meeting, _dir) = open_edited(cx, |meeting, _| {
            meeting.processing.status = ProcessingStatus::Done;
        });
        app.update(cx, |state, cx| {
            let mut config = state.config().clone();
            config
                .setup
                .completed_steps
                .push(super::super::setup::ASSISTANT_DONE.into());
            state.set_config(config, cx);
        });
        let data_dir = app.read_with(cx, |state, _| state.paths().data_dir.clone());
        window
            .update(cx, |view, _, cx| {
                let asr = view.state.read(cx).config().profile.asr.clone();
                view.set_tab(MeetingTab::Transcript, cx);
                view.word_tip_model = Some((asr, Some(false)));
                cx.notify();
            })
            .unwrap();
        let tip = data_dir.join("state/setup-words-tip-shown");
        let tree = cx
            .update_window(window.into(), |_, window, cx| {
                crate::ui::a11y_tree::draw_and_read(window, cx)
            })
            .unwrap();
        assert!(tree.all("setup-words-tip").is_empty());
        assert!(!tip.exists());

        window
            .update(cx, |view, _, cx| {
                view.word_tip_model.as_mut().unwrap().1 = Some(true);
                view.data.as_mut().unwrap().meeting.processing.status = ProcessingStatus::Queued;
                cx.notify();
            })
            .unwrap();
        let tree = cx
            .update_window(window.into(), |_, window, cx| {
                crate::ui::a11y_tree::draw_and_read(window, cx)
            })
            .unwrap();
        assert!(tree.all("setup-words-tip").is_empty());
        assert!(!tip.exists());

        window
            .update(cx, |view, _, cx| {
                view.data.as_mut().unwrap().meeting.processing.status = ProcessingStatus::Done;
                cx.notify();
            })
            .unwrap();
        let tree = cx
            .update_window(window.into(), |_, window, cx| {
                crate::ui::a11y_tree::draw_and_read(window, cx)
            })
            .unwrap();
        assert_eq!(tree.read(tree.find("setup-words-tip")).0, "Note");
        assert!(tip.exists());

        let reopened =
            cx.add_window(|window, cx| MeetingView::new(app, meeting.meeting, window, cx));
        cx.run_until_parked();
        reopened
            .update(cx, |view, _, cx| {
                view.set_tab(MeetingTab::Transcript, cx);
                view.word_tip_model =
                    Some((view.state.read(cx).config().profile.asr.clone(), Some(true)));
                cx.notify();
            })
            .unwrap();
        let tree = cx
            .update_window(reopened.into(), |_, window, cx| {
                crate::ui::a11y_tree::draw_and_read(window, cx)
            })
            .unwrap();
        assert!(tree.all("setup-words-tip").is_empty());
    }

    /// MTG-53 and INS-01 in the accessibility tree GPUI builds (every node,
    /// `.role().aria_label()` ones and gpui-component's included): on every
    /// tab, every node of the meeting view has a name and none has the name
    /// of a node it sits in; the Transcript and Speakers lists are named
    /// apart from their tab panels; a tag is read from its own node and
    /// from its removal button.
    #[gpui::test]
    fn no_node_of_the_meeting_view_repeats_a_parents_name(cx: &mut TestAppContext) {
        use crate::ui::a11y_tree;

        let (window, _app, _lib, _s, _dir) = open_edited(cx, |m, s| {
            header_meeting(m, s);
            m.enrichment = Some(
                serde_json::from_value(json!({
                    "provider": "agent-a", "created_at": "2026-11-15T00:00:00Z",
                    "summary": "The team planned the beta.\n\nBia Example owns the launch.",
                    "decisions": [{"id": "d1", "text": "Ship the beta in December",
                                   "at_s": 5.0, "by_name": "Bia Example"}],
                    "actions": [{"id": "a1", "text": "Draft the plan", "at_s": 10.0}]
                }))
                .unwrap(),
            );
            m.template_results.push(
                serde_json::from_value(json!({
                    "template_id": "meeting-notes", "template_name": "Meeting notes",
                    "instructions_sha256": "00", "provider": "agent-a",
                    "created_at": "2026-11-15T00:00:00Z",
                    // A result headed with the template's own name.
                    "markdown": "# Meeting notes\n\nAna Example will draft the plan.\n\n\
                                 - Send the agenda\n- Book the room"
                }))
                .unwrap(),
            );
        });
        for tab in [
            MeetingTab::Transcript,
            MeetingTab::Speakers,
            MeetingTab::Overview,
            MeetingTab::Templates,
        ] {
            window
                .update(cx, |view, _, cx| view.set_tab(tab, cx))
                .unwrap();
            cx.run_until_parked();
            let tree = cx
                .update_window(window.into(), |_, window, cx| {
                    a11y_tree::draw_and_read(window, cx)
                })
                .unwrap();
            tree.assert_title("meeting-title");
            if tab == MeetingTab::Overview {
                tree.assert_heading("overview-summary", 2);
            }
            let root = tree.find("meeting-view");
            assert_eq!(
                tree.read(root),
                ("Main", "Meeting: Synthetic sync"),
                "{tree:?}"
            );
            let panel = tree.find("meeting-tab-panel");
            assert_eq!(tree.read(panel), ("TabPanel", tab.label()), "{tree:?}");
            let mut unnamed = tree.unnamed_under(root);
            if tab == MeetingTab::Overview {
                // gpui-component Table exposes structural nodes with their
                // spoken text in descendants. Their roles supply context.
                unnamed.retain(|node| {
                    !matches!(
                        node.role.as_str(),
                        "RowGroup" | "Row" | "ColumnHeader" | "Cell"
                    ) || node.children.is_empty()
                });
            }
            assert!(
                unnamed.is_empty(),
                "{tab:?}: unnamed {unnamed:?} in {tree:?}"
            );
            let repeated = tree.names_repeated_from_ancestors(root);
            assert!(
                repeated.is_empty(),
                "{tab:?}: named as a parent {repeated:?} in {tree:?}"
            );
            assert_eq!(
                tree.read(tree.find("tag-0")),
                ("Label", "Tag: synthetic-tag")
            );
            assert_eq!(
                tree.read(tree.find("tag-remove-0")),
                ("Button", "Remove tag synthetic-tag")
            );
            let list = match tab {
                MeetingTab::Transcript => Some(("transcript", "Transcript lines")),
                MeetingTab::Speakers => Some(("speaker-list", "Meeting speakers")),
                _ => None,
            };
            if let Some((id, name)) = list {
                let list = tree.find(id);
                assert_eq!(tree.read(list), ("List", name));
                assert!(
                    std::iter::successors(tree.parent(list), |&a| tree.parent(a))
                        .any(|a| a == panel),
                    "{tree:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod lane_tests {
    use super::*;
    use crate::ui::a11y_tree::draw_and_read;
    use crate::ui::kit::dialog_gate::{click, draw};
    use gpui::{TestAppContext, VisualTestContext};

    use super::tests as fixture;

    fn focused(vcx: &mut VisualTestContext, id: &str) -> bool {
        vcx.update(|window, cx| {
            window.draw(cx).clear(cx);
            gpui::base::test_support::snapshots(window)
                .iter()
                .any(|node| {
                    node.path()
                        .last()
                        .is_some_and(|part| part.to_string() == id)
                        && node.focused() == Some(true)
                })
        })
    }

    #[gpui::test]
    fn tag_draft_and_per_control_feedback_survive_competing_and_refused_writes(
        cx: &mut TestAppContext,
    ) {
        let (window, _, lib, synthetic, _dir) = fixture::open_edited(cx, |meeting, _| {
            meeting.tags = vec!["Planning".into(), "Release".into()];
            meeting.enrichment = Some(
                serde_json::from_value(json!({
                    "provider": "agent-a",
                    "created_at": "2026-11-15T00:00:00Z",
                    "actions": [{"id":"a1","text":"Send synthetic notes","at_s":10.0}]
                }))
                .unwrap(),
            );
        });
        window
            .update(cx, |view, window, cx| {
                view.open_tag(window, cx);
                view.tag_input.update(cx, |input, cx| {
                    input.set_value("  newly typed, another  ", window, cx)
                });
                view.busy = Some((EditTarget::Title, "Wait until the title is saved."));
                view.submit_tag(window, cx);
                assert_eq!(
                    view.edit_status.get(&EditTarget::Tags),
                    Some(&Notice::Error("Wait until the title is saved.".into()))
                );
                assert_eq!(
                    view.tag_input.read(cx).value().as_str(),
                    "  newly typed, another  "
                );
            })
            .unwrap();
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("meeting-title-status")),
            ("Status", "Saving…")
        );
        assert_eq!(
            tree.read(tree.find("meeting-tags-status")),
            ("Alert", "Wait until the title is saved.")
        );
        assert!(tree.all("busy").is_empty());
        assert!(tree.all("notice").is_empty());
        assert!(
            tree.nodes
                .iter()
                .any(|node| node.name == "Remove tag Planning"
                    && node.actions.contains(&"Click".to_owned()))
        );

        window
            .update(&mut visual, |view, window, cx| {
                view.busy = None;
                view.submit_tag(window, cx);
                view.tag_input.update(cx, |input, cx| {
                    input.set_value("text typed while saving", window, cx)
                });
            })
            .unwrap();
        visual.run_until_parked();
        assert!(
            lib.load_meeting(&synthetic.meeting)
                .unwrap()
                .tags
                .contains(&"newly typed".into())
        );
        window
            .update(&mut visual, |view, window, cx| {
                assert_eq!(
                    view.tag_input.read(cx).value().as_str(),
                    "text typed while saving"
                );
                assert!(view.tag_input.read(cx).focus_handle(cx).is_focused(window));
                assert_eq!(
                    view.edit_status.get(&EditTarget::Tags),
                    Some(&Notice::Info("Tags saved.".into()))
                );
                view.set_tab(MeetingTab::Overview, cx);
            })
            .unwrap();
        draw(&mut visual);
        window
            .update(&mut visual, |view, window, cx| {
                view.set_action_status("a1".into(), ActionStatus::Done, window, cx);
            })
            .unwrap();
        visual.run_until_parked();
        draw(&mut visual);
        window
            .update(&mut visual, |view, _, _| {
                assert_eq!(
                    view.edit_status.get(&EditTarget::Action("a1".into())),
                    Some(&Notice::Info("Marked done.".into()))
                );
                assert!(view.edit_status.contains_key(&EditTarget::Tags));
            })
            .unwrap();

        // The removed chip's successor receives keyboard focus.
        let first = lib
            .load_meeting(&synthetic.meeting)
            .unwrap()
            .effective_tags()[0]
            .clone();
        let next = lib
            .load_meeting(&synthetic.meeting)
            .unwrap()
            .effective_tags()[1]
            .clone();
        window
            .update(&mut visual, |view, window, cx| {
                view.tag_focus[&first].focus(window, cx);
                view.remove_tag(first, window, cx);
            })
            .unwrap();
        visual.run_until_parked();
        draw(&mut visual);
        window
            .update(&mut visual, |view, window, _| {
                assert!(view.tag_focus[&next].is_focused(window))
            })
            .unwrap();

        // A durable refusal leaves the same draft, rather than just a busy refusal.
        std::fs::write(
            lib.meeting_dir(&synthetic.meeting)
                .unwrap()
                .join("meeting.json"),
            b"invalid json",
        )
        .unwrap();
        window
            .update(&mut visual, |view, window, cx| view.submit_tag(window, cx))
            .unwrap();
        visual.run_until_parked();
        window.update(&mut visual, |view, _, cx| {
            assert_eq!(view.tag_input.read(cx).value().as_str(), "text typed while saving");
            assert!(matches!(view.edit_status.get(&EditTarget::Tags), Some(Notice::Error(text)) if text.starts_with("Couldn't add the tag.")));
        }).unwrap();
    }

    #[gpui::test]
    fn row_commands_open_rename_and_tag_with_keyboard_cancellation(cx: &mut TestAppContext) {
        let (window, _, lib, synthetic, _dir) = fixture::open_edited(cx, |_, _| {});
        let mut reference = lib.load_meeting(&synthetic.meeting).unwrap();
        reference.id = MeetingId::random();
        reference.tags = vec!["Release".into()];
        lib.write(|tx| {
            tx.create_meeting_dir(&reference)?;
            tx.save_meeting(&reference)?;
            broscribe_core::index::Index::rebuild(tx)?;
            Ok(())
        })
        .unwrap();
        window
            .update(cx, |view, window, cx| view.reload(window, cx))
            .unwrap();
        cx.run_until_parked();
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        draw(&mut visual);
        let center = fixture::meeting_control_bounds(&mut visual, "meeting-title").center();
        // GPUI's desktop gesture dispatch uses the mouse-down click count.
        visual.update(|window, cx| {
            window.dispatch_event(
                gpui::PlatformInput::MouseDown(gpui::MouseDownEvent {
                    button: gpui::MouseButton::Left,
                    position: center,
                    modifiers: gpui::Modifiers::none(),
                    click_count: 2,
                    first_mouse: false,
                }),
                cx,
            );
        });
        draw(&mut visual);
        window
            .update(&mut visual, |view, window, cx| {
                let input = &view
                    .rename
                    .as_ref()
                    .expect("double-click opens the editor")
                    .0;
                assert!(input.read(cx).focus_handle(cx).is_focused(window));
                input.update(cx, |input, cx| {
                    input.set_value("Synthetic draft", window, cx)
                });
            })
            .unwrap();
        draw(&mut visual);
        window
            .update(&mut visual, |view, window, cx| {
                view.rename_save_focus.focus(window, cx);
            })
            .unwrap();
        assert!(focused(&mut visual, "rename-save"));
        window
            .update(&mut visual, |view, _, cx| {
                view.busy = Some((EditTarget::Title, "Wait until the title is saved."));
                cx.notify();
            })
            .unwrap();
        assert!(focused(&mut visual, "rename-save"));
        visual.simulate_keystrokes("escape");
        window
            .update(&mut visual, |view, window, cx| {
                assert!(view.rename.is_none());
                assert!(view.title_focus.is_focused(window));
                view.busy = None;
                view.open_library_command(crate::views::library::RowCommand::AddTag, window, cx);
                assert!(view.tag_open);
                view.tag_input
                    .update(cx, |input, cx| input.set_value("PLANNING, re", window, cx));
                assert_eq!(view.tag_suggestions(cx), ["Release"]);
            })
            .unwrap();
        draw(&mut visual);
        visual.simulate_keystrokes("escape");
        window
            .update(&mut visual, |view, window, cx| {
                assert!(!view.tag_open);
                assert!(view.add_tag_focus.is_focused(window));
                view.open_library_command(crate::views::library::RowCommand::Rename, window, cx);
                assert!(view.rename.is_some());
            })
            .unwrap();
    }

    #[gpui::test]
    fn meeting_history_replays_tags_and_rejects_a_stale_edit(cx: &mut TestAppContext) {
        let (window, _, lib, synthetic, _dir) = fixture::open_edited(cx, |_, _| {});
        window
            .update(cx, |view, window, cx| {
                view.add_tags(vec!["Outline".to_owned()], window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        let tags = || {
            lib.load_meeting(&synthetic.meeting)
                .unwrap()
                .effective_tags()
        };
        assert!(tags().iter().any(|tag| tag == "Outline"));
        window
            .update(cx, |view, window, cx| {
                assert_eq!(view.menu_state().unwrap().undo, Some("Add Tag"));
                view.replay_edit(false, window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert!(!tags().iter().any(|tag| tag == "Outline"));
        window
            .update(cx, |view, window, cx| {
                assert_eq!(view.menu_state().unwrap().redo, Some("Add Tag"));
                view.replay_edit(true, window, cx);
            })
            .unwrap();
        cx.run_until_parked();
        assert!(tags().iter().any(|tag| tag == "Outline"));
        edit_tags(&lib, &synthetic.meeting, &["Independent".to_owned()], &[]).unwrap();
        window
            .update(cx, |view, window, cx| view.replay_edit(false, window, cx))
            .unwrap();
        cx.run_until_parked();
        assert!(tags().iter().any(|tag| tag == "Outline"));
        assert!(tags().iter().any(|tag| tag == "Independent"));
        assert!(
            broscribe_core::index::Index::open(lib.root())
                .unwrap()
                .meeting_tags()
                .unwrap()
                .iter()
                .any(|(tag, _)| tag == "Independent")
        );
        window
            .update(cx, |view, _, _| {
                assert_eq!(
                    view.edit_status.get(&EditTarget::Tags),
                    Some(&Notice::Error(
                        "This changed since. Nothing was undone.".into()
                    ))
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn export_modal_cancel_and_success_restore_the_export_opener(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (app, _, synthetic) = crate::views::person_picker::fixtures::state_with_cli(
            cx,
            dir.path(),
            fixture::synthetic_cli(dir.path()),
        );
        fixture::reply(dir.path(), json!({ "sections":["transcript"] }));
        app.update(cx, |state, cx| {
            state.navigate(Route::meeting(synthetic.meeting), cx)
        });
        let (main, window) = fixture::meeting_test_shell(cx, app, gpui::size(px(1500.), px(650.)));
        let meeting = main.read_with(cx, |shell, _| shell.meeting_view().unwrap().clone());
        let mut visual = VisualTestContext::from_window(window, cx);
        click(&mut visual, "export-open", Role::Button);
        let tree = visual.update(draw_and_read);
        assert!(
            tree.nodes
                .iter()
                .any(|node| node.role == "Dialog" && node.name == "Export meeting")
        );
        assert_eq!(
            tree.read(tree.find("export-meeting-description")).0,
            "Label"
        );
        assert!(focused(&mut visual, "export-section-0"));
        visual.simulate_keystrokes("escape");
        assert!(focused(&mut visual, "export-open"));
        assert!(!visual.update(|_, cx| meeting.read(cx).export_form().is_some()));

        click(&mut visual, "export-open", Role::Button);
        let form = visual.update(|_, cx| meeting.read(cx).export_form().unwrap().clone());
        visual.update(|_, cx| form.update(cx, |form, cx| form.toggle_section(0, cx)));
        let tree = visual.update(draw_and_read);
        assert!(tree.all("export-local-note").is_empty());
        assert_eq!(
            tree.read(tree.find("export-no-sections")).1,
            "Choose at least one section."
        );
        visual.update(|_, cx| {
            form.update(cx, |form, cx| {
                form.toggle_section(0, cx);
                form.export(cx);
            })
        });
        let out = dir.path().join("Synthetic export.md");
        cx.simulate_new_path_selection(move |_| Some(out));
        cx.run_until_parked();
        draw(&mut visual);
        assert!(focused(&mut visual, "export-open"));
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("meeting-exported")),
            ("Status", "Exported “Synthetic export.md”.")
        );
        assert_eq!(tree.read(tree.find("show-export")).0, "Button");
        assert!(tree.all("export-form").is_empty());
    }

    #[gpui::test]
    fn deletion_rechecks_without_deleting_and_selects_the_next_library_row(
        cx: &mut TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (app, lib, synthetic) = crate::views::person_picker::fixtures::state_with_cli(
            cx,
            dir.path(),
            fixture::synthetic_cli(dir.path()),
        );
        let mut older = lib.load_meeting(&synthetic.meeting).unwrap();
        older.id = MeetingId::random();
        older.title = Some("Second synthetic meeting".into());
        older.started_at -= chrono::TimeDelta::days(1);
        lib.write(|tx| {
            tx.create_meeting_dir(&older)?;
            tx.save_meeting(&older)?;
            broscribe_core::index::Index::rebuild(tx)?;
            Ok(())
        })
        .unwrap();
        app.update(cx, |state, cx| {
            state.navigate(Route::meeting(synthetic.meeting.clone()), cx)
        });
        fixture::reply(
            dir.path(),
            json!({"error":{"code":"job_running","message":"Synthetic job running"}}),
        );
        let (main, window) =
            fixture::meeting_test_shell(cx, app.clone(), gpui::size(px(1100.), px(650.)));
        let meeting = main.read_with(cx, |shell, _| shell.meeting_view().unwrap().clone());
        let mut visual = VisualTestContext::from_window(window, cx);
        visual.update(|window, cx| {
            meeting.update(cx, |view, cx| {
                view.open_library_command(crate::views::library::RowCommand::Delete, window, cx)
            })
        });
        visual.run_until_parked();
        assert_eq!(
            visual
                .update(draw_and_read)
                .nodes
                .iter()
                .find(|node| node.element_id.as_deref() == Some("delete-recovery"))
                .unwrap()
                .name,
            "Check again"
        );
        fixture::reply(dir.path(), json!({"state":"planned","dry_run":true}));
        click(&mut visual, "delete-recovery", Role::Button);
        let command = std::fs::read_to_string(dir.path().join("args.log")).unwrap();
        assert_eq!(
            command
                .lines()
                .filter(|line| line.ends_with("--dry-run"))
                .count(),
            2
        );
        assert!(!command.contains("--yes"));
        let tree = visual.update(draw_and_read);
        assert_eq!(tree.read(tree.find("what-goes")), ("Heading", "What goes"));
        assert_eq!(
            tree.read(tree.find("what-stays")),
            ("Heading", "What stays")
        );

        fixture::reply(
            dir.path(),
            json!({"state":"cleanup_pending","cleanup_failures":["synthetic"],"dry_run":false}),
        );
        click(&mut visual, "delete-meeting-confirm", Role::Button);
        assert_eq!(
            app.read_with(cx, |state, _| state.route().selected_meeting().cloned()),
            Some(older.id.clone())
        );
        // ACC-07: focus lands on the list, on the next row, not nowhere.
        visual.run_until_parked();
        visual.update(|window, cx| {
            let library = main.read(cx).library_view().clone();
            assert!(library.focus_handle(cx).contains_focused(window, cx));
            assert_eq!(library.read(cx).selected_meeting(), Some(&older.id));
        });
        let tree = visual.update(draw_and_read);
        assert!(
            tree.read(tree.find("library-deleted-meeting"))
                .1
                .contains("1 leftover file couldn't be removed")
        );
    }

    #[gpui::test]
    fn overview_tabs_markers_and_idle_position_restore_for_the_session(cx: &mut TestAppContext) {
        use broscribe_core::schema::{Bookmark, Note};
        let dir = tempfile::tempdir().unwrap();
        let (app, lib, synthetic) = crate::views::person_picker::fixtures::state_with_cli(
            cx,
            dir.path(),
            fixture::listing_cli(dir.path()),
        );
        let mut meeting = lib.load_meeting(&synthetic.meeting).unwrap();
        meeting.bookmarks = vec![Bookmark {
            at_s: 2.,
            label: Some("Open question".into()),
        }];
        meeting.notes = vec![Note {
            at_s: 7.,
            text: "Read synthetic test".into(),
        }];
        meeting
            .segments
            .extend((10..100).map(|id| broscribe_core::schema::Segment {
                speech_overlap: Vec::new(),
                id,
                track: "t0".into(),
                start: f64::from(id) * 5.,
                end: f64::from(id) * 5. + 2.,
                text: format!("Invented line {id}"),
                speaker: Some(SpeakerId::new(id % 2 + 1).unwrap()),
                words: Vec::new(),
            }));
        meeting.duration_s = 510.;
        meeting.set_review_notes(ReviewSource::LivePreview, ["Check the notes".to_owned()]);
        lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
        app.update(cx, |state, cx| {
            state.navigate(Route::meeting(synthetic.meeting.clone()), cx)
        });
        let first = cx.add_window({
            let app = app.clone();
            let id = synthetic.meeting.clone();
            move |window, cx| MeetingView::new(app, id, window, cx)
        });
        cx.run_until_parked();
        let mut visual = VisualTestContext::from_window(first.into(), cx);
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("meeting-tabs")),
            ("TabList", "Meeting sections")
        );
        assert_eq!(
            tree.read(tree.find("meeting-tab-panel")),
            ("TabPanel", "Overview")
        );
        assert_eq!(
            tree.read(tree.find("overview-review-note-0")),
            ("Note", "Check the notes")
        );
        assert!(tree.all("overview-new-review-notes").is_empty());
        assert!(
            tree.nodes
                .iter()
                .any(|node| node.role == "Tab" && node.name.starts_with("Speakers,"))
        );
        assert_eq!(
            tree.nodes.iter().filter(|node| node.role == "Tab").count(),
            4
        );
        assert_eq!(
            tree.read(tree.find("overview-bookmarks")).1,
            "Bookmarks and notes (2)"
        );

        meeting.set_review_notes(
            ReviewSource::LivePreview,
            [
                "Check the notes".to_owned(),
                "New synthetic review note".to_owned(),
            ],
        );
        lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
        first
            .update(&mut visual, |view, window, cx| view.reload(window, cx))
            .unwrap();
        visual.run_until_parked();
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("overview-new-review-notes-1")),
            ("Status", "1 new review note")
        );
        first
            .update(&mut visual, |view, _, cx| {
                view.set_tab(MeetingTab::Transcript, cx);
                view.set_tab(MeetingTab::Overview, cx);
            })
            .unwrap();
        assert!(
            visual
                .update(draw_and_read)
                .all("overview-new-review-notes-1")
                .is_empty()
        );

        first
            .update(&mut visual, |view, window, cx| {
                let keys = crate::ui::keymap::key_bindings_for(
                    broscribe_platform::HostPlatform::Windows,
                    cx,
                );
                cx.bind_keys(keys);
                view.detail_focus.focus(window, cx);
            })
            .unwrap();
        visual.simulate_keystrokes("ctrl-tab");
        let tree = visual.update(draw_and_read);
        assert_eq!(
            tree.read(tree.find("meeting-tab-panel")),
            ("TabPanel", "Transcript")
        );
        assert_eq!(
            tree.read(tree.find("marker-0")),
            ("Note", "Bookmark · 2 seconds · Open question")
        );
        first
            .update(&mut visual, |view, window, cx| {
                assert_eq!(
                    view.transcript_entries[0],
                    saved_transcript::Entry::Marker(0)
                );
                assert_eq!(
                    view.transcript_entries[2],
                    saved_transcript::Entry::Marker(1)
                );
                view.select_transcript_row(0, window, cx);
                view.seek(18., window, cx);
                view.transcript_list.scroll_to(gpui::ListOffset {
                    item_ix: 20,
                    offset_in_item: px(0.),
                });
            })
            .unwrap();
        // The header sits on the desk and the transcript in a card below it:
        // the card's edge lines up with the title, the lines sit inside it.
        let title = fixture::meeting_control_bounds(&mut visual, "meeting-title");
        let card = fixture::meeting_control_bounds(&mut visual, "meeting-card");
        let transcript = fixture::meeting_control_bounds(&mut visual, "transcript");
        assert_eq!(card.left(), title.left());
        assert!(transcript.left() > card.left());
        draw(&mut visual);
        first
            .update(&mut visual, |view, window, _| {
                assert_eq!(view.transcript_list.logical_scroll_top().item_ix, 20);
                window.remove_window();
            })
            .unwrap();
        visual.run_until_parked();
        let reopened =
            cx.add_window(move |window, cx| MeetingView::new(app, synthetic.meeting, window, cx));
        cx.run_until_parked();
        reopened
            .update(cx, |view, _, _| {
                assert_eq!(view.tab, MeetingTab::Transcript);
                assert_eq!(view.playback.position_s, 18.);
                assert!(!view.playback.playing);
                assert_eq!(view.transcript_list.logical_scroll_top().item_ix, 20);
            })
            .unwrap();
    }

    #[test]
    fn meeting_menu_tracks_availability_and_playback() {
        let entries = |state| {
            native_menu(state)
                .into_iter()
                .filter_map(|item| match item {
                    gpui::MenuItem::Action { name, disabled, .. } => {
                        Some((name.to_string(), disabled))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert!(entries(None).iter().all(|(_, disabled)| *disabled));
        let entries = entries(Some(MeetingMenuState {
            find_matches: false,
            audio: true,
            playing: true,
            has_lines: true,
            summary: true,
            undo: None,
            redo: None,
        }));
        assert_eq!(entries[3], ("Pause".into(), false));
        assert!(entries.contains(&("Regenerate Summary…".into(), false)));
        assert!(entries.contains(&("Show Recording in Finder".into(), false)));
        assert_eq!(entries.last().unwrap(), &("Delete Meeting".into(), false));
        assert!(
            entries
                .iter()
                .all(|(name, _)| !name.starts_with("Export") && !name.starts_with("Find"))
        );
    }
}
