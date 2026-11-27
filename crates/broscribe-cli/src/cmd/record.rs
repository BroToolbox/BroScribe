//! Recording controls always belong to the capture agent.
use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::{
    Ctx,
    cli::{ParticipantArg, RecordCmd, RecordingAnswer, SessionArg},
    query::{checked_title, read_stdin_title, read_stdin_whole},
};
use broscribe_core::{Error, Result};
use broscribe_ipc::live::{LiveAction, LiveAttributeParams, LiveIdentitySettings, LiveSnapshot};
use broscribe_ipc::recording_words::{JOURNAL_LIBRARY_UNKNOWN, journal_library_issue};
use broscribe_ipc::{
    AutoStopStatus, Call, Event, IpcError, JournalResolution, NotificationRequest,
    ParticipantAction, RecordAutoStopParams, RecordStartParams, RequestId, StoppedRecording,
    StoppedRecordingState,
};
use serde::Deserialize;
use serde_json::{Value, json};

fn daemon_unavailable() -> Error {
    Error::DaemonUnavailable("BroScribe Agent isn't running; run `broscribe daemon start`".into())
}

/// `session`, or the active recording's session ID.
fn session_or_active(data_dir: &Path, session: Option<String>) -> Result<String> {
    if let Some(session) = session {
        return Ok(session);
    }
    let state = super::daemon::optional_call(data_dir, Call::RecordStatus)?
        .ok_or_else(daemon_unavailable)?;
    state["recording"]["session_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| {
            Error::InvalidInput("no active recording; name a stopped one with --session".into())
        })
}

