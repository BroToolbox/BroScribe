use broscribe_core::{Error, Library, Result, config::Config, ops};
use serde_json::json;

use crate::{Ctx, query::read_stdin};

pub(crate) fn snapshot(lib: &Library, config: &Config) -> Result<serde_json::Value> {
    let model = &config.profile.speaker_embedding;
    let thresholds = config.thresholds(model);
    let entries = match ops::review_queue(lib, None) {
        Ok(entries) => entries,
        Err(Error::NotFound(_)) if !broscribe_core::index::Index::path(lib.root()).exists() => {
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    Ok(json!({
        "identification": ops::identity_groups(lib, &entries, model, thresholds)?,
        "duplicates": ops::person_duplicates(lib, model, thresholds)?,
        "pending_merge": ops::pending_person_merge(lib)?,
    }))
}

pub(super) fn review(ctx: &mut Ctx<'_>) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let value = snapshot(&lib, &config)?;
    let groups = value["identification"].as_array().map_or(0, Vec::len);
    let pairs = value["duplicates"].as_array().map_or(0, Vec::len);
    ctx.emit(value, |out| {
        writeln!(
            out,
            "{groups} voices to identify, {pairs} possible duplicate profiles."
        )?;
        Ok(())
    })
}

pub(super) fn identify(ctx: &mut Ctx<'_>, batch: ops::IdentifyBatch) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let model = &config.profile.speaker_embedding;
    let outcome = ops::with_index(&lib, |tx, index| {
        ops::identify_batch(tx, index, &batch, model, config.thresholds(model))
    })?;
    ctx.emit(json!(outcome), |out| {
        writeln!(
            out,
            "Linked {} in {} meetings.",
            outcome.person_name, outcome.meeting_count
        )?;
        Ok(())
    })
}

pub(super) fn merge(ctx: &mut Ctx<'_>, resume: bool) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let model = &config.profile.speaker_embedding;
    let spec = if resume {
        ops::pending_person_merge(&lib)?
            .ok_or_else(|| Error::NotFound("pending profile merge".into()))?
    } else {
        read_stdin::<ops::MergePeople>("people merge")?
    };
    let outcome = ops::with_index(&lib, |tx, index| {
        ops::merge_people(tx, index, &spec, model, config.thresholds(model))
    })?;
    ctx.emit(json!(outcome), |out| {
        writeln!(
            out,
            "Merged {} into {}. Updated {} meetings.",
            outcome.source, outcome.keep, outcome.meetings_changed
        )?;
        Ok(())
    })
}

pub(super) fn not_same(ctx: &mut Ctx<'_>, a: &str, b: &str) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let pair = lib.write(|tx| {
        let resolve = |reference: &str| {
            ops::find_person(tx.library(), reference)?
                .map(|person| person.id)
                .ok_or_else(|| Error::NotFound(format!("person {}", reference.trim())))
        };
        let pair = ops::PersonPair::new(resolve(a)?, resolve(b)?);
        ops::dismiss_person_duplicate(tx, &pair)?;
        Ok(pair)
    })?;
    ctx.emit(json!({ "separate": pair }), |out| {
        writeln!(out, "Marked {} and {} as separate people.", pair.0, pair.1)?;
        Ok(())
    })
}

/// Undo receipts live for the length of the client session. Each command
/// releases the library lock before waiting for a new line.
#[derive(serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum SessionCommand {
    Rename {
        meeting: broscribe_core::MeetingId,
        title: String,
    },
    Tags {
        meeting: broscribe_core::MeetingId,
        #[serde(default)]
        add: Vec<String>,
        #[serde(default)]
        remove: Vec<String>,
    },
    AttributeLines {
        meeting: broscribe_core::MeetingId,
        fingerprints: Vec<ops::SegmentFingerprint>,
        person: Option<String>,
    },
    ActionStatus {
        meeting: broscribe_core::MeetingId,
        id: String,
        status: broscribe_core::schema::ActionStatus,
    },
    EditAction {
        meeting: broscribe_core::MeetingId,
        id: String,
        edit: ops::ActionEdit,
    },
    SpeakerReview {
        meeting: broscribe_core::MeetingId,
        speaker: broscribe_core::SpeakerId,
        dismiss: bool,
        fence: Option<ops::SpeakerFence>,
    },
    Undo,
    Redo,
    Identify {
        spec: ops::IdentifyBatch,
    },
    Merge {
        spec: ops::MergePeople,
    },
    DifferentPeople {
        pair: ops::PersonPair,
    },
    Name {
        meeting: broscribe_core::MeetingId,
        speaker: broscribe_core::SpeakerId,
        person: String,
        fence: Option<ops::SpeakerFence>,
    },
    Group {
        meeting: broscribe_core::MeetingId,
        kept: broscribe_core::SpeakerId,
        members: Vec<broscribe_core::SpeakerId>,
        fence: Option<ops::SpeakerFence>,
    },
    Ungroup {
        meeting: broscribe_core::MeetingId,
        group: String,
        fence: Option<ops::SpeakerFence>,
    },
    DifferentSpeakers {
        meeting: broscribe_core::MeetingId,
        a: broscribe_core::SpeakerId,
        b: broscribe_core::SpeakerId,
        fence: Option<ops::SpeakerFence>,
    },
}

