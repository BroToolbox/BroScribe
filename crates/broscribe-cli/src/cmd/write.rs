//! Write commands: speakers, tags, forget, people edit and its undo, index
//! rebuild. Each takes the library lock once through [`ops::with_index`];
//! embeddings are computed before the lock.

use std::io::Write;

use broscribe_core::index::Index;
use broscribe_core::live_identity::{self, LiveReviewMeeting};
use broscribe_core::ops::{self, SpeakerState};
use broscribe_core::render::{NameStyle, PeopleLookup, format_hms};
use broscribe_core::schema::{Assignment, GroupOrigin, RecordingState};
use broscribe_core::{Error, Library, MeetingId, PersonId, Result, SpeakerId};
use broscribe_integrations::{IntegrationError, IntegrationId, PhotoPurgeStatus};
use serde_json::{Value, json};

use super::voice::remove_enrollment_files;
use super::{meeting_id, speaker_id};
use crate::args::split_tag_changes;
use crate::cli::{ForgetArgs, PeopleEditArgs, SpeakersCmd};
use crate::query::read_stdin;
use crate::{Ctx, to_value};

/// Forget the voiceprints and delete the kept enrollment passage under the same lock.
pub(crate) fn forget_voice(
    tx: &mut broscribe_core::WriteTx<'_>,
    index: &mut broscribe_core::index::Index,
    person: &str,
) -> Result<ops::ForgetOutcome> {
    let outcome = ops::forget_voice(tx, index, person)?;
    remove_enrollment_files(&tx.library().person_dir(&outcome.person_id))?;
    Ok(outcome)
}

/// `people forget` / `people forget-voice`. Neither has an Undo. Once the
/// core forget committed, nothing after it fails the command: a photo purge
/// that is partial or failed (`photos`, `photos_error`) and search vectors
/// not refreshed (`vectors_error`) are reported beside the success with
/// sanitized stable codes, and the requested speaker recheck stays queued.
pub(super) fn forget(ctx: &mut Ctx<'_>, args: ForgetArgs, voice_only: bool) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let person = &args.person;
    if args.dry_run || !args.yes {
        let value = forget_preview(&lib, person, voice_only)?;
        return ctx.emit(value.clone(), |out| {
            for (heading, key) in [("Goes", "goes"), ("Changes", "changes"), ("Stays", "stays")] {
                writeln!(out, "{heading}")?;
                for line in value[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    writeln!(out, "  {line}")?;
                }
            }
            Ok(())
        });
    }
    let outcome = ops::with_index(&lib, |tx, index| {
        if voice_only {
            forget_voice(tx, index, person)
        } else {
            ops::forget_person(tx, index, person)
        }
    })?;
    let mut value = to_value(&outcome)?;
    // Directory photos go with the person, never with only the voice.
    let photos = (!voice_only).then(|| {
        broscribe_integrations::forget_person_photos(ctx.data_dir(), outcome.person_id.as_str())
    });
    match &photos {
        None => {}
        Some(Ok(purge)) => {
            value["photos"] = to_value(purge)?;
            for f in &purge.failures {
                ctx.note(format!(
                    "note: cached photos of {} remain for {} ({}): {}",
                    outcome.person_id, f.integration, f.error.code, f.error.message
                ));
            }
        }
        Some(Err(e)) => {
            let error = e.public();
            ctx.note(format!(
                "note: cached photos of {} were not removed ({}): {}",
                outcome.person_id, error.code, error.message
            ));
            value["photos"] = json!({ "status": PhotoPurgeStatus::Partial });
            value["photos_error"] = to_value(&error)?;
        }
    }
    // Removed names and line corrections can merge windows: embed the new ones.
    if !outcome.meetings_changed.is_empty()
        && let Err(e) = super::refresh_vectors(
            &lib,
            &config,
            ctx.data_dir(),
            ctx.deps.secrets.as_ref(),
            &ctx.deps.execution,
            Some(&outcome.meetings_changed),
        )
    {
        ctx.note(format!("note: search vectors not updated: {e}"));
        value["vectors_error"] = json!(e.code());
    }
    // Forgetting invalidates what other speakers were matched against: core
    // requested a recheck for each model that knew the voice.
    let agent_notified = !outcome.learning.is_empty() && super::kick_recheck(ctx);
    value["agent_notified"] = json!(agent_notified);
    let photos_partial = value["photos"]["status"] == "partial";
    ctx.emit(value, |out| {
        if voice_only {
            writeln!(out, "Forgot the voice of {}.", outcome.person_id)?;
        } else {
            writeln!(out, "Forgot {}.", outcome.person_id)?;
        }
        writeln!(
            out,
            "voiceprints deleted: {}; meetings changed: {}; speakers changed: {}; queued hints removed: {}",
            if outcome.voiceprints_deleted { "yes" } else { "no" },
            outcome.meetings_changed.len(),
            outcome.speakers_changed,
            outcome.hints_removed
        )?;
        if photos_partial {
            writeln!(
                out,
                "Some cached directory photos could not be removed; run the forget again to retry."
            )?;
        }
        Ok(())
    })
}

pub(crate) fn forget_preview(lib: &Library, person: &str, voice_only: bool) -> Result<Value> {
    let plan = ops::preview_forget(lib, person, !voice_only)?;
    let mut goes = vec![format!("{} voice sample(s)", plan.voice_samples)];
    if plan.kept_readings > 0 {
        goes.push(format!("{} kept voice reading(s)", plan.kept_readings));
    }
    let mut changes = Vec::new();
    let mut stays = vec!["Meetings, transcripts and recordings".to_owned()];
    if voice_only {
        goes.push("Stored voice data for speakers named as this person".to_owned());
        changes.push("BroScribe stops recognising this voice until it learns it again".to_owned());
        stays.push("The person, their notes and saved speaker names".to_owned());
    } else {
        goes.push("The person, their notes and any cached directory photos".to_owned());
        if plan.named_meetings > 0 {
            changes.push(format!(
                "Their speakers become unnamed in {} meeting(s)",
                plan.named_meetings
            ));
        }
        changes.push(
            "Their name is removed from linked participant lists and line corrections".to_owned(),
        );
    }
    if plan.is_self {
        stays.push("The microphone track still marks your lines as Me".to_owned());
    }
    Ok(
        json!({ "dry_run": true, "voice_only": voice_only, "plan": plan, "goes": goes, "changes": changes, "stays": stays }),
    )
}

