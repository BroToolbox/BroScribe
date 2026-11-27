//! AX observations are accepted only for the current, unpaused session and
//! its current reader incarnation.
//!
//! Reader incarnations are daemon-issued. `start_reader` names the current
//! one; a reader host that (re)starts a reader (first start, after its own
//! crash, a reconnect, a lagged event stream or a failed snapshot) asks for
//! a fresh one with `reader.attach`, naming the generation it was told. The
//! daemon issues it only while that generation is still current, so the
//! new reader's sequences start afresh while every batch of the earlier
//! incarnation is `stale_reader`. A plain subscription only reports the
//! current generation and never resets a running reader; neither does the
//! bounded re-announcement of [`tick`], which only a host that no longer
//! runs that incarnation acts on.
use crate::daemon::{Active, Shared, lock};
use broscribe_detect::DetectRules;
use broscribe_ipc::{Event, IpcError, ReaderDegraded, ReaderStatus, RecordingStatus};
use broscribe_readers::{IncompleteReason, ReaderBatch, ReaderEvent};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use std::{fs::OpenOptions, io::Write};

/// A reader sends a batch (its heartbeat at least) every two seconds; one
/// silent for this long, or with no batch this long after its incarnation
/// was issued (its start failed and the host could not say why), is
/// degraded.
const READER_SILENT: Duration = Duration::from_secs(10);

/// `RecordingState.reader`: what the session's meeting reader is doing,
/// apart from the live preview and capture health. `None`: the recording
/// has no meeting app to read.
pub(crate) fn status(active: &Active, recording: RecordingStatus) -> Option<ReaderStatus> {
    if active.reader.is_none() && active.auto_stop.app().is_none() && active.app.is_none() {
        return None;
    }
    Some(if recording == RecordingStatus::Paused {
        ReaderStatus::Paused
    } else if active.reader.is_none() {
        ReaderStatus::Unavailable
    } else {
        let silent = |since: Instant| since.elapsed() > READER_SILENT;
        match (active.reader_seen, active.reader_refusal) {
            // The host said why it could not start this incarnation.
            (None, Some(refusal)) => ReaderStatus::Refused { refusal },
            (None, None) if silent(active.reader_issued) => ReaderStatus::Degraded {
                reason: ReaderDegraded::Silent,
            },
            (None, None) => ReaderStatus::Starting,
            (Some((at, _)), _) if silent(at) => ReaderStatus::Degraded {
                reason: ReaderDegraded::Silent,
            },
            (Some((_, false)), _) => ReaderStatus::Degraded {
                reason: ReaderDegraded::UnusableCallState,
            },
            (Some((_, true)), _) => ReaderStatus::Reading,
        }
    })
}

/// A new reader incarnation of `active` (start, pause, resume, relink,
/// `reader.attach`): earlier batches become `stale_reader`, and nothing the
/// earlier one reported (when it was last heard of, its health, why it
/// could not start) carries over. The issuance time bounds how long it may
/// show as starting.
pub(crate) fn new_incarnation(s: &Shared, active: &mut Active) -> u64 {
    let generation = s.next_reader_generation();
    active.auto_stop.new_reader_generation(generation);
    active.reader_seen = None;
    active.reader_health = None;
    active.reader_refusal = None;
    active.reader_issued = Instant::now();
    active.reader_announced = Instant::now();
    generation
}

/// The 1 Hz reader check: publishes a state whose shown reader status
/// changed (a reader going silent), and announces the current incarnation
/// again when no reader of it is known to run (see [`announce_again`]). A
/// host that could not start it (no permission yet, a failed start, even
/// one it reported as refused), lost the answer of its `reader.attach`,
/// stopped it after a failed snapshot or abandoned it without a word has
/// forgotten it, so the announcement restarts it through `reader.attach`
/// from the incarnation that is current now; a host running it ignores
/// it. Opens the accessory host when a reader should run and no host is
/// subscribed.
pub(crate) fn tick(s: &Shared) {
    tick_at(s, Instant::now());
}

