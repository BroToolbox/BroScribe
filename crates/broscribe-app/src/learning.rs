//! The background speaker recheck as the app shows it (design §4 "Review
//! queue"). Naming, correcting or forgetting someone requests it in core
//! (`broscribe_core::learning`); the agent's queue (or the app's own `queue
//! work` child) runs it, the meeting named in first, then the rest of the
//! library. These helpers read its status off the main thread and turn it
//! into text: what is still being checked, then the committed counts, the
//! meeting named in first. Counts and meeting titles only, never people's
//! names. Nothing here runs the recheck or names anybody.

use broscribe_core::learning::{self, AffectedMeeting, LearningRequest, LearningStatus};
use broscribe_core::queue::JobState;
use broscribe_core::{Error, Library, MeetingId, Result};
use gpui::component::button::ButtonVariants as _;
use gpui::component::{ActiveTheme as _, Sizable as _};
use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, Context, Div, Entity, FontWeight, InteractiveElement as _, ParentElement as _, Role,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, Window, div,
};

use crate::state::{AppState, Route};
use crate::ui::kit::{self, StatusKind, button};
use crate::ui::text_line;
use crate::ui::type_scale::{TextRole, TextRoleExt as _};

/// Affected meetings linked from a result (the rest are counted only).
pub const LINKED_MEETINGS: usize = 8;

/// What the app read about the recheck of one model.
#[derive(Clone, Debug, Default)]
pub struct Recheck {
    /// `None` when none was ever requested.
    pub status: Option<LearningStatus>,
    /// Titles of the first [`LINKED_MEETINGS`] meetings of its report (the
    /// meetings named in first), for links; deleted meetings are left out.
    pub titles: Vec<(MeetingId, String)>,
}

impl Recheck {
    pub fn title(&self, id: &MeetingId) -> Option<&str> {
        self.titles
            .iter()
            .find(|(m, _)| m == id)
            .map(|(_, t)| t.as_str())
    }
}

/// Reads the recheck of `model` and the titles its report links to. Reads
/// files only, takes no lock. Call it off the main thread.
pub fn read_recheck(lib: &Library, model: &str) -> Result<Recheck> {
    let status = learning::status(lib, model)?;
    let mut titles = Vec::new();
    if let Some(report) = status.as_ref().and_then(|s| s.report.as_ref()) {
        for m in ordered(&report.meetings).take(LINKED_MEETINGS) {
            match lib.load_meeting(&m.meeting_id) {
                Ok(meeting) => titles.push((
                    m.meeting_id.clone(),
                    format!(
                        "{} ({})",
                        meeting.title.as_deref().unwrap_or("Untitled meeting"),
                        crate::ui::format::local_date(&meeting.started_at)
                    ),
                )),
                Err(Error::NotFound(_)) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(Recheck { status, titles })
}

/// The report's meetings, the ones named in first, then in report order.
fn ordered(meetings: &[AffectedMeeting]) -> impl Iterator<Item = &AffectedMeeting> {
    meetings
        .iter()
        .filter(|m| m.source)
        .chain(meetings.iter().filter(|m| !m.source))
}

/// Summed counts of `meetings`, zeros left out: "2 named automatically,
/// 1 replaced, 1 returned to review, 3 suggestions"; `None` when nothing
/// changed.
pub fn counts_text<'a>(meetings: impl IntoIterator<Item = &'a AffectedMeeting>) -> Option<String> {
    let (mut named, mut replaced, mut demoted, mut suggested, mut cleared) = (0, 0, 0, 0, 0);
    for m in meetings {
        named += m.newly_named;
        replaced += m.replaced;
        demoted += m.demoted;
        suggested += m.suggested;
        cleared += m.cleared;
    }
    let parts: Vec<String> = [
        (named, format!("{named} named automatically")),
        (replaced, format!("{replaced} replaced")),
        (demoted, format!("{demoted} returned to review")),
        (
            suggested,
            crate::ui::format::count(suggested as u64, "suggestion", "suggestions"),
        ),
        (
            cleared,
            crate::ui::format::count(
                cleared as u64,
                "suggestion withdrawn",
                "suggestions withdrawn",
            ),
        ),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(_, text)| text)
    .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// Which meeting a recheck text puts first.
#[derive(Clone, Copy, Debug)]
pub struct Focus<'a> {
    pub meeting: &'a MeetingId,
    /// "This meeting", or the meeting's title.
    pub label: &'a str,
}

/// A recheck as a view shows it: how it ended ([`recheck_outcome`]),
/// [`recheck_lines`] and the [`affected_links`] (meeting, label) a view
/// turns into buttons.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecheckText {
    pub outcome: Option<&'static str>,
    pub lines: Vec<String>,
    pub links: Vec<(MeetingId, String)>,
}

/// What a recheck that finished says, once.
pub const FINISHED: &str = "Speaker recheck finished.";
/// What a recheck says, once, when its pass is over but meetings other jobs
/// still hold wait for a retry.
pub const WAITING: &str = "Speaker recheck waiting for other jobs.";

/// How the recheck this app asked for (`request`, its newest request)
/// ended: [`FINISHED`], or [`WAITING`] while meetings other jobs hold are
/// retried later. `None` while it still checks (the jobs footer names it
/// live then), after a failure (the footer's Alert says it), and unless the
/// last complete pass covered exactly the generation asked here: every
/// recheck of a model reuses one job, so a later request made elsewhere
/// (the CLI) that finished is not this app's to announce; its counts still
/// show as non-live lines. Fixed words, never counts, so a view can show it
/// as a live Status that speaks once when the recheck ends and never on a
/// redraw.
pub fn recheck_outcome(status: &LearningStatus, request: &LearningRequest) -> Option<&'static str> {
    if status.job_id != request.job_id
        || status.processed != request.generation
        || status.state == JobState::Failed
        || pending(status, None)
    {
        return None;
    }
    Some(if status.deferred.is_empty() {
        FINISHED
    } else {
        WAITING
    })
}

