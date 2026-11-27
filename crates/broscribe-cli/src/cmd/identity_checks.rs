use broscribe_core::{Error, Library, Result, config::Config, ops};
use serde_json::json;

use crate::Ctx;

pub(crate) fn snapshot(
    lib: &Library,
    config: &Config,
    person: Option<&str>,
) -> Result<ops::IdentityChecks> {
    let id = person
        .map(|name| {
            ops::find_person(lib, name)?
                .map(|person| person.id)
                .ok_or_else(|| Error::NotFound(format!("person {name}")))
        })
        .transpose()?;
    let model = &config.profile.speaker_embedding;
    ops::identity_checks(lib, id.as_ref(), model, config.thresholds(model))
}

pub(super) fn recheck(ctx: &mut Ctx<'_>, person: Option<&str>) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let checks = snapshot(&lib, &config, person)?;
    ctx.emit(json!(checks), |out| {
        writeln!(
            out,
            "Compared {} confirmed identifications. {} to check.",
            checks.compared,
            checks.alerts.len()
        )?;
        for alert in &checks.alerts {
            writeln!(
                out,
                "{} {} · {} · voice similarity {:.2}",
                alert.decision.meeting_id,
                alert.decision.speaker_id,
                alert.person_name,
                alert.score
            )?;
        }
        if checks.unreadable_meetings > 0 {
            writeln!(
                out,
                "{} meetings could not be read.",
                checks.unreadable_meetings
            )?;
        }
        Ok(())
    })
}

pub(super) fn keep(ctx: &mut Ctx<'_>, decision: ops::IdentityCheckDecision) -> Result<()> {
    let config = ctx.config()?;
    let lib = ctx.library_existing(&config)?;
    let model = &config.profile.speaker_embedding;
    lib.write(|tx| ops::keep_identification(tx, &decision, model, config.thresholds(model)))?;
    ctx.emit(json!({ "kept": decision }), |out| {
        writeln!(out, "Identification kept.")?;
        Ok(())
    })
}