/// [`tick`] as of `now`: the one instant an announcement is measured
/// against and stamped with.
pub(crate) fn tick_at(s: &Shared, now: Instant) {
    let (started, changed, again) = lock(&s.session).as_mut().map_or((false, false, None), |a| {
        (
            matches!(event(a), Some(Event::StartReader { .. })),
            reader_shown_changed(a),
            announce_again(a, now),
        )
    });
    if let Some(event) = again {
        s.bus.publish(event);
    }
    if changed {
        s.publish_state();
    }
    if started && s.bus.subscriber_count() == 0 {
        s.launch_accessory();
    }
}

/// The current incarnation's announcement when it is due again at `now`
/// (see [`tick`]): only while a recovery announcement may be needed (see
/// [`recovery_may_be_needed`]), and at most once per [`READER_SILENT`]
/// since it was issued or last announced. A reader whose healthy
/// heartbeats keep arriving is never announced again; a host still
/// running the incarnation ignores the announcement.
fn announce_again(active: &mut Active, now: Instant) -> Option<Event> {
    if !recovery_may_be_needed(active, now)
        || now.saturating_duration_since(active.reader_announced) <= READER_SILENT
    {
        return None;
    }
    let start = event(active).filter(|e| matches!(e, Event::StartReader { .. }))?;
    active.reader_announced = now;
    Some(start)
}

/// Whether the host may no longer run the current incarnation at `now`:
/// nothing of it was heard, its latest batch reports a `Failed` snapshot
/// (after which a host stops the reader and forgets it), or its latest
/// batch is older than [`READER_SILENT`] (a host may have abandoned it
/// without saying so). None of these proves the reader stopped: the
/// announcement names the incarnation the host may still run, which it
/// then ignores, and only `reader.attach` issues a new one. A healthy
/// reader's heartbeat, or a transient failure's capped backoff, arrives
/// within [`READER_SILENT`].
fn recovery_may_be_needed(active: &Active, now: Instant) -> bool {
    active.reader_seen.is_none_or(|(at, _)| {
        now.saturating_duration_since(at) > READER_SILENT
            || active
                .reader_health
                .as_ref()
                .is_some_and(|h| h.incomplete == Some(IncompleteReason::Failed))
    })
}

/// The reader of `app` on the mic now, by the rules detection actually
/// runs on (the user's `detect.toml` when it loaded, else what detection
/// fell back to), never a second, independent load of `detect.toml` that
/// a broken file would make fail. Without a detector: the built-in rules.
/// Called under the session mutex (session → detector → monitor order).
pub(crate) fn resolve_for(s: &Shared, app: Option<&str>) -> Option<(String, u32)> {
    let app = app?;
    let rules = match lock(&s.detector).as_ref() {
        Some(detector) => detector.rules().clone(),
        None => DetectRules::builtin().ok()?,
    };
    let monitor = lock(&s.reader_monitor).clone();
    resolve(app, &rules, monitor.as_ref())
}

pub(crate) fn resolve(
    app: &str,
    rules: &DetectRules,
    monitor: &dyn broscribe_platform::MicUsageMonitor,
) -> Option<(String, u32)> {
    let rule = rules.app(app).or_else(|| rules.app_for_bundle(app))?;
    let users = monitor.mic_users().ok()?;
    let readers = broscribe_readers::rules::builtin_rules().ok()?;
    users.iter().find_map(|u| {
        let native = rules.resolve_app_bundle(u)?;
        if !rule.matches_bundle(&native) {
            return None;
        }
        // macOS bundle ids, Linux application ids and Windows executable
        // names all select AX/AT-SPI/UIA rules; anything else gets no reader.
        let reader = broscribe_readers::rules::reader_for_app(&readers, &native)?;
        Some((reader.id.clone(), u.responsible_pid.unwrap_or(u.pid)))
    })
}

pub(crate) fn lifecycle(s: &Shared, active: &Active) {
    if let Some(event) = event(active) {
        if matches!(&event, Event::StartReader { .. }) {
            s.launch_accessory();
        }
        s.bus.publish(event);
    }
}

pub(crate) fn event(active: &Active) -> Option<Event> {
    let id = active.handle.session_id().to_owned();
    let status = active.handle.status();
    if status.status == RecordingStatus::Paused {
        Some(Event::StopReader { session_id: id })
    } else {
        active
            .reader
            .as_ref()
            .map(|(app_id, pid)| Event::StartReader {
                session_id: id,
                app_id: app_id.clone(),
                pid: *pid,
                elapsed_s: status.elapsed_s,
                reader_generation: active.auto_stop.reader_generation(),
            })
    }
}