/// A recheck box's text as assistive technology gets it, top to bottom,
/// each node identified under `prefix` and named by its words: how the
/// recheck ended (`outcome`) as one polite Status, so a user who heard the
/// footer's "Checking other speakers" hears it end (ACC-01); every progress
/// or count line a non-live Label, since they change with each committed
/// meeting (ACC-18); and a failed newer read (`failure`) its own Alert. The
/// meeting's Speakers tab and the review queue both show it.
pub(crate) fn recheck_nodes(
    prefix: &'static str,
    outcome: Option<&'static str>,
    lines: Vec<String>,
    failure: Option<String>,
    cx: &App,
) -> Vec<Stateful<Div>> {
    let id = |part: &str| SharedString::from(format!("{prefix}-{part}"));
    let outcome = outcome.map(|outcome| {
        text_line(id("outcome"), Role::Status, outcome).text_role(TextRole::Caption)
    });
    let lines = lines
        .into_iter()
        .enumerate()
        .map(|(ix, line)| text_line((id("line"), ix), Role::Label, line).text_role(TextRole::Body));
    let failure = failure.map(|failure| {
        crate::ui::kit::status_line(id("error"), crate::ui::kit::StatusKind::Failed, failure, cx)
    });
    outcome.into_iter().chain(lines).chain(failure).collect()
}