pub(super) fn tag(ctx: &mut Ctx<'_>, meeting: &str, changes: &[String]) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let id = meeting_id(meeting)?;
    let (add, remove) = split_tag_changes(changes);
    let tags = ops::with_index(&lib, |tx, index| ops::tag(tx, index, &id, &add, &remove))?;
    ctx.emit(json!({ "meeting_id": id, "tags": tags }), |out| {
        if tags.is_empty() {
            writeln!(out, "{id}: no tags")
        } else {
            writeln!(out, "{id}: {}", tags.join(", "))
        }
    })
}

/// `transcript attribute --fingerprints-stdin` input: the segments as the
/// caller saw them, so the core's stale check applies to that view, and the
/// person (kept out of argv); `person` is omitted with `--clear`. `fence`
/// (optional) is the speakers as the caller saw them: the line is refused
/// once the meeting was reanalysed or its speakers regrouped.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FingerprintsStdin {
    fingerprints: Vec<FingerprintIn>,
    #[serde(default)]
    person: Option<String>,
    #[serde(default)]
    fence: Option<ops::SpeakerFence>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FingerprintIn {
    track: String,
    start: f64,
    end: f64,
    text_sha256: String,
}

/// `transcript attribute <meeting> (--segment <id>… (--person <p> | --clear)
/// | --fingerprints-stdin [--clear])`: a line correction (design §6.4). With
/// `--segment`, the segments' fingerprints are taken under the library
/// lock; with `--fingerprints-stdin` they (and the person) are the caller's
/// (the app's view). Either way a transcript that changed meanwhile is
/// refused rather than misattributed. Speakers, voiceprints and the queue
/// stay as they are; search vectors of the changed windows are refreshed.
pub(super) fn attribute(
    ctx: &mut Ctx<'_>,
    meeting: &str,
    segments: &[u32],
    fingerprints_stdin: bool,
    person: Option<String>,
    clear: bool,
) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let id = meeting_id(meeting)?;
    let (given, person, fence) = if fingerprints_stdin {
        let input: FingerprintsStdin = ctx.input_json("transcript attribute")?;
        let person = match (input.person, clear) {
            (Some(_), true) => {
                return Err(Error::InvalidInput(
                    "give `person` on stdin or --clear, not both".into(),
                ));
            }
            (None, false) => {
                return Err(Error::InvalidInput(
                    "give `person` on stdin, or --clear".into(),
                ));
            }
            (person, _) => person,
        };
        let fingerprints: Vec<ops::SegmentFingerprint> = input
            .fingerprints
            .into_iter()
            .map(|f| ops::SegmentFingerprint {
                track: f.track,
                start: f.start,
                end: f.end,
                text_sha256: f.text_sha256,
            })
            .collect();
        (Some(fingerprints), person, input.fence)
    } else {
        (None, person.filter(|_| !clear), None)
    };
    let mut wanted = segments.to_vec();
    wanted.sort_unstable();
    wanted.dedup();
    let (updated, fingerprints) = ops::with_index_checked(
        &lib,
        || ctx.check_request_cancelled(),
        |tx, index| {
            if let Some(fence) = &fence {
                ops::check_speaker_fence_at(tx, &id, fence)?;
            }
            let fingerprints = match &given {
                Some(f) => f.clone(),
                None => {
                    let current = tx.library().load_meeting(&id)?;
                    wanted
                        .iter()
                        .map(|seg| {
                            current
                            .segments
                            .iter()
                            .find(|s| s.id == *seg)
                            .map(ops::segment_fingerprint)
                            .ok_or_else(|| {
                                Error::InvalidInput(format!(
                                    "segment {seg} is not in {id}; the transcript changed; reload"
                                ))
                            })
                        })
                        .collect::<Result<Vec<_>>>()?
                }
            };
            let updated =
                ops::set_utterance_people(tx, index, &id, &fingerprints, person.as_deref())?;
            Ok((updated, fingerprints))
        },
    )?;
    super::after_identity(ctx, &lib, &config, Some(std::slice::from_ref(&id)));
    // The segments the fingerprints name now (the same IDs for --segment).
    let ids: Vec<u32> = updated
        .segments
        .iter()
        .filter(|s| fingerprints.contains(&ops::segment_fingerprint(s)))
        .map(|s| s.id)
        .collect();
    let person_id = person.as_ref().and_then(|_| {
        let first = ids.first()?;
        ops::effective_attribution(&updated)
            .into_iter()
            .find(|a| a.segment_id == *first && a.line)?
            .person_id
    });
    let name = person_id
        .as_ref()
        .and_then(|p| lib.people_lookup().ok()?.display_name(p).map(str::to_owned));
    let list = ids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    ctx.emit(
        json!({ "meeting_id": id, "segments": ids, "person_id": person_id }),
        |out| match (&person_id, &name) {
            (Some(p), name) => writeln!(
                out,
                "{id}: segment(s) {list} now attributed to {}",
                name.as_deref().unwrap_or(p.as_str())
            ),
            (None, _) => writeln!(
                out,
                "{id}: segment(s) {list} show their speaker's name again"
            ),
        },
    )
}

/// `--expect-generation INTEGRATION:N`.
fn expected_generation(
    text: &str,
) -> Result<(
    IntegrationId,
    Option<broscribe_core::calendar::AccountRef>,
    u64,
)> {
    let invalid = || {
        Error::InvalidInput(format!(
            "--expect-generation must be INTEGRATION:N, got {text:?}"
        ))
    };
    let (id, generation) = text.rsplit_once(':').ok_or_else(invalid)?;
    let (provider, account) = id
        .trim()
        .split_once('/')
        .map_or((id.trim(), None), |(p, a)| (p, Some(a)));
    let id = IntegrationId::parse(provider).map_err(super::integrations::core)?;
    let account = account
        .map(|account| super::integrations::qualified_account(&format!("{provider}/{account}")))
        .transpose()?;
    let generation = generation.trim().parse().map_err(|_| invalid())?;
    Ok((id, account, generation))
}

