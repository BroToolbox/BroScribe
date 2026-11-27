//! Automatic stop of a recording bound to a detected call (design §9, plan H).
//!
//! Each recording owns one [`AutoStop`] evaluator. It is fed from every
//! detector poll's actual observation of the bound app (mic held, rule
//! matching) and from the meeting reader's call-state observations, and it
//! is the only source of automatic stops: the detector's `MeetingEnded` is
//! bookkeeping. An end decision opens a fixed [`GRACE`] the user can cancel
//! with Keep recording; only a decision still current when the daemon takes
//! the session under its mutex stops it, through the normal Stop-and-save
//! path.
//!
//! Evidence:
//! - **Explicit call end** (`CallEnded`): the reader saw the call active in
//!   this reader generation, then an explicit post-call screen, from complete
//!   snapshots, for the mic-release debounce, even while the app keeps its
//!   microphone. Active, unknown, stale or expired observations cancel it.
//! - **Microphone-release fallback** (`MicReleased…`): the detector's rule —
//!   mic released for the debounce and the rule no longer matching, or
//!   released for the whole end confirmation. Re-acquiring the mic cancels it.
//! - **Veto**: a fresh active-call observation clears the release timer and
//!   any pending decision; fallback counting starts afresh once it expires.
//!
//! Time comes from the detector's clock only. A poll gap above three poll
//! intervals, a backward wall clock, a wall/monotonic discontinuity or a
//! failed poll clears every timer, the reader evidence and the grace. Pause
//! clears them as well and nothing is evaluated while paused. Reader
//! timestamps are checked for age only; they never authorize a stop.

use std::time::{Duration, Instant, UNIX_EPOCH};

use broscribe_detect::{ClockReading, Detector, Sighting, Timing};
use broscribe_ipc::{AutoStopApp, AutoStopPending, AutoStopReason, AutoStopStatus};
use broscribe_readers::{CallObservation, CallState};

/// The fixed cancellation grace after an end decision.
pub(crate) const GRACE: Duration = Duration::from_secs(10);
/// Accepted reader evidence expires this long after it was received.
pub(crate) const EVIDENCE_TTL: Duration = Duration::from_secs(5);
/// Oldest accepted snapshot: from its start to its receipt, wall clock.
pub(crate) const MAX_SNAPSHOT_AGE_MS: u64 = 5_000;
/// Polls further apart than this many intervals are a gap (sleep, stall).
const MAX_POLL_GAP: u32 = 3;
/// Wall and monotonic progress between polls may differ by this much
/// before the clocks count as discontinuous.
const MAX_CLOCK_SKEW: Duration = Duration::from_secs(2);
/// `record.auto_stop` named a binding generation that is no longer current.
pub(crate) const STALE_BINDING: &str = "stale_binding";

/// An end decision, carried from evaluation to the fenced stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StopDecision {
    pub session_id: String,
    pub generation: u64,
    pub evidence: u64,
    pub reason: AutoStopReason,
    /// The bound app held its mic at the decision: the post-stop notice
    /// explains that its next call needs a manual start until it releases.
    pub mic_held: bool,
    pub app_name: String,
}

