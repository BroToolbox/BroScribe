//! Recording retention policy (design §7.2, plan §9.10): whether a meeting's
//! recording is due for deletion. Pure: no files, no clock; the pipeline's
//! `retention` module applies the verdicts.
//!
//! Only recordings the library owns (`copy` or `move` handling) are ever
//! deleted; a referenced file is never touched. A meeting is considered only
//! once `processing.status = done` and while no queue job works on it.

use chrono::{DateTime, FixedOffset, TimeDelta};
use serde::Serialize;

use crate::SpeakerId;
use crate::config::{Config, Retention};
use crate::schema::{Handling, Meeting, ProcessingStatus, RecordingState, RetentionOverride};

/// How long a "Can't tell" dismissal waits before `wait_until_named`
/// counts it as settled: longer than the app's Undo window, so the
/// daemon's retention pass never deletes a recording while the dismissal
/// can still be undone.
pub const DISMISS_GRACE: TimeDelta = TimeDelta::minutes(10);

/// Folder under the application cache directory holding playback caches.
/// Each cache file's name starts with the recording's SHA-256, so retention
/// can delete a meeting's caches along with its recording.
pub const PLAYBACK_CACHE_DIR: &str = "playback";

/// What retention decides for one meeting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", content = "reason", rename_all = "snake_case")]
pub enum Verdict {
    /// Delete the recording now.
    Delete(DeleteReason),
    /// Leave it.
    Keep(KeepReason),
}