/// `people edit`: name, aliases (+/-), e-mails (+/-) and notes of a person,
/// or with `--spec-stdin` one [`ops::PersonEditSpec`] (the app's path, with
/// the `expected` revision it read). [`ops::edit_person`] validates, fences
/// every addressed field by its revision (`stale_person`), records the
/// Undo entry and re-renders what a rename changes. With
/// `--expect-generation` the commit runs under that account's lock, only
/// while it is still signed in at that generation.
pub(super) fn edit_person(ctx: &mut Ctx<'_>, a: PeopleEditArgs) -> Result<()> {
    let expected = a
        .expect_generation
        .as_deref()
        .map(expected_generation)
        .transpose()?;
    let spec = if a.spec_stdin {
        ctx.input_json::<ops::PersonEditSpec>("people edit")?
    } else {
        let (add_aliases, remove_aliases) = split_tag_changes(&a.aliases);
        let (add_emails, remove_emails) = split_tag_changes(&a.emails);
        ops::PersonEditSpec {
            name: a.name,
            add_aliases,
            remove_aliases,
            add_emails,
            remove_emails,
            notes: a
                .notes_file
                .as_deref()
                .map(|p| std::fs::read_to_string(p).map_err(|e| Error::io_at(p, e)))
                .transpose()?,
            ..ops::PersonEditSpec::default()
        }
    };
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let commit = || {
        ops::with_index_checked(
            &lib,
            || ctx.check_request_cancelled(),
            |tx, index| {
                let id = person_id_in(tx, &a.person)?;
                ops::edit_person(tx, index, &id, &spec)
            },
        )
    };
    let outcome = match expected {
        None => commit()?,
        Some((id, account, generation)) => {
            let service = super::integrations::service(ctx)?;
            match account {
                Some(account) => {
                    service.commit_for_account_generation(&account, generation, || {
                        commit().map_err(IntegrationError::Core)
                    })
                }
                None => service.commit_for_generation(id, generation, || {
                    commit().map_err(IntegrationError::Core)
                }),
            }
            .map_err(super::integrations::core)?
        }
    };
    emit_person_edit(ctx, &outcome, false)
}

/// `people undo-edit <person> --undo-id <id>`: [`ops::undo_person_edit`]
/// restores exactly the fields that edit changed, while none changed
/// since (`stale_person`); an unknown, used or expired entry is
/// `undo_unavailable`, a forgotten person `not_found` (forgetting has no
/// Undo and nothing is recreated).
pub(super) fn undo_edit(ctx: &mut Ctx<'_>, person: &str, undo_id: &str) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let outcome = ops::with_index_checked(
        &lib,
        || ctx.check_request_cancelled(),
        |tx, index| {
            let id = person_id_in(tx, person)?;
            ops::undo_person_edit(tx, index, &id, undo_id)
        },
    )?;
    emit_person_edit(ctx, &outcome, true)
}

/// The person `reference` names, read under the write lock.
fn person_id_in(tx: &broscribe_core::WriteTx<'_>, reference: &str) -> Result<PersonId> {
    ops::find_person(tx.library(), reference)?
        .map(|p| p.id)
        .ok_or_else(|| Error::NotFound(format!("person {}", reference.trim())))
}

fn emit_person_edit(ctx: &mut Ctx<'_>, outcome: &ops::PersonEditOutcome, undo: bool) -> Result<()> {
    let person = &outcome.person;
    ctx.emit(to_value(outcome)?, |out| {
        let name = NameStyle::Plain.apply(&person.name);
        if !outcome.changed {
            return writeln!(out, "Nothing changed for {name} ({}).", person.id);
        }
        let verb = if undo { "Undid the edit of" } else { "Updated" };
        let fields: Vec<&str> = outcome.fields.iter().map(|f| f.as_str()).collect();
        writeln!(out, "{verb} {name} ({}): {}.", person.id, fields.join(", "))?;
        if !person.aliases.is_empty() {
            let aliases: Vec<String> = person
                .aliases
                .iter()
                .map(|a| NameStyle::Plain.apply(a))
                .collect();
            writeln!(out, "aliases: {}", aliases.join(", "))?;
        }
        if !person.emails.is_empty() {
            writeln!(out, "e-mails: {}", person.emails.join(", "))?;
        }
        if let Some(undo_id) = &outcome.undo_id {
            writeln!(
                out,
                "Undo with: people undo-edit {} --undo-id {undo_id}",
                person.id
            )?;
        }
        Ok(())
    })
}

pub(super) fn rebuild(ctx: &mut Ctx<'_>) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let model = match broscribe_pipeline::vectors::text_model(
        &config,
        ctx.data_dir(),
        ctx.deps.secrets.as_ref(),
        &ctx.deps.execution,
    ) {
        Ok(m) => Some(m),
        Err(e) => {
            ctx.note(format!("note: no search vectors: {e}"));
            None
        }
    };
    let outcome = broscribe_pipeline::vectors::rebuild_index(
        &lib,
        model.as_ref().and_then(|m| m.embedder()),
    )?;
    let report = &outcome.index;
    for s in &report.skipped {
        ctx.note(format!("skipped {}: {}", s.path.display(), s.error));
    }
    if let Some(e) = &outcome.vectors_error {
        ctx.note(format!("note: search vectors not updated: {e}"));
    }
    let mut value = to_value(report)?;
    value["vectors"] = to_value(&outcome.vectors)?;
    value["vectors_error"] = json!(outcome.vectors_error);
    ctx.emit(value, |out| {
        writeln!(
            out,
            "Indexed {} meetings, {} people, {} utterances ({} files skipped).",
            report.meetings,
            report.people,
            report.utterances,
            report.skipped.len()
        )?;
        if let Some(v) = &outcome.vectors {
            writeln!(
                out,
                "Search vectors ({}): {} embedded of {} pending.",
                v.model, v.embedded, v.pending
            )?;
        }
        Ok(())
    })
}

fn name_of(people: &PeopleLookup, id: Option<&broscribe_core::PersonId>) -> String {
    match id {
        Some(id) => people
            .display_name(id)
            .map(str::to_owned)
            .unwrap_or_else(|| id.to_string()),
        None => "nobody".to_owned(),
    }
}

fn state_str(people: &PeopleLookup, s: &SpeakerState) -> String {
    let who = name_of(people, s.person_id.as_ref());
    match s.assignment {
        Assignment::Confirmed => format!("{who} (confirmed)"),
        Assignment::Auto => format!("{who} (auto)"),
        Assignment::Suggested => format!("suggested {who}"),
        Assignment::None => "unnamed".to_owned(),
    }
}

