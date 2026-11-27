//! Meeting lifecycle writes: `title` and `delete` (design §7.1, §11). Both run
//! the shared core service under the library lock; neither has a path that
//! puts a title or a name in argv.

use std::path::Path;

use broscribe_core::lifecycle::{self, DeleteOptions};
use broscribe_core::{Library, MeetingId, Result};
use broscribe_ipc::Call;
use serde_json::{Value, json};

use super::meeting_id;
use crate::query::{checked_title, read_stdin_title};
use crate::{Ctx, to_value};

/// Renames `id` (the shared session-aware service: a meeting still fed by a
/// recording session also updates that session's title sidecar). The running
/// agent is then asked to re-read that sidecar (`record.title_refresh`, no
/// second edit), so the recording panel shows the latest title; that is best
/// effort and reported as `agent_notified`.
pub(crate) fn set_title(
    lib: &Library,
    data_dir: &Path,
    id: &MeetingId,
    title: &str,
) -> Result<Value> {
    set_title_checked(lib, data_dir, id, title, || Ok(()))
}

/// [`set_title`] with request admission checked after the library lock is held.
pub(crate) fn set_title_checked(
    lib: &Library,
    data_dir: &Path,
    id: &MeetingId,
    title: &str,
    check: impl FnOnce() -> Result<()>,
) -> Result<Value> {
    let outcome = lifecycle::set_meeting_title_checked(
        lib,
        &lifecycle::journal_root(data_dir),
        id,
        title,
        check,
    )?;
    let mut value = to_value(&outcome)?;
    if let Some(session_id) = &outcome.session_id {
        let notified = super::daemon::optional_call(
            data_dir,
            Call::RecordTitleRefresh {
                session_id: session_id.clone(),
            },
        );
        value["agent_notified"] = json!(matches!(notified, Ok(Some(_))));
    }
    Ok(value)
}

pub(super) fn title(
    ctx: &mut Ctx<'_>,
    meeting: &str,
    title: Option<String>,
    stdin: bool,
) -> Result<()> {
    let id = meeting_id(meeting)?;
    let title = match (title, stdin) {
        (_, true) => read_stdin_title("title")?,
        (Some(title), false) => checked_title(&title)?,
        (None, false) => {
            return Err(broscribe_core::Error::InvalidInput(
                "give a title or --stdin".into(),
            ));
        }
    };
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let value = set_title(&lib, ctx.data_dir(), &id, &title)?;
    ctx.emit(value, |out| writeln!(out, "{id}: renamed"))
}

/// `delete <id> --dry-run | --yes`: the plan, or the deletion, as the core
/// service reports it. Learned voice samples leave with the meeting, so a
/// deletion that touched people kicks the agent's speaker recheck.
pub(super) fn delete(ctx: &mut Ctx<'_>, meeting: &str, dry_run: bool) -> Result<()> {
    let id = meeting_id(meeting)?;
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let opts = DeleteOptions {
        journal_root: lifecycle::journal_root(ctx.data_dir()),
        cache_dir: Some(ctx.paths.cache_dir.clone()),
    };
    let probe = ctx.deps.process.as_ref();
    let report = if dry_run {
        lifecycle::delete_plan(&lib, probe, &opts, &id)?
    } else {
        lifecycle::delete_meeting(&lib, probe, &opts, &id)?
    };
    let mut value = to_value(&report)?;
    if !dry_run && report.people_affected > 0 {
        value["agent_notified"] = json!(super::kick_recheck(ctx));
    }
    let text = value.clone();
    ctx.emit(value, |out| {
        super::write_value(out, &text, 0)?;
        if dry_run {
            writeln!(out, "Nothing was deleted; run again with --yes to delete.")?;
        }
        Ok(())
    })
}