impl Verdict {
    pub fn is_delete(self) -> bool {
        matches!(self, Verdict::Delete(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteReason {
    /// The user asked for this meeting (`retention delete-now`).
    Override,
    /// `delete_immediately`.
    Immediately,
    /// `delete_after_days`, and the time has come.
    AfterDays,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeepReason {
    NoRecording,
    /// Left in place: never deleted by BroScribe.
    Reference,
    /// Already deleted, or missing.
    NotAvailable,
    /// Transcript and identity pass not finished.
    NotProcessed,
    /// A queue job is working on the meeting.
    LiveJobs,
    /// The user asked to keep this recording.
    Override,
    /// `keep_forever`.
    Policy,
    /// `delete_immediately` with `wait_until_named`, and a speaker is
    /// neither named nor dismissed from review (past [`DISMISS_GRACE`]).
    WaitingForNames,
    /// `delete_after_days`, not due yet (see [`delete_after`]).
    NotYetDue,
}

/// A shown speaker holding a `wait_until_named` recording: neither named
/// nor dismissed from review past [`DISMISS_GRACE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct BlockingSpeaker {
    /// The speaker as shown (a group by its kept speaker).
    pub speaker_id: SpeakerId,
    /// When the shown speaker was dismissed ("Can't tell") while still
    /// inside [`DISMISS_GRACE`]: it settles at `dismissed_at +
    /// DISMISS_GRACE`. `None` when it is waiting for a name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dismissed_at: Option<DateTime<FixedOffset>>,
}

/// The retention verdict for one meeting with what decided it, for the
/// app, the CLI and diagnostics.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RetentionStatus {
    #[serde(flatten)]
    pub verdict: Verdict,
    /// When `delete_after_days` deletes the recording ([`delete_after`]).
    /// `None` under `delete_after_days` with a deletable recording means
    /// the date lies past the calendar's range: it is never due.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_at: Option<DateTime<FixedOffset>>,
    /// The shown speakers [`KeepReason::WaitingForNames`] waits for, in
    /// meeting order; empty for every other verdict.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocking_speakers: Vec<BlockingSpeaker>,
}

/// The retention verdict for `meeting` at `now`. `has_live_jobs`: whether
/// a queue job (queued, running, or holding a live lease) refers to it.
pub fn eligible(
    meeting: &Meeting,
    config: &Config,
    now: DateTime<FixedOffset>,
    has_live_jobs: bool,
) -> Verdict {
    explain(meeting, config, now, has_live_jobs).verdict
}

/// [`eligible`] with its reasons: the due date under `delete_after_days`
/// and the shown speakers `wait_until_named` waits for.
pub fn explain(
    meeting: &Meeting,
    config: &Config,
    now: DateTime<FixedOffset>,
    has_live_jobs: bool,
) -> RetentionStatus {
    let due_at = delete_after(meeting, config);
    let status = |verdict: Verdict, blocking_speakers: Vec<BlockingSpeaker>| RetentionStatus {
        verdict,
        due_at,
        blocking_speakers,
    };
    let keep = |reason: KeepReason| status(Verdict::Keep(reason), Vec::new());
    let Some(rec) = meeting.recording.as_ref() else {
        return keep(KeepReason::NoRecording);
    };
    if rec.handling == Handling::Reference {
        return keep(KeepReason::Reference);
    }
    if rec.state != RecordingState::Available {
        return keep(KeepReason::NotAvailable);
    }
    if meeting.processing.status != ProcessingStatus::Done {
        return keep(KeepReason::NotProcessed);
    }
    if has_live_jobs {
        return keep(KeepReason::LiveJobs);
    }
    match rec.retention_override {
        Some(RetentionOverride::Keep) => return keep(KeepReason::Override),
        Some(RetentionOverride::DeleteNow) => {
            return status(Verdict::Delete(DeleteReason::Override), Vec::new());
        }
        None => {}
    }
    match config.retention {
        Retention::KeepForever => keep(KeepReason::Policy),
        Retention::DeleteImmediately { wait_until_named } => {
            let blocking = if wait_until_named {
                unsettled_speakers(meeting, now)
            } else {
                Vec::new()
            };
            if blocking.is_empty() {
                status(Verdict::Delete(DeleteReason::Immediately), blocking)
            } else {
                status(Verdict::Keep(KeepReason::WaitingForNames), blocking)
            }
        }
        Retention::DeleteAfterDays { .. } => match due_at {
            Some(due) if now >= due => status(Verdict::Delete(DeleteReason::AfterDays), Vec::new()),
            _ => keep(KeepReason::NotYetDue),
        },
    }
}

/// The shown speakers of `meeting` not settled for `wait_until_named`: a
/// shown speaker (a group counts once, by its kept speaker: a member's own
/// frozen assignment is hidden) is settled when named (`confirmed` or
/// `auto`), or dismissed from review ("Can't tell",
/// [`crate::ops::review_dismissed_at`], a group by its latest dismissal) at
/// least [`DISMISS_GRACE`] before `now`.
fn unsettled_speakers(meeting: &Meeting, now: DateTime<FixedOffset>) -> Vec<BlockingSpeaker> {
    crate::ops::effective_speakers(meeting)
        .filter(|s| s.named_person().is_none())
        .filter_map(|s| {
            let dismissed_at = crate::ops::review_dismissed_at(meeting, s.id);
            let settled =
                dismissed_at.is_some_and(|at| now.signed_duration_since(at) >= DISMISS_GRACE);
            (!settled).then_some(BlockingSpeaker {
                speaker_id: s.id,
                dismissed_at,
            })
        })
        .collect()
}

/// When `delete_after_days` will delete the meeting's recording (counted from
/// `started_at`), for `recording.delete_after`; `None` under other policies,
/// with a `keep` override, when nothing is deletable, or when the date lies
/// past the calendar's range (never due).
pub fn delete_after(meeting: &Meeting, config: &Config) -> Option<DateTime<FixedOffset>> {
    let rec = meeting.recording.as_ref()?;
    if rec.handling == Handling::Reference
        || rec.state != RecordingState::Available
        || rec.retention_override == Some(RetentionOverride::Keep)
    {
        return None;
    }
    match config.retention {
        Retention::DeleteAfterDays { days } => meeting
            .started_at
            .checked_add_signed(TimeDelta::try_days(i64::from(days))?),
        Retention::KeepForever | Retention::DeleteImmediately { .. } => None,
    }
}

/// Whether a playback cache file named `file_name` belongs to the recording
/// with this SHA-256 (see [`PLAYBACK_CACHE_DIR`]).
pub fn is_playback_cache_of(file_name: &str, sha256: &str) -> bool {
    !sha256.is_empty() && file_name.starts_with(sha256)
}

/// Why a check of a proposed policy (`retention apply --dry-run --policy`)
/// failed, in words with no path, meeting or person ID (SET-01). The CLI
/// sends [`Self::text`] as its error message beside the error's own machine
/// code; the app shows that cause only when it is one of these texts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreviewFailure {
    /// The data folder (settings, the mode lock) could not be used.
    DataFolderUnreadable,
    /// `config.toml` could not be read.
    SettingsUnreadable,
    /// The library folder is not there.
    LibraryMissing,
    /// A folder or file of the library could not be read.
    LibraryUnreadable,
    /// A file of the library does not parse.
    DamagedFile,
    /// Another process holds the library.
    LibraryBusy,
    /// Anything else.
    Failed,
}

impl PreviewFailure {
    pub const ALL: [Self; 7] = [
        Self::DataFolderUnreadable,
        Self::SettingsUnreadable,
        Self::LibraryMissing,
        Self::LibraryUnreadable,
        Self::DamagedFile,
        Self::LibraryBusy,
        Self::Failed,
    ];

    /// The cause as a capitalised clause without a final full stop, for
    /// "Couldn't check which recordings this deletes. {cause}. Nothing
    /// changed."
    pub fn text(self) -> &'static str {
        match self {
            Self::DataFolderUnreadable => "BroScribe's data folder is unreadable",
            Self::SettingsUnreadable => "Settings file is unreadable",
            Self::LibraryMissing => "Library folder wasn't found",
            Self::LibraryUnreadable => "Library folder is unreadable",
            Self::DamagedFile => "A file in the library folder is damaged",
            Self::LibraryBusy => "Another task is using the library",
            Self::Failed => "The check failed",
        }
    }

    /// The failure whose [`Self::text`] is `text`.
    pub fn from_text(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|failure| failure.text() == text)
    }