const CLI_UNDO_LIMIT: usize = 20;
const MCP_UNDO_LIMIT: usize = 64;

#[derive(Default)]
pub(crate) struct EditHistory {
    undo: Vec<ops::ReviewEdit>,
    redo: Vec<ops::ReviewEdit>,
}

pub(super) fn edit_session(ctx: &mut Ctx<'_>) -> Result<()> {
    use std::io::BufRead as _;

    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    if let Some(history) = ctx.request_edit_history.clone() {
        let command = ctx.input_json("identity edit-session")?;
        let mut history = history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let value =
            apply_session_command(ctx, &lib, &config, command, &mut history, MCP_UNDO_LIMIT)?;
        return crate::print_json(ctx.out, &value);
    }
    let mut history = EditHistory::default();
    for line in std::io::stdin().lock().lines() {
        let line = line.map_err(|e| Error::io("reading the identity edit session", e))?;
        if line.trim().is_empty() {
            continue;
        }
        let result = serde_json::from_str(&line)
            .map_err(|e| Error::InvalidInput(format!("identity edit session request: {e}")))
            .and_then(|command| {
                apply_session_command(ctx, &lib, &config, command, &mut history, CLI_UNDO_LIMIT)
            });
        let value = match result {
            Ok(result) => result,
            Err(e) => crate::error_envelope(&e),
        };
        if ctx.json {
            writeln!(ctx.out, "{value}")?;
        } else {
            super::write_value(ctx.out, &value, 0)?;
        }
        ctx.out.flush()?;
    }
    Ok(())
}

fn apply_session_command(
    ctx: &mut Ctx<'_>,
    lib: &Library,
    config: &Config,
    command: SessionCommand,
    history: &mut EditHistory,
    undo_limit: usize,
) -> Result<serde_json::Value> {
    ctx.check_request_cancelled()?;
    let named = match &command {
        SessionCommand::Name { meeting, .. } => Some(meeting.clone()),
        _ => None,
    };
    let result = session_command(
        lib,
        config,
        command,
        &mut history.undo,
        &mut history.redo,
        undo_limit,
    )?;
    if let Some(meeting) = named {
        super::after_naming(ctx, lib, config, Some(std::slice::from_ref(&meeting)));
    }
    if let Err(e) = super::kick_agent(ctx) {
        ctx.note(format!("note: the agent could not be notified: {e}"));
    }
    Ok(
        json!({ "result": result, "can_undo": !history.undo.is_empty(), "can_redo": !history.redo.is_empty() }),
    )
}