pub(super) fn speakers(ctx: &mut Ctx<'_>, action: SpeakersCmd) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let model = config.profile.speaker_embedding.clone();
    let thresholds = config.thresholds(&model).copied();
    match action {
        SpeakersCmd::Review {
            limit,
            dismissed: true,
        } => {
            let mut rows = ops::dismissed_speakers(&lib)?;
            let total = rows.len();
            rows.truncate(limit.unwrap_or(usize::MAX));
            ctx.emit(
                json!({ "speakers": to_value(&rows)?, "dismissed": total }),
                |out| {
                    if rows.is_empty() {
                        writeln!(out, "No speakers dismissed from review.")?;
                    }
                    for r in &rows {
                        writeln!(
                            out,
                            "{} {}  {}  {}  talk {}  dismissed {}",
                            r.meeting_id,
                            r.speaker_id,
                            r.started_at.format("%Y-%m-%d"),
                            r.meeting_title.as_deref().unwrap_or("(untitled)"),
                            format_hms(r.talk_s),
                            r.dismissed_at.format("%Y-%m-%d %H:%M")
                        )?;
                        for t in &r.sample_text {
                            writeln!(out, "    \"{t}\"")?;
                        }
                    }
                    if rows.len() < total {
                        writeln!(out, "… {} more", total - rows.len())?;
                    }
                    if !rows.is_empty() {
                        writeln!(out, "Restore with: speakers restore <meeting> <speaker>")?;
                    }
                    Ok(())
                },
            )
        }
        SpeakersCmd::Review { limit, .. } => {
            let entries = ops::review_queue(&lib, limit)?;
            let dismissed = ops::review_dismissed_count(&lib)?;
            let (entries, without_transcript): (Vec<_>, Vec<_>) =
                entries.into_iter().partition(|e| !e.without_transcript);
            ctx.emit(json!({
                "speakers": to_value(&entries)?,
                "without_transcript": to_value(&without_transcript)?,
                "dismissed": dismissed
            }), |out| {
                if entries.is_empty() {
                    // `limit` bounds only primary rows; it hides them only at 0.
                    if without_transcript.is_empty() && limit != Some(0) {
                        if dismissed > 0 {
                            writeln!(out, "Nothing to review · {dismissed} dismissed (list with --dismissed).")?;
                        } else {
                            writeln!(out, "Nothing to review.")?;
                        }
                    } else {
                        writeln!(out, "No primary naming rows in this result.")?;
                    }
                }
                for e in &entries {
                    let i = &e.item;
                    let state = match (&i.person_name, &i.suggested_name) {
                        (Some(n), _) if i.assignment == Assignment::Auto => {
                            format!("auto: {n}")
                        }
                        (_, Some(n)) => format!(
                            "suggested: {n} ({:.2})",
                            i.suggested_score.unwrap_or_default()
                        ),
                        _ => "unnamed".to_owned(),
                    };
                    writeln!(
                        out,
                        "{} {}  {}  {}  talk {}  {state}",
                        i.meeting_id,
                        i.speaker_id,
                        i.started_at.format("%Y-%m-%d"),
                        i.meeting_title.as_deref().unwrap_or("(untitled)"),
                        format_hms(i.talk_s)
                    )?;
                    for t in &e.sample_text {
                        writeln!(out, "    \"{t}\"")?;
                    }
                    if let Some(n) = e.live_suggestion_lines {
                        writeln!(out, "    Named while recording on {n} {}", if n == 1 { "line" } else { "lines" })?;
                    }
                    if let Some(note) = &e.identity_conflict {
                        writeln!(out, "    {note}")?;
                    }
                }
                if !without_transcript.is_empty() {
                    let duration: f64 = without_transcript.iter().map(|e| e.item.talk_s).sum();
                    writeln!(out, "Without transcript — unresolved: {} clusters, {} diarized (may contain missed speech).", without_transcript.len(), format_hms(duration))?;
                    for entry in &without_transcript {
                        let item = &entry.item;
                        writeln!(out, "{} {}  talk {}  no canonical transcript; identity unresolved",
                            item.meeting_id, item.speaker_id, format_hms(item.talk_s))?;
                    }
                }
                if dismissed > 0 && !(entries.is_empty() && without_transcript.is_empty()) {
                    writeln!(out, "{dismissed} dismissed (list with --dismissed).")?;
                }
                Ok(())
            })
        }
        SpeakersCmd::Samples { meeting, speaker } => {
            let mid = meeting_id(&meeting)?;
            let sid = speaker_id(&speaker)?;
            let m = lib.load_meeting(&mid)?;
            if m.speaker(sid).is_none() {
                return Err(Error::NotFound(format!("speaker {sid} in meeting {mid}")));
            }
            let shown = ops::effective_speaker(&m, sid);
            let clips = ops::sample_clips(&m, shown);
            ctx.emit(
                json!({
                    "meeting_id": mid,
                    "speaker_id": shown,
                    "redirected_from": (shown != sid).then_some(sid),
                    "max_sample_s": ops::MAX_SAMPLE_S,
                    "samples": to_value(&clips)?,
                }),
                |out| {
                    if clips.is_empty() {
                        writeln!(out, "{mid} {shown}: no lines to play.")?;
                    }
                    for (i, c) in clips.iter().enumerate() {
                        writeln!(
                            out,
                            "{}. {}–{}  \"{}\"",
                            i + 1,
                            format_hms(c.start),
                            format_hms(c.end),
                            c.text
                        )?;
                    }
                    Ok(())
                },
            )
        }
        SpeakersCmd::Dismiss {
            meeting,
            speaker,
            fence_stdin,
        } => dismissal(ctx, &lib, &meeting, &speaker, fence_stdin, true),
        SpeakersCmd::Restore {
            meeting,
            speaker,
            fence_stdin,
        } => dismissal(ctx, &lib, &meeting, &speaker, fence_stdin, false),
        SpeakersCmd::Matches {
            meeting,
            speaker,
            limit,
        } => super::read::speaker_matches(ctx, &lib, &config, &meeting, &speaker, Some(limit)),
        SpeakersCmd::Duplicates { meeting } => {
            super::read::duplicates(ctx, &lib, &config, &meeting)
        }
        SpeakersCmd::Name {
            meeting,
            speaker,
            person,
            expect,
        } => {
            let mid = meeting_id(&meeting)?;
            let sid = speaker_id(&speaker)?;
            // A merged member names its group's kept speaker, whose clean
            // turns are embedded here, outside the lock; the fence refuses
            // the naming if the speakers were regrouped meanwhile.
            let current = lib.load_meeting(&mid)?;
            if let Some(expected) = &expect {
                ops::check_speaker_revision(&current, sid, expected)?;
            }
            if current.speaker(sid).is_none() {
                return Err(Error::NotFound(format!("speaker {sid} in meeting {mid}")));
            }
            let fence = ops::SpeakerFence::of(&current);
            let kept = ops::effective_speaker(&current, sid);
            // Without the model or audio, ops falls back to the centroid.
            let samples = match ctx.deps.models.load_embedder(&model) {
                Ok(embedder) => broscribe_pipeline::confirmation_samples(
                    &lib,
                    &ctx.deps.decoder,
                    embedder.as_ref(),
                    &mid,
                    kept,
                )?,
                Err(e @ Error::ModelMissing(_)) => {
                    ctx.note(format!("note: {e}; using the speaker's stored centroid"));
                    Vec::new()
                }
                Err(e) => return Err(e),
            };
            // Core saves the naming and requests the speaker recheck (this
            // meeting first) in the same write; the recheck itself runs in
            // the queue, never here.
            let outcome = ops::with_index(&lib, |tx, index| {
                let saved = tx.load_meeting(&mid)?;
                ops::check_speaker_fence(&saved, &fence)?;
                if let Some(expected) = &expect {
                    ops::check_speaker_revision(&saved, sid, expected)?;
                }
                ops::name_speaker(tx, index, &mid, sid, &person, samples, &model)
            })?;
            let touched = [mid.clone()];
            if outcome.separated.is_empty() {
                super::after_naming(ctx, &lib, &config, Some(&touched));
            } else {
                // Separating members changes what the transcript shows.
                super::after_identity(ctx, &lib, &config, Some(&touched));
            }
            let agent_notified = super::kick_recheck(ctx);
            let people = lib.people_lookup()?;
            let mut value = to_value(&outcome)?;
            value["meeting_id"] = json!(mid);
            value["person_name"] = json!(people.display_name(&outcome.person_id));
            value["agent_notified"] = json!(agent_notified);
            ctx.emit(value, |out| {
                let created = if outcome.created { " (new person)" } else { "" };
                writeln!(
                    out,
                    "Named {mid} {} as {} ({}){created}.",
                    outcome.speaker_id,
                    name_of(&people, Some(&outcome.person_id)),
                    outcome.person_id
                )?;
                write_redirect(out, outcome.redirected_from, outcome.speaker_id)?;
                if !outcome.members_renamed.is_empty() {
                    writeln!(
                        out,
                        "Merged speakers named with it: {}.",
                        list(&outcome.members_renamed)
                    )?;
                }
                write_separations(out, &outcome.separated)?;
                if !outcome.voiceprint_added {
                    writeln!(
                        out,
                        "No voiceprint added: the speaker has no audio or centroid."
                    )?;
                }
                writeln!(
                    out,
                    "Checking other speakers: this meeting first, then the rest of the \
                     library (`broscribe identity status` shows the result)."
                )
            })
        }
        SpeakersCmd::Reject {
            meeting,
            speaker,
            person,
        } => {
            let mid = meeting_id(&meeting)?;
            let sid = speaker_id(&speaker)?;
            let outcome = ops::with_index(&lib, |tx, index| {
                ops::reject_suggestion(
                    tx,
                    index,
                    &mid,
                    sid,
                    person.as_deref(),
                    &model,
                    thresholds.as_ref(),
                )
            })?;
            if outcome.separated.is_empty() {
                super::after_naming(ctx, &lib, &config, Some(std::slice::from_ref(&mid)));
            } else {
                super::after_identity(ctx, &lib, &config, Some(std::slice::from_ref(&mid)));
            }
            let people = lib.people_lookup()?;
            let mut value = to_value(&outcome)?;
            value["meeting_id"] = json!(mid);
            ctx.emit(value, |out| {
                writeln!(
                    out,
                    "Rejected {} for {mid} {}; now {}.",
                    name_of(&people, Some(&outcome.rejected)),
                    outcome.speaker_id,
                    state_str(&people, &outcome.after)
                )?;
                write_redirect(out, outcome.redirected_from, outcome.speaker_id)?;
                write_separations(out, &outcome.separated)
            })
        }
        SpeakersCmd::Merge {
            targets,
            auto: true,
            all,
            ..
        } => merge_auto(ctx, &lib, &config, &targets, all),
        SpeakersCmd::Merge {
            targets,
            fence_stdin,
            ..
        } => {
            let [meeting, kept, members @ ..] = targets.as_slice() else {
                return Err(merge_usage());
            };
            if members.is_empty() {
                return Err(merge_usage());
            }
            let mid = meeting_id(meeting)?;
            let kept = speaker_id(kept)?;
            let members = members
                .iter()
                .map(|s| speaker_id(s))
                .collect::<Result<Vec<_>>>()?;
            let fence = read_fence(fence_stdin, "speakers merge")?;
            let model = config.profile.speaker_embedding.clone();
            let thresholds = config.thresholds(&model).copied();
            let outcome = ops::with_index(&lib, |tx, index| {
                let fence = fence_or_current(tx, &mid, fence)?;
                ops::group_speakers(
                    tx,
                    index,
                    &mid,
                    kept,
                    &members,
                    GroupOrigin::User,
                    &fence,
                    &model,
                    thresholds.as_ref(),
                )
            })?;
            if outcome.changed {
                super::after_identity(ctx, &lib, &config, Some(std::slice::from_ref(&mid)));
            }
            let g = &outcome.group;
            let mut value = to_value(&outcome)?;
            if outcome.kept_swapped {
                value["message"] = json!(kept_swap_message(outcome.requested_kept, g.kept));
            }
            ctx.emit(value, |out| {
                if outcome.changed {
                    writeln!(
                        out,
                        "Merged {mid} {} into {} as {}.",
                        list(&g.members),
                        g.kept,
                        g.id
                    )?;
                } else {
                    writeln!(out, "{mid}: already merged as {}.", g.id)?;
                }
                if outcome.kept_swapped {
                    writeln!(out, "{}", kept_swap_message(outcome.requested_kept, g.kept))?;
                }
                writeln!(out, "Undo with: speakers unmerge {mid} {}", g.id)
            })
        }
        SpeakersCmd::Unmerge {
            meeting,
            group,
            fence_stdin,
        } => {
            let mid = meeting_id(&meeting)?;
            let fence = read_fence(fence_stdin, "speakers unmerge")?;
            let outcome = ops::with_index(&lib, |tx, index| {
                let fence = fence_or_current(tx, &mid, fence)?;
                ops::ungroup_speakers(tx, index, &mid, &group, &fence)
            })?;
            super::after_identity(ctx, &lib, &config, Some(std::slice::from_ref(&mid)));
            let speakers: Vec<SpeakerId> = outcome.group.speakers().collect();
            let message = format!(
                "{} show separately again; they won't be merged automatically again.",
                list(&speakers)
            );
            let mut value = to_value(&outcome)?;
            value["message"] = json!(message);
            ctx.emit(value, |out| {
                writeln!(out, "Unmerged {mid} {}: {message}", outcome.group.id)
            })
        }
        SpeakersCmd::NotSame {
            meeting,
            a,
            b,
            fence_stdin,
        } => {
            let mid = meeting_id(&meeting)?;
            let (a, b) = (speaker_id(&a)?, speaker_id(&b)?);
            let fence = read_fence(fence_stdin, "speakers not-same")?;
            let outcome = ops::with_index(&lib, |tx, index| {
                let fence = fence_or_current(tx, &mid, fence)?;
                ops::mark_not_same(tx, index, &mid, a, b, &fence)
            })?;
            let [x, y] = outcome.pair;
            let message =
                format!("{x} and {y} won't be merged automatically or suggested as duplicates.");
            let mut value = to_value(&outcome)?;
            value["message"] = json!(message);
            ctx.emit(value, |out| {
                if outcome.added {
                    writeln!(out, "{mid}: {message}")
                } else {
                    writeln!(out, "{mid}: {x} and {y} were already marked as different.")
                }
            })
        }
        SpeakersCmd::LiveReview { meeting } => live_review(ctx, &lib, meeting.as_deref()),
        SpeakersCmd::UndoLiveName { meeting, speaker } => {
            let mid = meeting_id(&meeting)?;
            let sid = speaker_id(&speaker)?;
            let undone =
                ops::with_index(&lib, |tx, index| undo_recording_name(tx, index, &mid, sid))?;
            super::after_identity(ctx, &lib, &config, Some(std::slice::from_ref(&mid)));
            ctx.emit(
                json!({"meeting_id": mid, "speaker_id": sid, "undone": undone}),
                |out| {
                    writeln!(out, "Undid the name given while recording.")?;
                    Ok(())
                },
            )
        }
        SpeakersCmd::LiveDismiss {
            meeting,
            decision,
            review_key,
        } => {
            let mid = meeting_id(&meeting)?;
            let outcome = ops::with_index_checked(
                &lib,
                || ctx.check_request_cancelled(),
                |tx, index| ops::dismiss_live_review(tx, index, &mid, decision, &review_key),
            )?;
            ctx.emit(to_value(&outcome)?, |out| {
                if outcome.changed {
                    writeln!(
                        out,
                        "{mid}: live decision {decision} left as transcribed; nothing was named, \
                         corrected or learned."
                    )?;
                } else {
                    writeln!(out, "{mid}: live decision {decision} was already settled.")?;
                }
                writeln!(
                    out,
                    "{} live decision(s) of {mid} await review.",
                    outcome.remaining
                )
            })
        }
    }
}