/// The notice after an automatic stop, naming the evidence it came from
/// honestly. The recording is stopped and queued for its final transcript:
/// "saved" is said only once that is done.
pub(crate) fn saved_notice(decision: &StopDecision) -> broscribe_ipc::NotificationRequest {
    let app = if decision.app_name.is_empty() {
        "The meeting app"
    } else {
        decision.app_name.as_str()
    };
    let mut body = match decision.reason {
        AutoStopReason::CallEnded => format!("{app} showed the call ended."),
        AutoStopReason::MicReleasedRuleChanged | AutoStopReason::MicReleasedTimeout => {
            format!("{app} released the microphone.")
        }
    };
    body.push_str(" BroScribe is transcribing it.");
    if decision.mic_held {
        body.push_str(&format!(
            " {app} still has the microphone, so its next call won't start recording by \
             itself. Start it yourself if you need it."
        ));
    }
    broscribe_ipc::NotificationRequest {
        id: format!("auto-stopped-{}", decision.session_id),
        title: broscribe_platform_native::NativePlatform::notification_title(
            "Recording Stopped Automatically",
            "Recording stopped automatically",
        ),
        body,
        actions: Vec::new(),
        meeting_id: None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReaderState {
    Active,
    Ended,
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    reason: AutoStopReason,
    since: Instant,
    remaining_s: u64,
    due: bool,
}

/// What accepting a reader observation did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Accepted {
    /// Positive evidence (active, or explicit end after active) now in use.
    Evidence,
    /// Not usable (old sequence, unknown, incomplete, over-age, future
    /// timestamp, end without a prior active): pending end evidence cleared.
    Unknown,
}

/// Automatic-stop state of one recording.
#[derive(Debug)]
pub(crate) struct AutoStop {
    enabled: bool,
    app: Option<AutoStopApp>,
    generation: u64,
    /// Bumped by every change of the evidence or pending decision; a stop
    /// decision is valid only while it is unchanged.
    evidence: u64,
    reader_generation: u64,
    last_sequence: Option<u64>,
    seen_active: bool,
    reader: Option<(ReaderState, Instant)>,
    release_since: Option<Instant>,
    ended_since: Option<Instant>,
    pending: Option<Pending>,
    last_poll: Option<ClockReading>,
}

impl AutoStop {
    /// `app`: the detection that started the recording (enabled), or `None`
    /// for a manual one, which stays manual until the user links an app.
    pub fn new(app: Option<AutoStopApp>, reader_generation: u64) -> Self {
        Self {
            enabled: app.is_some(),
            app,
            generation: 1,
            evidence: 0,
            reader_generation,
            last_sequence: None,
            seen_active: false,
            reader: None,
            release_since: None,
            ended_since: None,
            pending: None,
            last_poll: None,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn reader_generation(&self) -> u64 {
        self.reader_generation
    }

    /// The app every detector poll must watch for this recording.
    pub fn watched_app(&self) -> Option<&str> {
        self.app
            .as_ref()
            .filter(|_| self.enabled)
            .map(|a| a.app_id.as_str())
    }

    pub fn app(&self) -> Option<&AutoStopApp> {
        self.app.as_ref()
    }

    /// `RecordingState.auto_stop`.
    pub fn status(&self, candidates: &[AutoStopApp]) -> AutoStopStatus {
        AutoStopStatus {
            enabled: self.enabled,
            app: self.app.clone(),
            generation: self.generation,
            reader_available: self.enabled && self.reader.is_some(),
            pending: self.pending.map(|p| AutoStopPending {
                reason: p.reason,
                remaining_s: p.remaining_s,
            }),
            candidates: candidates.to_vec(),
        }
    }

    /// Enables or disables it, optionally linking `link` first. Changes
    /// bump the binding generation and start from fresh evidence; an
    /// unchanged request changes nothing. Returns whether the binding app
    /// changed (the caller resolves a new reader).
    pub fn configure(
        &mut self,
        enabled: bool,
        link: Option<AutoStopApp>,
    ) -> Result<bool, &'static str> {
        if link.is_some() && !enabled {
            return Err("linking an app also enables automatic stop");
        }
        let relinked = link.as_ref().is_some_and(|l| self.app.as_ref() != Some(l));
        if enabled && link.is_none() && self.app.is_none() {
            return Err("this recording is not linked to a detected call");
        }
        if !relinked && enabled == self.enabled {
            return Ok(false);
        }
        if let Some(link) = link {
            self.app = Some(link);
        }
        self.enabled = enabled;
        self.generation += 1;
        self.reset_evidence();
        Ok(relinked)
    }

    /// A new reader incarnation (start, pause, resume, relink): earlier
    /// batches are rejected and an explicit end needs a fresh active call.
    pub fn new_reader_generation(&mut self, generation: u64) {
        self.reader_generation = generation;
        self.last_sequence = None;
        self.seen_active = false;
        self.clear_reader();
    }

    /// Clears every timer, the reader evidence and the grace (pause, poll
    /// failure, clock discontinuity, configuration change). The reader's
    /// sequence stays, so older batches are still refused.
    pub fn reset_evidence(&mut self) {
        let had = self.pending.is_some()
            || self.reader.is_some()
            || self.release_since.is_some()
            || self.ended_since.is_some();
        self.pending = None;
        self.reader = None;
        self.release_since = None;
        self.ended_since = None;
        self.seen_active = false;
        if had {
            self.evidence += 1;
        }
    }

    /// A failed detector poll: nothing observed can be trusted across it.
    pub fn poll_failed(&mut self) {
        self.reset_evidence();
        self.last_poll = None;
    }

    /// A reader observation of `generation`, received at `received` (the
    /// detector's clock). The caller already refused another generation.
    pub fn accept(&mut self, observation: CallObservation, received: ClockReading) -> Accepted {
        let increasing = self
            .last_sequence
            .is_none_or(|last| observation.sequence > last);
        if !increasing {
            self.observation_unknown();
            return Accepted::Unknown;
        }
        self.last_sequence = Some(observation.sequence);
        let received_ms = received
            .wall
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .ok();
        let fresh = received_ms.is_some_and(|now| {
            now >= observation.snapshot_started_at_ms
                && now - observation.snapshot_started_at_ms <= MAX_SNAPSHOT_AGE_MS
        });
        if !fresh || !observation.complete {
            self.observation_unknown();
            return Accepted::Unknown;
        }
        match observation.state {
            CallState::Active => {
                self.seen_active = true;
                self.reader = Some((ReaderState::Active, received.mono));
                self.veto();
                Accepted::Evidence
            }
            CallState::Ended if self.seen_active => {
                // Refreshes the evidence; the debounce and any grace keep
                // the time the end was first seen.
                self.reader = Some((ReaderState::Ended, received.mono));
                if self.ended_since.is_none() {
                    self.ended_since = Some(received.mono);
                    self.evidence += 1;
                }
                Accepted::Evidence
            }
            CallState::Ended | CallState::Unknown => {
                self.observation_unknown();
                Accepted::Unknown
            }
        }
    }

    /// One detector poll of the bound app: `sighting` is `None` when the
    /// detector could not tell (no rule). Returns a decision once a pending
    /// end has run its whole grace.
    pub fn poll(
        &mut self,
        at: ClockReading,
        sighting: Option<&Sighting>,
        timing: Timing,
        paused: bool,
    ) -> Option<(AutoStopReason, u64, bool)> {
        if let Some(last) = self.last_poll
            && discontinuous(last, at)
        {
            self.reset_evidence();
        }
        self.last_poll = Some(at);
        if paused || !self.enabled || self.app.is_none() {
            self.reset_evidence();
            return None;
        }
        let now = at.mono;
        if let Some((state, received)) = self.reader
            && now.saturating_duration_since(received) > EVIDENCE_TTL
        {
            self.reader = None;
            if state == ReaderState::Ended {
                self.clear_explicit_end();
            }
        }
        let mic_held = sighting.is_some_and(|s| s.mic_held);
        match self.reader.map(|(state, _)| state) {
            Some(ReaderState::Active) => self.veto(),
            Some(ReaderState::Ended) => {
                if self.pending.is_none()
                    && let Some(since) = self.ended_since
                    && now.saturating_duration_since(since) >= timing.mic_release_debounce
                {
                    self.open_grace(AutoStopReason::CallEnded, now);
                }
            }
            None => self.fallback(now, sighting, timing),
        }
        let pending = self.pending.as_mut()?;
        let elapsed = now.saturating_duration_since(pending.since);
        pending.remaining_s = GRACE.saturating_sub(elapsed).as_secs_f64().ceil() as u64;
        if elapsed >= GRACE {
            pending.due = true;
            return Some((pending.reason, self.evidence, mic_held));
        }
        None
    }

    /// Whether `decision` is still this session's current, due decision:
    /// checked under the session mutex that then takes the recording.
    pub fn confirms(&self, decision: &StopDecision) -> bool {
        self.enabled
            && self.generation == decision.generation
            && self.evidence == decision.evidence
            && self
                .pending
                .is_some_and(|p| p.due && p.reason == decision.reason)
    }

    fn fallback(&mut self, now: Instant, sighting: Option<&Sighting>, timing: Timing) {
        let Some(sighting) = sighting else {
            // Nothing observed of the app: no evidence either way.
            return;
        };
        if sighting.mic_held {
            self.release_since = None;
            if self
                .pending
                .is_some_and(|p| p.reason != AutoStopReason::CallEnded)
            {
                self.pending = None;
                self.evidence += 1;
            }
            return;
        }
        let since = *self.release_since.get_or_insert(now);
        let released = now.saturating_duration_since(since);
        if self.pending.is_some() || released < timing.mic_release_debounce {
            return;
        }
        if !sighting.still_matches() {
            self.open_grace(AutoStopReason::MicReleasedRuleChanged, now);
        } else if released >= timing.end_confirmation {
            self.open_grace(AutoStopReason::MicReleasedTimeout, now);
        }
    }

    fn open_grace(&mut self, reason: AutoStopReason, now: Instant) {
        self.pending = Some(Pending {
            reason,
            since: now,
            remaining_s: GRACE.as_secs(),
            due: false,
        });
        self.evidence += 1;
    }

    /// Fresh active-call evidence: no end, by any route, stands.
    fn veto(&mut self) {
        if self.pending.is_some() || self.release_since.is_some() || self.ended_since.is_some() {
            self.evidence += 1;
        }
        self.pending = None;
        self.release_since = None;
        self.ended_since = None;
    }

    /// An unusable observation clears explicit-end evidence, never a newer
    /// active observation, and leaves the microphone fallback alone.
    fn observation_unknown(&mut self) {
        if self
            .reader
            .is_some_and(|(state, _)| state == ReaderState::Ended)
        {
            self.reader = None;
        }
        self.clear_explicit_end();
    }

    fn clear_explicit_end(&mut self) {
        let pending_end = self
            .pending
            .is_some_and(|p| p.reason == AutoStopReason::CallEnded);
        if pending_end || self.ended_since.is_some() {
            self.evidence += 1;
        }
        if pending_end {
            self.pending = None;
        }
        self.ended_since = None;
    }

    fn clear_reader(&mut self) {
        self.reader = None;
        self.clear_explicit_end();
    }
}

/// Polls too far apart, a backward wall clock, or wall and monotonic
/// progress that disagree.
fn discontinuous(last: ClockReading, now: ClockReading) -> bool {
    let Some(mono) = now.mono.checked_duration_since(last.mono) else {
        return true;
    };
    if mono > Detector::POLL_INTERVAL * MAX_POLL_GAP {
        return true;
    }
    match now.wall.duration_since(last.wall) {
        Err(_) => true,
        Ok(wall) => wall.abs_diff(mono) > MAX_CLOCK_SKEW,
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use broscribe_detect::TitleState;

    use super::*;

    const S: Duration = Duration::from_secs(1);

    fn zoom() -> AutoStopApp {
        AutoStopApp {
            app_id: "zoom".into(),
            app_name: "Zoom".into(),
        }
    }

    /// A synthetic clock and observation stream for one bound recording.
    struct Run {
        auto: AutoStop,
        mono: Instant,
        wall: SystemTime,
        sequence: u64,
    }

    impl Run {
        fn new() -> Self {
            Self {
                auto: AutoStop::new(Some(zoom()), 1),
                mono: Instant::now(),
                wall: SystemTime::now(),
                sequence: 0,
            }
        }
        fn at(&self) -> ClockReading {
            ClockReading {
                mono: self.mono,
                wall: self.wall,
            }
        }
        fn sighting(mic_held: bool, matches: bool) -> Sighting {
            Sighting {
                mic_held,
                bundle_id: Some("us.zoom.xos".into()),
                has_windows: matches,
                title: TitleState::NotRequired,
            }
        }
        /// One poll after a second; `Some(reason)` when it decided to stop.
        fn tick(&mut self, mic_held: bool, matches: bool) -> Option<AutoStopReason> {
            self.mono += S;
            self.wall += S;
            let sighting = Self::sighting(mic_held, matches);
            self.auto
                .poll(self.at(), Some(&sighting), Timing::default(), false)
                .map(|(reason, _, _)| reason)
        }
        /// Ticks `n` times; the first decision, if any, with its tick.
        fn ticks(
            &mut self,
            n: u32,
            mic_held: bool,
            matches: bool,
        ) -> Option<(u32, AutoStopReason)> {
            (1..=n).find_map(|i| self.tick(mic_held, matches).map(|r| (i, r)))
        }
        fn observe(&mut self, state: CallState) -> Accepted {
            self.observe_aged(state, 0, true)
        }
        fn observe_aged(&mut self, state: CallState, age_ms: u64, complete: bool) -> Accepted {
            self.sequence += 1;
            let wall_ms = self.wall.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
            let observation = CallObservation {
                sequence: self.sequence,
                snapshot_started_at_ms: wall_ms - age_ms,
                state,
                complete,
            };
            self.auto.accept(observation, self.at())
        }
        fn pending(&self) -> Option<AutoStopPending> {
            self.auto.status(&[]).pending
        }
    }

    #[test]
    fn mic_release_with_a_window_left_waits_for_confirmation_then_the_full_grace() {
        let mut run = Run::new();
        run.ticks(5, true, true);
        // Released with the window still there: 60 s confirmation + 10 s grace.
        let (tick, reason) = run.ticks(80, false, true).unwrap();
        assert_eq!(reason, AutoStopReason::MicReleasedTimeout);
        // Released at tick 1: confirmation at tick 61, grace due at 71.
        assert_eq!(tick, 71);
    }

    #[test]
    fn rule_no_longer_matching_after_25_s_still_gets_the_full_grace() {
        let mut run = Run::new();
        run.ticks(2, true, true);
        assert!(run.ticks(25, false, true).is_none());
        // The window goes after 25 s: decision now (past the 20 s debounce),
        // stop only after the whole 10 s grace.
        assert!(run.tick(false, false).is_none());
        assert_eq!(
            run.pending(),
            Some(AutoStopPending {
                reason: AutoStopReason::MicReleasedRuleChanged,
                remaining_s: 10
            })
        );
        let (tick, reason) = run.ticks(20, false, false).unwrap();
        assert_eq!((tick, reason), (10, AutoStopReason::MicReleasedRuleChanged));
    }

    #[test]
    fn brief_release_and_reconnection_do_not_stop() {
        let mut run = Run::new();
        for _ in 0..5 {
            assert!(run.ticks(15, false, false).is_none());
            assert!(run.ticks(2, true, true).is_none());
        }
        // Released into the grace, then the mic comes back: cancelled.
        assert!(run.ticks(22, false, false).is_none());
        assert!(run.pending().is_some());
        assert!(run.tick(true, true).is_none());
        assert!(run.pending().is_none());
        assert!(run.ticks(30, true, true).is_none());
    }

    #[test]
    fn positive_active_call_vetoes_a_muted_release_and_later_fallback_starts_fresh() {
        let mut run = Run::new();
        // A muted call: the app releases the mic but the reader sees it active.
        for _ in 0..40 {
            assert_eq!(run.observe(CallState::Active), Accepted::Evidence);
            assert!(run.tick(false, false).is_none());
        }
        assert!(run.auto.status(&[]).reader_available);
        assert!(run.pending().is_none());
        // The reader goes quiet: after the 5 s expiry the fallback counts
        // from then, not from the original release.
        let (tick, reason) = run.ticks(60, false, false).unwrap();
        assert_eq!(reason, AutoStopReason::MicReleasedRuleChanged);
        assert!(tick > 30, "the veto consumed no fallback time: {tick}");
    }

    #[test]
    fn explicit_end_while_the_mic_stays_held_stops_after_debounce_and_grace() {
        let mut run = Run::new();
        run.observe(CallState::Active);
        run.tick(true, true);
        // A quiet post-call screen re-read every 2 s (the reader heartbeat).
        let mut stopped = None;
        for i in 1..=40u32 {
            if i % 2 == 0 {
                assert_eq!(run.observe(CallState::Ended), Accepted::Evidence);
            }
            if let Some(reason) = run.tick(true, true) {
                stopped = Some((i, reason));
                break;
            }
        }
        // First Ended seen before tick 2 (1 s after the start of the loop):
        // the 20 s debounce opens the grace at tick 21, due at tick 31.
        assert_eq!(stopped, Some((31, AutoStopReason::CallEnded)));
    }

    #[test]
    fn ended_needs_an_active_call_first_in_the_same_reader_generation() {
        let mut run = Run::new();
        assert_eq!(run.observe(CallState::Ended), Accepted::Unknown);
        run.observe(CallState::Active);
        run.auto.new_reader_generation(2);
        assert_eq!(run.observe(CallState::Ended), Accepted::Unknown);
    }

    #[test]
    fn unknown_stale_and_old_observations_cancel_an_explicit_end() {
        for spoil in ["unknown", "incomplete", "over_age", "future", "expired"] {
            let mut run = Run::new();
            run.observe(CallState::Active);
            for _ in 0..22 {
                run.observe(CallState::Ended);
                run.tick(true, true);
            }
            assert!(run.pending().is_some(), "{spoil}");
            match spoil {
                "unknown" => assert_eq!(run.observe(CallState::Unknown), Accepted::Unknown),
                "incomplete" => {
                    assert_eq!(
                        run.observe_aged(CallState::Ended, 0, false),
                        Accepted::Unknown
                    )
                }
                "over_age" => assert_eq!(
                    run.observe_aged(CallState::Ended, MAX_SNAPSHOT_AGE_MS + 1, true),
                    Accepted::Unknown
                ),
                "future" => {
                    run.sequence += 1;
                    let ms = run.wall.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
                    let observation = CallObservation {
                        sequence: run.sequence,
                        snapshot_started_at_ms: ms + 1_000,
                        state: CallState::Ended,
                        complete: true,
                    };
                    assert_eq!(run.auto.accept(observation, run.at()), Accepted::Unknown);
                }
                _ => {
                    run.ticks(6, true, true);
                }
            }
            assert!(run.pending().is_none(), "{spoil}");
            assert!(run.ticks(40, true, true).is_none(), "{spoil}");
        }
    }

    #[test]
    fn a_delayed_end_after_a_newer_active_observation_is_rejected() {
        let mut run = Run::new();
        run.observe(CallState::Active);
        let wall_ms = run.wall.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let late_end = CallObservation {
            sequence: 2,
            snapshot_started_at_ms: wall_ms,
            state: CallState::Ended,
            complete: true,
        };
        run.sequence = 2;
        assert_eq!(run.observe(CallState::Active), Accepted::Evidence);
        assert_eq!(run.auto.accept(late_end, run.at()), Accepted::Unknown);
        assert!(run.auto.status(&[]).reader_available, "the active stays");
    }

    #[test]
    fn sleep_poll_gaps_and_clock_adjustments_reset_pending_evidence() {
        for jump in ["gap", "backward", "skew"] {
            let mut run = Run::new();
            run.ticks(2, true, true);
            run.ticks(25, false, false);
            assert!(run.pending().is_some(), "{jump}");
            match jump {
                "gap" => {
                    run.mono += S * 4;
                    run.wall += S * 4;
                }
                "backward" => run.wall -= S * 2,
                _ => run.wall += S * 5,
            }
            assert!(run.tick(false, false).is_none(), "{jump}");
            // Counting starts again: debounce + grace from here.
            assert!(run.pending().is_none(), "{jump}");
            let (tick, _) = run.ticks(40, false, false).unwrap();
            // Released counting restarts at the jump: grace at 20, due at 30.
            assert_eq!(tick, 30, "{jump}");
        }
    }

    #[test]
    fn a_failed_poll_and_pause_clear_everything_and_paused_never_decides() {
        let mut run = Run::new();
        run.ticks(25, false, false);
        assert!(run.pending().is_some());
        run.auto.poll_failed();
        assert!(run.pending().is_none());
        for _ in 0..100 {
            run.mono += S;
            run.wall += S;
            let absent = Run::sighting(false, false);
            assert!(
                run.auto
                    .poll(run.at(), Some(&absent), Timing::default(), true)
                    .is_none()
            );
        }
        assert!(run.pending().is_none());
    }

    #[test]
    fn keep_recording_disables_and_a_fresh_enable_needs_fresh_evidence() {
        let mut run = Run::new();
        run.ticks(25, false, false);
        assert!(run.pending().is_some());
        let generation = run.auto.generation();
        assert_eq!(run.auto.configure(false, None), Ok(false));
        assert_eq!(run.auto.generation(), generation + 1);
        assert!(run.pending().is_none());
        assert!(run.ticks(60, false, false).is_none());
        // Unchanged requests change nothing.
        assert_eq!(run.auto.configure(false, None), Ok(false));
        assert_eq!(run.auto.generation(), generation + 1);
        run.auto.configure(true, None).unwrap();
        assert!(run.tick(false, false).is_none());
        assert!(run.pending().is_none(), "evidence restarted at enable");
        let (tick, _) = run.ticks(40, false, false).unwrap();
        assert_eq!(tick, 30);
    }

    #[test]
    fn manual_recordings_stay_manual_until_explicitly_linked() {
        let mut auto = AutoStop::new(None, 1);
        assert!(!auto.status(&[]).enabled);
        assert!(auto.watched_app().is_none());
        assert!(auto.configure(true, None).is_err());
        assert!(auto.configure(false, Some(zoom())).is_err());
        assert_eq!(auto.configure(true, Some(zoom())), Ok(true));
        assert_eq!(auto.watched_app(), Some("zoom"));
        assert_eq!(auto.generation(), 2);
    }

    #[test]
    fn a_decision_is_confirmed_only_while_nothing_changed() {
        let mut run = Run::new();
        run.ticks(2, true, true);
        let mut decided = None;
        for _ in 0..40 {
            run.mono += S;
            run.wall += S;
            let absent = Run::sighting(false, false);
            if let Some((reason, evidence, mic_held)) =
                run.auto
                    .poll(run.at(), Some(&absent), Timing::default(), false)
            {
                decided = Some(StopDecision {
                    session_id: "s-1".into(),
                    generation: run.auto.generation(),
                    evidence,
                    reason,
                    mic_held,
                    app_name: "Zoom".into(),
                });
                break;
            }
        }
        let decision = decided.unwrap();
        assert!(run.auto.confirms(&decision));
        // A renewed active signal before the stop takes the session.
        run.observe(CallState::Active);
        assert!(!run.auto.confirms(&decision));
    }

    fn notice(reason: AutoStopReason, mic_held: bool, app_name: &str) -> NotificationRequest {
        saved_notice(&StopDecision {
            session_id: "s-1".into(),
            generation: 1,
            evidence: 1,
            reason,
            mic_held,
            app_name: app_name.into(),
        })
    }

    use broscribe_ipc::NotificationRequest;

    #[test]
    fn the_automatic_stop_notice_names_its_evidence_and_never_says_saved() {
        let ended = notice(AutoStopReason::CallEnded, false, "Zoom");
        assert_eq!(
            ended.title,
            broscribe_platform_native::NativePlatform::notification_title(
                "Recording Stopped Automatically",
                "Recording stopped automatically"
            )
        );
        assert_eq!(
            ended.body,
            "Zoom showed the call ended. BroScribe is transcribing it."
        );
        // Both microphone-release fallbacks read the same.
        for reason in [
            AutoStopReason::MicReleasedRuleChanged,
            AutoStopReason::MicReleasedTimeout,
        ] {
            let released = notice(reason, false, "Zoom");
            assert_eq!(
                released.title,
                broscribe_platform_native::NativePlatform::notification_title(
                    "Recording Stopped Automatically",
                    "Recording stopped automatically"
                )
            );
            assert_eq!(
                released.body,
                "Zoom released the microphone. BroScribe is transcribing it."
            );
        }
        let held = notice(AutoStopReason::CallEnded, true, "Zoom");
        assert_eq!(
            held.title,
            broscribe_platform_native::NativePlatform::notification_title(
                "Recording Stopped Automatically",
                "Recording stopped automatically"
            )
        );
        assert_eq!(
            held.body,
            "Zoom showed the call ended. BroScribe is transcribing it. Zoom still has \
             the microphone, so its next call won't start recording by itself. Start it \
             yourself if you need it."
        );
        // An app without a name is still a sentence.
        assert_eq!(
            notice(AutoStopReason::CallEnded, true, "").body,
            "The meeting app showed the call ended. BroScribe is transcribing it. The \
             meeting app still has the microphone, so its next call won't start recording \
             by itself. Start it yourself if you need it."
        );
        for reason in [
            AutoStopReason::CallEnded,
            AutoStopReason::MicReleasedRuleChanged,
            AutoStopReason::MicReleasedTimeout,
        ] {
            for mic_held in [false, true] {
                let n = notice(reason, mic_held, "Zoom");
                // The final transcript is still to come: "saved" waits for it.
                for text in [&n.title, &n.body] {
                    assert!(!text.to_lowercase().contains("saved"), "{text}");
                    assert!(!text.to_lowercase().contains("processed"), "{text}");
                }
                // A notice about a session, not a meeting: no action, and
                // nothing a state snapshot would withdraw.
                assert_eq!(n.id, "auto-stopped-s-1");
                assert!(n.actions.is_empty());
                assert!(n.meeting_id.is_none());
            }
        }
    }
}