/// The session a recording command is fenced to: `--session`, or the
/// recording active now (resolved before sending). The agent refuses it
/// with `stale_session` if another session replaced it meanwhile, so the
/// command never lands on a newer recording.
fn fenced_session(data_dir: &Path, session: SessionArg) -> Result<String> {
    if let Some(session) = session.session {
        return Ok(session);
    }
    let state = super::daemon::optional_call(data_dir, Call::RecordStatus)?
        .ok_or_else(daemon_unavailable)?;
    state["recording"]["session_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::NotFound("no recording is running".into()))
}

/// `record.live` for `session` (default: the active recording): the live
/// preview’s turns as the agent holds them for the CLI.
fn live_snapshot(data_dir: &Path, session: Option<String>) -> Result<Value> {
    let session_id = session_or_active(data_dir, session)?;
    super::daemon::optional_call(data_dir, Call::RecordLive { session_id })?
        .ok_or_else(daemon_unavailable)
}

/// A person reference read whole from stdin, trimmed and non-empty.
fn read_stdin_ref(command: &str) -> Result<String> {
    let text = read_stdin_whole(command)?;
    let text = text.trim();
    if text.is_empty() {
        return Err(Error::InvalidInput(format!("{command}: no name on stdin")));
    }
    Ok(text.to_owned())
}

/// `record.auto_stop` for the active recording, fenced by the binding
/// generation read just before (a change in between is refused with
/// `stale_binding`, nothing changed). An agent whose state has no
/// `auto_stop` does not support it: refused, never approximated.
fn auto_stop_call(
    ctx: &Ctx<'_>,
    session: Option<String>,
    enabled: bool,
    app_id: Option<String>,
) -> Result<Call> {
    let state = crate::mcp::recording_call(ctx, Call::RecordStatus)?;
    let recording = &state["recording"];
    let active = recording["session_id"]
        .as_str()
        .ok_or_else(|| Error::InvalidInput("no active recording".into()))?;
    if let Some(session) = session.as_deref()
        && session != active
    {
        return Err(Error::Service {
            code: "stale_session",
            message: format!(
                "{session} is not the active recording; automatic stop applies to the active \
                 recording only"
            ),
        });
    }
    let status = match recording.get("auto_stop") {
        Some(status) if !status.is_null() => {
            serde_json::from_value::<AutoStopStatus>(status.clone())
                .map_err(|e| Error::Internal(format!("auto_stop status: {e}")))?
        }
        _ => {
            return Err(Error::Unsupported(
                "Automatic stop isn't available with this version of BroScribe Agent; update and restart it".into(),
            ));
        }
    };
    Ok(Call::RecordAutoStop(RecordAutoStopParams {
        session_id: active.to_owned(),
        request_id: RequestId::random(),
        expected_generation: status.generation,
        enabled,
        app_id,
    }))
}

// ---- retry ----

/// Whether stopped recording `session` (from the agent's `status`) may be
/// retried: only one whose processing failed. An unknown or saved session
/// is `stale_session`, any other state `not_retryable`; nothing is sent.
fn retryable(state: &Value, session: &str) -> Result<()> {
    let stopped: Vec<StoppedRecording> = match state.get("stopped") {
        None | Some(Value::Null) => Vec::new(),
        Some(list) => serde_json::from_value(list.clone())
            .map_err(|e| Error::Internal(format!("BroScribe Agent’s stopped recordings: {e}")))?,
    };
    match stopped.iter().find(|r| r.session_id == session) {
        Some(StoppedRecording {
            state: StoppedRecordingState::Failed { .. },
            ..
        }) => Ok(()),
        Some(_) => Err(Error::Service {
            code: IpcError::NOT_RETRYABLE,
            message: format!(
                "recording {session} has not failed; it is still being saved or discarded"
            ),
        }),
        None => Err(Error::Service {
            code: "stale_session",
            message: format!(
                "{session} is not a stopped recording waiting to be saved (already saved, \
                 discarded or unknown)"
            ),
        }),
    }
}

/// `record retry --session <id>`: runs the final job of a stopped
/// recording whose processing failed again, with fresh attempts. Only that
/// session, never the active one by default; the agent decides again under
/// its lock (`not_retryable`, `stale_session`) and reconciles a lost reply
/// by the request ID, so the job is never queued twice.
pub(crate) fn retry(ctx: &mut Ctx<'_>, session: String) -> Result<()> {
    let state = crate::mcp::recording_call(ctx, Call::Status)?;
    retryable(&state, &session)?;
    let value = crate::mcp::recording_call(
        ctx,
        Call::RecordRetry {
            session_id: session,
            request_id: RequestId::random(),
        },
    )?;
    super::daemon::emit(ctx, value)
}

fn resolve_journal(
    ctx: &mut Ctx<'_>,
    session: String,
    resolution: JournalResolution,
    yes: bool,
    dry_run: bool,
) -> Result<()> {
    if !yes && !dry_run {
        return Err(Error::InvalidInput(
            "confirm with --yes or preview with --dry-run".into(),
        ));
    }
    let state = crate::mcp::recording_call(ctx, Call::Status)?;
    let code = state["journal_issues"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|issue| issue["session_id"].as_str() == Some(&session))
        .and_then(|issue| issue["code"].as_str());
    match code {
        Some(JOURNAL_LIBRARY_UNKNOWN) => {}
        Some(code) if journal_library_issue(code).is_some() => {
            return Err(Error::Service {
                code: broscribe_ipc::recording_words::JOURNAL_LIBRARY_MISMATCH,
                message: journal_library_issue(code).unwrap_or_default().into(),
            });
        }
        _ => {
            return Err(Error::InvalidInput(format!(
                "{session} is not a recording with an unknown library"
            )));
        }
    }
    let library_root = state["library"]
        .as_str()
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| Error::Internal("BroScribe Agent did not report its open library".into()))?;
    if let Some(requested) = ctx
        .library_flag
        .as_ref()
        .or(ctx.paths.library_from_env.as_ref())
    {
        crate::mcp::check_library_path(requested, &library_root)?;
    }
    if dry_run {
        let preview = json!({
            "dry_run": true, "session_id": session, "resolution": resolution, "library": library_root,
        });
        return ctx.emit(preview, |out| {
            match resolution {
                JournalResolution::Adopt => {
                    writeln!(out, "Would save {session} in {}.", library_root.display())
                }
                JournalResolution::Discard => {
                    writeln!(out, "Would permanently discard the journal for {session}.")
                }
            }?;
            Ok(())
        });
    }
    let value = crate::mcp::recording_call(
        ctx,
        Call::RecordResolveJournal {
            session_id: session,
            request_id: RequestId::random(),
            resolution,
            library_root,
            confirmed: yes,
        },
    )?;
    ctx.emit(value, |out| {
        match resolution {
            JournalResolution::Adopt => writeln!(out, "Recording adopted into the open library."),
            JournalResolution::Discard => writeln!(out, "Recording journal discard requested."),
        }?;
        Ok(())
    })
}

