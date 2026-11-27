//! Detection policy overrides are saved in the data directory's config (no
//! library is opened or created), then the running agent is asked to
//! reload. The saved policy is the result: an agent that refuses or loses
//! the reload is reported (`daemon_reloaded: false` and a note), never
//! turned into a failure of a change that already happened.
use crate::{
    Ctx,
    cli::{DetectCmd, DetectPolicyArg},
};
use broscribe_core::config::Config;
use broscribe_core::{Error, Result};
use broscribe_detect::{DetectRules, Settings};
use broscribe_ipc::Call;
use serde_json::json;
pub(super) fn run(ctx: &mut Ctx<'_>, action: DetectCmd) -> Result<()> {
    match action {
        DetectCmd::Status => {
            // Settings that do not load are reported, never fatal: detection
            // runs on what does load (the built-in rules here), and so does
            // this report.
            let config = ctx.config();
            let settings = Settings::load(
                ctx.data_dir(),
                config
                    .as_ref()
                    .map(|c| &c.detect.policies)
                    .map_err(ToString::to_string),
            );
            let problem = settings.problem();
            let rules = match settings.rules {
                Some(rules) => rules,
                None => DetectRules::builtin().map_err(|e| Error::Internal(e.to_string()))?,
            };
            let policies = Settings::policies(settings.overrides, &rules);
            let policy_of = |app: &broscribe_detect::AppRule| {
                policies
                    .get(&app.id)
                    .copied()
                    .unwrap_or(app.policy)
                    .as_str()
            };
            let system = broscribe_platform_native::NativePlatform::system_info();
            let describe = |app: &broscribe_detect::AppRule| {
                json!({"app_id": app.id, "name": app.name, "policy": policy_of(app),
                    "installed": system.application_installed(&app.bundle_ids)})
            };
            let rule_apps = || rules.apps().iter().map(describe).collect::<Vec<_>>();
            if let Some(mut value) =
                super::daemon::optional_call(ctx.data_dir(), Call::DetectStatus)?
            {
                let listed = value
                    .get("apps")
                    .and_then(|a| a.as_array())
                    .is_some_and(|a| !a.is_empty());
                if value["enabled"] == false || !listed {
                    // Detection is off in the daemon: still list the apps
                    // and their policies for settings to show.
                    value["apps"] = json!(rule_apps());
                } else if let Some(apps) = value.get_mut("apps").and_then(|a| a.as_array_mut()) {
                    // The daemon reports live state; name and effective
                    // policy come from the same rules and config so both
                    // shapes carry them.
                    for entry in apps {
                        let rule = entry
                            .get("app_id")
                            .and_then(|id| id.as_str())
                            .and_then(|id| rules.app(id));
                        if let (Some(rule), Some(obj)) = (rule, entry.as_object_mut()) {
                            obj.insert("name".into(), json!(rule.name));
                            obj.insert("policy".into(), json!(policy_of(rule)));
                            obj.insert(
                                "installed".into(),
                                json!(system.application_installed(&rule.bundle_ids)),
                            );
                        }
                    }
                }
                if value.get("problem").is_none_or(serde_json::Value::is_null) {
                    value["problem"] = json!(problem);
                }
                return super::daemon::emit(ctx, value);
            }
            super::daemon::emit(
                ctx,
                json!({"running": false, "apps": rule_apps(), "problem": problem}),
            )
        }
        DetectCmd::Policy { app, policy } => {
            let result = set_policy(ctx, app, policy)?;
            super::daemon::emit(ctx, result)
        }
    }
}

pub(super) fn set_policy(
    ctx: &mut Ctx<'_>,
    app: String,
    policy: DetectPolicyArg,
) -> Result<serde_json::Value> {
    let rules =
        DetectRules::load(ctx.data_dir()).map_err(|e| Error::InvalidInput(e.to_string()))?;
    let rule = rules
        .app(&app)
        .or_else(|| rules.app_for_bundle(&app))
        .ok_or_else(|| Error::InvalidInput(format!("unknown detection app {app:?}")))?;
    let policy = match policy {
        DetectPolicyArg::Auto => "auto",
        DetectPolicyArg::Ask => "ask",
        DetectPolicyArg::Off => "off",
    };
    let id = rule.id.clone();
    Config::update(ctx.data_dir(), |c| {
        ctx.check_request_cancelled()?;
        c.detect.policies.insert(id, policy.into());
        Ok(())
    })?;
    let reloaded = match super::daemon::optional_call(ctx.data_dir(), Call::DetectReload) {
        Ok(answer) => answer.is_some(),
        Err(e) => {
            ctx.note(format!(
                "The policy is saved, but BroScribe Agent did not reload it ({}); it applies the policy at the latest after it restarts.",
                e.code()
            ));
            false
        }
    };
    Ok(json!({"app":rule.id,"policy":policy,"daemon_reloaded":reloaded}))
}
