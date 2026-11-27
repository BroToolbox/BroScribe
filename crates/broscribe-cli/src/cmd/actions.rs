//! `actions [--owner me|<person>] [--status s] [--since] [--until]
//! [--meeting] [--query | --query-stdin] [--limit N | --page-size N
//! [--cursor C]]`,
//! `actions set-status <meeting> <action-id> <status>` and
//! `decisions [--topic t] [--since] [--until] [--meeting]
//! [--query | --query-stdin]` (plan M2 §2, §10.4). No default limit, unlike
//! the MCP tools; `--page-size` reads the same keyset pages MCP and the app
//! read.

use broscribe_core::index::{ActionItem, Index};
use broscribe_core::render::{NameStyle, format_hms};
use broscribe_core::schema::ActionStatus;
use broscribe_core::{Error, Library, PersonId, Result, ops};
use serde_json::{Value, json};

use super::meeting_id;
use crate::cli::{ActionStatusArg, ActionsArgs, DecisionsArgs};
use crate::query::{
    ActionFilters, DecisionFilters, action_query, decision_query, page_request, page_value,
    read_stdin, resolve_at,
};
use crate::{Ctx, to_value};

/// `actions --query-stdin` input, kept out of argv.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionsStdin {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    owner: Option<String>,
}

/// `decisions --query-stdin` input, kept out of argv.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionsStdin {
    #[serde(default)]
    query: Option<String>,
}

pub(crate) fn status_of(s: ActionStatusArg) -> ActionStatus {
    match s {
        ActionStatusArg::Open => ActionStatus::Open,
        ActionStatusArg::Done => ActionStatus::Done,
        ActionStatusArg::Dropped => ActionStatus::Dropped,
    }
}

fn status_str(s: ActionStatus) -> &'static str {
    match s {
        ActionStatus::Open => "open",
        ActionStatus::Done => "done",
        ActionStatus::Dropped => "dropped",
    }
}

/// `me` is the self person; anything else a person ID, e-mail, name or alias.
pub(crate) fn owner_id(lib: &Library, owner: &str) -> Result<PersonId> {
    if owner.trim().eq_ignore_ascii_case("me") {
        return lib.self_person()?.map(|p| p.id).ok_or_else(|| {
            Error::NotFound("no Me person yet (run `broscribe setup --name …`)".into())
        });
    }
    ops::find_person(lib, owner)?
        .map(|p| p.id)
        .ok_or_else(|| Error::NotFound(format!("no person matches {:?}", owner.trim())))
}

pub(super) fn list(ctx: &mut Ctx<'_>, mut a: ActionsArgs) -> Result<()> {
    if a.query_stdin {
        let input: ActionsStdin = read_stdin("actions")?;
        a.query = input.query;
        a.owner = input.owner;
    }
    let now = ctx.deps.clock.now();
    let page = page_request(a.page.page_size, a.page.cursor.as_deref(), now)?;
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let q = action_query(
        &lib,
        &ActionFilters {
            owner: a.owner.as_deref(),
            status: a.status.map(status_of),
            since: a.since.as_deref(),
            until: a.until.as_deref(),
            meeting: a.meeting.as_deref(),
            query: a.query.as_deref(),
            // The grammar refuses --limit with --page-size: a page is
            // bounded by its size.
            limit: a.limit,
        },
        resolve_at(page.as_ref(), now),
    )?;
    let index = Index::open_readonly(lib.root())?;
    let (items, value): (Vec<ActionItem>, Value) = match &page {
        None => {
            let items = index.list_actions(&q)?;
            let value = json!({ "actions": to_value(&items)? });
            (items, value)
        }
        Some(request) => {
            let mut items = Vec::new();
            let value = page_value("actions", index.list_actions_page(&q, request)?, |i| {
                let v = to_value(&i)?;
                items.push(i);
                Ok(v)
            })?;
            (items, value)
        }
    };
    let next = value["next_cursor"].as_str().map(str::to_owned);
    ctx.emit(value, |out| {
        if items.is_empty() {
            writeln!(out, "No action items.")?;
        }
        for i in &items {
            let mut flags = String::new();
            if i.stale {
                flags.push_str(" (no longer in the summary)");
            }
            if i.enrichment_stale {
                flags.push_str(" (summary predates transcript changes)");
            }
            writeln!(
                out,
                "[{}] {}  {} {}  [{}]  {}{}{}{}",
                status_str(i.status),
                i.action_id,
                i.meeting_id,
                i.started_at.format("%Y-%m-%d"),
                format_hms(i.at_s),
                i.text,
                i.owner
                    .as_deref()
                    .map(|o| format!(" — {}", NameStyle::Plain.apply(o)))
                    .unwrap_or_default(),
                i.due.map(|d| format!(" (due {d})")).unwrap_or_default(),
                flags
            )?;
        }
        if let Some(cursor) = &next {
            writeln!(out, "More: run again with --cursor {cursor}")?;
        }
        Ok(())
    })
}

pub(super) fn set_status(
    ctx: &mut Ctx<'_>,
    meeting: &str,
    action: &str,
    status: ActionStatusArg,
) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let id = meeting_id(meeting)?;
    let status = status_of(status);
    let updated = ops::with_index(&lib, |tx, index| {
        ops::set_action_status(tx, index, &id, action, status)
    })?;
    ctx.emit(
        json!({ "meeting_id": id, "action": to_value(&updated)? }),
        |out| writeln!(out, "{id} {}: {}", updated.id, status_str(updated.status)),
    )
}

pub(super) fn decisions(ctx: &mut Ctx<'_>, mut a: DecisionsArgs) -> Result<()> {
    if a.query_stdin {
        let input: DecisionsStdin = read_stdin("decisions")?;
        a.query = input.query;
    }
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let q = decision_query(
        &DecisionFilters {
            topic: a.topic.as_deref(),
            since: a.since.as_deref(),
            until: a.until.as_deref(),
            meeting: a.meeting.as_deref(),
            query: a.query.as_deref(),
            limit: a.limit,
        },
        ctx.deps.clock.now(),
    )?;
    let items = Index::open_readonly(lib.root())?.list_decisions(&q)?;
    ctx.emit(json!({ "decisions": to_value(&items)? }), |out| {
        if items.is_empty() {
            writeln!(out, "No decisions.")?;
        }
        for i in &items {
            writeln!(
                out,
                "{} {}  [{}]  {}{}{}",
                i.meeting_id,
                i.started_at.format("%Y-%m-%d"),
                format_hms(i.at_s),
                i.text,
                i.by.as_deref()
                    .map(|b| format!(" — {b}"))
                    .unwrap_or_default(),
                if i.enrichment_stale {
                    " (summary predates transcript changes)"
                } else {
                    ""
                }
            )?;
        }
        Ok(())
    })
}
