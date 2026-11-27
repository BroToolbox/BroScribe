use broscribe_core::ops::{self, ActionEdit, NewPersonSpec};
use broscribe_core::queue::{JobId, Queue};
use broscribe_core::schema::Meeting;
use broscribe_core::{Error, Result};
use chrono::{DateTime, FixedOffset};
use serde_json::{Value, json};

use super::meeting_id;
use crate::Ctx;

fn review_notes_value(meeting: &Meeting) -> Vec<Value> {
    meeting
        .review_notes
        .iter()
        .enumerate()
        .map(|(i, note)| {
            json!({
                "position": i + 1,
                "source": note.source,
                "text": note.text,
                "revision": ops::review_note_revision(note),
            })
        })
        .collect()
}

pub(super) fn review_notes(
    ctx: &mut Ctx<'_>,
    meeting: &str,
    dismiss: Option<usize>,
    expect: Option<&str>,
) -> Result<()> {
    let lib = ctx.library_existing(&ctx.config()?)?;
    let id = meeting_id(meeting)?;
    let shown = lib.load_meeting(&id)?;
    if let Some(position) = dismiss {
        let expected = expect
            .map(str::to_owned)
            .or_else(|| {
                position
                    .checked_sub(1)
                    .and_then(|i| shown.review_notes.get(i))
                    .map(ops::review_note_revision)
            })
            .ok_or_else(|| Error::NotFound("review note".into()))?;
        ops::with_index(&lib, |tx, index| {
            ops::dismiss_review_note(tx, index, &id, position, &expected)
        })?;
        return ctx.emit(json!({ "meeting_id": id, "checked": position }), |out| {
            writeln!(out, "Review note marked as checked.")
        });
    }
    ctx.emit(
        json!({ "meeting_id": id, "review_notes": review_notes_value(&shown) }),
        |out| {
            for (i, note) in shown.review_notes.iter().enumerate() {
                writeln!(out, "{}. {}", i + 1, note.text)?;
            }
            Ok(())
        },
    )
}

pub(super) fn add_person(ctx: &mut Ctx<'_>, spec: NewPersonSpec) -> Result<()> {
    let lib = ctx.library_existing(&ctx.config()?)?;
    let person = ops::with_index(&lib, |tx, index| ops::add_person(tx, index, &spec))?;
    ctx.emit(json!({ "person": person }), |out| {
        writeln!(out, "Added {}.", person.name)
    })
}

pub(super) fn edit_action(
    ctx: &mut Ctx<'_>,
    meeting: &str,
    action: &str,
    edit: ActionEdit,
) -> Result<()> {
    let lib = ctx.library_existing(&ctx.config()?)?;
    let id = meeting_id(meeting)?;
    let action = ops::with_index(&lib, |tx, index| {
        ops::edit_action(tx, index, &id, action, edit)
    })?;
    ctx.emit(json!({ "meeting_id": id, "action": action }), |out| {
        writeln!(out, "Updated action {}.", action.id)
    })
}

pub(super) fn cancel_generation(ctx: &mut Ctx<'_>, id: &JobId, run: Option<String>) -> Result<()> {
    let lib = ctx.library_existing(&ctx.config()?)?;
    let queue = Queue::open(lib.root())?;
    let run = match run {
        Some(run) => run,
        None => queue
            .get(id)?
            .generation_run
            .ok_or_else(|| Error::InvalidInput("This job has no generation run token".into()))?,
    };
    lib.write(|tx| queue.cancel_generation(tx, id, &run))?;
    ctx.emit(
        json!({ "job_id": id, "run": run, "cancelled": true }),
        |out| writeln!(out, "Cancelled generation {id}."),
    )
}

pub(super) fn restore(
    ctx: &mut Ctx<'_>,
    meeting: &str,
    template: Option<&str>,
    expected: DateTime<FixedOffset>,
) -> Result<()> {
    let lib = ctx.library_existing(&ctx.config()?)?;
    let id = meeting_id(meeting)?;
    let created_at =
        broscribe_pipeline::enrich::restore_previous(&lib, &id, template, expected, || Ok(()))?;
    ctx.emit(json!({ "meeting_id": id, "template_id": template, "created_at": created_at, "restored": true }), |out| writeln!(out, "Restored the previous result."))
}
