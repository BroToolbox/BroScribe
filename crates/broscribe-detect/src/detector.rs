//! The poller the daemon drives (about once per second): reads mic users and
//! windows through the platform traits and feeds the state machine.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use broscribe_platform::{MicUsageMonitor, Result, WindowInspector};

use crate::machine::{Answer, DetectEvent, DetectState, DetectionMachine, OpenDetection};
use crate::observe::{Sighting, observe};
use crate::rules::{DetectRules, PolicyOverrides};

/// Time source, so tests can drive the detector with a fake clock. Both
/// readings come from the same source, so a consumer can compare them to
/// notice sleep, missed polls and wall-clock adjustments.
pub trait Clock: Send + Sync {
    /// Monotonic time.
    fn now(&self) -> Instant;
    /// Host wall-clock time (`SystemTime`, which readers also stamp with).
    fn wall(&self) -> SystemTime;
}

/// One reading of a [`Clock`]: monotonic and wall time together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockReading {
    pub mono: Instant,
    pub wall: SystemTime,
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wall(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A clock that only moves when told to. [`advance`](Self::advance) moves
/// both readings together; [`shift_wall`](Self::shift_wall) adjusts only the
/// wall clock, as a user or NTP changing the system time does.
pub struct FakeClock {
    now: Mutex<(Instant, SystemTime)>,
}

impl FakeClock {
    pub fn new() -> Self {
        Self {
            now: Mutex::new((Instant::now(), SystemTime::now())),
        }
    }

    pub fn advance(&self, by: Duration) {
        let mut now = self.now.lock().unwrap_or_else(|e| e.into_inner());
        now.0 += by;
        now.1 += by;
    }

    /// Moves the wall clock alone, forward or (`forward = false`) back.
    pub fn shift_wall(&self, by: Duration, forward: bool) {
        let mut now = self.now.lock().unwrap_or_else(|e| e.into_inner());
        now.1 = if forward { now.1 + by } else { now.1 - by };
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Instant {
        self.now.lock().unwrap_or_else(|e| e.into_inner()).0
    }

    fn wall(&self) -> SystemTime {
        self.now.lock().unwrap_or_else(|e| e.into_inner()).1
    }
}

/// What one successful poll saw: per-app sightings (absent apps missing)
/// and when, by the detector's clock.
#[derive(Clone, Debug, PartialEq)]
pub struct Observation {
    pub at: ClockReading,
    pub sightings: BTreeMap<String, Sighting>,
}

/// Meeting detection for the daemon. Call [`poll`](Self::poll) every
/// [`POLL_INTERVAL`](Self::POLL_INTERVAL) and act on the returned events.
pub struct Detector {
    rules: DetectRules,
    policies: PolicyOverrides,
    machine: DetectionMachine,
    mic: Box<dyn MicUsageMonitor>,
    windows: Box<dyn WindowInspector>,
    clock: Box<dyn Clock>,
    meeting_app_on_mic: bool,
    /// Apps whose windows are read on every poll even when no detection is
    /// open (a recording bound to them watches for its call's end).
    watched: BTreeSet<String>,
    /// The last successful poll; cleared by a failed one.
    last: Option<Observation>,
}

impl Detector {
    pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

    pub fn new(
        rules: DetectRules,
        mic: Box<dyn MicUsageMonitor>,
        windows: Box<dyn WindowInspector>,
        clock: Box<dyn Clock>,
    ) -> Self {
        Self {
            rules,
            policies: PolicyOverrides::new(),
            machine: DetectionMachine::new(),
            mic,
            windows,
            clock,
            meeting_app_on_mic: false,
            watched: BTreeSet::new(),
            last: None,
        }
    }

    pub fn rules(&self) -> &DetectRules {
        &self.rules
    }

    /// Replaces the rules (e.g. after the user edited `detect.toml`); state
    /// of apps that remain is kept.
    pub fn set_rules(&mut self, rules: DetectRules) {
        self.rules = rules;
    }

    /// Per-app policies from settings (`broscribe detect policy`). Open
    /// detections whose policy changed are reclassified (see
    /// [`DetectionMachine::reclassify`]), except `recording`'s, whose
    /// recording runs; act on the returned events as on [`poll`](Self::poll)'s.
    #[must_use]
    pub fn set_policies(
        &mut self,
        policies: PolicyOverrides,
        recording: Option<&str>,
    ) -> Vec<DetectEvent> {
        self.policies = policies;
        self.machine
            .reclassify(&self.rules, &self.policies, recording)
    }

    pub fn state(&self, app_id: &str) -> DetectState {
        self.machine.state(app_id)
    }

    /// Apps whose meeting is detected or asked about.
    pub fn open_detections(&self) -> Vec<OpenDetection> {
        self.machine.open_detections()
    }

    /// Whether a rule-matched meeting app held the mic at the last poll,
    /// whatever its policy or detection state.
    pub fn meeting_app_on_mic(&self) -> bool {
        self.meeting_app_on_mic
    }

    /// A reading of the detector's clock (also used to time reader
    /// evidence, so polls and observations share one clock).
    pub fn reading(&self) -> ClockReading {
        ClockReading {
            mono: self.clock.now(),
            wall: self.clock.wall(),
        }
    }

    /// Apps whose windows every poll reads, whatever their detection state.
    pub fn set_watched(&mut self, apps: impl IntoIterator<Item = String>) {
        self.watched = apps.into_iter().collect();
    }

    /// The last successful poll's observation (`None` after a failed one).
    pub fn last_observation(&self) -> Option<&Observation> {
        self.last.as_ref()
    }

    /// `app_id`'s sighting at the last successful poll (absent when it
    /// neither held the mic nor showed a window); `None` without a poll or
    /// for an app the rules do not know.
    pub fn sighting(&self, app_id: &str) -> Option<Sighting> {
        let rule = self.rules.app(app_id)?;
        let last = self.last.as_ref()?;
        Some(
            last.sightings
                .get(app_id)
                .cloned()
                .unwrap_or_else(|| Sighting::absent(rule)),
        )
    }

    /// Apps that, at the last successful poll, held the mic with a rule
    /// that does not rule them out: `(app_id, display name)`, by ID.
    pub fn candidates(&self) -> Vec<(String, String)> {
        let Some(last) = self.last.as_ref() else {
            return Vec::new();
        };
        last.sightings
            .iter()
            .filter(|(_, s)| s.should_start())
            .filter_map(|(id, _)| self.rules.app(id).map(|r| (r.id.clone(), r.name.clone())))
            .collect()
    }

    /// One observation: mic users always; windows only when a meeting app
    /// holds the mic, a detection is open (to confirm its end) or an app is
    /// watched. A failure clears the last observation.
    pub fn poll(&mut self) -> Result<Vec<DetectEvent>> {
        self.last = None;
        let users = self.mic.mic_users()?;
        let any_app_on_mic = users.iter().any(|u| {
            self.rules
                .resolve_app_bundle(u)
                .is_some_and(|b| self.rules.app_for_bundle(&b).is_some())
        });
        self.meeting_app_on_mic = any_app_on_mic;
        let windows = if any_app_on_mic || self.machine.has_open_apps() || !self.watched.is_empty()
        {
            match self.windows.windows() {
                Ok(windows) => windows,
                // Wayland and other explicitly unavailable inspectors still
                // permit mic/app detection with the rule's unknown-title cap.
                Err(broscribe_platform::Error::Unsupported(_)) => Vec::new(),
                Err(error) => return Err(error),
            }
        } else {
            Vec::new()
        };
        let sightings = observe(&self.rules, &users, &windows);
        let at = self.reading();
        let events = self
            .machine
            .step(at.mono, &self.rules, &self.policies, &sightings);
        self.last = Some(Observation { at, sightings });
        Ok(events)
    }

    /// The user's Record / Ignore answer for `app_id`.
    pub fn answer(&mut self, app_id: &str, answer: Answer) -> Vec<DetectEvent> {
        self.machine.answer(app_id, answer)
    }

    /// The user stopped a detected recording by hand.
    pub fn dismiss(&mut self, app_id: &str) {
        self.machine.dismiss(app_id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use broscribe_platform::fakes::{FakeMicUsageMonitor, FakeWindowInspector};
    use broscribe_platform::{MicUser, WindowInfo};

    use super::*;
    use crate::machine::EndReason;

    struct SharedMic(Arc<FakeMicUsageMonitor>);

    impl MicUsageMonitor for SharedMic {
        fn mic_users(&self) -> Result<Vec<MicUser>> {
            self.0.mic_users()
        }
    }

    struct SharedClock(Arc<FakeClock>);

    impl Clock for SharedClock {
        fn now(&self) -> Instant {
            self.0.now()
        }

        fn wall(&self) -> SystemTime {
            self.0.wall()
        }
    }

    struct UnavailableWindows(bool);

    impl WindowInspector for UnavailableWindows {
        fn windows(&self) -> Result<Vec<WindowInfo>> {
            if self.0 {
                Err(broscribe_platform::Error::Unsupported("Wayland".into()))
            } else {
                Err(broscribe_platform::Error::Internal(
                    "inspection failed".into(),
                ))
            }
        }
    }

    #[test]
    fn unsupported_windows_preserve_detection_and_release() {
        let mic = Arc::new(FakeMicUsageMonitor::default());
        let clock = Arc::new(FakeClock::new());
        let mut detector = Detector::new(
            DetectRules::builtin().unwrap(),
            Box::new(SharedMic(mic.clone())),
            Box::new(UnavailableWindows(true)),
            Box::new(SharedClock(clock.clone())),
        );
        mic.set(vec![MicUser {
            pid: 7,
            bundle_id: Some("chrome".into()),
            ..Default::default()
        }]);
        assert!(matches!(
            &detector.poll().unwrap()[..],
            [DetectEvent::AskUser { app_id, title: None, .. }] if app_id == "meet"
        ));
        detector.answer("meet", Answer::Record);
        mic.set(Vec::new());
        detector.poll().unwrap();
        clock.advance(Duration::from_secs(21));
        assert!(matches!(
            &detector.poll().unwrap()[..],
            [DetectEvent::MeetingEnded { app_id, .. }] if app_id == "meet"
        ));
    }

    #[test]
    fn operational_window_errors_are_not_suppressed() {
        let mic = FakeMicUsageMonitor::default();
        mic.set(vec![MicUser {
            pid: 7,
            bundle_id: Some("us.zoom.xos".into()),
            ..Default::default()
        }]);
        let mut detector = Detector::new(
            DetectRules::builtin().unwrap(),
            Box::new(mic),
            Box::new(UnavailableWindows(false)),
            Box::new(FakeClock::new()),
        );
        assert!(matches!(
            detector.poll(),
            Err(broscribe_platform::Error::Internal(_))
        ));
    }

    #[test]
    fn zoom_meeting_through_fakes() {
        let mic = Arc::new(FakeMicUsageMonitor::default());
        let clock = Arc::new(FakeClock::new());
        let windows = FakeWindowInspector {
            windows: vec![WindowInfo {
                pid: 7,
                app: "Zoom".into(),
                bundle_id: Some("us.zoom.xos".into()),
                title: None,
            }],
        };
        let mut detector = Detector::new(
            DetectRules::builtin().unwrap(),
            Box::new(SharedMic(mic.clone())),
            Box::new(windows),
            Box::new(SharedClock(clock.clone())),
        );
        assert!(detector.poll().unwrap().is_empty());

        mic.set(vec![MicUser {
            pid: 8,
            bundle_id: Some("us.zoom.CptHost".into()),
            responsible_pid: Some(7),
            responsible_bundle_id: Some("us.zoom.xos".into()),
        }]);
        let events = detector.poll().unwrap();
        assert!(matches!(
            &events[..],
            [DetectEvent::MeetingDetected { app_id, .. }] if app_id == "zoom"
        ));

        mic.set(Vec::new());
        let mut ended = Vec::new();
        for _ in 0..70 {
            clock.advance(Detector::POLL_INTERVAL);
            ended.extend(detector.poll().unwrap());
        }
        // The Zoom window stays, so the end waits for the 60 s confirmation.
        assert_eq!(
            ended,
            [DetectEvent::MeetingEnded {
                app_id: "zoom".into(),
                reason: EndReason::ConfirmationTimeout
            }]
        );
        assert_eq!(detector.state("zoom"), DetectState::Cooldown);
    }

    /// A recording bound to an app keeps its windows read after the
    /// detection left `Active`; candidates are the apps on the mic now, and
    /// a failed poll leaves no observation to act on.
    #[test]
    fn watched_apps_candidates_and_failed_polls() {
        let mic = Arc::new(FakeMicUsageMonitor::default());
        let zoom_window = WindowInfo {
            pid: 7,
            app: "Zoom".into(),
            bundle_id: Some("us.zoom.xos".into()),
            title: None,
        };
        let mut detector = Detector::new(
            DetectRules::builtin().unwrap(),
            Box::new(SharedMic(mic.clone())),
            Box::new(FakeWindowInspector {
                windows: vec![zoom_window],
            }),
            Box::new(FakeClock::new()),
        );
        assert!(detector.last_observation().is_none());
        detector.poll().unwrap();
        // Nothing holds the mic and nothing is open: windows were not read.
        assert!(!detector.sighting("zoom").unwrap().has_windows);
        assert!(detector.candidates().is_empty());
        detector.set_watched(["zoom".to_owned()]);
        detector.poll().unwrap();
        assert!(detector.sighting("zoom").unwrap().has_windows);
        mic.set(vec![MicUser {
            pid: 7,
            bundle_id: Some("us.zoom.xos".into()),
            ..Default::default()
        }]);
        detector.poll().unwrap();
        assert_eq!(
            detector
                .candidates()
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            ["zoom"]
        );
        assert!(detector.sighting("no-such-app").is_none());

        let mut failing = Detector::new(
            DetectRules::builtin().unwrap(),
            Box::new(SharedMic(mic)),
            Box::new(UnavailableWindows(false)),
            Box::new(FakeClock::new()),
        );
        assert!(failing.poll().is_err());
        assert!(failing.last_observation().is_none());
        assert!(failing.candidates().is_empty());
    }

    #[test]
    fn fake_clock_moves_wall_and_monotonic_time_together_unless_shifted() {
        let clock = FakeClock::new();
        let (mono, wall) = (clock.now(), clock.wall());
        clock.advance(Duration::from_secs(3));
        assert_eq!(clock.now() - mono, Duration::from_secs(3));
        assert_eq!(
            clock.wall().duration_since(wall).unwrap(),
            Duration::from_secs(3)
        );
        clock.shift_wall(Duration::from_secs(60), false);
        assert_eq!(clock.now() - mono, Duration::from_secs(3));
        assert!(clock.wall() < wall);
    }
}