// ---- live follow ----

/// Lines buffered between the agent's event stream and stdout. A slow
/// reader blocks the stream thread; the agent then drops events for this
/// subscriber and says so (`lagged`), answered by a fresh snapshot.
const FOLLOW_BUFFER: usize = 64;

/// What one `record live --follow` decided for an agent event.
#[derive(Debug, PartialEq)]
enum Step {
    /// Nothing to write.
    Skip,
    Line(Value),
    /// Events were dropped: write a fresh snapshot.
    Resnapshot,
    End(&'static str),
}

/// The state of one followed session: only its own events, each turn
/// never written at or below a revision already shown while the agent
/// still holds that turn. Bounded like the agent's own preview: the latest
/// `window` turns, the oldest shown dropped first, as the agent drops them.
struct Follow {
    session_id: String,
    /// The revision shown per turn ID, oldest shown first.
    shown: VecDeque<(String, u64)>,
    /// Turns the agent keeps (`LiveIdentitySettings::max_turns`), or more
    /// when one of its snapshots held more.
    window: usize,
}

impl Follow {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            shown: VecDeque::new(),
            window: LiveIdentitySettings::default().max_turns,
        }
    }

    /// The `snapshot` line of `record.live`'s answer; turn revisions start
    /// again from it. A snapshot of another session ends the follow.
    fn snapshot(&mut self, value: Value) -> Result<Step> {
        let snapshot = LiveSnapshot::deserialize(&value)
            .map_err(|e| Error::Internal(format!("live snapshot: {e}")))?;
        if snapshot.session_id != self.session_id {
            return Ok(Step::End("stale_session"));
        }
        self.window = self.window.max(snapshot.turns.len());
        self.shown.clear();
        self.shown
            .extend(snapshot.turns.into_iter().map(|t| (t.turn_id, t.revision)));
        let mut line = json!({"type": "snapshot"});
        if let (Some(line), Value::Object(fields)) = (line.as_object_mut(), value) {
            line.extend(fields);
        }
        Ok(Step::Line(line))
    }

    fn on_event(&mut self, event: Event) -> Step {
        let session = &self.session_id;
        match event {
            Event::State(state) => match &state.recording {
                Some(r) if r.session_id == *session => Step::Skip,
                _ => Step::End("session_ended"),
            },
            Event::LiveTurn(turn) => {
                if turn.session_id != *session {
                    return Step::Skip;
                }
                let known = self.shown.iter_mut().find(|(id, _)| *id == turn.turn_id);
                if known
                    .as_deref()
                    .is_some_and(|&(_, shown)| shown >= turn.revision)
                {
                    return Step::Skip;
                }
                let line = json!({"type": "turn", "session_id": session, "turn": turn});
                match known {
                    Some((_, shown)) => *shown = turn.revision,
                    None => {
                        let turn = *turn;
                        self.shown.push_back((turn.turn_id, turn.revision));
                        if self.shown.len() > self.window {
                            self.shown.pop_front();
                        }
                    }
                }
                Step::Line(line)
            }
            // Every live event names the recording that produced it; a
            // stopped recording's late output is never written as this one.
            Event::LiveIdentity {
                session_id,
                identity,
            } => {
                if session_id != *session {
                    return Step::Skip;
                }
                Step::Line(json!({"type": "identity", "session_id": session, "identity": identity}))
            }
            Event::LiveTranscript {
                session_id,
                segment,
            } => {
                if session_id != *session {
                    return Step::Skip;
                }
                Step::Line(json!({"type": "transcript", "session_id": session, "segment": segment}))
            }
            Event::Lagged { .. } => Step::Resnapshot,
            _ => Step::Skip,
        }
    }
}

fn write_line(out: &mut dyn Write, line: &Value) -> Result<()> {
    let text = serde_json::to_string(line).map_err(|e| Error::Internal(e.to_string()))?;
    writeln!(out, "{text}")
        .and_then(|()| out.flush())
        .map_err(|e| Error::io("writing output", e))
}