pub(crate) fn restart(s: &Shared) {
    if let Some(active) = lock(&s.session).as_ref() {
        lifecycle(s, active);
    }
}

/// `reader.attach`: a fresh incarnation for the reader host that was told
/// `told` for `session_id`, while that is still the session's current,
/// unpaused reader generation. Earlier batches become `stale_reader`, call
/// evidence starts afresh (an end needs a fresh active call), and the new
/// reader's sequence 1 is accepted. Nothing is published: the host starts
/// the returned incarnation itself, and later subscriptions report it.
pub(crate) fn attach(s: &Shared, session_id: &str, told: u64) -> Result<Value, IpcError> {
    let mut session = lock(&s.session);
    let active = session
        .as_mut()
        .filter(|a| a.handle.session_id() == session_id)
        .ok_or_else(|| IpcError::from(broscribe_core::lifecycle::stale_session(session_id)))?;
    let Some((app_id, pid)) = active.reader.clone() else {
        return Err(IpcError::new(
            IpcError::STALE_READER,
            "this recording has no meeting reader now",
        ));
    };
    if active.handle.status().status == RecordingStatus::Paused
        || active.auto_stop.reader_generation() != told
    {
        return Err(IpcError::new(
            IpcError::STALE_READER,
            "the reader of this recording changed meanwhile",
        ));
    }
    let generation = new_incarnation(s, active);
    drop(session);
    s.publish_state();
    Ok(json!({
        "session_id": session_id,
        "app_id": app_id,
        "pid": pid,
        "reader_generation": generation,
    }))
}

/// `reader.refused`: the reader host could not start incarnation `told` of
/// `session_id` for `reader` (app ID and process). Kept, diagnostics only,
/// while that is the session's current, unpaused incarnation of that
/// reader and none of its batches arrived; a stale report (an older or
/// guessed generation, one superseded by an attach, a reader that did
/// report) changes nothing. Never call evidence: automatic stop, the
/// incarnation's announcement and its health are untouched, so the reader
/// is announced again and a new incarnation clears the refusal.
pub(crate) fn refused(
    s: &Shared,
    session_id: &str,
    reader: (&str, u32),
    told: u64,
    refusal: broscribe_readers::ReaderStartRefusal,
) -> Result<Value, IpcError> {
    let mut session = lock(&s.session);
    let active = session
        .as_mut()
        .filter(|a| a.handle.session_id() == session_id)
        .ok_or_else(|| IpcError::from(broscribe_core::lifecycle::stale_session(session_id)))?;
    if active
        .reader
        .as_ref()
        .is_none_or(|(app, pid)| (app.as_str(), *pid) != reader)
    {
        return Err(IpcError::new(
            "invalid_input",
            "reader app does not own this session",
        ));
    }
    if active.handle.status().status == RecordingStatus::Paused
        || active.auto_stop.reader_generation() != told
        || active.reader_seen.is_some()
    {
        return Err(IpcError::new(
            IpcError::STALE_READER,
            "the reader of this recording changed or reported meanwhile",
        ));
    }
    active.reader_refusal = Some(refusal);
    let changed = reader_shown_changed(active);
    drop(session);
    if changed {
        s.publish_state();
    }
    Ok(json!({ "retained": true }))
}