pub(crate) fn undo_recording_name(
    tx: &mut broscribe_core::WriteTx<'_>,
    index: &mut Index,
    meeting: &MeetingId,
    speaker: SpeakerId,
) -> Result<live_identity::LiveSpeakerNaming> {
    let current = tx.library().load_meeting(meeting)?;
    let naming = live_identity::speaker_namings(&current)
        .into_iter()
        .find(|naming| naming.speaker_id == speaker)
        .ok_or_else(|| {
            Error::InvalidInput("This speaker has no name to undo from the recording.".into())
        })?;
    ops::undo_live_promotion(tx, index, meeting, &naming)?;
    Ok(naming)
}

/// `speakers live-review [--meeting]`: live decisions awaiting review
/// (partial, conflicting or unmapped) as the app's review queue lists them
/// ([`live_identity::live_reviews`]; one meeting's
/// [`live_identity::live_review_items`] with `--meeting`). Read only:
/// correct a line with `transcript attribute`, or dismiss an item with
/// `speakers live-dismiss` and its `review_key`.
fn live_review(
    ctx: &mut Ctx<'_>,
    lib: &broscribe_core::Library,
    meeting: Option<&str>,
) -> Result<()> {
    let (meetings, promotions) = match meeting {
        None => live_identity::live_reviews_and_namings(lib)?,
        Some(meeting) => {
            let m = lib.load_meeting(&meeting_id(meeting)?)?;
            let items = live_identity::live_review_items(&m);
            let promotions = live_identity::speaker_namings(&m)
                .into_iter()
                .map(|naming| live_identity::RecordedSpeakerNaming {
                    meeting_id: m.id.clone(),
                    naming,
                })
                .collect();
            let meetings = if items.is_empty() {
                Vec::new()
            } else {
                vec![LiveReviewMeeting {
                    recording_sha256: m
                        .recording
                        .as_ref()
                        .filter(|r| r.state == RecordingState::Available)
                        .map(|r| r.sha256.clone()),
                    meeting_id: m.id,
                    title: m.title,
                    started_at: m.started_at,
                    items,
                }]
            };
            (meetings, promotions)
        }
    };
    let people = lib.people_lookup()?;
    ctx.emit(
        json!({ "meetings": to_value(&meetings)?, "speaker_promotions": promotions }),
        |out| {
            if meetings.is_empty() {
                writeln!(out, "No live decisions await review.")?;
            }
            for m in &meetings {
                writeln!(
                    out,
                    "{}  {}  {}",
                    m.meeting_id,
                    m.started_at.format("%Y-%m-%d %H:%M"),
                    m.title.as_deref().unwrap_or("(untitled)")
                )?;
                for i in &m.items {
                    let outcome = serde_json::to_value(i.outcome)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default();
                    let time = if i.mapped {
                        String::new()
                    } else {
                        " (recording time; not on the transcript)".to_owned()
                    };
                    writeln!(
                        out,
                        "  decision {}  {outcome}  {}  {}–{}{time}  {} line(s)  key {}",
                        i.decision,
                        NameStyle::Plain.apply(&name_of(&people, i.person_id.as_ref())),
                        format_hms(i.from_s),
                        format_hms(i.to_s),
                        i.lines.len(),
                        i.review_key
                    )?;
                }
            }
            for entry in &promotions {
                let naming = &entry.naming;
                writeln!(
                    out,
                    "{} {} was named {} from {} lines named while recording.",
                    entry.meeting_id,
                    naming.speaker_id,
                    NameStyle::Plain.apply(&name_of(&people, Some(&naming.promotion.person_id))),
                    naming.promotion.line_count
                )?;
            }
            Ok(())
        },
    )
}

