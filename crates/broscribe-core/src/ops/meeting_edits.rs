//! Fenced edits to the saved meeting and its generated actions.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{attribution::person_label, save_and_index};
use crate::index::Index;
use crate::schema::{Action, ActionOwner, ReviewNote};
use crate::{Error, MeetingId, PersonId, Result, WriteTx};

/// A text and source fence that can travel in argv without carrying the note.
pub fn review_note_revision(note: &ReviewNote) -> String {
    let mut hash = Sha256::new();
    hash.update(format!("{:?}\0", note.source));
    hash.update(&note.text);
    crate::schema::hex(&hash.finalize())
}

/// Dismisses only the note shown at this one-based position.
pub fn dismiss_review_note(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    id: &MeetingId,
    position: usize,
    expected: &str,
) -> Result<()> {
    let mut meeting = tx.load_meeting(id)?;
    let i = position
        .checked_sub(1)
        .filter(|&i| {
            meeting
                .review_notes
                .get(i)
                .is_some_and(|n| review_note_revision(n) == expected)
        })
        .ok_or_else(|| Error::Service {
            code: "review_note_changed",
            message: "The review note changed. Reload and try again.".into(),
        })?;
    meeting.review_notes.remove(i);
    save_and_index(tx, index, &mut meeting)
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionEdit {
    pub owner: Option<PersonId>,
    #[serde(default)]
    pub clear_owner: bool,
    pub due: Option<NaiveDate>,
    #[serde(default)]
    pub clear_due: bool,
}

/// Changes the fields explicitly present in the request. A cleared field is
/// also an explicit correction and carries forward through regeneration.
pub fn edit_action(
    tx: &mut WriteTx<'_>,
    index: &mut Index,
    id: &MeetingId,
    action_id: &str,
    edit: ActionEdit,
) -> Result<Action> {
    if (edit.clear_owner && edit.owner.is_some()) || (edit.clear_due && edit.due.is_some()) {
        return Err(Error::InvalidInput(
            "Choose either a new value or clear the field".into(),
        ));
    }
    if let Some(owner) = &edit.owner {
        tx.library().load_person(owner)?;
    }
    let mut meeting = tx.load_meeting(id)?;
    let (enrichment, i) = super::find_action(&mut meeting, action_id)?;
    let mut action = enrichment.actions[i].clone();
    if edit.owner.is_some() || edit.clear_owner {
        action.original_owner.get_or_insert_with(|| ActionOwner {
            speaker: action.owner_speaker,
            name: action.owner_name.clone(),
        });
        action.owner_speaker = None;
        action.owner_name = edit
            .owner
            .as_ref()
            .map(|p| person_label(&mut enrichment.line_people, p));
    }
    if edit.due.is_some() || edit.clear_due {
        action.due = edit.due;
        action.due_user_set = true;
    }
    if action != enrichment.actions[i] {
        enrichment.actions[i] = action.clone();
        save_and_index(tx, index, &mut meeting)?;
    }
    Ok(action)
}
