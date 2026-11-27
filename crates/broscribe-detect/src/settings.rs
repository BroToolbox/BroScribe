//! The user's detection settings: rules (`detect.toml`) and per-app policy
//! overrides (`config.toml` `[detect.policies]`). A part that does not load
//! never weakens the user's policies: broken rules leave the caller's
//! current ones (the built-in rules when it has none) with the valid
//! overrides; an invalid override is skipped alone; an unreadable
//! `config.toml` caps every app at `ask`.

use std::collections::BTreeMap;
use std::path::Path;

use crate::rules::{DetectRules, Policy, PolicyOverrides};

pub struct Settings {
    /// `None`: `detect.toml` does not load.
    pub rules: Option<DetectRules>,
    /// `None`: `config.toml` does not load.
    pub overrides: Option<PolicyOverrides>,
    problems: Vec<String>,
}

impl Settings {
    /// `policies`: `config.toml`'s `[detect.policies]`, or why `config.toml`
    /// does not load.
    pub fn load(data_dir: &Path, policies: Result<&BTreeMap<String, String>, String>) -> Self {
        let mut problems = Vec::new();
        let rules = DetectRules::load(data_dir)
            .map_err(|e| problems.push(format!("detect.toml: {e}")))
            .ok();
        let overrides = match policies {
            Ok(policies) => Some(
                policies
                    .iter()
                    .filter_map(|(app, policy)| match Policy::parse(policy) {
                        Ok(policy) => Some((app.clone(), policy)),
                        Err(e) => {
                            problems.push(format!("config.toml policy for {app:?}: {e}"));
                            None
                        }
                    })
                    .collect(),
            ),
            Err(e) => {
                problems.push(format!(
                    "config.toml: {e}; every meeting app is capped at ask"
                ));
                None
            }
        };
        Self {
            rules,
            overrides,
            problems,
        }
    }

    /// What does not load, or `None` when everything does.
    pub fn problem(&self) -> Option<String> {
        (!self.problems.is_empty()).then(|| self.problems.join("; "))
    }

    /// The overrides to run `rules` with: the loaded ones, or every app
    /// capped at `ask` when `config.toml` does not load.
    pub fn policies(overrides: Option<PolicyOverrides>, rules: &DetectRules) -> PolicyOverrides {
        overrides.unwrap_or_else(|| {
            rules
                .apps()
                .iter()
                .map(|app| (app.id.clone(), app.policy.min(Policy::Ask)))
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broken_parts_never_weaken_policies() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            DetectRules::user_path(dir.path()),
            "[[apps]]\nnot_a_field = 1\n",
        )
        .unwrap();
        let policies = BTreeMap::from([
            ("zoom".to_owned(), "off".to_owned()),
            ("teams".to_owned(), "sometimes".to_owned()),
        ]);
        let settings = Settings::load(dir.path(), Ok(&policies));
        assert!(settings.rules.is_none());
        let problem = settings.problem().unwrap();
        assert!(
            problem.contains("detect.toml") && problem.contains("\"teams\""),
            "{problem}"
        );
        let overrides = settings.overrides.unwrap();
        assert_eq!(
            overrides,
            PolicyOverrides::from([("zoom".to_owned(), Policy::Off)])
        );

        let builtin = DetectRules::builtin().unwrap();
        let capped = Settings::policies(
            Settings::load(dir.path(), Err("synthetic parse error".into())).overrides,
            &builtin,
        );
        assert_eq!(capped["zoom"], Policy::Ask);
        assert!(capped.values().all(|p| *p <= Policy::Ask));
        assert_eq!(capped.len(), builtin.apps().len());
    }
}
