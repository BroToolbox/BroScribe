//! One-Hz platform polling, independent of inference and UI availability.
use crate::{
    Daemon,
    daemon::{AutoStart, lock},
};
use broscribe_detect::{DetectEvent, Detector};
use broscribe_ipc::RecordStartParams;
use std::sync::Arc;

impl Daemon {
    /// Install the platform-backed detector before serving; tests may omit it.
    pub fn set_detector(&self, detector: Detector) {
        *lock(&self.shared.detector) = Some(detector);
    }

    /// Installs a detector on the user's rules and policies. A part of them
    /// that does not load falls back without weakening the user's policies
    /// ([`broscribe_detect::Settings`]), so a typo in `detect.toml` or `config.toml` never
    /// keeps the daemon (and recording) from starting; `detect status`
    /// reports the problem.
    pub fn start_detection(
        &self,
        mic: Box<dyn broscribe_platform::MicUsageMonitor>,
        windows: Box<dyn broscribe_platform::WindowInspector>,
        clock: Box<dyn broscribe_detect::Clock>,
    ) -> broscribe_platform::Result<()> {
        let builtin = broscribe_detect::DetectRules::builtin()?;
        self.set_detector(Detector::new(builtin, mic, windows, clock));
        // A fresh detector has no detection to reclassify: no events. The
        // detector is installed, so the reload cannot fail.
        let _ = reload(&self.shared);
        Ok(())
    }

    /// `detect.reload`: applies the user's rules and policies, then starts,
    /// asks about or withdraws the detections a changed policy reclassified.
    pub(crate) async fn reload_detection(
        &self,
    ) -> Result<serde_json::Value, broscribe_ipc::IpcError> {
        let s = Arc::clone(&self.shared);
        let (status, events) = tokio::task::spawn_blocking(move || reload(&s))
            .await
            .map_err(|e| broscribe_ipc::IpcError::new("internal", format!("task failed: {e}")))??;
        for event in events {
            self.on_detection(event);
        }
        Ok(status)
    }

    /// Acts on a start, prompt or withdrawal. While recording, a detected
    /// app stays `Active` (or `Asking`, without a prompt); the stop starts
    /// or asks about it (`daemon::resume_detections`). `MeetingEnded` is
    /// detection bookkeeping: only the recording's automatic-stop evaluator
    /// ([`tick`]) stops a recording; a policy change never ends a meeting.
    fn on_detection(&self, event: DetectEvent) {
        match event {
            DetectEvent::MeetingDetected { app_id, title, .. } => {
                if !self.shared.is_recording() {
                    // Spawned so polling continues while the start waits
                    // out an audio check or reports a failure.
                    let d = self.clone();
                    let params = detected_start(app_id, title);
                    tokio::spawn(async move { d.auto_record(params, AutoStart::Detected).await });
                }
            }
            DetectEvent::AskUser { app_id, title, .. } => {
                if !self.shared.is_recording() {
                    self.ask_to_record(detected_start(app_id, title));
                }
            }
            DetectEvent::AskExpired { app_id } => self.expire_prompt(&app_id),
            DetectEvent::MeetingEnded { .. } => {}
        }
    }
}

/// One detector poll. A meeting app on the mic is also remote-meeting
/// evidence for the recording's final setup (design §5). The running
/// recording's bound app is watched (its windows read whatever its
/// detection state), and the apps on the mic become the linkable
/// candidates.
pub(crate) fn poll(
    s: &crate::daemon::Shared,
) -> broscribe_platform::Result<Option<Vec<DetectEvent>>> {
    let watched = lock(&s.session)
        .as_ref()
        .and_then(|a| a.auto_stop.watched_app().map(str::to_owned));
    let polled = {
        let mut detector = lock(&s.detector);
        let Some(detector) = detector.as_mut() else {
            return Ok(None);
        };
        detector.set_watched(watched);
        let polled = detector.poll();
        *lock(&s.auto_stop_candidates) = detector
            .candidates()
            .into_iter()
            .map(|(app_id, app_name)| broscribe_ipc::AutoStopApp { app_id, app_name })
            .collect();
        polled.map(|events| (events, detector.meeting_app_on_mic()))
    };
    let (events, on_mic) = polled?;
    if on_mic {
        crate::daemon::meeting_app_on_mic(s);
    }
    Ok(Some(events))
}