/// A reader batch of the active, unpaused session from its current reader
/// incarnation (`reader_generation`); an older incarnation's batch is
/// `stale_reader` and changes nothing. Its call observation feeds the
/// session's automatic stop, timed by the detector's clock on receipt.
pub(crate) fn append(s: &Shared, session_id: &str, batch: ReaderBatch) -> Result<Value, IpcError> {
    // Read before the session mutex; the detector is never locked inside it here.
    let received = lock(&s.detector).as_ref().map(|d| d.reading());
    let mut session = lock(&s.session);
    let active = session
        .as_mut()
        .ok_or_else(|| IpcError::new("not_found", "no active recording"))?;
    if batch.session_id != session_id
        || active.handle.session_id() != session_id
        || active.handle.status().status == RecordingStatus::Paused
    {
        return Err(IpcError::new(
            "invalid_input",
            "reader batch does not belong to the active recording",
        ));
    }
    if active.reader.as_ref() != Some(&(batch.app.clone(), batch.pid)) {
        return Err(IpcError::new(
            "invalid_input",
            "reader app does not own this session",
        ));
    }
    if batch.reader_generation != active.auto_stop.reader_generation() {
        return Err(IpcError::new(
            IpcError::STALE_READER,
            "reader batch comes from an earlier reader of this recording",
        ));
    }
    let before = active.auto_stop.status(&[]);
    let call_state = received.map(|at| active.auto_stop.accept(batch.call_observation, at));
    let auto_stop_changed = before != active.auto_stop.status(&[]);
    let usable = call_state.map(|a| a == crate::autostop::Accepted::Evidence);
    // Without a detector clock nothing judges its call state: usable unless
    // it said otherwise.
    active.reader_seen = Some((Instant::now(), usable.unwrap_or(true)));
    // The incarnation did start after all: a refusal of it no longer holds.
    active.reader_refusal = None;
    // Diagnostics only, kept once the batch proved its incarnation current.
    let health_changed = active.reader_health.as_ref() != Some(&batch.health);
    if health_changed {
        active.reader_health = Some(batch.health);
    }
    let reader_changed = reader_shown_changed(active) || health_changed;
    let dir = s.cfg.journal_root().join(session_id);
    let mut captions = None;
    let count = batch.events.len();
    for event in batch.events {
        match &event {
            ReaderEvent::Roster(roster) => {
                if roster.members.is_empty() {
                    continue;
                }
                merge_roster(&mut active.observed_roster, &roster.members);
                let union = broscribe_readers::RosterEvent {
                    observed_s: roster.observed_s,
                    members: active.observed_roster.clone(),
                };
                broscribe_core::fsutil::write_json_atomic(&dir.join("roster.json"), &union)
                    .map_err(IpcError::from)?
            }
            ReaderEvent::Caption(_) | ReaderEvent::ActiveSpeaker(_) => {
                if captions.is_none() {
                    captions = Some(
                        OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(dir.join("captions.jsonl"))
                            .map_err(|_| IpcError::new("io", "opening reader journal"))?,
                    );
                }
                let file = captions.as_mut().expect("reader journal opened");
                serde_json::to_writer(&mut *file, &event)
                    .map_err(|_| IpcError::new("io", "serializing reader event"))?;
                file.write_all(b"\n")
                    .map_err(|_| IpcError::new("io", "writing reader event"))?;
            }
        }
    }
    if let Some(file) = captions {
        file.sync_data()
            .map_err(|_| IpcError::new("io", "syncing reader journal"))?;
    }
    drop(session);
    // Quiet heartbeats change nothing shown: no state event, no redraw.
    if count > 0 || auto_stop_changed || reader_changed {
        s.publish_state();
    }
    Ok(json!({ "accepted": count, "call_state_usable": usable }))
}

/// Records the reader status now shown for `active`; whether it changed
/// since the last one shown (a state event is due).
pub(crate) fn reader_shown_changed(active: &mut Active) -> bool {
    let now = status(active, active.handle.status().status);
    let changed = active.reader_shown != now;
    active.reader_shown = now;
    changed
}

