//! `app …`: the desktop app's own settings and About.
//!
//! - `open-at-login`, `reading-size`, `live-identity` and
//!   `uncalibrated-me-suggestions` change one setting through
//!   [`Config::update`]: a fresh read under the data directory's config
//!   lock, saved as the next revision only when it changed, so settings the
//!   app or another command saved meanwhile survive. The app and the agent
//!   follow `config.toml` themselves (the agent re-reads it at least every
//!   second while it records); nothing has to be reloaded, and a saved
//!   setting is never reported as failed because the agent is unreachable.
//! - `about` states what the running build is, as the platform's About
//!   reader finds it: missing or unclassifiable facts stay unknown, and a
//!   code signature alone never claims notarization.

use broscribe_core::Result;
use broscribe_core::config::{Config, ReadingSize};
use broscribe_platform::about::{
    AboutFacts, BuildInfoState, LicenceInventoryState, LocationNote, Notarization, Packaging,
    SealState, SignatureKind,
};
use serde_json::{Value, json};

use crate::Ctx;
use crate::cli::{ReadingSizeArg, SwitchArg};

/// Sets (or, with `None`, only reads) one setting; returns the config as it
/// now stands and whether this command changed it.
fn set(ctx: &Ctx<'_>, apply: Option<impl FnOnce(&mut Config)>) -> Result<(Config, bool)> {
    match apply {
        None => Ok((ctx.config()?, false)),
        Some(apply) => {
            let (config, changed) = Config::update(ctx.data_dir(), |config| {
                ctx.check_request_cancelled()?;
                let before = config.clone();
                apply(config);
                Ok(*config != before)
            })?;
            Ok((config, changed))
        }
    }
}

/// One on/off setting: `name` in JSON, `label` in text.
fn switch(
    ctx: &mut Ctx<'_>,
    state: SwitchArg,
    name: &str,
    label: &str,
    read: fn(&Config) -> bool,
    write: fn(&mut Config, bool),
) -> Result<()> {
    let wanted = match state {
        SwitchArg::Status => None,
        SwitchArg::On => Some(true),
        SwitchArg::Off => Some(false),
    };
    let (config, changed) = set(ctx, wanted.map(|on| move |c: &mut Config| write(c, on)))?;
    let on = read(&config);
    let mut value = json!({ "changed": changed, "revision": config.revision() });
    value[name] = on.into();
    ctx.emit(value, |out| {
        writeln!(out, "{label}: {}", if on { "on" } else { "off" })
    })
}

pub(super) fn open_at_login(ctx: &mut Ctx<'_>, state: SwitchArg) -> Result<()> {
    switch(
        ctx,
        state,
        "open_at_login",
        "Start BroScribe at login",
        Config::open_at_login,
        |c, on| c.app.open_at_login = Some(on),
    )
}

pub(super) fn status_item(ctx: &mut Ctx<'_>, state: SwitchArg) -> Result<()> {
    switch(
        ctx,
        state,
        "show_status_item",
        "Show BroScribe in the menu bar",
        Config::show_status_item,
        |c, on| c.app.show_status_item = Some(on),
    )
}

pub(super) fn panel_compact(ctx: &mut Ctx<'_>, state: SwitchArg) -> Result<()> {
    switch(
        ctx,
        state,
        "compact",
        "Compact recording controls",
        |c| c.panel.compact,
        |c, on| c.panel.compact = on,
    )
}

pub(super) fn hide_controls_from_screen_sharing(ctx: &mut Ctx<'_>, state: SwitchArg) -> Result<()> {
    switch(
        ctx,
        state,
        "hide_controls_from_screen_sharing",
        "Hide Recording controls from supported screen sharing",
        |c| c.app.hide_controls_from_screen_sharing,
        |c, on| c.app.hide_controls_from_screen_sharing = on,
    )
}

/// `app live-identity on|off|status` (`[live] identity`, on when unset).
/// Off never stops capture or the live transcript.
pub(super) fn live_identity(ctx: &mut Ctx<'_>, state: SwitchArg) -> Result<()> {
    switch(
        ctx,
        state,
        "live_identity",
        "Live speaker identification",
        Config::live_identity_enabled,
        |c, on| c.live.identity = Some(on),
    )
}