    /// Classifies an error of the check by its machine code; `settings`:
    /// it came from reading `config.toml`.
    pub fn from_code(code: &str, settings: bool) -> Self {
        match code {
            _ if settings => Self::SettingsUnreadable,
            "not_found" => Self::LibraryMissing,
            "io" => Self::LibraryUnreadable,
            "schema" | "decode" => Self::DamagedFile,
            "library_busy" => Self::LibraryBusy,
            _ => Self::Failed,
        }
    }

    /// Classifies an error met before the check ran, while resolving the
    /// data folder or taking its admission lock, by its machine code.
    pub fn from_data_folder_code(code: &str) -> Self {
        match code {
            "io" | "not_found" => Self::DataFolderUnreadable,
            _ => Self::Failed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Assignment;
    use crate::schema::tests::sample_meeting;
    use crate::{PersonId, SpeakerId};

    fn ts(s: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(s).unwrap()
    }

    /// The sample meeting (2026-11-13 14:00 -03:00, copied recording), done.
    fn done() -> Meeting {
        let mut m = sample_meeting();
        m.processing.status = ProcessingStatus::Done;
        m
    }

    fn config(retention: Retention) -> Config {
        let mut config = Config::default();
        config.retention = retention;
        config
    }

    const NOW: &str = "2026-11-14T09:00:00-03:00";

    #[test]
    fn keep_forever_keeps_unless_delete_now() {
        let c = config(Retention::KeepForever);
        let mut m = done();
        assert_eq!(
            eligible(&m, &c, ts(NOW), false),
            Verdict::Keep(KeepReason::Policy)
        );
        assert_eq!(delete_after(&m, &c), None);
        m.recording.as_mut().unwrap().retention_override = Some(RetentionOverride::DeleteNow);
        assert_eq!(
            eligible(&m, &c, ts(NOW), false),
            Verdict::Delete(DeleteReason::Override)
        );
    }

    #[test]
    fn delete_immediately_and_wait_until_named() {
        let mut m = done();
        let now = ts(NOW);
        let plain = config(Retention::DeleteImmediately {
            wait_until_named: false,
        });
        // S2 is only suggested: immediate deletion does not wait.
        assert_eq!(
            eligible(&m, &plain, now, false),
            Verdict::Delete(DeleteReason::Immediately)
        );
        let wait = config(Retention::DeleteImmediately {
            wait_until_named: true,
        });
        assert_eq!(
            eligible(&m, &wait, now, false),
            Verdict::Keep(KeepReason::WaitingForNames)
        );
        // Naming S2 (auto counts as named) makes it due.
        let s2 = m.speaker_mut(SpeakerId::new(2).unwrap()).unwrap();
        s2.person_id = Some(PersonId::parse("p-b2b2b2").unwrap());
        s2.assignment = Assignment::Auto;
        assert_eq!(
            eligible(&m, &wait, now, false),
            Verdict::Delete(DeleteReason::Immediately)
        );
        // A user "keep" wins over the policy.
        m.recording.as_mut().unwrap().retention_override = Some(RetentionOverride::Keep);
        assert_eq!(
            eligible(&m, &wait, now, false),
            Verdict::Keep(KeepReason::Override)
        );
    }

    /// Group used by the effective-speaker tests: S2 grouped under S1.
    fn grouped(m: &mut Meeting) {
        let (s1, s2) = (SpeakerId::new(1).unwrap(), SpeakerId::new(2).unwrap());
        m.speaker_groups = vec![crate::schema::SpeakerGroup::new(
            s1,
            [s2],
            crate::schema::GroupOrigin::User,
        )];
    }

    #[test]
    fn wait_until_named_follows_the_shown_speakers_not_frozen_members() {
        let wait = config(Retention::DeleteImmediately {
            wait_until_named: true,
        });
        let now = ts(NOW);
        let (s1, s2) = (SpeakerId::new(1).unwrap(), SpeakerId::new(2).unwrap());
        // S1 confirmed as Ana; S2 only suggested, never dismissed.
        let mut m = done();
        let before = explain(&m, &wait, now, false);
        assert_eq!(before.verdict, Verdict::Keep(KeepReason::WaitingForNames));
        assert_eq!(
            before.blocking_speakers,
            [BlockingSpeaker {
                speaker_id: s2,
                dismissed_at: None
            }]
        );
        // Grouped under named S1, S2 is shown as Ana: its frozen own
        // assignment no longer holds the recording.
        grouped(&mut m);
        let named = explain(&m, &wait, now, false);
        assert_eq!(named.verdict, Verdict::Delete(DeleteReason::Immediately));
        assert!(named.blocking_speakers.is_empty());
        assert_eq!(
            eligible(&m, &wait, now, false),
            Verdict::Delete(DeleteReason::Immediately)
        );
        // The reverse: a named member under an unnamed kept speaker shows
        // the kept speaker's state, so the group still waits, as S1.
        let mut m = done();
        let ana = m.speaker(s1).unwrap().person_id.clone();
        m.speaker_mut(s1).unwrap().assignment = Assignment::Suggested;
        let member = m.speaker_mut(s2).unwrap();
        member.person_id = ana;
        member.assignment = Assignment::Confirmed;
        grouped(&mut m);
        let waiting = explain(&m, &wait, now, false);
        assert_eq!(waiting.verdict, Verdict::Keep(KeepReason::WaitingForNames));
        assert_eq!(
            waiting.blocking_speakers,
            [BlockingSpeaker {
                speaker_id: s1,
                dismissed_at: None
            }]
        );
        // A dismissal of the whole group inside the grace is reported with
        // its time, then settles.
        let at = now - TimeDelta::minutes(3);
        m.speaker_mut(s1).unwrap().review_dismissed_at = Some(at);
        m.speaker_mut(s2).unwrap().review_dismissed_at = Some(at);
        let grace = explain(&m, &wait, now, false);
        assert_eq!(grace.verdict, Verdict::Keep(KeepReason::WaitingForNames));
        assert_eq!(grace.blocking_speakers[0].dismissed_at, Some(at));
        let v = serde_json::to_value(&grace).unwrap();
        assert_eq!(v["verdict"], "keep");
        assert_eq!(v["reason"], "waiting_for_names");
        assert_eq!(v["blocking_speakers"][0]["speaker_id"], "S1");
        assert!(v.get("due_at").is_none());
        assert!(eligible(&m, &wait, at + DISMISS_GRACE, false).is_delete());
    }

    #[test]
    fn huge_delete_after_days_is_never_due_and_never_panics() {
        let m = done();
        // Past `TimeDelta`'s range, then past the calendar's.
        for days in [u32::MAX, 100_000_000] {
            let c = config(Retention::DeleteAfterDays { days });
            assert_eq!(delete_after(&m, &c), None, "{days}");
            let status = explain(&m, &c, ts(NOW), false);
            assert_eq!(status.verdict, Verdict::Keep(KeepReason::NotYetDue));
            assert_eq!(status.due_at, None);
        }
        // A long but representable delay is reported as is.
        let c = config(Retention::DeleteAfterDays { days: 36_500 });
        let status = explain(&m, &c, ts(NOW), false);
        assert_eq!(status.verdict, Verdict::Keep(KeepReason::NotYetDue));
        assert_eq!(
            status.due_at,
            Some(ts("2026-11-13T14:00:00-03:00") + TimeDelta::days(36_500))
        );
        let v = serde_json::to_value(&status).unwrap();
        assert_eq!(v["reason"], "not_yet_due");
        assert!(v["due_at"].is_string());
    }

    #[test]
    fn wait_until_named_settles_a_dismissal_after_the_grace() {
        let mut m = done();
        let now = ts(NOW);
        let wait = config(Retention::DeleteImmediately {
            wait_until_named: true,
        });
        let (s1, s2) = (SpeakerId::new(1).unwrap(), SpeakerId::new(2).unwrap());
        assert!(m.speaker(s1).unwrap().named_person().is_some());
        // S2, only suggested, dismissed ("Can't tell") just now: within
        // the grace (the Undo window), still waiting.
        m.speaker_mut(s2).unwrap().review_dismissed_at = Some(now - TimeDelta::minutes(1));
        assert_eq!(
            eligible(&m, &wait, now, false),
            Verdict::Keep(KeepReason::WaitingForNames)
        );
        // Past the grace: settled.
        m.speaker_mut(s2).unwrap().review_dismissed_at = Some(now - DISMISS_GRACE);
        assert_eq!(
            eligible(&m, &wait, now, false),
            Verdict::Delete(DeleteReason::Immediately)
        );
        // Grouped under an unflagged kept speaker, S2 is not dismissed.
        m.speaker_groups = vec![crate::schema::SpeakerGroup::new(
            s1,
            [s2],
            crate::schema::GroupOrigin::User,
        )];
        m.speaker_mut(s1).unwrap().assignment = Assignment::Suggested;
        assert_eq!(
            eligible(&m, &wait, now, false),
            Verdict::Keep(KeepReason::WaitingForNames)
        );
        // The whole group dismissed: its latest dismissal counts.
        m.speaker_mut(s1).unwrap().review_dismissed_at = Some(now - TimeDelta::minutes(2));
        assert_eq!(
            eligible(&m, &wait, now, false),
            Verdict::Keep(KeepReason::WaitingForNames)
        );
        m.speaker_mut(s1).unwrap().review_dismissed_at = Some(now - DISMISS_GRACE);
        assert_eq!(
            eligible(&m, &wait, now, false),
            Verdict::Delete(DeleteReason::Immediately)
        );
    }

    #[test]
    fn delete_after_days_counts_from_the_start() {
        let c = config(Retention::DeleteAfterDays { days: 30 });
        let m = done();
        let due = ts("2026-12-13T14:00:00-03:00");
        assert_eq!(delete_after(&m, &c), Some(due));
        assert_eq!(
            eligible(&m, &c, ts("2026-12-13T13:59:59-03:00"), false),
            Verdict::Keep(KeepReason::NotYetDue)
        );
        assert_eq!(
            eligible(&m, &c, due, false),
            Verdict::Delete(DeleteReason::AfterDays)
        );
        let zero = config(Retention::DeleteAfterDays { days: 0 });
        assert!(eligible(&m, &zero, ts(NOW), false).is_delete());
    }

    #[test]
    fn reference_is_never_deleted() {
        let mut m = done();
        let rec = m.recording.as_mut().unwrap();
        rec.handling = Handling::Reference;
        rec.retention_override = Some(RetentionOverride::DeleteNow);
        for r in [
            Retention::DeleteImmediately {
                wait_until_named: false,
            },
            Retention::DeleteAfterDays { days: 0 },
        ] {
            let c = config(r);
            assert_eq!(
                eligible(&m, &c, ts(NOW), false),
                Verdict::Keep(KeepReason::Reference)
            );
            assert_eq!(delete_after(&m, &c), None);
        }
    }

    #[test]
    fn live_jobs_unfinished_and_gone_recordings_are_kept() {
        let c = config(Retention::DeleteImmediately {
            wait_until_named: false,
        });
        let now = ts(NOW);
        let mut m = done();
        assert_eq!(
            eligible(&m, &c, now, true),
            Verdict::Keep(KeepReason::LiveJobs)
        );
        m.recording.as_mut().unwrap().retention_override = Some(RetentionOverride::DeleteNow);
        assert_eq!(
            eligible(&m, &c, now, true),
            Verdict::Keep(KeepReason::LiveJobs)
        );
        m.processing.status = ProcessingStatus::Analyzed;
        assert_eq!(
            eligible(&m, &c, now, false),
            Verdict::Keep(KeepReason::NotProcessed)
        );
        m.processing.status = ProcessingStatus::Done;
        m.recording.as_mut().unwrap().state = RecordingState::Deleted;
        assert_eq!(
            eligible(&m, &c, now, false),
            Verdict::Keep(KeepReason::NotAvailable)
        );
        m.recording = None;
        assert_eq!(
            eligible(&m, &c, now, false),
            Verdict::Keep(KeepReason::NoRecording)
        );
    }

    #[test]
    fn playback_cache_names() {
        let sha = "ab".repeat(32);
        assert!(is_playback_cache_of(&format!("{sha}.wav"), &sha));
        assert!(is_playback_cache_of(&format!("{sha}-t0.caf"), &sha));
        assert!(!is_playback_cache_of("other.wav", &sha));
        assert!(!is_playback_cache_of("x.wav", ""));
    }

    #[test]
    fn preview_failures_are_path_free_capitalised_and_round_trip() {
        for failure in PreviewFailure::ALL {
            let text = failure.text();
            assert_eq!(PreviewFailure::from_text(text), Some(failure));
            assert!(text.starts_with(|c: char| c.is_ascii_uppercase()), "{text}");
            assert!(!text.ends_with('.'), "{text}");
            assert!(!text.contains(['/', '\\', ':']), "{text}");
        }
        assert_eq!(PreviewFailure::from_text("/synthetic/library: no"), None);
        assert_eq!(
            PreviewFailure::from_code("io", false),
            PreviewFailure::LibraryUnreadable
        );
        assert_eq!(
            PreviewFailure::from_code("schema", false),
            PreviewFailure::DamagedFile
        );
        assert_eq!(
            PreviewFailure::from_code("io", true),
            PreviewFailure::SettingsUnreadable
        );
        assert_eq!(
            PreviewFailure::from_code("internal", false),
            PreviewFailure::Failed
        );
        assert_eq!(
            PreviewFailure::from_data_folder_code("io"),
            PreviewFailure::DataFolderUnreadable
        );
        assert_eq!(
            PreviewFailure::from_data_folder_code("invalid_input"),
            PreviewFailure::Failed
        );
    }
}