/// `speakers dismiss` / `speakers restore` ("Can't tell" and its undo):
/// fenced like a merge, never applying retention (the daemon's pass does,
/// after the grace).
fn dismissal(
    ctx: &mut Ctx<'_>,
    lib: &broscribe_core::Library,
    meeting: &str,
    speaker: &str,
    fence_stdin: bool,
    dismiss: bool,
) -> Result<()> {
    let mid = meeting_id(meeting)?;
    let sid = speaker_id(speaker)?;
    let command = if dismiss {
        "speakers dismiss"
    } else {
        "speakers restore"
    };
    let fence = read_fence(fence_stdin, command)?;
    let outcome = ops::with_index(lib, |tx, index| {
        let fence = fence_or_current(tx, &mid, fence)?;
        if dismiss {
            ops::dismiss_review(tx, index, &mid, sid, &fence)
        } else {
            ops::restore_review(tx, index, &mid, sid, &fence)
        }
    })?;
    ctx.emit(to_value(&outcome)?, |out| {
        let s = outcome.speaker_id;
        match (dismiss, outcome.changed) {
            (true, true) => writeln!(
                out,
                "Dismissed {mid} {s} from review (can't tell). Undo with: speakers restore {mid} {s}"
            )?,
            (true, false) => writeln!(out, "{mid} {s} was already dismissed from review.")?,
            (false, true) => writeln!(out, "Restored {mid} {s} to review.")?,
            (false, false) => writeln!(out, "{mid} {s} was not dismissed from review.")?,
        }
        write_redirect(out, outcome.redirected_from, s)
    })
}

