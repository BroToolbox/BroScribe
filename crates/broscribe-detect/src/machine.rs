//! The detection state machine (plan M4 §5), pure: time comes in as an
//! [`Instant`] on every call.
//!
//! Per meeting app:
//! - **start** at once when the app holds the mic and its rule matches —
//!   `auto` emits [`DetectEvent::MeetingDetected`], `ask` emits
//!   [`DetectEvent::AskUser`] and waits for [`DetectionMachine::answer`],
//!   `off` does nothing;
//! - **end** after the mic stayed released for the debounce (20 s) *and* the
//!   end is confirmed: the rule no longer matches, or the confirmation time
//!   (60 s since the release) passed;
//! - **cooldown** (30 s) before the same app can start again.
//!
//! Re-acquiring the mic before an end is confirmed keeps the meeting going.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::observe::{Sighting, TitleState};
use crate::rules::{AppRule, DetectRules, Policy, PolicyOverrides, Timing};

/// Why a meeting ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// Mic released past the debounce and the rule stopped matching.
    RuleNoLongerMatches,
    /// Mic released for the whole confirmation time.
    ConfirmationTimeout,
}

/// What the detector tells the daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DetectEvent {
    /// Record this meeting (policy `auto`, or `ask` answered with Record).
    MeetingDetected {
        app_id: String,
        app_name: String,
        bundle_id: Option<String>,
        /// The matching window title, when titles are readable.
        title: Option<String>,
        /// `auto`, or `ask` when the user accepted.
        policy: Policy,
    },
    /// Show Record / Ignore for this meeting.
    AskUser {
        app_id: String,
        app_name: String,
        bundle_id: Option<String>,
        title: Option<String>,
    },
    /// The meeting asked about ended before an answer; withdraw the prompt.
    AskExpired { app_id: String },
    /// A detected meeting ended; stop its recording.
    MeetingEnded { app_id: String, reason: EndReason },
}

/// The user's answer to [`DetectEvent::AskUser`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    Record,
    Ignore,
}

/// Per-app state, for status displays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectState {
    Idle,
    /// Waiting for Record / Ignore.
    Asking,
    /// A meeting is detected (recording).
    Active,
    /// Ignored by the user (or stopped manually) until the app releases the mic.
    Ignored,
    /// Just ended; no new detection for this app until the cooldown passes.
    Cooldown,
}

#[derive(Clone, Debug)]
struct Pending {
    app_name: String,
    bundle_id: Option<String>,
    title: Option<String>,
    /// The policy this state was reached under: `auto` for a detected
    /// start, `ask` for a prompt or a start the user accepted.
    policy: Policy,
    /// Titles were unreadable at detection, so the rule's cap applies.
    titles_unknown: bool,
}

#[derive(Clone, Debug)]
enum AppState {
    Idle,
    Asking {
        released_at: Option<Instant>,
        pending: Pending,
    },
    Active {
        released_at: Option<Instant>,
        /// What the start event carried, to start it again after another
        /// app's recording ends.
        pending: Pending,
    },
    Ignored {
        released_at: Option<Instant>,
    },
    Cooldown {
        until: Instant,
    },
}

impl AppState {
    fn public(&self) -> DetectState {
        match self {
            AppState::Idle => DetectState::Idle,
            AppState::Asking { .. } => DetectState::Asking,
            AppState::Active { .. } => DetectState::Active,
            AppState::Ignored { .. } => DetectState::Ignored,
            AppState::Cooldown { .. } => DetectState::Cooldown,
        }
    }
}

/// Tracks the release of the mic: `None` while held, else how long ago it
/// was released (the first release time is kept across polls).
fn released_for(
    released_at: &mut Option<Instant>,
    mic_held: bool,
    now: Instant,
) -> Option<Duration> {
    if mic_held {
        *released_at = None;
        return None;
    }
    let since = *released_at.get_or_insert(now);
    Some(now.saturating_duration_since(since))
}