/// `record.live` for the followed session as a [`Step`]: a session the
/// agent no longer records ends the follow, a vanished agent too.
fn fetch_snapshot(data_dir: &Path, follow: &mut Follow, ended: &'static str) -> Result<Step> {
    let call = Call::RecordLive {
        session_id: follow.session_id.clone(),
    };
    match super::daemon::optional_call(data_dir, call) {
        Ok(Some(value)) => follow.snapshot(value),
        Ok(None) => Ok(Step::End("agent_unavailable")),
        Err(e) if e.code() == "stale_session" => Ok(Step::End(ended)),
        Err(e) => Err(e),
    }
}

/// When a follow stops by itself.
struct Bounds {
    /// Lines written after the first snapshot.
    max_events: Option<usize>,
    deadline: Option<Instant>,
}

/// Writes `step`, then what each event of `events` makes of it, until the
/// follow ends; returns why. The deadline is checked on `now` before each
/// next event or fresh snapshot, so events still queued once it passed are
/// never written, however fast they keep coming.
fn pump(
    out: &mut dyn Write,
    follow: &mut Follow,
    mut step: Step,
    events: &mpsc::Receiver<Event>,
    bounds: &Bounds,
    now: impl Fn() -> Instant,
    mut resnapshot: impl FnMut(&mut Follow) -> Result<Step>,
) -> Result<&'static str> {
    let mut written = 0usize;
    let mut first = true;
    loop {
        match step {
            Step::End(reason) => return Ok(reason),
            Step::Line(line) => {
                write_line(out, &line)?;
                if !std::mem::take(&mut first) {
                    written += 1;
                }
                if bounds.max_events.is_some_and(|max| written >= max) {
                    return Ok("max_events");
                }
            }
            Step::Resnapshot => {
                if bounds.deadline.is_some_and(|deadline| now() >= deadline) {
                    return Ok("max_seconds");
                }
                step = resnapshot(follow)?;
                continue;
            }
            Step::Skip => {}
        }
        let event = match bounds.deadline {
            Some(deadline) => {
                let at = now();
                if at >= deadline {
                    return Ok("max_seconds");
                }
                match events.recv_timeout(deadline - at) {
                    Ok(event) => event,
                    Err(RecvTimeoutError::Timeout) => return Ok("max_seconds"),
                    Err(RecvTimeoutError::Disconnected) => return Ok("agent_unavailable"),
                }
            }
            None => match events.recv() {
                Ok(event) => event,
                Err(mpsc::RecvError) => return Ok("agent_unavailable"),
            },
        };
        step = follow.on_event(event);
    }
}

/// `record live --follow`: NDJSON of one session, bounded by its end,
/// `max_events` lines after the first snapshot and `max_seconds`.
fn follow_live(
    ctx: &mut Ctx<'_>,
    session: Option<String>,
    max_events: Option<usize>,
    max_seconds: Option<u64>,
) -> Result<()> {
    let data_dir = ctx.data_dir().to_path_buf();
    let session_id = session_or_active(&data_dir, session)?;
    let bounds = Bounds {
        max_events,
        deadline: max_seconds.map(|s| Instant::now() + Duration::from_secs(s)),
    };
    let mut follow = Follow::new(session_id);
    let outcome = (|| {
        // Subscribe before reading the snapshot: a change in between is
        // retained, then deduplicated against the snapshot's revisions.
        let subscription = super::daemon::connect(&data_dir, Duration::from_secs(5))?
            .ok_or_else(daemon_unavailable)?
            .subscribe()
            .map_err(super::daemon::call_error)?;
        let (send, events) = mpsc::sync_channel::<Event>(FOLLOW_BUFFER);
        std::thread::spawn(move || {
            let mut subscription = subscription;
            while let Ok(Some(event)) = subscription.next_event() {
                if send.send(event).is_err() {
                    break;
                }
            }
        });
        let step = fetch_snapshot(&data_dir, &mut follow, "stale_session")?;
        pump(
            ctx.out,
            &mut follow,
            step,
            &events,
            &bounds,
            Instant::now,
            |follow| fetch_snapshot(&data_dir, follow, "session_ended"),
        )
    })();
    let mut end = json!({"type": "end", "session_id": follow.session_id});
    match outcome {
        Ok(reason) => end["reason"] = reason.into(),
        Err(error) => {
            // Follow is NDJSON even without --json. Do not let the outer
            // dispatcher append a human error or a pretty JSON document.
            end["reason"] = "error".into();
            end["error"] = json!({"code": error.code()});
            ctx.exit_code = Some(1);
        }
    }
    write_line(ctx.out, &end)
}