/// `identity re-resolve [--dry-run]`: plans (and, unless a dry run,
/// applies in the same library write) re-resolution with the current
/// thresholds; prints counts per meeting and in total.
pub(super) fn re_resolve(ctx: &mut Ctx<'_>, dry_run: bool) -> Result<()> {
    use broscribe_pipeline::reresolve::{self, ReresolveCounts};
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let model = config.profile.speaker_embedding.clone();
    let thresholds = config.thresholds(&model).copied();
    let plans = if dry_run {
        reresolve::plan_with_live(&lib, &model, thresholds.as_ref(), &config.live)?
    } else {
        reresolve::re_resolve_with_live(&lib, &model, thresholds.as_ref(), &config.live)?
    };
    let touched: Vec<MeetingId> = plans
        .iter()
        .filter(|p| !p.changes.is_empty())
        .map(|p| p.meeting_id.clone())
        .collect();
    if !dry_run && !touched.is_empty() {
        super::after_identity(ctx, &lib, &config, Some(&touched));
    }
    let total = ReresolveCounts::total(&plans);
    ctx.emit(
        json!({
            "dry_run": dry_run,
            "model": model,
            "meetings": to_value(&plans)?,
            "total": to_value(&total)?,
        }),
        |out| {
            let counts = |c: &ReresolveCounts| {
                format!(
                    "auto-named {}, demoted {}, suggested {}, cleared {}, skipped busy {}",
                    c.auto_named, c.demoted, c.suggested, c.cleared, c.skipped_busy
                )
            };
            for p in &plans {
                writeln!(out, "{}  {}", p.meeting_id, counts(&p.counts))?;
            }
            let verb = if dry_run { "Would change" } else { "Changed" };
            writeln!(
                out,
                "{verb} {} meeting(s): {}",
                plans.iter().filter(|p| p.counts.skipped_busy == 0).count(),
                counts(&total)
            )?;
            if dry_run {
                writeln!(out, "Dry run: nothing written.")?;
            }
            Ok(())
        },
    )
}

fn merge_usage() -> Error {
    Error::InvalidInput(
        "usage: speakers merge <meeting> <kept> <member>…, or speakers merge --auto \
         (--all | <meeting>…)"
            .into(),
    )
}

/// The fence a grouping write must still match: the caller's (stdin, the
/// app's view) or, from a terminal, the meeting as it is under the lock.
fn read_fence(fence_stdin: bool, command: &str) -> Result<Option<ops::SpeakerFence>> {
    fence_stdin.then(|| read_stdin(command)).transpose()
}

fn fence_or_current(
    tx: &broscribe_core::WriteTx<'_>,
    meeting: &MeetingId,
    given: Option<ops::SpeakerFence>,
) -> Result<ops::SpeakerFence> {
    match given {
        Some(fence) => Ok(fence),
        None => Ok(ops::SpeakerFence::of(&tx.library().load_meeting(meeting)?)),
    }
}

fn kept_swap_message(requested: SpeakerId, kept: SpeakerId) -> String {
    format!(
        "{kept} is kept instead of {requested}: a merge keeps the confirmed (or \
         caption-proven) speaker with the most talk time, else the one with the most talk time."
    )
}