fn policy_for(rule: &AppRule, overrides: &PolicyOverrides, titles_unknown: bool) -> Policy {
    let policy = overrides.get(&rule.id).copied().unwrap_or(rule.policy);
    if rule.needs_title() && titles_unknown {
        policy.min(rule.policy_without_titles.unwrap_or(policy))
    } else {
        policy
    }
}

/// An app whose meeting is detected (`Active`) or asked about (`Asking`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenDetection {
    pub app_id: String,
    pub state: DetectState,
    pub title: Option<String>,
}

#[derive(Debug, Default)]
pub struct DetectionMachine {
    states: HashMap<String, AppState>,
}

impl DetectionMachine {
    pub fn new() -> Self {
        Self::default()
    }

    /// The state of one app (`Idle` for apps never seen).
    pub fn state(&self, app_id: &str) -> DetectState {
        self.states
            .get(app_id)
            .map_or(DetectState::Idle, AppState::public)
    }

    /// Whether any app is past `Idle`/`Cooldown` (the poller then keeps
    /// reading windows to confirm ends).
    pub fn has_open_apps(&self) -> bool {
        self.states.values().any(|s| {
            matches!(
                s,
                AppState::Asking { .. } | AppState::Active { .. } | AppState::Ignored { .. }
            )
        })
    }

    /// Apps in `Active` or `Asking`, by ID. While a recording runs the
    /// daemon leaves them be; once it stops it starts the active ones and
    /// asks about the asking ones (back-to-back meetings).
    pub fn open_detections(&self) -> Vec<OpenDetection> {
        let mut open: Vec<_> = self
            .states
            .iter()
            .filter_map(|(app_id, state)| match state {
                AppState::Active { pending, .. } | AppState::Asking { pending, .. } => {
                    Some(OpenDetection {
                        app_id: app_id.clone(),
                        state: state.public(),
                        title: pending.title.clone(),
                    })
                }
                _ => None,
            })
            .collect();
        open.sort_by(|a, b| a.app_id.cmp(&b.app_id));
        open
    }

    /// Advances every app of `rules` with one poll's sightings (apps missing
    /// from `sightings` are absent).
    pub fn step(
        &mut self,
        now: Instant,
        rules: &DetectRules,
        overrides: &PolicyOverrides,
        sightings: &BTreeMap<String, Sighting>,
    ) -> Vec<DetectEvent> {
        let timing = rules.timing();
        let mut events = Vec::new();
        for rule in rules.apps() {
            let absent;
            let sighting = match sightings.get(&rule.id) {
                Some(s) => s,
                None => {
                    absent = Sighting::absent(rule);
                    &absent
                }
            };
            let state = self.states.entry(rule.id.clone()).or_insert(AppState::Idle);
            if let Some(event) = advance(state, now, timing, rule, overrides, sighting) {
                events.push(event);
            }
        }
        // Apps removed from the rules table drop their state.
        self.states.retain(|id, _| rules.app(id).is_some());
        events
    }