/// Shared status block for the action that requested a speaker recheck.
pub(crate) fn render_recheck<V: 'static>(
    prefix: &'static str,
    text: RecheckText,
    failure: Option<String>,
    phase: StatusKind,
    state: &Entity<AppState>,
    dismiss: impl Fn(&mut V, &mut Window, &mut Context<V>) + 'static,
    cx: &Context<V>,
) -> Stateful<Div> {
    let theme = cx.theme();
    let links = text
        .links
        .into_iter()
        .enumerate()
        .map(|(index, (id, title))| {
            let state = state.clone();
            button(
                (SharedString::from(format!("{prefix}-open")), index),
                format!("Open meeting · {title}"),
                true,
                cx,
                move |_, _, cx| {
                    state.update(cx, |state, cx| {
                        state.navigate(Route::meeting(id.clone()), cx)
                    })
                },
            )
            .link()
            .small()
            .accessibility_label(format!("Open meeting “{title}”"))
        });
    let nodes = recheck_nodes(prefix, text.outcome, text.lines, failure, cx);
    // Counts are Labels so progress stays quiet. Only the outcome and read
    // failure have announcement roles.
    div()
        .id(prefix)
        .role(Role::Group)
        .aria_label("Speaker recheck")
        .flex()
        .flex_col()
        .gap_1()
        .p_3()
        .rounded(theme.radius_lg)
        .border_1()
        .border_color(theme.border)
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child("Speaker recheck"),
        )
        .children(nodes.into_iter().enumerate().map(|(index, line)| {
            div()
                .flex()
                .items_center()
                .gap_1()
                .when(index == 0, |row| {
                    row.child(kit::glyph(phase.glyph_path(), theme.muted_foreground))
                })
                .child(line)
        }))
        .child(
            div().flex().flex_wrap().gap_2().children(links).child(
                button(
                    SharedString::from(format!("{prefix}-hide")),
                    "Dismiss",
                    true,
                    cx,
                    dismiss,
                )
                .ghost()
                .small()
                .accessibility_label("Dismiss the speaker recheck's result"),
            ),
        )
}

pub(crate) fn recheck_phase(status: Option<&LearningStatus>, since: Option<u64>) -> StatusKind {
    match status {
        Some(status) if status.state == JobState::Failed => StatusKind::Failed,
        Some(status) if !pending(status, since) => StatusKind::Done,
        _ => StatusKind::Pending,
    }
}

/// Whether the recheck still has work for a request of generation `since`
/// (or the newest request when `None`).
pub fn pending(status: &LearningStatus, since: Option<u64>) -> bool {
    status.processed < since.unwrap_or(0).max(status.requested)
}

/// A failed read of the speaker recheck, `cause` in words: with lines still
/// shown (`stale_shown`), says they are its last known state.
pub fn recheck_failure(cause: &str, stale_shown: bool) -> String {
    if stale_shown {
        crate::jobs::failure_text(
            "Couldn't update the speaker recheck.",
            cause,
            "The lines above are its last known state.",
        )
    } else {
        crate::jobs::failure_text("Couldn't read the speaker recheck.", cause, "")
    }
}

/// The lines describing the recheck: still checking (with the worker's
/// `progress` text), then the committed counts of the focused meeting (or
/// of the meetings named in), then of the other meetings, then the
/// meetings waiting because they are busy. Counts older than `since` (the
/// generation of a request made here) are never shown as its result.
pub fn recheck_lines(
    status: &LearningStatus,
    focus: Option<Focus<'_>>,
    since: Option<u64>,
    progress: Option<&str>,
) -> Vec<String> {
    let pending = pending(status, since);
    let mut lines = Vec::new();
    if status.state == JobState::Failed {
        let cause = status
            .error
            .as_deref()
            .and_then(crate::jobs::stored_error)
            .map(|error| crate::jobs::core_user_text(&error))
            .unwrap_or_default();
        lines.push(crate::jobs::failure_text(
            "Checking other speakers stopped.",
            &cause,
            "Names you already saved are kept.",
        ));
    } else if pending {
        let mut line = "Checking other speakers".to_owned();
        if focus.is_some_and(|f| status.source_meetings.contains(f.meeting)) {
            line.push_str(", this meeting first");
        }
        match progress {
            Some(p) => {
                line.push_str(": ");
                line.push_str(p);
            }
            None => line.push('…'),
        }
        lines.push(line);
    }
    let report = status
        .report
        .as_ref()
        .filter(|r| since.is_none_or(|g| r.generation >= g));
    match report {
        Some(r) => {
            let (first, others): (Vec<&AffectedMeeting>, Vec<&AffectedMeeting>) = match focus {
                Some(f) => r.meetings.iter().partition(|m| m.meeting_id == *f.meeting),
                None => r.meetings.iter().partition(|m| m.source),
            };
            let so_far = if pending { " so far" } else { "" };
            let label = focus.map_or("Meetings named in", |f| f.label);
            match counts_text(first.iter().copied()) {
                Some(counts) => lines.push(format!("{label}{so_far}: {counts}.")),
                None if !pending && focus.is_some() => {
                    lines.push(format!("{label}: no other speaker changed."));
                }
                None => {}
            }
            match counts_text(others.iter().copied()) {
                Some(counts) => lines.push(format!("Other meetings{so_far}: {counts}.")),
                None if !pending => lines.push("Other meetings: nothing changed.".to_owned()),
                None => {}
            }
            if !r.unreadable.is_empty() {
                lines.push(match r.unreadable.len() {
                    1 => "1 meeting couldn't be read. Its speakers didn't change.".into(),
                    count => {
                        format!("{count} meetings couldn't be read. Their speakers didn't change.")
                    }
                });
            }
        }
        None if !pending && status.state != JobState::Failed => {
            lines.push("Other speakers are up to date.".to_owned());
        }
        None => {}
    }
    if !status.deferred.is_empty() {
        lines.push(format!(
            "{} until their other jobs finish.",
            crate::ui::format::count(
                status.deferred.len() as u64,
                "meeting waits",
                "meetings wait"
            )
        ));
    }
    lines
}