pub(super) fn run(ctx: &mut Ctx<'_>, action: RecordCmd) -> Result<()> {
    let data_dir = ctx.data_dir().to_path_buf();
    let call = match action {
        RecordCmd::Start { app, title, setup } => Call::RecordStart(RecordStartParams {
            app,
            title,
            setup: setup.map(|setup| setup.to_string()),
            ..Default::default()
        }),
        RecordCmd::Stop { session } => Call::RecordStop {
            session_id: fenced_session(&data_dir, session)?,
        },
        RecordCmd::Status => Call::RecordStatus,
        RecordCmd::Prompts => {
            let mut state = crate::mcp::recording_call(ctx, Call::Status)?;
            let mut prompts: Vec<NotificationRequest> =
                serde_json::from_value(state["pending_notifications"].take()).map_err(|_| {
                    Error::Internal("BroScribe Agent returned invalid pending notifications".into())
                })?;
            prompts.retain(NotificationRequest::is_recording_prompt);
            return super::daemon::emit(ctx, json!({"prompts": prompts}));
        }
        RecordCmd::Answer { id, action } => Call::NotificationAction {
            notification_id: id,
            action: match action {
                RecordingAnswer::Record => "record",
                RecordingAnswer::Ignore => "ignore",
            }
            .into(),
        },
        RecordCmd::Retry { session } => return retry(ctx, session),
        RecordCmd::ResolveJournal {
            session,
            resolution,
            yes,
            dry_run,
        } => {
            return resolve_journal(ctx, session, resolution.into(), yes, dry_run);
        }
        RecordCmd::Pause { session } => Call::RecordPause {
            session_id: fenced_session(&data_dir, session)?,
        },
        RecordCmd::Resume { session } => Call::RecordResume {
            session_id: fenced_session(&data_dir, session)?,
        },
        RecordCmd::Lang { session, language } => Call::RecordLanguage {
            session_id: fenced_session(&data_dir, session)?,
            language,
        },
        RecordCmd::Bookmark { session, label } => Call::RecordBookmark {
            session_id: fenced_session(&data_dir, session)?,
            label,
        },
        RecordCmd::Note { session, text } => Call::RecordNote {
            session_id: fenced_session(&data_dir, session)?,
            text,
        },
        RecordCmd::Tag { session, changes } => Call::RecordTag {
            tags: changes,
            session_id: fenced_session(&data_dir, session)?,
        },
        RecordCmd::ProcessNow { session } => Call::RecordProcessNow {
            session_id: fenced_session(&data_dir, session)?,
        },
        RecordCmd::AutoStop {
            session,
            on,
            off: _,
            app,
        } => auto_stop_call(ctx, session, on, app)?,
        RecordCmd::Participant {
            action,
            person,
            session,
        } => Call::RecordParticipant {
            session_id: session_or_active(&data_dir, session)?,
            request_id: RequestId::random(),
            person_ref: person,
            action: match action {
                ParticipantArg::Confirm => ParticipantAction::Confirm,
                ParticipantArg::Remove => ParticipantAction::Remove,
            },
        },
        RecordCmd::Title {
            title,
            stdin,
            session,
        } => {
            let title = match (title, stdin) {
                (_, true) => read_stdin_title("record title")?,
                (Some(title), false) => checked_title(&title)?,
                (None, false) => return Err(Error::InvalidInput("give a title or --stdin".into())),
            };
            Call::RecordTitle {
                session_id: session_or_active(&data_dir, session)?,
                request_id: RequestId::random(),
                title,
            }
        }
        RecordCmd::Discard { session, yes } => {
            if !yes {
                return Err(Error::InvalidInput(
                    "discarding cannot be undone: confirm with --yes".into(),
                ));
            }
            Call::RecordDiscard {
                session_id: session,
                request_id: RequestId::random(),
            }
        }
        RecordCmd::Live {
            session,
            follow,
            max_events,
            max_seconds,
        } => {
            if follow {
                return follow_live(ctx, session, max_events, max_seconds);
            }
            let value = live_snapshot(&data_dir, session)?;
            return super::daemon::emit(ctx, value);
        }
        RecordCmd::LiveAttribute {
            session,
            turn,
            revision,
            epoch,
            evidence,
            name,
            name_stdin,
            confirm,
            clear,
        } => {
            let (action, person_ref) = match (name, name_stdin, confirm, clear) {
                (Some(name), false, None, false) => (LiveAction::Name, Some(name)),
                (None, true, None, false) => (
                    LiveAction::Name,
                    Some(read_stdin_ref("record live-attribute")?),
                ),
                (None, false, Some(person), false) => (LiveAction::Confirm, Some(person)),
                (None, false, None, true) => (LiveAction::Clear, None),
                _ => {
                    return Err(Error::InvalidInput(
                        "give one of --name, --name-stdin, --confirm or --clear".into(),
                    ));
                }
            };
            Call::RecordLiveAttribute(LiveAttributeParams {
                session_id: session_or_active(&data_dir, session)?,
                request_id: RequestId::random(),
                turn_id: turn,
                revision,
                epoch,
                evidence,
                action,
                person_ref,
            })
        }
    };
    let value = crate::mcp::recording_call(ctx, call)?;
    super::daemon::emit(ctx, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(session: &str, id: &str, revision: u64) -> Value {
        json!({
            "session_id": session, "turn_id": id, "revision": revision, "epoch": 0,
            "track": "mic", "from_sample": 0, "to_sample": 16000, "start_s": 0.0,
            "end_s": 1.0, "text": "synthetic", "status": "preview", "evidence": "ev-0",
        })
    }

    fn turn_event(session: &str, id: &str, revision: u64) -> Event {
        Event::LiveTurn(Box::new(
            serde_json::from_value(turn(session, id, revision)).unwrap(),
        ))
    }

    fn snapshot(session: &str, turns: Vec<Value>) -> Value {
        json!({
            "session_id": session,
            "identity": {"status": {"state": "idle"}, "epoch": 0},
            "evidence": "ev-0",
            "turns": turns,
        })
    }

    #[test]
    fn follow_writes_only_newer_turns_of_its_own_session() {
        let mut follow = Follow::new("s-a".into());
        let Step::Line(first) = follow
            .snapshot(snapshot("s-a", vec![turn("s-a", "mic-0", 3)]))
            .unwrap()
        else {
            panic!("no snapshot line")
        };
        assert_eq!(first["type"], "snapshot");
        assert_eq!(first["turns"][0]["revision"], 3);
        // Already in the snapshot, or older: never replayed.
        assert_eq!(follow.on_event(turn_event("s-a", "mic-0", 3)), Step::Skip);
        assert_eq!(follow.on_event(turn_event("s-a", "mic-0", 2)), Step::Skip);
        // Another session's turn is never written.
        assert_eq!(follow.on_event(turn_event("s-b", "mic-9", 1)), Step::Skip);
        let Step::Line(newer) = follow.on_event(turn_event("s-a", "mic-0", 4)) else {
            panic!("newer revision not written")
        };
        assert_eq!(newer["type"], "turn");
        assert_eq!(newer["turn"]["revision"], 4);
        assert_eq!(follow.on_event(turn_event("s-a", "mic-0", 4)), Step::Skip);
        assert!(matches!(
            follow.on_event(turn_event("s-a", "mic-1", 1)),
            Step::Line(_)
        ));
    }

    #[test]
    fn a_lag_asks_for_a_fresh_snapshot_that_resets_revisions() {
        let mut follow = Follow::new("s-a".into());
        follow
            .snapshot(snapshot("s-a", vec![turn("s-a", "mic-0", 5)]))
            .unwrap();
        assert_eq!(
            follow.on_event(Event::Lagged { dropped: 3 }),
            Step::Resnapshot
        );
        follow
            .snapshot(snapshot("s-a", vec![turn("s-a", "mic-0", 9)]))
            .unwrap();
        assert_eq!(follow.on_event(turn_event("s-a", "mic-0", 8)), Step::Skip);
        // A snapshot of another session never continues this follow.
        assert_eq!(
            follow.snapshot(snapshot("s-b", vec![])).unwrap(),
            Step::End("stale_session")
        );
    }

    #[test]
    fn a_state_without_the_session_ends_the_follow() {
        let mut follow = Follow::new("s-a".into());
        let state = |recording: Value| {
            let mut state = json!({
                "pid": 1, "version": "0", "protocol": broscribe_ipc::PROTOCOL_VERSION,
                "library": "/synthetic",
                "jobs": {"running": null, "waiting": 0, "deferred": 0},
                "preview": {"state": "off"},
            });
            state["recording"] = recording;
            Event::State(Box::new(serde_json::from_value(state).unwrap()))
        };
        assert_eq!(
            follow.on_event(state(Value::Null)),
            Step::End("session_ended")
        );
        assert_eq!(follow.on_event(Event::ShowMainWindow), Step::Skip);
    }

    #[test]
    fn follow_keeps_only_the_agents_window_of_turns() {
        let mut follow = Follow::new("s-a".into());
        let window = LiveIdentitySettings::default().max_turns;
        follow
            .snapshot(snapshot("s-a", vec![turn("s-a", "mic-0", 3)]))
            .unwrap();
        let total = window * 3;
        for n in 1..=total {
            assert!(
                matches!(
                    follow.on_event(turn_event("s-a", &format!("mic-{n}"), 2)),
                    Step::Line(_)
                ),
                "new turn {n} not written"
            );
            assert!(follow.shown.len() <= window);
        }
        assert_eq!(follow.shown.len(), window);
        // The oldest turn still in the agent's window keeps its revision.
        let oldest = format!("mic-{}", total - window + 1);
        assert_eq!(follow.on_event(turn_event("s-a", &oldest, 1)), Step::Skip);
        assert_eq!(follow.on_event(turn_event("s-a", &oldest, 2)), Step::Skip);
        assert!(matches!(
            follow.on_event(turn_event("s-a", &oldest, 3)),
            Step::Line(_)
        ));
        assert_eq!(follow.shown.len(), window);
    }

    #[test]
    fn an_expired_deadline_stops_before_queued_events() {
        let (send, events) = mpsc::sync_channel(FOLLOW_BUFFER);
        for n in 1..=3 {
            send.send(turn_event("s-a", &format!("mic-{n}"), 1))
                .unwrap();
        }
        let start = Instant::now();
        let bounds = Bounds {
            max_events: None,
            deadline: Some(start),
        };
        let mut follow = Follow::new("s-a".into());
        let first = follow.snapshot(snapshot("s-a", vec![])).unwrap();
        let mut out = Vec::new();
        let reason = pump(
            &mut out,
            &mut follow,
            first,
            &events,
            &bounds,
            || start,
            |_| unreachable!("no snapshot after the deadline"),
        )
        .unwrap();
        assert_eq!(reason, "max_seconds");
        let lines: Vec<Value> = serde_json::Deserializer::from_slice(&out)
            .into_iter()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], "snapshot");
        // Every queued event is still there: none was taken past the deadline.
        assert_eq!(events.try_iter().count(), 3);
        // A lag reported past the deadline asks for no fresh snapshot either.
        let reason = pump(
            &mut out,
            &mut follow,
            Step::Resnapshot,
            &events,
            &bounds,
            || start,
            |_| unreachable!("no snapshot after the deadline"),
        )
        .unwrap();
        assert_eq!(reason, "max_seconds");
        drop(send);
    }

    #[test]
    fn only_a_failed_stopped_recording_is_retried() {
        let state = json!({"stopped": [
            {"session_id": "s-failed", "title": null, "stopped_at": "2026-11-17T10:00:00Z",
             "state": {"phase": "failed", "error": "synthetic"}},
            {"session_id": "s-queued", "title": null, "stopped_at": "2026-11-17T10:00:00Z",
             "state": {"phase": "queued", "ahead": 0}},
        ]});
        assert!(retryable(&state, "s-failed").is_ok());
        assert_eq!(
            retryable(&state, "s-queued").unwrap_err().code(),
            IpcError::NOT_RETRYABLE
        );
        assert_eq!(
            retryable(&state, "s-saved").unwrap_err().code(),
            "stale_session"
        );
        assert_eq!(
            retryable(&json!({}), "s-failed").unwrap_err().code(),
            "stale_session"
        );
    }
}