/// `app uncalibrated-me-suggestions on|off|status` (`[live]
/// allow_uncalibrated_me_suggestions`, off unless the user turns it on).
/// Suggestions only: nothing is named, enrolled or recalibrated.
pub(super) fn uncalibrated_me_suggestions(ctx: &mut Ctx<'_>, state: SwitchArg) -> Result<()> {
    switch(
        ctx,
        state,
        "uncalibrated_me_suggestions",
        "Suggest you live while the voice model is uncalibrated",
        Config::allow_uncalibrated_me_suggestions,
        |c, on| c.live.allow_uncalibrated_me_suggestions = on,
    )
}

fn size_name(size: ReadingSize) -> &'static str {
    match size {
        ReadingSize::Small => "small",
        ReadingSize::Standard => "standard",
        ReadingSize::Large => "large",
        ReadingSize::Larger => "larger",
        ReadingSize::Largest => "largest",
    }
}

/// `app reading-size [small|standard|large|larger|largest]`: sets the
/// reading size, or reports it with the available steps.
pub(super) fn reading_size(ctx: &mut Ctx<'_>, size: Option<ReadingSizeArg>) -> Result<()> {
    let wanted = size.map(ReadingSize::from);
    let (config, changed) = set(
        ctx,
        wanted.map(|size| move |c: &mut Config| c.appearance.reading_size = size),
    )?;
    let size = config.appearance.reading_size;
    let steps: Vec<&str> = ReadingSize::ALL.into_iter().map(size_name).collect();
    let value = json!({
        "reading_size": size_name(size),
        "scale": size.scale(),
        "steps": steps,
        "changed": changed,
        "revision": config.revision(),
    });
    ctx.emit(value, |out| {
        writeln!(
            out,
            "Reading size: {} ({}% of standard)",
            size_name(size),
            (size.scale() * 100.0).round()
        )
    })
}