fn merge_roster(
    union: &mut Vec<broscribe_readers::RosterMember>,
    snapshot: &[broscribe_readers::RosterMember],
) {
    use broscribe_readers::captions::folded_name;
    for member in snapshot {
        if let Some(existing) = union.iter_mut().find(|old| {
            old.email
                .as_deref()
                .zip(member.email.as_deref())
                .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
                || (folded_name(&old.name) == folded_name(&member.name)
                    && (old.email.is_none() || member.email.is_none()))
        }) {
            if existing.email.is_none() {
                existing.email = member.email.clone();
            }
            existing.is_self |= member.is_self;
        } else {
            union.push(member.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn record_roster_is_additive_across_empty_and_folded_snapshots() {
        let member = |name: &str, email: Option<&str>| broscribe_readers::RosterMember {
            name: name.into(),
            email: email.map(str::to_owned),
            is_self: false,
        };
        let mut roster = vec![member("Álex Example", None)];
        merge_roster(
            &mut roster,
            &[member("Alex Example", Some("alex@example.invalid"))],
        );
        merge_roster(&mut roster, &[]);
        merge_roster(&mut roster, &[member("Casey Example", None)]);
        assert_eq!(roster.len(), 2);
        assert_eq!(roster[0].email.as_deref(), Some("alex@example.invalid"));
    }
    #[test]
    fn record_reader_rule_is_resolved_from_bundle_not_detector_id() {
        let rules = broscribe_readers::builtin_rules().unwrap();
        for (bundle, expected) in [
            ("com.google.Chrome", "meet-chrome"),
            ("com.microsoft.edgemac", "meet-edge"),
            ("com.apple.Safari", "meet-safari"),
            ("Cisco-Systems.Spark", "webex"),
            ("com.cisco.webexmeetingsapp", "webex"),
        ] {
            let reader = broscribe_readers::rules::reader_for_app(&rules, bundle).unwrap();
            assert_eq!(reader.id, expected);
        }
    }

    /// Detected identities without a meeting UI to read; they record without
    /// a reader on purpose.
    const NO_READER: &[&str] = &["ptoneclk.exe"];

    #[test]
    fn every_detected_native_identity_selects_its_reader_or_explicitly_none() {
        let detect = broscribe_detect::DetectRules::builtin().unwrap();
        let readers = broscribe_readers::builtin_rules().unwrap();
        for app in detect.apps() {
            for id in &app.bundle_ids {
                let reader = broscribe_readers::rules::reader_for_app(&readers, id);
                if NO_READER.contains(&id.as_str()) {
                    assert!(reader.is_none(), "{id} must have no reader");
                    continue;
                }
                let reader = reader.unwrap_or_else(|| panic!("{id} has no reader rule"));
                let family = reader.id.split('-').next().unwrap();
                assert_eq!(family, app.id, "{id} selects {}", reader.id);
            }
        }
    }

    #[test]
    fn linux_and_windows_mic_users_resolve_reader_rules() {
        use broscribe_platform::{MicUser, fakes::FakeMicUsageMonitor};
        let rules = broscribe_detect::DetectRules::builtin().unwrap();
        let user = |id: &str| MicUser {
            pid: 4242,
            bundle_id: Some(id.into()),
            responsible_pid: None,
            responsible_bundle_id: None,
        };
        for (detected, native, expected) in [
            ("teams", "teams-for-linux", Some("teams")),
            ("teams", "MS-Teams.exe", Some("teams")),
            ("zoom", "us.zoom.Zoom", Some("zoom")),
            ("meet", "google-chrome-stable", Some("meet-chrome")),
            ("meet", "msedge.exe", Some("meet-edge")),
            ("slack", "com.slack.Slack", Some("slack")),
            ("discord", "discord.exe", Some("discord")),
            ("webex", "CiscoCollabHost.exe", Some("webex")),
            ("webex", "ptoneclk.exe", None),
        ] {
            let monitor = FakeMicUsageMonitor::new(vec![user(native)]);
            let resolved = resolve(detected, &rules, &monitor);
            assert_eq!(
                resolved,
                expected.map(|id| (id.to_owned(), 4242)),
                "{native}"
            );
        }
    }

    /// A browser's audio helper captures the mic; the reader must target the
    /// browser process the platform monitor named responsible for it.
    #[test]
    fn browser_audio_helper_reader_targets_the_responsible_browser() {
        use broscribe_platform::{MicUser, fakes::FakeMicUsageMonitor};
        let rules = broscribe_detect::DetectRules::builtin().unwrap();
        for (helper, browser, expected) in [
            (
                "google-chrome-stable",
                "google-chrome-stable",
                "meet-chrome",
            ),
            ("msedge.exe", "msedge.exe", "meet-edge"),
        ] {
            let monitor = FakeMicUsageMonitor::new(vec![MicUser {
                pid: 5001,
                bundle_id: Some(helper.into()),
                responsible_pid: Some(5000),
                responsible_bundle_id: Some(browser.into()),
            }]);
            assert_eq!(
                resolve("meet", &rules, &monitor),
                Some((expected.to_owned(), 5000)),
                "{helper}"
            );
        }
    }
}
