//! One poll of mic users and windows → what each meeting app is doing.

use std::collections::{BTreeMap, HashSet};

use broscribe_platform::{MicUser, WindowInfo};

use crate::rules::{AppRule, DetectRules};

/// What the window titles say about an app's rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TitleState {
    /// The rule has no title patterns.
    NotRequired,
    /// A window title matched; carries that title (e.g. for the meeting name).
    Matched(String),
    /// Titles are readable and none matches.
    NotMatched,
    /// Titles are unreadable (no Screen Recording) or the app shows no window.
    Unknown,
}

/// One meeting app's state in one poll.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sighting {
    /// Some process of the app holds a microphone.
    pub mic_held: bool,
    /// The app bundle id holding the mic (or owning the windows).
    pub bundle_id: Option<String>,
    /// The app has at least one window.
    pub has_windows: bool,
    pub title: TitleState,
}

impl Sighting {
    /// The app is neither holding the mic nor showing a window.
    pub fn absent(rule: &AppRule) -> Self {
        Self {
            mic_held: false,
            bundle_id: None,
            has_windows: false,
            title: if rule.needs_title() {
                TitleState::NotMatched
            } else {
                TitleState::NotRequired
            },
        }
    }

    /// Start condition: the app holds the mic and its rule does not rule it
    /// out (a readable, non-matching title does).
    pub fn should_start(&self) -> bool {
        self.mic_held && self.title != TitleState::NotMatched
    }

    /// The rule still matches: used to confirm an end. A readable
    /// non-matching title, or an app with neither mic nor windows, no
    /// longer matches.
    pub fn still_matches(&self) -> bool {
        match self.title {
            TitleState::NotMatched => false,
            _ => self.mic_held || self.has_windows,
        }
    }

    pub fn matched_title(&self) -> Option<&str> {
        match &self.title {
            TitleState::Matched(t) => Some(t),
            _ => None,
        }
    }
}

/// Builds a [`Sighting`] for every meeting app that holds the mic or has a
/// window, keyed by [`AppRule::id`]. Apps missing from the map are absent.
pub fn observe(
    rules: &DetectRules,
    users: &[MicUser],
    windows: &[WindowInfo],
) -> BTreeMap<String, Sighting> {
    let mut out: BTreeMap<String, Sighting> = BTreeMap::new();
    // Pids (and responsible pids) of each app's mic users, so windows with
    // no bundle id still count for that app.
    let mut pids: BTreeMap<String, HashSet<u32>> = BTreeMap::new();
    for user in users {
        let Some(bundle) = rules.resolve_app_bundle(user) else {
            continue;
        };
        let Some(rule) = rules.app_for_bundle(&bundle) else {
            continue;
        };
        let s = out
            .entry(rule.id.clone())
            .or_insert_with(|| Sighting::absent(rule));
        s.mic_held = true;
        s.bundle_id.get_or_insert(bundle);
        let set = pids.entry(rule.id.clone()).or_default();
        set.insert(user.pid);
        set.extend(user.responsible_pid);
    }

    // Per app: any window, any readable title, the first matching title.
    let mut seen_title: HashSet<String> = HashSet::new();
    let mut matched: BTreeMap<String, String> = BTreeMap::new();
    for window in windows {
        let rule = window
            .bundle_id
            .as_deref()
            .and_then(|b| rules.app_for_bundle(&rules.map_helper(b)))
            .or_else(|| {
                pids.iter()
                    .find(|(_, set)| set.contains(&window.pid))
                    .and_then(|(id, _)| rules.app(id))
            });
        let Some(rule) = rule else {
            continue;
        };
        let s = out
            .entry(rule.id.clone())
            .or_insert_with(|| Sighting::absent(rule));
        s.has_windows = true;
        if s.bundle_id.is_none() {
            s.bundle_id = window.bundle_id.as_deref().map(|b| rules.map_helper(b));
        }
        if let Some(title) = &window.title {
            seen_title.insert(rule.id.clone());
            if rule.needs_title() && !matched.contains_key(&rule.id) && rule.title_matches(title) {
                matched.insert(rule.id.clone(), title.clone());
            }
        }
    }

    for (id, s) in &mut out {
        let Some(rule) = rules.app(id) else {
            continue;
        };
        s.title = if !rule.needs_title() {
            TitleState::NotRequired
        } else if let Some(title) = matched.remove(id) {
            TitleState::Matched(title)
        } else if seen_title.contains(id) {
            TitleState::NotMatched
        } else if s.has_windows || s.mic_held {
            TitleState::Unknown
        } else {
            TitleState::NotMatched
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mic(pid: u32, bundle: &str) -> MicUser {
        MicUser {
            pid,
            bundle_id: Some(bundle.into()),
            ..MicUser::default()
        }
    }

    fn window(pid: u32, bundle: &str, title: Option<&str>) -> WindowInfo {
        WindowInfo {
            pid,
            app: "App".into(),
            bundle_id: Some(bundle.into()),
            title: title.map(str::to_owned),
        }
    }

    #[test]
    fn teams_needs_no_title() {
        let rules = DetectRules::builtin().unwrap();
        let s = observe(&rules, &[mic(10, "com.microsoft.teams2")], &[]);
        let teams = &s["teams"];
        assert!(teams.mic_held && teams.should_start());
        assert_eq!(teams.title, TitleState::NotRequired);
        assert_eq!(teams.bundle_id.as_deref(), Some("com.microsoft.teams2"));
    }

    #[test]
    fn meet_title_states() {
        let rules = DetectRules::builtin().unwrap();
        let users = [mic(21, "com.google.Chrome.helper")];

        let on_meet = observe(
            &rules,
            &users,
            &[window(20, "com.google.Chrome", Some("Meet - abc-defg-hij"))],
        );
        assert_eq!(
            on_meet["meet"].title,
            TitleState::Matched("Meet - abc-defg-hij".into())
        );
        assert!(on_meet["meet"].should_start());

        let other_site = observe(
            &rules,
            &users,
            &[window(
                20,
                "com.google.Chrome",
                Some("Voice memo - Example"),
            )],
        );
        assert_eq!(other_site["meet"].title, TitleState::NotMatched);
        assert!(!other_site["meet"].should_start());

        let no_titles = observe(&rules, &users, &[window(20, "com.google.Chrome", None)]);
        assert_eq!(no_titles["meet"].title, TitleState::Unknown);
        assert!(no_titles["meet"].should_start());

        // Window left after the call, mic released: still matches by title.
        let after = observe(
            &rules,
            &[],
            &[window(20, "com.google.Chrome", Some("Meet - abc-defg-hij"))],
        );
        assert!(!after["meet"].mic_held && after["meet"].still_matches());
    }

    #[test]
    fn windows_without_bundle_count_by_mic_pid() {
        let rules = DetectRules::builtin().unwrap();
        let user = MicUser {
            pid: 31,
            bundle_id: Some("com.apple.WebKit.GPU".into()),
            responsible_pid: Some(30),
            responsible_bundle_id: Some("com.apple.Safari".into()),
        };
        let w = WindowInfo {
            pid: 30,
            app: "Safari".into(),
            bundle_id: None,
            title: Some("Google Meet".into()),
        };
        let s = observe(&rules, &[user], &[w]);
        assert!(matches!(s["meet"].title, TitleState::Matched(_)));
    }

    #[test]
    fn unrelated_apps_are_ignored() {
        let rules = DetectRules::builtin().unwrap();
        let s = observe(
            &rules,
            &[mic(40, "com.example.Recorder")],
            &[window(40, "com.example.Recorder", Some("Meet - x"))],
        );
        assert!(s.is_empty());
    }
}