fn list(ids: &[SpeakerId]) -> String {
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn write_redirect(
    out: &mut dyn Write,
    redirected_from: Option<SpeakerId>,
    kept: SpeakerId,
) -> std::io::Result<()> {
    match redirected_from {
        Some(member) => writeln!(
            out,
            "{member} is merged into {kept}: the change applies to {kept}'s group."
        ),
        None => Ok(()),
    }
}

fn write_separations(out: &mut dyn Write, separated: &[ops::Separation]) -> std::io::Result<()> {
    for s in separated {
        writeln!(out, "{}", s.message)?;
    }
    Ok(())
}

/// `speakers merge --auto (--all | <meeting>…)`: automatic grouping of
/// existing meetings, each in its own write (design §6.4). Meetings not done
/// or busy are skipped; a second run adds nothing.
fn merge_auto(
    ctx: &mut Ctx<'_>,
    lib: &broscribe_core::Library,
    config: &broscribe_core::config::Config,
    targets: &[String],
    all: bool,
) -> Result<()> {
    let ids: Vec<std::result::Result<MeetingId, String>> = match (all, targets.is_empty()) {
        (true, true) => lib
            .list_meeting_dirs()?
            .iter()
            .map(|dir| {
                lib.load_meeting_at(dir)
                    .map(|m| m.id)
                    .map_err(|e| format!("{}: {e}", dir.display()))
            })
            .collect(),
        (false, false) => targets
            .iter()
            .map(|t| meeting_id(t).map(Ok))
            .collect::<Result<_>>()?,
        _ => return Err(merge_usage()),
    };
    let model = config.profile.speaker_embedding.clone();
    let thresholds = config.thresholds(&model).copied();
    if thresholds.and_then(|t| t.auto_merge).is_none() {
        ctx.note(format!(
            "note: no auto_merge threshold is set for {model}; nothing is merged automatically"
        ));
    }
    let mut outcomes: Vec<Value> = Vec::new();
    let mut changed = Vec::new();
    for id in ids {
        let id = match id {
            Ok(id) => id,
            // Only with --all: an unreadable meeting folder is reported, not fatal.
            Err(e) => {
                outcomes.push(json!({ "path_error": e }));
                continue;
            }
        };
        let outcome = ops::with_index(lib, |tx, index| {
            ops::auto_group_meeting(tx, index, &id, &model, thresholds.as_ref())
        });
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) if all => {
                outcomes.push(json!({ "meeting_id": id, "skipped": e.to_string() }));
                continue;
            }
            Err(e) => return Err(e),
        };
        if outcome.groups_added > 0 || outcome.speakers_after != outcome.speakers_before {
            changed.push(id);
        }
        outcomes.push(to_value(&outcome)?);
    }
    if !changed.is_empty() {
        super::after_identity(ctx, lib, config, Some(&changed));
    }
    let groups_added: u64 = outcomes
        .iter()
        .filter_map(|o| o["groups_added"].as_u64())
        .sum();
    ctx.emit(
        json!({ "meetings": outcomes, "groups_added": groups_added }),
        |out| {
            for o in &outcomes {
                if let Some(e) = o["path_error"].as_str() {
                    writeln!(out, "skipped {e}")?;
                } else if let Some(why) = o["skipped"].as_str() {
                    writeln!(
                        out,
                        "{}  skipped: {why}",
                        o["meeting_id"].as_str().unwrap_or("?")
                    )?;
                } else {
                    writeln!(
                        out,
                        "{}  {} group(s) added; speakers {} -> {}",
                        o["meeting_id"].as_str().unwrap_or("?"),
                        o["groups_added"],
                        o["speakers_before"],
                        o["speakers_after"]
                    )?;
                }
            }
            writeln!(out, "{groups_added} group(s) added.")
        },
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use broscribe_core::Library;
    use broscribe_core::schema::Person;
    use serde_json::Value;

    use super::super::retention::tests::run;

    fn person(root: &Path, name: &str) -> String {
        let lib = Library::open(root.join("library")).unwrap();
        let p = Person::new(lib.new_person_id(), name);
        lib.write(|tx| tx.save_person(&p)).unwrap();
        p.id.to_string()
    }

    #[test]
    fn a_photo_purge_failure_after_forget_is_reported_beside_the_success() {
        let tmp = tempfile::tempdir().unwrap();
        let id = person(tmp.path(), "Alex Example");
        // A folder where the cached photo file should be: it cannot be
        // removed as a file, so the purge stays partial.
        let photo = broscribe_integrations::account_dir(
            &tmp.path().join("data"),
            broscribe_integrations::IntegrationId::Google,
        )
        .join("photos")
        .join(format!("{id}.img"));
        std::fs::create_dir_all(photo.join("blocker")).unwrap();
        let (code, value) = run(tmp.path(), &["people", "forget", &id, "--yes"]);
        assert_eq!(code, 0, "{value}");
        assert_eq!(value["person_id"], id.as_str());
        assert_eq!(value["person_deleted"], true);
        assert_eq!(value["photos"]["status"], "partial", "{value}");
        let reported = value["photos"]["failures"]
            .as_array()
            .map(|f| f.iter().map(|f| f["code"].clone()).collect::<Vec<Value>>())
            .unwrap_or_else(|| vec![value["photos_error"]["code"].clone()]);
        assert!(
            reported
                .iter()
                .all(|c| c.as_str().is_some_and(|c| !c.is_empty())),
            "every failure carries a stable code: {value}"
        );
        // The forget stands: the person is gone and is not recreated.
        let (code, shown) = run(tmp.path(), &["people", "show", &id]);
        assert_ne!(code, 0);
        assert_eq!(shown["error"]["code"], "not_found");
    }

    #[test]
    fn an_edit_is_undone_once_by_its_undo_id() {
        let tmp = tempfile::tempdir().unwrap();
        let id = person(tmp.path(), "Alex Example");
        let (code, edited) = run(
            tmp.path(),
            &[
                "people",
                "edit",
                &id,
                "--name",
                "Alex Renamed",
                "--alias",
                "+Al",
            ],
        );
        assert_eq!(code, 0, "{edited}");
        assert_eq!(edited["changed"], true);
        assert_eq!(edited["person"]["name"], "Alex Renamed");
        assert_eq!(edited["fields"], serde_json::json!(["name", "aliases"]));
        assert!(edited["revision"]["revision"].is_string());
        let undo_id = edited["undo_id"].as_str().unwrap().to_owned();

        let (code, undone) = run(
            tmp.path(),
            &["people", "undo-edit", &id, "--undo-id", &undo_id],
        );
        assert_eq!(code, 0, "{undone}");
        assert_eq!(undone["person"]["name"], "Alex Example");
        assert_eq!(undone["person"]["aliases"], serde_json::json!([]));
        assert_eq!(undone["undo_id"], Value::Null);

        let (code, again) = run(
            tmp.path(),
            &["people", "undo-edit", &id, "--undo-id", &undo_id],
        );
        assert_ne!(code, 0);
        assert_eq!(again["error"]["code"], "undo_unavailable");
    }
}