    /// Applies changed policies to open detections, so a meeting deferred
    /// behind another's recording is admitted under the current policy:
    /// - `off`: `Active`/`Asking` → `Ignored` until the app releases the
    ///   mic (a shown prompt is withdrawn);
    /// - `ask`: an `auto`-detected `Active` → `Asking` ([`DetectEvent::AskUser`]);
    ///   a start the user accepted stays;
    /// - `auto`: `Asking` → `Active` (prompt withdrawn, [`DetectEvent::MeetingDetected`]).
    ///
    /// `keep` is the app whose recording runs: it is left as it is, so the
    /// end of its meeting still stops the recording.
    pub fn reclassify(
        &mut self,
        rules: &DetectRules,
        overrides: &PolicyOverrides,
        keep: Option<&str>,
    ) -> Vec<DetectEvent> {
        let mut ids: Vec<_> = self
            .states
            .keys()
            .filter(|id| Some(id.as_str()) != keep)
            .cloned()
            .collect();
        ids.sort();
        let mut events = Vec::new();
        for app_id in ids {
            let (Some(rule), Some(state)) = (rules.app(&app_id), self.states.get_mut(&app_id))
            else {
                continue;
            };
            let asking = matches!(state, AppState::Asking { .. });
            let (AppState::Active {
                released_at,
                pending,
            }
            | AppState::Asking {
                released_at,
                pending,
            }) = state
            else {
                continue;
            };
            let released_at = *released_at;
            let mut pending = pending.clone();
            let policy = policy_for(rule, overrides, pending.titles_unknown);
            *state = match (policy, asking, pending.policy) {
                (Policy::Off, ..) => {
                    if asking {
                        events.push(DetectEvent::AskExpired { app_id });
                    }
                    AppState::Ignored { released_at }
                }
                (Policy::Ask, false, Policy::Auto) => {
                    pending.policy = Policy::Ask;
                    events.push(DetectEvent::AskUser {
                        app_id,
                        app_name: pending.app_name.clone(),
                        bundle_id: pending.bundle_id.clone(),
                        title: pending.title.clone(),
                    });
                    AppState::Asking {
                        released_at,
                        pending,
                    }
                }
                (Policy::Auto, true, _) => {
                    pending.policy = Policy::Auto;
                    events.push(DetectEvent::AskExpired {
                        app_id: app_id.clone(),
                    });
                    events.push(DetectEvent::MeetingDetected {
                        app_id,
                        app_name: pending.app_name.clone(),
                        bundle_id: pending.bundle_id.clone(),
                        title: pending.title.clone(),
                        policy: Policy::Auto,
                    });
                    AppState::Active {
                        released_at,
                        pending,
                    }
                }
                _ => continue,
            };
        }
        events
    }

    /// The user's answer to an [`DetectEvent::AskUser`] for `app_id`.
    /// `Record` starts the meeting; `Ignore` mutes the app until it releases
    /// the mic. Answers for apps not asking are ignored.
    pub fn answer(&mut self, app_id: &str, answer: Answer) -> Vec<DetectEvent> {
        let Some(state) = self.states.get_mut(app_id) else {
            return Vec::new();
        };
        let AppState::Asking {
            released_at,
            pending,
        } = state
        else {
            return Vec::new();
        };
        let released_at = *released_at;
        match answer {
            Answer::Record => {
                let event = DetectEvent::MeetingDetected {
                    app_id: app_id.to_owned(),
                    app_name: pending.app_name.clone(),
                    bundle_id: pending.bundle_id.clone(),
                    title: pending.title.clone(),
                    policy: Policy::Ask,
                };
                *state = AppState::Active {
                    released_at,
                    pending: pending.clone(),
                };
                vec![event]
            }
            Answer::Ignore => {
                *state = AppState::Ignored { released_at };
                Vec::new()
            }
        }
    }

    /// The user stopped a detected recording (or dismissed a prompt) by
    /// hand: no events; the app stays quiet until it releases the mic.
    pub fn dismiss(&mut self, app_id: &str) {
        if let Some(state) = self.states.get_mut(app_id)
            && matches!(state, AppState::Active { .. } | AppState::Asking { .. })
        {
            *state = AppState::Ignored { released_at: None };
        }
    }
}

