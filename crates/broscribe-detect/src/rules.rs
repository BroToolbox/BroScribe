//! The `detect.toml` rules table: meeting apps, their policies and title
//! patterns, helper → app mapping and timing (design §9: data, not code).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use broscribe_platform::{Error, MicUser, Result};
use serde::{Deserialize, Serialize};

use crate::glob::glob_match;

/// The embedded default table.
pub const BUILTIN_RULES: &str = include_str!("detect.toml");

/// File name of the user override in the data directory.
pub const USER_RULES_FILE: &str = "detect.toml";

/// What happens when a meeting is detected. Ordered from least to most
/// automatic, so `min` picks the more cautious of two.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    /// Never record or ask.
    Off,
    /// Ask with Record / Ignore.
    Ask,
    /// Start recording.
    Auto,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Policy::Off => "off",
            Policy::Ask => "ask",
            Policy::Auto => "auto",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "off" => Ok(Policy::Off),
            "ask" => Ok(Policy::Ask),
            "auto" => Ok(Policy::Auto),
            other => Err(Error::InvalidInput(format!(
                "unknown detection policy {other:?} (auto, ask or off)"
            ))),
        }
    }
}

/// Per-app policy chosen by the user (settings / `broscribe detect policy`),
/// keyed by [`AppRule::id`]; overrides the rule's default policy.
pub type PolicyOverrides = HashMap<String, Policy>;

/// Start/end timing (plan M4 §5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// The mic must stay released this long before an end is considered.
    pub mic_release_debounce: Duration,
    /// After the debounce, the end needs the rule to stop matching or this
    /// long since the release.
    pub end_confirmation: Duration,
    /// No new detection for the same app this long after an end.
    pub cooldown: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            mic_release_debounce: Duration::from_secs(20),
            end_confirmation: Duration::from_secs(60),
            cooldown: Duration::from_secs(30),
        }
    }
}

/// Maps helper processes (by bundle id prefix) to their app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelperRule {
    pub prefix: String,
    /// The app's bundle id; `None` = the process the system holds
    /// responsible for the helper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

/// One meeting app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppRule {
    /// Stable key for policies and events (`teams`, `zoom`, `meet`, …).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Platform app identities (bundles, PipeWire ids/binaries, exe file names);
    /// a trailing `*` matches by prefix. Case-insensitive.
    pub bundle_ids: Vec<String>,
    /// When non-empty, a window title must match one of these globs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub title_patterns: Vec<String>,
    pub policy: Policy,
    /// Policy cap when titles are unreadable (no Screen Recording) and the
    /// rule needs them; defaults to `policy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_without_titles: Option<Policy>,
}

impl AppRule {
    pub fn matches_bundle(&self, bundle_id: &str) -> bool {
        self.bundle_ids
            .iter()
            .any(|pattern| bundle_matches(pattern, bundle_id))
    }

    pub fn needs_title(&self) -> bool {
        !self.title_patterns.is_empty()
    }

    pub fn title_matches(&self, title: &str) -> bool {
        self.title_patterns.iter().any(|p| glob_match(p, title))
    }
}

fn bundle_matches(pattern: &str, bundle_id: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => starts_with_ci(bundle_id, prefix),
        None => pattern.eq_ignore_ascii_case(bundle_id),
    }
}

fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// The resolved rules table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetectRules {
    timing: Timing,
    helpers: Vec<HelperRule>,
    apps: Vec<AppRule>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RulesFile {
    version: Option<u32>,
    #[serde(default)]
    timing: TimingFile,
    #[serde(default)]
    helpers: Vec<HelperRule>,
    #[serde(default)]
    apps: Vec<AppRule>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TimingFile {
    mic_release_debounce_s: Option<u64>,
    end_confirmation_s: Option<u64>,
    cooldown_s: Option<u64>,
}

impl RulesFile {
    fn parse(text: &str, what: &str) -> Result<Self> {
        let file: RulesFile =
            toml::from_str(text).map_err(|e| Error::InvalidInput(format!("{what}: {e}")))?;
        if let Some(v) = file.version
            && v != 1
        {
            return Err(Error::InvalidInput(format!(
                "{what}: unsupported version {v} (expected 1)"
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for app in &file.apps {
            if app.id.trim().is_empty() {
                return Err(Error::InvalidInput(format!("{what}: app with an empty id")));
            }
            if !seen.insert(app.id.as_str()) {
                return Err(Error::InvalidInput(format!(
                    "{what}: app id {:?} appears twice",
                    app.id
                )));
            }
            if app.bundle_ids.iter().all(|b| b.trim().is_empty()) {
                return Err(Error::InvalidInput(format!(
                    "{what}: app {:?} has no bundle ids",
                    app.id
                )));
            }
        }
        if file.helpers.iter().any(|h| h.prefix.trim().is_empty()) {
            return Err(Error::InvalidInput(format!(
                "{what}: helper with an empty prefix"
            )));
        }
        Ok(file)
    }
}

impl DetectRules {
    /// The embedded default table.
    pub fn builtin() -> Result<Self> {
        let file = RulesFile::parse(BUILTIN_RULES, "built-in detect.toml")?;
        let mut rules = Self {
            timing: Timing::default(),
            helpers: Vec::new(),
            apps: Vec::new(),
        };
        rules.apply(file);
        Ok(rules)
    }

    /// Where the user override lives.
    pub fn user_path(data_dir: &Path) -> PathBuf {
        data_dir.join(USER_RULES_FILE)
    }

    /// The default table with `<data_dir>/detect.toml` applied when present.
    pub fn load(data_dir: &Path) -> Result<Self> {
        let mut rules = Self::builtin()?;
        let path = Self::user_path(data_dir);
        match std::fs::read_to_string(&path) {
            Ok(text) => rules.apply_override(&text, &path.display().to_string())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::Io {
                    context: format!("reading {}", path.display()),
                    source,
                });
            }
        }
        Ok(rules)
    }

    /// Applies an override table: apps with a known id replace the default
    /// one, new ids are added, timing keys override one by one, and the
    /// override's helpers are tried before the defaults.
    pub fn apply_override(&mut self, text: &str, what: &str) -> Result<()> {
        let file = RulesFile::parse(text, what)?;
        self.apply(file);
        Ok(())
    }

    fn apply(&mut self, file: RulesFile) {
        let t = file.timing;
        if let Some(s) = t.mic_release_debounce_s {
            self.timing.mic_release_debounce = Duration::from_secs(s);
        }
        if let Some(s) = t.end_confirmation_s {
            self.timing.end_confirmation = Duration::from_secs(s);
        }
        if let Some(s) = t.cooldown_s {
            self.timing.cooldown = Duration::from_secs(s);
        }
        let mut helpers = file.helpers;
        helpers.append(&mut self.helpers);
        self.helpers = helpers;
        for app in file.apps {
            match self.apps.iter_mut().find(|a| a.id == app.id) {
                Some(existing) => *existing = app,
                None => self.apps.push(app),
            }
        }
    }

    pub fn timing(&self) -> Timing {
        self.timing
    }

    pub fn helpers(&self) -> &[HelperRule] {
        &self.helpers
    }

    pub fn apps(&self) -> &[AppRule] {
        &self.apps
    }

    pub fn app(&self, id: &str) -> Option<&AppRule> {
        self.apps.iter().find(|a| a.id == id)
    }

    /// The meeting app a bundle id belongs to (after helper mapping).
    pub fn app_for_bundle(&self, bundle_id: &str) -> Option<&AppRule> {
        self.apps.iter().find(|a| a.matches_bundle(bundle_id))
    }

    fn helper_for(&self, bundle_id: &str) -> Option<&HelperRule> {
        self.helpers
            .iter()
            .find(|h| starts_with_ci(bundle_id, &h.prefix))
    }

    /// `bundle_id` with helper prefixes mapped to their named parent app.
    /// Helpers without a named parent (resolved through the responsible
    /// process) and other bundles come back unchanged.
    pub fn map_helper(&self, bundle_id: &str) -> String {
        match self.helper_for(bundle_id).and_then(|h| h.parent.as_deref()) {
            Some(parent) => parent.to_owned(),
            None => bundle_id.to_owned(),
        }
    }

    /// The app bundle id a mic-using process stands for:
    /// 1. a helper prefix with a named parent → that parent;
    /// 2. a helper prefix without parent → the responsible process's app;
    /// 3. a bundle id that is a meeting app → itself;
    /// 4. otherwise the responsible process's app when there is one (an
    ///    unlisted helper), else the process's own bundle id.
    pub fn resolve_app_bundle(&self, user: &MicUser) -> Option<String> {
        let responsible = || {
            user.responsible_bundle_id
                .as_deref()
                .map(|b| self.map_helper(b))
        };
        let Some(own) = user.bundle_id.as_deref() else {
            return responsible();
        };
        if let Some(helper) = self.helper_for(own) {
            return match &helper.parent {
                Some(parent) => Some(parent.clone()),
                None => responsible(),
            };
        }
        if self.app_for_bundle(own).is_some() {
            return Some(own.to_owned());
        }
        responsible().or_else(|| Some(own.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(bundle: Option<&str>, responsible: Option<&str>) -> MicUser {
        MicUser {
            pid: 200,
            bundle_id: bundle.map(str::to_owned),
            responsible_pid: responsible.map(|_| 100),
            responsible_bundle_id: responsible.map(str::to_owned),
        }
    }

    #[test]
    fn builtin_table_loads() {
        let rules = DetectRules::builtin().unwrap();
        assert_eq!(rules.timing(), Timing::default());
        let ids: Vec<&str> = rules.apps().iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["teams", "zoom", "meet", "slack", "discord", "webex"]);
        assert_eq!(
            rules.app_for_bundle("com.microsoft.teams2").unwrap().id,
            "teams"
        );
        assert_eq!(rules.app_for_bundle("us.zoom.xos").unwrap().id, "zoom");
        let meet = rules.app("meet").unwrap();
        assert!(meet.needs_title());
        assert_eq!(meet.policy_without_titles, Some(Policy::Ask));
        // Prefix entries (Meet PWA shims) and case-insensitive ids.
        assert_eq!(
            rules
                .app_for_bundle("com.google.Chrome.app.abcdef")
                .unwrap()
                .id,
            "meet"
        );
        assert_eq!(rules.app_for_bundle("COM.APPLE.SAFARI").unwrap().id, "meet");
        assert!(rules.app_for_bundle("com.example.Recorder").is_none());
    }

    #[test]
    fn native_microphone_identities_resolve_to_meeting_apps() {
        let rules = DetectRules::builtin().unwrap();
        for (identity, expected) in [
            ("teams", "teams"),
            ("com.microsoft.Teams", "teams"),
            ("ms-teams.exe", "teams"),
            ("Teams.exe", "teams"),
            ("zoom", "zoom"),
            ("us.zoom.Zoom", "zoom"),
            ("Zoom.exe", "zoom"),
            ("chrome", "meet"),
            ("google-chrome", "meet"),
            ("com.google.Chrome", "meet"),
            ("chrome.exe", "meet"),
            ("microsoft-edge", "meet"),
            ("msedge", "meet"),
            ("com.microsoft.Edge", "meet"),
            ("msedge.exe", "meet"),
            ("slack", "slack"),
            ("com.slack.Slack", "slack"),
            ("slack.exe", "slack"),
            ("discord", "discord"),
            ("com.discordapp.Discord", "discord"),
            ("Discord.exe", "discord"),
            ("webex", "webex"),
            ("com.cisco.Webex", "webex"),
            ("CiscoCollabHost.exe", "webex"),
            ("WebexHost.exe", "webex"),
        ] {
            let resolved = rules
                .resolve_app_bundle(&user(Some(identity), None))
                .unwrap();
            assert_eq!(
                rules.app_for_bundle(&resolved).map(|app| app.id.as_str()),
                Some(expected),
                "{identity}"
            );
        }
    }

    #[test]
    fn helpers_map_to_parent_apps() {
        let rules = DetectRules::builtin().unwrap();
        // Named parent.
        assert_eq!(
            rules
                .resolve_app_bundle(&user(Some("com.google.Chrome.helper"), None))
                .as_deref(),
            Some("com.google.Chrome")
        );
        assert_eq!(
            rules
                .resolve_app_bundle(&user(Some("com.google.Chrome.helper.renderer"), None))
                .as_deref(),
            Some("com.google.Chrome")
        );
        assert_eq!(
            rules
                .resolve_app_bundle(&user(Some("com.microsoft.teams2.helper"), None))
                .as_deref(),
            Some("com.microsoft.teams2")
        );
        // Responsible process (WebKit GPU process → Safari).
        assert_eq!(
            rules
                .resolve_app_bundle(&user(
                    Some("com.apple.WebKit.GPU"),
                    Some("com.apple.Safari")
                ))
                .as_deref(),
            Some("com.apple.Safari")
        );
        assert_eq!(
            rules.resolve_app_bundle(&user(Some("com.apple.WebKit.GPU"), None)),
            None
        );
        // A meeting app stays itself even when something else launched it.
        assert_eq!(
            rules
                .resolve_app_bundle(&user(Some("us.zoom.xos"), Some("com.apple.Terminal")))
                .as_deref(),
            Some("us.zoom.xos")
        );
        // Unlisted helper → its responsible app (mapped through helpers too).
        assert_eq!(
            rules
                .resolve_app_bundle(&user(
                    Some("com.example.SomeHelper"),
                    Some("com.hnc.Discord.helper")
                ))
                .as_deref(),
            Some("com.hnc.Discord")
        );
        // No bundle id at all → responsible app.
        assert_eq!(
            rules
                .resolve_app_bundle(&user(None, Some("com.tinyspeck.slackmacgap")))
                .as_deref(),
            Some("com.tinyspeck.slackmacgap")
        );
        // Plain non-meeting app.
        assert_eq!(
            rules
                .resolve_app_bundle(&user(Some("com.example.Recorder"), None))
                .as_deref(),
            Some("com.example.Recorder")
        );
        assert_eq!(rules.resolve_app_bundle(&user(None, None)), None);
    }

    #[test]
    fn user_override_replaces_adds_and_tunes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            DetectRules::user_path(dir.path()),
            r#"
version = 1

[timing]
cooldown_s = 5

[[helpers]]
prefix = "com.example.Voice.helper"
parent = "com.example.Voice"

[[apps]]
id = "zoom"
name = "Zoom (work)"
bundle_ids = ["us.zoom.xos"]
policy = "ask"

[[apps]]
id = "voice"
name = "Example Voice"
bundle_ids = ["com.example.Voice"]
policy = "auto"
"#,
        )
        .unwrap();
        let rules = DetectRules::load(dir.path()).unwrap();
        let timing = rules.timing();
        assert_eq!(timing.cooldown, Duration::from_secs(5));
        assert_eq!(timing.mic_release_debounce, Duration::from_secs(20));
        assert_eq!(timing.end_confirmation, Duration::from_secs(60));
        let zoom = rules.app("zoom").unwrap();
        assert_eq!(zoom.name, "Zoom (work)");
        assert_eq!(zoom.policy, Policy::Ask);
        assert_eq!(rules.apps().len(), 7);
        assert_eq!(rules.apps().last().unwrap().id, "voice");
        assert_eq!(rules.helpers()[0].prefix, "com.example.Voice.helper");
        assert_eq!(
            rules
                .resolve_app_bundle(&user(Some("com.example.Voice.helper"), None))
                .as_deref(),
            Some("com.example.Voice")
        );
        assert_eq!(
            rules.app_for_bundle("com.example.Voice").unwrap().id,
            "voice"
        );
    }

    #[test]
    fn missing_override_uses_builtin() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            DetectRules::load(dir.path()).unwrap(),
            DetectRules::builtin().unwrap()
        );
    }

    #[test]
    fn bad_override_is_rejected() {
        let mut rules = DetectRules::builtin().unwrap();
        let bad_policy = r#"
[[apps]]
id = "x"
name = "X"
bundle_ids = ["com.example.X"]
policy = "sometimes"
"#;
        assert!(rules.apply_override(bad_policy, "test").is_err());
        let duplicate = r#"
[[apps]]
id = "x"
name = "X"
bundle_ids = ["com.example.X"]
policy = "auto"

[[apps]]
id = "x"
name = "X again"
bundle_ids = ["com.example.X"]
policy = "auto"
"#;
        assert!(rules.apply_override(duplicate, "test").is_err());
        let no_bundles = r#"
[[apps]]
id = "x"
name = "X"
bundle_ids = []
policy = "auto"
"#;
        assert!(rules.apply_override(no_bundles, "test").is_err());
        assert!(rules.apply_override("version = 2", "test").is_err());
        assert!(rules.apply_override("unknown_key = 1", "test").is_err());
        // Failed overrides leave the table unchanged.
        assert_eq!(rules, DetectRules::builtin().unwrap());
    }

    #[test]
    fn policy_order_and_parse() {
        assert!(Policy::Off < Policy::Ask && Policy::Ask < Policy::Auto);
        assert_eq!(Policy::Auto.min(Policy::Ask), Policy::Ask);
        for p in [Policy::Off, Policy::Ask, Policy::Auto] {
            assert_eq!(Policy::parse(p.as_str()).unwrap(), p);
        }
        assert!(Policy::parse("always").is_err());
    }
}