/// `app about`: the running build's About facts, with every disagreement
/// between them.
pub(super) fn about(ctx: &mut Ctx<'_>) -> Result<()> {
    let facts = ctx.deps.agents.about.read()?;
    let notices = facts
        .documents
        .iter()
        .find(|doc| doc.kind == broscribe_platform::about::AboutDocumentKind::ThirdPartyNotices)
        .map(|doc| doc.path());
    let mismatches = facts.mismatches();
    let mut value =
        serde_json::to_value(&facts).map_err(|e| broscribe_core::Error::Internal(e.to_string()))?;
    value["mismatches"] = serde_json::to_value(&mismatches)
        .map_err(|e| broscribe_core::Error::Internal(e.to_string()))?;
    value["third_party_notices_path"] = json!(notices);
    ctx.emit(value, |out| {
        for line in about_lines(&facts) {
            writeln!(out, "{line}")?;
        }
        if let Some(path) = notices {
            writeln!(out, "Third-party notices: {}", path.display())?;
        }
        if !mismatches.is_empty() {
            writeln!(
                out,
                "Inconsistent build facts: {}",
                mismatches
                    .iter()
                    .map(|m| serde_json::to_value(m)
                        .ok()
                        .and_then(|v| v["kind"].as_str().map(str::to_owned))
                        .unwrap_or_else(|| "unknown".into()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )?;
        }
        Ok(())
    })
}

fn team(team: Option<&String>) -> String {
    team.map_or_else(|| "team unknown".into(), |t| format!("team {t}"))
}

/// The human text of About, one fact per line; unknowns say so.
fn about_lines(facts: &AboutFacts) -> Vec<String> {
    let mut lines = vec![format!("BroScribe {}", facts.app_version)];
    lines.push(match &facts.build {
        BuildInfoState::Present(info) => {
            format!("Build {} ({}, {})", info.build, info.version, info.target)
        }
        BuildInfoState::Missing => "Build: no build information (a development build?)".into(),
        BuildInfoState::Invalid { reason } => format!("Build: unreadable ({reason})"),
    });
    let arch = &facts.architecture;
    lines.push(format!(
        "Architecture: {} on {}{}",
        arch.arch,
        arch.os,
        match arch.translated {
            Some(true) => " (translated)",
            Some(false) => "",
            None => " (translation unknown)",
        }
    ));
    lines.push(
        match facts.packaging {
            Packaging::AppBundle => "Packaging: app bundle",
            Packaging::InstallFolder => "Packaging: installation folder",
            Packaging::Unbundled => "Packaging: unbundled (run in place)",
        }
        .into(),
    );
    for note in &facts.location_notes {
        lines.push(
            match note {
                LocationNote::Translocated => "Location: translocated by macOS (read-only copy)",
                LocationNote::ReadOnlyVolume => "Location: a read-only volume",
                LocationNote::NetworkVolume => "Location: a network volume",
                LocationNote::Temporary => "Location: a temporary folder",
            }
            .into(),
        );
    }
    let signature = &facts.signature;
    let kind = match &signature.kind {
        SignatureKind::Unsigned => "unsigned".into(),
        SignatureKind::AdHoc => "ad hoc (no certificate)".into(),
        SignatureKind::AppleDevelopment { team: t } => {
            format!(
                "Apple Development, {} (not for distribution)",
                team(t.as_ref())
            )
        }
        SignatureKind::DeveloperId { team: t } => format!("Developer ID, {}", team(t.as_ref())),
        SignatureKind::AppleOther { team: t } => {
            format!("another Apple certificate, {}", team(t.as_ref()))
        }
        SignatureKind::Authenticode { fingerprint } => match fingerprint {
            Some(f) => format!("Authenticode, leaf certificate {f}"),
            None => "Authenticode, not verified".into(),
        },
        SignatureKind::ManifestOnly => {
            "no OS code signing (releases are vouched for by the signed release manifest)".into()
        }
        SignatureKind::Unknown { reason } => format!("unknown ({reason})"),
    };
    let seal = match signature.seal {
        SealState::Intact => "seal intact".to_owned(),
        SealState::Broken { status } => format!("seal broken (status {status})"),
        SealState::NotChecked => "seal not checked".to_owned(),
    };
    let notarization = match signature.notarization {
        Notarization::NotApplicable => "notarization not applicable",
        Notarization::NotEligible => "not eligible for notarization",
        Notarization::Unknown => "notarization unknown",
    };
    lines.push(format!("Signature: {kind}; {seal}; {notarization}"));
    lines.push(match &facts.licences {
        LicenceInventoryState::Present(inventory) => format!(
            "Licences: {} components{}",
            inventory.components,
            if inventory.complete {
                String::new()
            } else {
                format!(", {} without a resolved licence text", inventory.unresolved)
            }
        ),
        LicenceInventoryState::Missing => "Licences: no licence inventory in this build".into(),
        LicenceInventoryState::Invalid { reason } => format!("Licences: unreadable ({reason})"),
    });
    let documents: Vec<Value> = facts
        .documents
        .iter()
        .filter_map(|d| serde_json::to_value(d.kind).ok())
        .collect();
    if !documents.is_empty() {
        lines.push(format!(
            "Licence documents: {}",
            documents
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    lines
}

pub(super) fn permissions(
    ctx: &mut Ctx<'_>,
    request: Option<crate::cli::AppPermissionArg>,
) -> Result<()> {
    use crate::cli::AppPermissionArg;
    use broscribe_platform::shell::{HANDOFF_TIMEOUT, HandoffRequest};
    let channel = broscribe_platform_native::NativePlatform::instance_channel(ctx.data_dir());
    if let Some(permission) = request {
        channel.send(
            match permission {
                AppPermissionArg::Notifications => HandoffRequest::RequestNotifications,
                AppPermissionArg::Accessibility => HandoffRequest::RequestAccessibility,
            },
            HANDOFF_TIMEOUT,
        )?;
        return ctx.emit(
            json!({"requested": match permission {
                AppPermissionArg::Notifications => "notifications",
                AppPermissionArg::Accessibility => "accessibility",
            }}),
            |out| writeln!(out, "Asked the desktop app to request permission."),
        );
    }
    let notifications = channel.request(HandoffRequest::NotificationStatus, HANDOFF_TIMEOUT)?;
    let accessibility = channel.request(HandoffRequest::AccessibilityStatus, HANDOFF_TIMEOUT)?;
    if !matches!(
        notifications.as_str(),
        "granted" | "denied" | "unknown" | "unsupported" | "not_determined"
    ) || !matches!(accessibility.as_str(), "granted" | "denied")
    {
        return Err(broscribe_core::Error::Unsupported(
            "the running desktop app did not report its permissions".into(),
        ));
    }
    ctx.emit(
        json!({"notifications": notifications, "accessibility": accessibility}),
        |out| {
            writeln!(out, "Notifications: {notifications}")?;
            writeln!(out, "Accessibility: {accessibility}")
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_size_names_follow_the_config_steps() {
        for size in ReadingSize::ALL {
            assert_eq!(
                serde_json::to_value(size).unwrap(),
                size_name(size),
                "the CLI name is the stored name"
            );
        }
    }
}