fn advance(
    state: &mut AppState,
    now: Instant,
    timing: Timing,
    rule: &AppRule,
    overrides: &PolicyOverrides,
    sighting: &Sighting,
) -> Option<DetectEvent> {
    if let AppState::Cooldown { until } = state {
        if now < *until {
            return None;
        }
        *state = AppState::Idle;
    }
    let cooldown = AppState::Cooldown {
        until: now + timing.cooldown,
    };
    match state {
        AppState::Cooldown { .. } => None,
        AppState::Idle => {
            if !sighting.should_start() {
                return None;
            }
            let titles_unknown = sighting.title == TitleState::Unknown;
            let policy = policy_for(rule, overrides, titles_unknown);
            let pending = Pending {
                app_name: rule.name.clone(),
                bundle_id: sighting.bundle_id.clone(),
                title: sighting.matched_title().map(str::to_owned),
                policy,
                titles_unknown,
            };
            match policy {
                Policy::Off => None,
                Policy::Auto => {
                    let event = DetectEvent::MeetingDetected {
                        app_id: rule.id.clone(),
                        app_name: pending.app_name.clone(),
                        bundle_id: pending.bundle_id.clone(),
                        title: pending.title.clone(),
                        policy: Policy::Auto,
                    };
                    *state = AppState::Active {
                        released_at: None,
                        pending,
                    };
                    Some(event)
                }
                Policy::Ask => {
                    let event = DetectEvent::AskUser {
                        app_id: rule.id.clone(),
                        app_name: pending.app_name.clone(),
                        bundle_id: pending.bundle_id.clone(),
                        title: pending.title.clone(),
                    };
                    *state = AppState::Asking {
                        released_at: None,
                        pending,
                    };
                    Some(event)
                }
            }
        }
        AppState::Active { released_at, .. } => {
            let released = released_for(released_at, sighting.mic_held, now)?;
            if released < timing.mic_release_debounce {
                return None;
            }
            let reason = if !sighting.still_matches() {
                EndReason::RuleNoLongerMatches
            } else if released >= timing.end_confirmation {
                EndReason::ConfirmationTimeout
            } else {
                return None;
            };
            *state = cooldown;
            Some(DetectEvent::MeetingEnded {
                app_id: rule.id.clone(),
                reason,
            })
        }
        AppState::Asking { released_at, .. } => {
            let released = released_for(released_at, sighting.mic_held, now)?;
            if released < timing.mic_release_debounce {
                return None;
            }
            *state = cooldown;
            Some(DetectEvent::AskExpired {
                app_id: rule.id.clone(),
            })
        }
        AppState::Ignored { released_at } => {
            let released = released_for(released_at, sighting.mic_held, now)?;
            if released >= timing.mic_release_debounce {
                *state = cooldown;
            }
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    struct Timeline {
        t0: Instant,
        rules: DetectRules,
        overrides: PolicyOverrides,
        machine: DetectionMachine,
    }

    impl Timeline {
        fn new() -> Self {
            Self {
                t0: Instant::now(),
                rules: DetectRules::builtin().unwrap(),
                overrides: PolicyOverrides::new(),
                machine: DetectionMachine::new(),
            }
        }

        /// One poll at `secs` with `app` in the given state (others absent).
        fn at(&mut self, secs: u64, app: &str, s: Option<Sighting>) -> Vec<DetectEvent> {
            let mut map = BTreeMap::new();
            if let Some(s) = s {
                map.insert(app.to_owned(), s);
            }
            self.machine.step(
                self.t0 + S * secs as u32,
                &self.rules,
                &self.overrides,
                &map,
            )
        }

        /// Polls every second over `from..=to` and returns all events with their time.
        fn run(
            &mut self,
            from: u64,
            to: u64,
            app: &str,
            s: impl Fn(u64) -> Option<Sighting>,
        ) -> Vec<(u64, DetectEvent)> {
            let mut out = Vec::new();
            for t in from..=to {
                for e in self.at(t, app, s(t)) {
                    out.push((t, e));
                }
            }
            out
        }
    }

    fn held(title: TitleState) -> Sighting {
        Sighting {
            mic_held: true,
            bundle_id: Some("com.example".into()),
            has_windows: true,
            title,
        }
    }

    fn released_with_window(title: TitleState) -> Sighting {
        Sighting {
            mic_held: false,
            ..held(title)
        }
    }

    fn ended(app: &str, reason: EndReason) -> DetectEvent {
        DetectEvent::MeetingEnded {
            app_id: app.into(),
            reason,
        }
    }

    fn is_detected(e: &DetectEvent, app: &str) -> bool {
        matches!(e, DetectEvent::MeetingDetected { app_id, .. } if app_id == app)
    }

    #[test]
    fn auto_start_then_end_by_confirmation_timeout() {
        let mut tl = Timeline::new();
        // Teams holds the mic 0..=99, then releases it with its window still open.
        let events = tl.run(0, 160, "teams", |t| {
            Some(if t < 100 {
                held(TitleState::NotRequired)
            } else {
                released_with_window(TitleState::NotRequired)
            })
        });
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].0, 0);
        assert!(is_detected(&events[0].1, "teams"));
        // Released at 100: the window keeps the rule matching, so the end
        // waits for the 60 s confirmation.
        assert_eq!(
            events[1],
            (160, ended("teams", EndReason::ConfirmationTimeout))
        );
        assert_eq!(tl.machine.state("teams"), DetectState::Cooldown);
        // The 30 s cooldown expires at 190; a released mic cannot restart it.
        assert!(
            tl.run(161, 200, "teams", |_| {
                Some(released_with_window(TitleState::NotRequired))
            })
            .is_empty()
        );
        assert_eq!(tl.machine.state("teams"), DetectState::Idle);
    }

    #[test]
    fn end_after_debounce_when_rule_stops_matching() {
        let mut tl = Timeline::new();
        // Zoom quits at 50 (no mic, no window).
        let events = tl.run(0, 120, "zoom", |t| {
            (t < 50).then(|| held(TitleState::NotRequired))
        });
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1],
            (70, ended("zoom", EndReason::RuleNoLongerMatches))
        );
    }

    #[test]
    fn short_mic_release_keeps_the_meeting() {
        let mut tl = Timeline::new();
        // Mute-like gaps of 15 s never reach the 20 s debounce.
        let events = tl.run(0, 300, "teams", |t| {
            (t % 40 < 25).then(|| held(TitleState::NotRequired))
        });
        assert_eq!(events.len(), 1);
        assert!(is_detected(&events[0].1, "teams"));
        assert_eq!(tl.machine.state("teams"), DetectState::Active);
    }

    #[test]
    fn cooldown_blocks_restart_for_30_s() {
        let mut tl = Timeline::new();
        // Mic 0..10, gone 10..80 → ends at 30 (no window), cooldown to 60.
        // Mic again from 40 on: the restart waits for the cooldown.
        let events = tl.run(0, 100, "teams", |t| {
            (!(10..40).contains(&t)).then(|| held(TitleState::NotRequired))
        });
        let times: Vec<u64> = events.iter().map(|(t, _)| *t).collect();
        assert_eq!(times, [0, 30, 60], "{events:?}");
        assert!(is_detected(&events[2].1, "teams"));
    }

    #[test]
    fn ask_then_record() {
        let mut tl = Timeline::new();
        let e = tl.at(0, "discord", Some(held(TitleState::NotRequired)));
        assert!(matches!(&e[..], [DetectEvent::AskUser { app_id, .. }] if app_id == "discord"));
        assert_eq!(tl.machine.state("discord"), DetectState::Asking);
        // No repeated prompt while waiting.
        assert!(
            tl.at(1, "discord", Some(held(TitleState::NotRequired)))
                .is_empty()
        );
        let e = tl.machine.answer("discord", Answer::Record);
        assert!(matches!(
            &e[..],
            [DetectEvent::MeetingDetected {
                policy: Policy::Ask,
                ..
            }]
        ));
        assert_eq!(tl.machine.state("discord"), DetectState::Active);
        // A second answer does nothing.
        assert!(tl.machine.answer("discord", Answer::Record).is_empty());
    }

    #[test]
    fn open_detections_list_active_and_asking_apps() {
        let mut tl = Timeline::new();
        let mut map = BTreeMap::new();
        map.insert("teams".to_owned(), held(TitleState::NotRequired));
        map.insert("discord".to_owned(), held(TitleState::NotRequired));
        tl.machine.step(tl.t0, &tl.rules, &tl.overrides, &map);
        assert_eq!(
            tl.machine.open_detections(),
            [
                OpenDetection {
                    app_id: "discord".into(),
                    state: DetectState::Asking,
                    title: None,
                },
                OpenDetection {
                    app_id: "teams".into(),
                    state: DetectState::Active,
                    title: None,
                },
            ]
        );
        tl.machine.dismiss("teams");
        assert_eq!(tl.machine.open_detections().len(), 1);
    }

    /// Teams (`auto`, Active) and Discord (`ask`, Asking) both on the mic.
    fn teams_and_discord_open() -> Timeline {
        let mut tl = Timeline::new();
        let mut map = BTreeMap::new();
        map.insert("teams".to_owned(), held(TitleState::NotRequired));
        map.insert("discord".to_owned(), held(TitleState::NotRequired));
        tl.machine.step(tl.t0, &tl.rules, &tl.overrides, &map);
        tl
    }

    fn reclassify(
        tl: &mut Timeline,
        policies: &[(&str, Policy)],
        keep: Option<&str>,
    ) -> Vec<DetectEvent> {
        tl.overrides = policies
            .iter()
            .map(|(a, p)| ((*a).to_owned(), *p))
            .collect();
        tl.machine.reclassify(&tl.rules, &tl.overrides, keep)
    }

    #[test]
    fn reclassify_off_quiets_open_detections_until_release() {
        let mut tl = teams_and_discord_open();
        let events = reclassify(
            &mut tl,
            &[("teams", Policy::Off), ("discord", Policy::Off)],
            None,
        );
        assert_eq!(
            events,
            [DetectEvent::AskExpired {
                app_id: "discord".into()
            }]
        );
        assert_eq!(tl.machine.state("teams"), DetectState::Ignored);
        assert_eq!(tl.machine.state("discord"), DetectState::Ignored);
        assert!(tl.machine.open_detections().is_empty());
    }

    #[test]
    fn reclassify_ask_turns_an_auto_detection_into_a_prompt() {
        let mut tl = teams_and_discord_open();
        let events = reclassify(&mut tl, &[("teams", Policy::Ask)], None);
        assert!(matches!(
            &events[..],
            [DetectEvent::AskUser { app_id, .. }] if app_id == "teams"
        ));
        assert_eq!(tl.machine.state("teams"), DetectState::Asking);
        // Applying the same policies again changes nothing.
        assert!(reclassify(&mut tl, &[("teams", Policy::Ask)], None).is_empty());
        // A start the user accepted under `ask` stays.
        tl.machine.answer("discord", Answer::Record);
        assert!(reclassify(&mut tl, &[("teams", Policy::Ask)], None).is_empty());
        assert_eq!(tl.machine.state("discord"), DetectState::Active);
    }

    #[test]
    fn reclassify_auto_starts_an_asked_meeting() {
        let mut tl = teams_and_discord_open();
        let events = reclassify(&mut tl, &[("discord", Policy::Auto)], None);
        assert!(matches!(
            &events[..],
            [
                DetectEvent::AskExpired { app_id: expired },
                DetectEvent::MeetingDetected { app_id, policy: Policy::Auto, .. },
            ] if expired == "discord" && app_id == "discord"
        ));
        assert_eq!(tl.machine.state("discord"), DetectState::Active);
    }

    #[test]
    fn reclassify_leaves_unchanged_policies_and_the_recording_app() {
        let mut tl = teams_and_discord_open();
        assert!(reclassify(&mut tl, &[], None).is_empty());
        assert_eq!(tl.machine.state("teams"), DetectState::Active);
        assert_eq!(tl.machine.state("discord"), DetectState::Asking);
        // Teams records: its detection stays so its end stops the recording.
        assert!(reclassify(&mut tl, &[("teams", Policy::Off)], Some("teams")).is_empty());
        assert_eq!(tl.machine.state("teams"), DetectState::Active);
    }

    #[test]
    fn ask_ignored_stays_quiet_until_release_and_cooldown() {
        let mut tl = Timeline::new();
        assert_eq!(
            tl.at(0, "discord", Some(held(TitleState::NotRequired)))
                .len(),
            1
        );
        assert!(tl.machine.answer("discord", Answer::Ignore).is_empty());
        // Mic held until 100, released 100..140, held again from 140.
        let events = tl.run(1, 200, "discord", |t| {
            (!(100..140).contains(&t)).then(|| held(TitleState::NotRequired))
        });
        // Released 20 s at 120 → cooldown until 150 → asked again at 150.
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].0, 150);
        assert!(matches!(events[0].1, DetectEvent::AskUser { .. }));
    }

    #[test]
    fn unanswered_ask_expires() {
        let mut tl = Timeline::new();
        let events = tl.run(0, 60, "discord", |t| {
            (t < 10).then(|| held(TitleState::NotRequired))
        });
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1],
            (
                30,
                DetectEvent::AskExpired {
                    app_id: "discord".into()
                }
            )
        );
    }

    #[test]
    fn off_policy_never_fires() {
        let mut tl = Timeline::new();
        tl.overrides.insert("teams".into(), Policy::Off);
        assert!(
            tl.run(0, 100, "teams", |_| Some(held(TitleState::NotRequired)))
                .is_empty()
        );
        assert_eq!(tl.machine.state("teams"), DetectState::Idle);
    }

    #[test]
    fn user_policy_override_applies() {
        let mut tl = Timeline::new();
        tl.overrides.insert("zoom".into(), Policy::Ask);
        let e = tl.at(0, "zoom", Some(held(TitleState::NotRequired)));
        assert!(matches!(&e[..], [DetectEvent::AskUser { .. }]));
    }

    #[test]
    fn meet_title_rules() {
        // Titles readable and matching → auto, with the title.
        let mut tl = Timeline::new();
        let e = tl.at(
            0,
            "meet",
            Some(held(TitleState::Matched("Meet - abc".into()))),
        );
        assert!(matches!(
            &e[..],
            [DetectEvent::MeetingDetected { title: Some(t), policy: Policy::Auto, .. }] if t == "Meet - abc"
        ));

        // Titles unreadable → capped at ask, even when the user chose auto.
        let mut tl = Timeline::new();
        tl.overrides.insert("meet".into(), Policy::Auto);
        let e = tl.at(0, "meet", Some(held(TitleState::Unknown)));
        assert!(matches!(&e[..], [DetectEvent::AskUser { title: None, .. }]));

        // Browser holds the mic on another site → nothing.
        let mut tl = Timeline::new();
        assert!(
            tl.run(0, 30, "meet", |_| Some(held(TitleState::NotMatched)))
                .is_empty()
        );
    }

    #[test]
    fn meet_ends_early_when_tab_closes() {
        let mut tl = Timeline::new();
        let events = tl.run(0, 120, "meet", |t| {
            Some(if t < 30 {
                held(TitleState::Matched("Meet - abc".into()))
            } else {
                // Browser window stays, title no longer Meet.
                released_with_window(TitleState::NotMatched)
            })
        });
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1],
            (50, ended("meet", EndReason::RuleNoLongerMatches))
        );
    }

    #[test]
    fn dismiss_mutes_until_release() {
        let mut tl = Timeline::new();
        assert_eq!(
            tl.at(0, "teams", Some(held(TitleState::NotRequired))).len(),
            1
        );
        tl.machine.dismiss("teams");
        assert_eq!(tl.machine.state("teams"), DetectState::Ignored);
        assert!(
            tl.run(1, 50, "teams", |_| Some(held(TitleState::NotRequired)))
                .is_empty()
        );
    }

    #[test]
    fn events_serialize_tagged() {
        let e = DetectEvent::MeetingEnded {
            app_id: "teams".into(),
            reason: EndReason::ConfirmationTimeout,
        };
        let json = serde_json::to_string(&e).unwrap();
        assert_eq!(
            json,
            r#"{"event":"meeting_ended","app_id":"teams","reason":"confirmation_timeout"}"#
        );
        assert_eq!(serde_json::from_str::<DetectEvent>(&json).unwrap(), e);
    }
}