/// Links to the meetings a result changed, the ones named in first: the
/// meeting, its title and its counts ("2 named automatically"). At most
/// [`LINKED_MEETINGS`]; meetings without a read title are left out.
pub fn affected_links(recheck: &Recheck, since: Option<u64>) -> Vec<(MeetingId, String)> {
    let Some(report) = recheck
        .status
        .as_ref()
        .and_then(|s| s.report.as_ref())
        .filter(|r| since.is_none_or(|g| r.generation >= g))
    else {
        return Vec::new();
    };
    ordered(&report.meetings)
        .filter_map(|m| {
            let title = recheck.title(&m.meeting_id)?;
            let counts = counts_text([m])?;
            Some((m.meeting_id.clone(), format!("{title}: {counts}")))
        })
        .take(LINKED_MEETINGS)
        .collect()
}

#[cfg(test)]
mod tests {
    use broscribe_core::learning::{LearningReport, UnreadableMeeting};
    use broscribe_core::queue::JobId;

    use super::*;

    fn id(n: u32) -> MeetingId {
        MeetingId::parse(&format!("m-{n:06x}")).unwrap()
    }

    fn affected(n: u32, source: bool, counts: [usize; 5]) -> AffectedMeeting {
        let [newly_named, replaced, demoted, suggested, cleared] = counts;
        AffectedMeeting {
            meeting_id: id(n),
            source,
            newly_named,
            replaced,
            demoted,
            suggested,
            cleared,
            changes: Vec::new(),
        }
    }

    fn status(
        requested: u64,
        processed: u64,
        report: Option<(u64, Vec<AffectedMeeting>)>,
    ) -> LearningStatus {
        LearningStatus {
            job_id: JobId::parse("j-0123456789abcdef").unwrap(),
            model: "synthetic-model".into(),
            state: if processed < requested {
                JobState::Queued
            } else {
                JobState::Done
            },
            running: false,
            requested,
            processed,
            requested_source: None,
            source_meetings: if processed < requested {
                vec![id(1)]
            } else {
                Vec::new()
            },
            deferred: Vec::new(),
            not_before: None,
            error: None,
            report: report.map(|(generation, meetings)| {
                let sum = |f: fn(&AffectedMeeting) -> usize| meetings.iter().map(f).sum();
                LearningReport {
                    generation,
                    newly_named: sum(|m| m.newly_named),
                    replaced: sum(|m| m.replaced),
                    demoted: sum(|m| m.demoted),
                    suggested: sum(|m| m.suggested),
                    cleared: sum(|m| m.cleared),
                    deferred: 0,
                    unreadable: Vec::new(),
                    meetings,
                }
            }),
        }
    }