/// One one-Hz step: poll, act on starts/prompts (an end is bookkeeping
/// only), then feed the running recording's automatic stop, which is the
/// sole source of automatic stops.
pub(crate) async fn tick(daemon: &Daemon) {
    let s = Arc::clone(&daemon.shared);
    match tokio::task::spawn_blocking(move || poll(&s)).await {
        Ok(Ok(Some(events))) => {
            for event in events {
                daemon.on_detection(event);
            }
        }
        Ok(Ok(None)) => return,
        Ok(Err(_)) | Err(_) => {
            crate::daemon::auto_stop_poll_failed(&daemon.shared);
            return;
        }
    }
    let s = Arc::clone(&daemon.shared);
    let decision = tokio::task::spawn_blocking(move || crate::daemon::evaluate_auto_stop(&s))
        .await
        .ok()
        .flatten();
    if let Some(decision) = decision {
        daemon.auto_stop(decision).await;
    }
}

pub(crate) async fn run(daemon: Daemon) {
    let mut interval = tokio::time::interval(Detector::POLL_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! { () = daemon.shared.shutdown.wait() => return, _ = interval.tick() => {} }
        tick(&daemon).await;
    }
}

/// A detected meeting's window title is an observation, not a user decision, so
/// it is marked as such and a matching calendar title may replace it.
pub(crate) fn detected_start(app_id: String, title: Option<String>) -> RecordStartParams {
    RecordStartParams {
        app: Some(app_id),
        title_detected: title.is_some(),
        title,
        ..Default::default()
    }
}

/// The user's detection settings (`broscribe_detect::Settings`).
fn load_settings(data_dir: &std::path::Path) -> broscribe_detect::Settings {
    let config = broscribe_core::config::Config::load(data_dir);
    broscribe_detect::Settings::load(
        data_dir,
        config
            .as_ref()
            .map(|c| &c.detect.policies)
            .map_err(ToString::to_string),
    )
}

/// Detection state per app; `problem` says which of the user's rules or
/// policies do not load and what detection runs on instead.
pub(crate) fn status(
    shared: &crate::daemon::Shared,
) -> Result<serde_json::Value, broscribe_ipc::IpcError> {
    let problem = load_settings(&shared.cfg.data_dir).problem();
    let detector = lock(&shared.detector);
    let Some(detector) = detector.as_ref() else {
        return Ok(serde_json::json!({"enabled":false,"apps":[],"problem":problem}));
    };
    let apps: Vec<_> = detector
        .rules()
        .apps()
        .iter()
        .map(|app| serde_json::json!({"app_id":app.id,"state":detector.state(&app.id)}))
        .collect();
    Ok(serde_json::json!({"enabled":true,"apps":apps,"problem":problem}))
}

/// Applies the user's rules and policies and returns the status with the
/// events of detections a changed policy reclassified. Rules that do not
/// load leave the current ones (the built-in rules at start-up). The
/// session lock is held throughout, as admission holds it, so the running
/// recording's app is known and left alone.
pub(crate) fn reload(
    shared: &crate::daemon::Shared,
) -> Result<(serde_json::Value, Vec<DetectEvent>), broscribe_ipc::IpcError> {
    let settings = load_settings(&shared.cfg.data_dir);
    if let Some(problem) = settings.problem() {
        tracing::warn!("detection settings: {problem}");
    }
    let events = {
        let session = lock(&shared.session);
        let recording = session.as_ref().and_then(|a| a.app.as_deref());
        let mut detector = lock(&shared.detector);
        let detector = detector.as_mut().ok_or_else(|| {
            broscribe_ipc::IpcError::new("unsupported", "meeting detection is not configured")
        })?;
        if let Some(rules) = settings.rules {
            detector.set_rules(rules);
        }
        let policies = broscribe_detect::Settings::policies(settings.overrides, detector.rules());
        detector.set_policies(policies, recording)
    };
    Ok((status(shared)?, events))
}

#[cfg(test)]
mod tests {
    #[test]
    fn record_detected_window_titles_are_marked_as_detected() {
        let params = super::detected_start("meet".into(), Some("Meet - synthetic".into()));
        assert!(params.title_detected);
        assert_eq!(params.title.as_deref(), Some("Meet - synthetic"));
        assert!(!super::detected_start("meet".into(), None).title_detected);
        // A user-started recording is never marked detected.
        assert!(!broscribe_ipc::RecordStartParams::default().title_detected);
    }
}