fn session_command(
    lib: &Library,
    config: &Config,
    command: SessionCommand,
    undo: &mut Vec<ops::ReviewEdit>,
    redo: &mut Vec<ops::ReviewEdit>,
    undo_limit: usize,
) -> Result<serde_json::Value> {
    if matches!(command, SessionCommand::Undo | SessionCommand::Redo) {
        let redoing = matches!(command, SessionCommand::Redo);
        let (from, to) = if redoing { (redo, undo) } else { (undo, redo) };
        let mut edit = from
            .pop()
            .ok_or_else(|| Error::NotFound("identity edit".into()))?;
        match edit.replay(lib, redoing) {
            Ok(()) => {
                to.push(edit);
                return Ok(if redoing {
                    json!("redone")
                } else {
                    json!("undone")
                });
            }
            Err(e) => {
                if e.code() != "identity_edit_changed" {
                    from.push(edit);
                }
                return Err(e);
            }
        }
    }
    let model = &config.profile.speaker_embedding;
    let thresholds = config.thresholds(model);
    let (value, edit) = ops::with_index(lib, |tx, index| {
        ops::record_review_edit(tx, index, model, |tx, index| {
            use broscribe_core::queue::{CONFIRM_KIND, Queue, VECTORS_KIND};
            use broscribe_core::schema::GroupOrigin;

            let vectors = |tx: &mut broscribe_core::WriteTx<'_>,
                           meeting: &broscribe_core::MeetingId| {
                Queue::open(tx.root())?.enqueue(
                    tx,
                    VECTORS_KIND,
                    meeting.as_str(),
                    json!({ "meeting_id": meeting }),
                )
            };
            let current_fence =
                |tx: &broscribe_core::WriteTx<'_>, meeting: &broscribe_core::MeetingId, fence| {
                    match fence {
                        Some(fence) => Ok(fence),
                        None => tx
                            .load_meeting(meeting)
                            .map(|meeting| ops::SpeakerFence::of(&meeting)),
                    }
                };
            let value = match command {
                SessionCommand::Rename { meeting, title } => {
                    let before = tx.load_meeting(&meeting)?;
                    if before.processing.status != broscribe_core::schema::ProcessingStatus::Done {
                        return Err(Error::LibraryBusy("wait for the meeting to finish processing before making a reversible rename".into()));
                    }
                    let saved = ops::set_title(tx, index, &meeting, &title)?;
                    json!({ "meeting_id": saved.id, "title": saved.title })
                }
                SessionCommand::Tags {
                    meeting,
                    add,
                    remove,
                } => {
                    json!({ "meeting_id": meeting, "tags": ops::tag(tx, index, &meeting, &add, &remove)? })
                }
                SessionCommand::AttributeLines {
                    meeting,
                    fingerprints,
                    person,
                } => {
                    ops::set_utterance_people(
                        tx,
                        index,
                        &meeting,
                        &fingerprints,
                        person.as_deref(),
                    )?;
                    vectors(tx, &meeting)?;
                    json!({ "meeting_id": meeting, "lines": fingerprints.len() })
                }
                SessionCommand::ActionStatus {
                    meeting,
                    id,
                    status,
                } => json!(ops::set_action_status(tx, index, &meeting, &id, status)?),
                SessionCommand::EditAction { meeting, id, edit } => {
                    json!(ops::edit_action(tx, index, &meeting, &id, edit)?)
                }
                SessionCommand::SpeakerReview {
                    meeting,
                    speaker,
                    dismiss,
                    fence,
                } => {
                    let fence = current_fence(tx, &meeting, fence)?;
                    let work = if dismiss {
                        ops::dismiss_review
                    } else {
                        ops::restore_review
                    };
                    json!(work(tx, index, &meeting, speaker, &fence)?)
                }
                SessionCommand::Identify { spec } => {
                    json!(ops::identify_batch(tx, index, &spec, model, thresholds)?)
                }
                SessionCommand::Merge { spec } => {
                    json!(ops::merge_people(tx, index, &spec, model, thresholds)?)
                }
                SessionCommand::DifferentPeople { pair } => {
                    ops::dismiss_person_duplicate(tx, &pair)?;
                    json!({ "separate": pair })
                }
                SessionCommand::Name {
                    meeting,
                    speaker,
                    person,
                    fence,
                } => {
                    if let Some(fence) = fence {
                        ops::check_speaker_fence_at(tx, &meeting, &fence)?;
                    }
                    let outcome = ops::name_speaker(
                        tx,
                        index,
                        &meeting,
                        speaker,
                        &person,
                        Vec::new(),
                        model,
                    )?;
                    let input = broscribe_pipeline::worker::ConfirmInput {
                        meeting_id: meeting.clone(),
                        speaker_id: outcome.speaker_id,
                        person_id: outcome.person_id.clone(),
                        model: model.clone(),
                    };
                    Queue::open(tx.root())?.enqueue(
                        tx,
                        CONFIRM_KIND,
                        &input.key(),
                        input.to_json(),
                    )?;
                    if !outcome.separated.is_empty() {
                        vectors(tx, &meeting)?;
                    }
                    json!(outcome)
                }
                SessionCommand::Group {
                    meeting,
                    kept,
                    members,
                    fence,
                } => {
                    let fence = current_fence(tx, &meeting, fence)?;
                    let outcome = ops::group_speakers(
                        tx,
                        index,
                        &meeting,
                        kept,
                        &members,
                        GroupOrigin::User,
                        &fence,
                        model,
                        thresholds,
                    )?;
                    if outcome.changed {
                        vectors(tx, &meeting)?;
                    }
                    json!(outcome)
                }
                SessionCommand::Ungroup {
                    meeting,
                    group,
                    fence,
                } => {
                    let fence = current_fence(tx, &meeting, fence)?;
                    let outcome = ops::ungroup_speakers(tx, index, &meeting, &group, &fence)?;
                    vectors(tx, &meeting)?;
                    json!(outcome)
                }
                SessionCommand::DifferentSpeakers {
                    meeting,
                    a,
                    b,
                    fence,
                } => {
                    let fence = current_fence(tx, &meeting, fence)?;
                    json!(ops::mark_not_same(tx, index, &meeting, a, b, &fence)?)
                }
                SessionCommand::Undo | SessionCommand::Redo => {
                    unreachable!("handled before the write")
                }
            };
            Ok(value)
        })
    })?;
    if let Some(edit) = edit {
        if undo.len() == undo_limit {
            undo.remove(0);
        }
        undo.push(edit);
        redo.clear();
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_core::{
        MeetingId, SpeakerId,
        schema::{Meeting, ProcessingStatus, Source, Speaker},
    };

    #[test]
    fn undo_history_evicts_at_the_cli_and_mcp_limits() {
        for limit in [CLI_UNDO_LIMIT, MCP_UNDO_LIMIT] {
            let dir = tempfile::tempdir().unwrap();
            let lib = Library::open(dir.path()).unwrap();
            let config = Config::default();
            let id = MeetingId::parse("m-0000a3").unwrap();
            let mut meeting =
                Meeting::new(id.clone(), Source::Import, broscribe_core::schema::now());
            meeting.processing.status = ProcessingStatus::Done;
            lib.write(|tx| {
                tx.create_meeting_dir(&meeting)?;
                tx.save_meeting(&meeting)
            })
            .unwrap();
            let rename = |title: &str| {
                serde_json::from_value(json!({
                    "action":"rename","meeting":id,"title":title,
                }))
                .unwrap()
            };

            let mut undo = Vec::new();
            let mut redo = Vec::new();
            session_command(
                &lib,
                &config,
                rename("First edit"),
                &mut undo,
                &mut redo,
                limit,
            )
            .unwrap();
            undo.resize(limit, undo[0].clone());
            session_command(
                &lib,
                &config,
                rename("Latest edit"),
                &mut undo,
                &mut redo,
                limit,
            )
            .unwrap();
            assert_eq!(undo.len(), limit);
            session_command(
                &lib,
                &config,
                SessionCommand::Undo,
                &mut undo,
                &mut redo,
                limit,
            )
            .unwrap();
            assert_eq!(
                lib.load_meeting(&id).unwrap().title.as_deref(),
                Some("First edit")
            );
        }
    }

    #[test]
    fn identity_edit_session_accepts_undo_and_redo_as_separate_requests() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path()).unwrap();
        let config = Config::default();
        let id = MeetingId::parse("m-0000a1").unwrap();
        let mut meeting = Meeting::new(id.clone(), Source::Import, broscribe_core::schema::now());
        meeting.processing.status = ProcessingStatus::Done;
        for number in [1, 2] {
            meeting
                .speakers
                .push(Speaker::new(SpeakerId::new(number).unwrap(), "t0"));
        }
        lib.write(|tx| {
            tx.create_meeting_dir(&meeting)?;
            tx.save_meeting(&meeting)
        })
        .unwrap();
        let mut undo = Vec::new();
        let mut redo = Vec::new();
        for (request, groups) in [
            (
                r#"{"action":"group","meeting":"m-0000a1","kept":"S1","members":["S2"]}"#,
                1,
            ),
            (r#"{"action":"undo"}"#, 0),
            (r#"{"action":"redo"}"#, 1),
        ] {
            session_command(
                &lib,
                &config,
                serde_json::from_str(request).unwrap(),
                &mut undo,
                &mut redo,
                CLI_UNDO_LIMIT,
            )
            .unwrap();
            assert_eq!(lib.load_meeting(&id).unwrap().speaker_groups.len(), groups);
        }
    }
    #[test]
    fn parity_session_replays_meeting_and_action_edits() {
        let dir = tempfile::tempdir().unwrap();
        let lib = Library::open(dir.path()).unwrap();
        let config = Config::default();
        let id = MeetingId::parse("m-0000a2").unwrap();
        let now = broscribe_core::schema::now();
        let mut meeting = Meeting::new(id.clone(), Source::Import, now);
        meeting.processing.status = ProcessingStatus::Done;
        meeting.title = Some("Synthetic former title".into());
        meeting.duration_s = 5.0;
        meeting
            .speakers
            .push(Speaker::new(SpeakerId::new(1).unwrap(), "t0"));
        let segment = broscribe_core::schema::Segment {
            id: 1,
            track: "t0".into(),
            speaker: Some(SpeakerId::new(1).unwrap()),
            start: 1.0,
            end: 2.0,
            text: "Synthetic line.".into(),
            words: Vec::new(),
            speech_overlap: Vec::new(),
        };
        let fingerprint = ops::segment_fingerprint(&segment);
        meeting.segments.push(segment);
        let enrichment = serde_json::json!({
            "provider":"synthetic", "created_at":now, "summary":"Synthetic summary",
            "actions": [{"id":"a-synthetic","text":"Synthetic task"}]
        });
        meeting.enrichment = Some(serde_json::from_value(enrichment).unwrap());
        let person = broscribe_core::schema::Person::new(
            broscribe_core::PersonId::parse("p-0000a2").unwrap(),
            "Synthetic Owner",
        );
        lib.write(|tx| {
            tx.save_person(&person)?;
            tx.create_meeting_dir(&meeting)?;
            tx.save_meeting(&meeting)
        })
        .unwrap();
        let mut undo = Vec::new();
        let mut redo = Vec::new();
        let mut apply = |request| {
            session_command(
                &lib,
                &config,
                serde_json::from_value(request).unwrap(),
                &mut undo,
                &mut redo,
                CLI_UNDO_LIMIT,
            )
            .unwrap()
        };
        let changes = [
            json!({"action":"rename","meeting":id,"title":"Synthetic corrected title"}),
            json!({"action":"tags","meeting":id,"add":["synthetic-tag"]}),
            json!({"action":"attribute_lines","meeting":id,"fingerprints":[fingerprint],"person":person.id}),
            json!({"action":"action_status","meeting":id,"id":"a-synthetic","status":"done"}),
            json!({"action":"edit_action","meeting":id,"id":"a-synthetic","edit":{"owner":person.id,"due":"2026-12-31"}}),
            json!({"action":"speaker_review","meeting":id,"speaker":"S1","dismiss":true}),
        ];
        for change in &changes {
            apply(change.clone());
        }
        let expected = lib.load_meeting(&id).unwrap();
        assert_eq!(expected.title.as_deref(), Some("Synthetic corrected title"));
        assert_eq!(
            expected.enrichment.as_ref().unwrap().actions[0].status,
            broscribe_core::schema::ActionStatus::Done
        );
        for _ in &changes {
            apply(json!({"action":"undo"}));
        }
        let undone = lib.load_meeting(&id).unwrap();
        assert_eq!(undone.title, meeting.title);
        assert_eq!(undone.tags, meeting.tags);
        assert_eq!(undone.utterance_people, meeting.utterance_people);
        assert_eq!(
            undone.enrichment.unwrap().actions,
            meeting.enrichment.unwrap().actions
        );
        for _ in &changes {
            apply(json!({"action":"redo"}));
        }
        let redone = lib.load_meeting(&id).unwrap();
        assert_eq!(redone.title, expected.title);
        assert_eq!(redone.tags, expected.tags);
        assert_eq!(redone.utterance_people, expected.utterance_people);
        assert_eq!(
            redone.enrichment.as_ref().unwrap().actions,
            expected.enrichment.as_ref().unwrap().actions
        );
        assert!(ops::review_dismissed(&redone, SpeakerId::new(1).unwrap()));
    }
}