    #[test]
    fn persisted_recheck_links_keep_the_same_date_for_the_same_instant() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path()).unwrap();
        let mut meeting: broscribe_core::schema::Meeting =
            serde_json::from_value(serde_json::json!({
                "schema_version": 2, "id": "m-000001", "source": "import",
                "title": "Synthetic recheck", "started_at": "2026-01-01T12:00:00Z",
                "created_at": "2026-01-01T12:00:00Z", "updated_at": "2026-01-01T12:00:00Z",
                "tracks": [], "speakers": [], "segments": []
            }))
            .unwrap();
        lib.write(|tx| {
            tx.create_meeting_dir(&meeting)?;
            tx.save_meeting(&meeting)
        })
        .unwrap();
        let report = status(1, 1, Some((1, vec![affected(1, true, [1, 0, 0, 0, 0])])))
            .report
            .unwrap();
        lib.write(|tx| {
            broscribe_core::queue::Queue::open(tx.root())?.enqueue(
                tx,
                learning::REIDENTIFY_KIND,
                "synthetic-model",
                serde_json::json!({
                    "model": "synthetic-model", "requested": 1, "processed": 1,
                    "report": report
                }),
            )?;
            Ok(())
        })
        .unwrap();
        let first = affected_links(&read_recheck(&lib, "synthetic-model").unwrap(), Some(1));
        meeting.started_at =
            chrono::DateTime::parse_from_rfc3339("2026-01-02T02:00:00+14:00").unwrap();
        lib.write(|tx| tx.save_meeting(&meeting)).unwrap();
        let second = affected_links(&read_recheck(&lib, "synthetic-model").unwrap(), Some(1));
        assert_eq!(first.len(), 1);
        assert!(first[0].1.contains("Synthetic recheck"));
        assert_eq!(
            first, second,
            "the stored offset must not change a recheck link's date"
        );
    }

    #[test]
    fn counts_distinguish_new_names_replacements_demotions_and_suggestions() {
        let m = [
            affected(1, true, [1, 0, 0, 0, 0]),
            affected(2, false, [1, 1, 1, 2, 1]),
        ];
        assert_eq!(
            counts_text(&m).as_deref(),
            Some(
                "2 named automatically, 1 replaced, 1 returned to review, 2 suggestions, \
                 1 suggestion withdrawn"
            )
        );
        assert_eq!(counts_text(&[affected(3, false, [0; 5])]), None);
    }

    /// Right after a naming, the previous run's counts are not presented as
    /// its result: only "checking", this meeting first.
    #[test]
    fn a_new_request_hides_older_counts_and_says_this_meeting_comes_first() {
        let old = status(3, 2, Some((2, vec![affected(9, false, [4, 0, 0, 0, 0])])));
        let focus = Focus {
            meeting: &id(1),
            label: "This meeting",
        };
        assert_eq!(
            recheck_lines(&old, Some(focus), Some(3), None),
            vec!["Checking other speakers, this meeting first…".to_owned()]
        );
        // Read before the request reached the file: still pending for it.
        let stale = status(2, 2, Some((2, Vec::new())));
        assert!(pending(&stale, Some(3)));
        assert_eq!(
            recheck_lines(&stale, Some(focus), Some(3), Some("1 of 4 meetings")),
            vec!["Checking other speakers: 1 of 4 meetings".to_owned()]
        );
    }

    #[test]
    fn the_focused_meeting_comes_first_then_other_meetings_then_waiting_ones() {
        let mut s = status(
            3,
            3,
            Some((
                3,
                vec![
                    affected(2, false, [1, 0, 1, 0, 0]),
                    affected(1, true, [1, 0, 0, 0, 0]),
                ],
            )),
        );
        s.deferred = vec![id(5), id(6)];
        let focus = Focus {
            meeting: &id(1),
            label: "This meeting",
        };
        assert_eq!(
            recheck_lines(&s, Some(focus), Some(3), None),
            vec![
                "This meeting: 1 named automatically.".to_owned(),
                "Other meetings: 1 named automatically, 1 returned to review.".to_owned(),
                "2 meetings wait until their other jobs finish.".to_owned(),
            ]
        );
        // Without a focus, the meetings named in come first.
        assert_eq!(
            recheck_lines(&s, None, None, None)[0],
            "Meetings named in: 1 named automatically."
        );
        let quiet = status(1, 1, Some((1, vec![affected(1, true, [0; 5])])));
        assert_eq!(
            recheck_lines(&quiet, Some(focus), None, None),
            vec![
                "This meeting: no other speaker changed.".to_owned(),
                "Other meetings: nothing changed.".to_owned(),
            ]
        );
        let mut unreadable = status(1, 1, Some((1, Vec::new())));
        let report = unreadable.report.as_mut().unwrap();
        for n in [7, 8] {
            report.unreadable.push(UnreadableMeeting {
                meeting_id: id(n),
                code: "schema".into(),
            });
        }
        assert_eq!(
            recheck_lines(&unreadable, None, None, None),
            vec![
                "Other meetings: nothing changed.".to_owned(),
                "2 meetings couldn't be read. Their speakers didn't change.".to_owned(),
            ]
        );
    }

    #[test]
    fn links_list_changed_meetings_named_in_first() {
        let recheck = Recheck {
            status: Some(status(
                1,
                1,
                Some((
                    1,
                    vec![
                        affected(2, false, [0, 1, 0, 0, 0]),
                        affected(3, false, [0; 5]),
                        affected(1, true, [1, 0, 0, 0, 0]),
                    ],
                )),
            )),
            titles: vec![
                (id(1), "Synthetic sync (2026-11-02)".into()),
                (id(2), "Synthetic review (2026-11-01)".into()),
                (id(3), "Synthetic quiet (2026-10-30)".into()),
            ],
        };
        assert_eq!(
            affected_links(&recheck, Some(1)),
            vec![
                (
                    id(1),
                    "Synthetic sync (2026-11-02): 1 named automatically".into()
                ),
                (id(2), "Synthetic review (2026-11-01): 1 replaced".into()),
            ]
        );
        assert!(affected_links(&recheck, Some(2)).is_empty());
    }

    /// The end is said in fixed words, only once the request asked here is
    /// the one processed: never while checking, never after a failure (the
    /// footer's Alert says that), never for a later request made elsewhere
    /// on the same job or for another model's job, and as waiting while
    /// meetings other jobs hold stay.
    #[test]
    fn a_recheck_says_how_it_ended_only_once_its_request_is_processed() {
        let asked = |generation| LearningRequest {
            job_id: JobId::parse("j-0123456789abcdef").unwrap(),
            generation,
            source_meeting: None,
        };
        let done = status(2, 2, Some((2, Vec::new())));
        assert_eq!(recheck_outcome(&done, &asked(2)), Some(FINISHED));
        // A newer request made here, not read back yet: still checking.
        assert_eq!(recheck_outcome(&done, &asked(3)), None);
        assert_eq!(recheck_outcome(&status(3, 2, None), &asked(3)), None);
        // This app's request was processed, then one made elsewhere waits.
        assert_eq!(recheck_outcome(&status(3, 2, None), &asked(2)), None);
        // A later request made elsewhere finished: not this app's to say.
        assert_eq!(recheck_outcome(&done, &asked(1)), None);
        let other = LearningRequest {
            job_id: JobId::parse("j-fedcba9876543210").unwrap(),
            ..asked(2)
        };
        assert_eq!(recheck_outcome(&done, &other), None);
        let mut waiting = done.clone();
        waiting.deferred = vec![id(4)];
        assert_eq!(recheck_outcome(&waiting, &asked(2)), Some(WAITING));
        let mut failed = done;
        failed.state = JobState::Failed;
        assert_eq!(recheck_outcome(&failed, &asked(2)), None);
    }
}
