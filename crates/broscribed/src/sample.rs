//! Bounded setup capture under the helper's TCC identity.
use crate::bus::EventBus;
use crate::daemon::{Shared, lock};
use broscribe_core::Error;
use broscribe_ipc::{Event, IpcError, SetupLevel};
use broscribe_platform::paths::{
    HELPER_SAMPLE_PREFIX, helper_sample_owner_key, helper_sample_session_key,
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime},
};

/// Longest setup capture a requester may ask for.
const MAX_SECONDS: f64 = 120.0;
/// How long a finished sample waits for its requester, which consumes it within milliseconds.
const UNCLAIMED_GRACE: Duration = Duration::from_secs(60);
/// Sample dirs older than the longest capture plus a margin belong to no live request.
const ABANDONED_AFTER: Duration = Duration::from_secs(180);
/// Setup level events at most this often (10 Hz), however fast the
/// recording meters are configured.
const LEVELS_MIN_INTERVAL: Duration = Duration::from_millis(100);
const SILENT: SetupLevel = SetupLevel {
    peak_dbfs: -120.0,
    rms_dbfs: -120.0,
};

/// Linear level → dBFS in 0.1 dB steps, −120 for silence.
fn dbfs(linear: f32) -> f32 {
    if linear.is_finite() && linear > 0.0 {
        ((20.0 * linear.log10()).clamp(-120.0, 0.0) * 10.0).round() / 10.0
    } else {
        -120.0
    }
}

/// Peak and energy of what one track delivered since the last event.
#[derive(Default)]
struct Accumulator {
    peak: f32,
    sum_squares: f64,
    count: usize,
}

impl Accumulator {
    fn push(&mut self, samples: &[f32]) {
        for &sample in samples.iter().filter(|s| s.is_finite()) {
            self.peak = self.peak.max(sample.abs());
            self.sum_squares += f64::from(sample) * f64::from(sample);
        }
        self.count += samples.len();
    }

    fn take(&mut self) -> Option<SetupLevel> {
        let taken = std::mem::take(self);
        (taken.count > 0).then(|| SetupLevel {
            peak_dbfs: dbfs(taken.peak),
            rms_dbfs: dbfs((taken.sum_squares / taken.count as f64).sqrt() as f32),
        })
    }
}

/// Level events of one setup capture or audio check (design §4.1): peak and
/// RMS of what each track delivered since the last event, never audio. At
/// most one event per interval and none when nothing changed; events end
/// when the meter is dropped with its capture.
pub(crate) struct LevelMeter {
    bus: EventBus,
    session: Option<String>,
    interval: Duration,
    sent: Option<(Instant, SetupLevel, Option<SetupLevel>)>,
    me: Accumulator,
    others: Option<Accumulator>,
}

impl LevelMeter {
    /// `others`: meter a second track too (the audio check's test sound).
    pub(crate) fn new(shared: &Shared, session: Option<String>, others: bool) -> Self {
        Self::on(
            shared.bus.clone(),
            session,
            shared.cfg.meter_interval.max(LEVELS_MIN_INTERVAL),
            others,
        )
    }

    fn on(bus: EventBus, session: Option<String>, interval: Duration, others: bool) -> Self {
        Self {
            bus,
            session,
            interval,
            sent: None,
            me: Accumulator::default(),
            others: others.then(Accumulator::default),
        }
    }

    pub(crate) fn me(&mut self, samples: &[f32]) {
        self.me.push(samples);
    }

    pub(crate) fn others(&mut self, samples: &[f32]) {
        if let Some(others) = &mut self.others {
            others.push(samples);
        }
    }

    /// Ends the meter period once `interval` passed since the previous one
    /// and the microphone delivered something; publishes its levels unless
    /// they equal the last event's.
    pub(crate) fn tick(&mut self, now: Instant) {
        if self
            .sent
            .as_ref()
            .is_some_and(|(at, ..)| now.saturating_duration_since(*at) < self.interval)
        {
            return;
        }
        let Some(me) = self.me.take() else {
            return;
        };
        let others = self
            .others
            .as_mut()
            .map(|others| others.take().unwrap_or(SILENT));
        let unchanged = self
            .sent
            .as_ref()
            .is_some_and(|(_, m, o)| *m == me && *o == others);
        // A skipped period still counts, so the next one covers a whole
        // interval of both tracks.
        self.sent = Some((now, me, others));
        if !unchanged {
            self.bus.publish(Event::SetupLevels {
                session: self.session.clone(),
                me,
                others,
            });
        }
    }
}

fn capture_root(shared: &Shared) -> PathBuf {
    shared.cfg.cache_dir.join("helper-capture")
}

fn remove_sample_dir(dir: &Path) {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!("removing setup sample {}: {e}", dir.display()),
    }
}

/// A `sample-*` dir under the capture root.
struct SampleDir {
    path: PathBuf,
    name: String,
    /// Time since its mtime (zero if in the future); `None` if unreadable.
    age: Option<Duration>,
}

fn sample_dirs(root: &Path, now: SystemTime) -> Vec<SampleDir> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!("listing setup samples in {}: {e}", root.display());
            return Vec::new();
        }
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.starts_with(HELPER_SAMPLE_PREFIX) {
                return None;
            }
            let age = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .map(|at| now.duration_since(at).unwrap_or_default());
            Some(SampleDir {
                path: entry.path(),
                name,
                age,
            })
        })
        .collect()
}

/// Removes `sample-*` dirs under `root` whose mtime is older than `max_age`, sparing `active`'s.
fn reclaim_in(root: &Path, active: Option<&str>, max_age: Duration, now: SystemTime) {
    let active_key = active.map(helper_sample_session_key);
    for dir in sample_dirs(root, now) {
        if active_key.is_some() && helper_sample_owner_key(&dir.name) == active_key.as_deref() {
            continue;
        }
        if dir.age.is_some_and(|age| age > max_age) {
            remove_sample_dir(&dir.path);
        }
    }
}

/// Removes every `sample-*` dir under `root`, whatever its age or session.
fn remove_all_samples(root: &Path) {
    for dir in sample_dirs(root, SystemTime::now()) {
        remove_sample_dir(&dir.path);
    }
}

/// No requester survives a daemon restart: startup removes every sample a
/// previous daemon left behind.
pub(crate) fn reclaim_after_restart(shared: &Shared) {
    remove_all_samples(&capture_root(shared));
}

/// Removes setup samples whose requester is gone, never the active capture's.
pub(crate) fn reclaim_abandoned(shared: &Shared) {
    let active = lock(&shared.sample).as_ref().map(|c| c.session.clone());
    reclaim_in(
        &capture_root(shared),
        active.as_deref(),
        ABANDONED_AFTER,
        SystemTime::now(),
    );
}

/// Removes every sample dir of `session`.
fn remove_session_samples(root: &Path, session: &str) {
    let key = helper_sample_session_key(session);
    for dir in sample_dirs(root, SystemTime::now()) {
        if helper_sample_owner_key(&dir.name) == Some(key.as_str()) {
            remove_sample_dir(&dir.path);
        }
    }
}

/// Removes `dir` after `grace` unless its requester already consumed it.
fn reclaim_after(dir: PathBuf, grace: Duration) {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                tokio::time::sleep(grace).await;
                remove_sample_dir(&dir);
            });
        }
        // The abandoned-sample sweep reclaims it instead.
        Err(_) => tracing::warn!("no runtime to reclaim setup sample {}", dir.display()),
    }
}

#[derive(Default)]
pub(crate) struct Control {
    session: String,
    stop: AtomicBool,
    discard: AtomicBool,
}

/// The sample's capture slot; dropping it ends the sample's control too.
struct Admission<'a> {
    shared: &'a Shared,
    _reservation: crate::daemon::Reserved<'a>,
}
impl Drop for Admission<'_> {
    fn drop(&mut self) {
        lock(&self.shared.sample).take();
    }
}

pub(crate) fn stop(shared: &Shared, session: &str, discard: bool) -> Result<Value, IpcError> {
    broscribe_ipc::RequestId::parse(session)?;
    let sample = lock(&shared.sample);
    if let Some(control) = sample.as_ref().filter(|c| c.session == session) {
        if discard {
            control.discard.store(true, Ordering::Release);
        }
        control.stop.store(true, Ordering::Release);
    } else {
        let mut cancelled = lock(&shared.sample_cancelled);
        cancelled.push_back(session.into());
        while cancelled.len() > 128 {
            cancelled.pop_front();
        }
        drop(cancelled);
        drop(sample);
        if discard {
            // The requester discarded a finished capture it may never consume.
            remove_session_samples(&capture_root(shared), session);
        }
    }
    Ok(json!({"stopping":true}))
}

pub(crate) fn capture(
    shared: &Shared,
    id: &str,
    seconds: f64,
    purpose: &str,
) -> Result<Value, IpcError> {
    broscribe_ipc::RequestId::parse(id)?;
    if !seconds.is_finite()
        || !(0.1..=MAX_SECONDS).contains(&seconds)
        || !matches!(purpose, "enrollment" | "try_it" | "audio_check")
    {
        return Err(IpcError::new(
            "invalid_input",
            "invalid setup capture duration or purpose",
        ));
    }
    reclaim_abandoned(shared);
    let session = lock(&shared.session);
    let slot = shared.admit_capture(&session, "setup capture")?;
    let control = Arc::new(Control {
        session: id.into(),
        ..Default::default()
    });
    {
        let mut sample = lock(&shared.sample);
        if lock(&shared.sample_cancelled)
            .iter()
            .any(|cancelled| cancelled == id)
        {
            return Err(IpcError::new(
                "cancelled",
                "setup capture already discarded",
            ));
        }
        *sample = Some(control.clone());
    }
    let _admission = Admission {
        shared,
        _reservation: slot,
    };
    drop(session);
    let mic = lock(&shared.sample_mic).clone();
    // The microphone a recording would open (RT06): enrollment and try-it
    // samples describe the voice as recordings will hear it.
    let config = crate::daemon::config_or_default(&shared.cfg.data_dir);
    let device = crate::setup::capture_mic(mic.as_ref(), &config);
    let (send, receive) = mpsc::sync_channel(256);
    let overflow = Arc::new(AtomicBool::new(false));
    let dropped = overflow.clone();
    let epoch = Instant::now();
    let session = mic
        .start(
            device.as_deref(),
            Box::new(move |chunk| {
                if send.try_send(chunk).is_err() {
                    dropped.store(true, Ordering::Release);
                }
            }),
        )
        .map_err(|e| IpcError::new("io", format!("microphone capture: {e}")))?;
    let device_name = session.device_name();
    let mut chunks = Vec::new();
    let mut count = 0usize;
    let limit = (seconds * 384_000.0) as usize + 384_000;
    let mut levels = LevelMeter::new(shared, Some(id.into()), false);
    while epoch.elapsed().as_secs_f64() < seconds && !control.stop.load(Ordering::Acquire) {
        match receive.recv_timeout(Duration::from_millis(20)) {
            Ok(chunk) => {
                count += chunk.samples_mono_f32.len();
                if count > limit {
                    overflow.store(true, Ordering::Release);
                    break;
                }
                levels.me(&chunk.samples_mono_f32);
                chunks.push(chunk);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        levels.tick(Instant::now());
    }
    // No level event once the capture stopped.
    drop(levels);
    let failure = session.failure();
    session
        .stop()
        .map_err(|e| IpcError::new("io", format!("stopping microphone: {e}")))?;
    for chunk in receive.try_iter() {
        count += chunk.samples_mono_f32.len();
        if count > limit {
            overflow.store(true, Ordering::Release);
            break;
        }
        chunks.push(chunk);
    }
    if control.discard.load(Ordering::Acquire) {
        return Err(IpcError::new("cancelled", "setup capture discarded"));
    }
    if failure.is_some() || overflow.load(Ordering::Acquire) || chunks.is_empty() {
        return Err(IpcError::new("io", "setup capture failed or overran"));
    }
    let duration = epoch.elapsed().as_secs_f64().min(seconds);
    let samples = crate::audio_check::place_chunks(
        chunks,
        epoch,
        (duration * 16000.0).ceil() as usize,
        16000,
    )
    .map_err(IpcError::from)?;
    let mut published = publish(shared, &control, &samples)?;
    // The microphone actually opened: a connected pinned one (`device_id`),
    // or the default input (`null`).
    published["device_id"] = device.into();
    published["device_name"] = device_name.into();
    Ok(published)
}

/// Writes `samples` as `control`'s sample. Serialized with [`stop`] on
/// `shared.sample`: a discard that wins leaves nothing on disk, and one that
/// comes later finds no control and removes the published sample.
fn publish(shared: &Shared, control: &Arc<Control>, samples: &[f32]) -> Result<Value, IpcError> {
    let root = capture_root(shared);
    broscribe_platform_native::ipc::ensure_private_dir(&root)
        .map_err(|e| IpcError::from(Error::io_at(&root, e)))?;
    let key = helper_sample_session_key(&control.session);
    let dir = tempfile::Builder::new()
        .prefix(HELPER_SAMPLE_PREFIX)
        .suffix(&key)
        .tempdir_in(&root)
        .map_err(|e| IpcError::from(Error::io_at(&root, e)))?;
    broscribe_platform_native::ipc::ensure_private_dir(dir.path())
        .map_err(|e| IpcError::from(Error::io_at(dir.path(), e)))?;
    let path = dir.path().join("sample.wav");
    let mut file = tempfile::NamedTempFile::new_in(dir.path())
        .map_err(|e| IpcError::from(Error::io_at(dir.path(), e)))?;
    crate::audio_check::write_wav(file.as_file_mut(), samples, 16000).map_err(IpcError::from)?;
    file.as_file()
        .sync_all()
        .map_err(|e| IpcError::from(Error::io_at(&path, e)))?;
    let mut active = lock(&shared.sample);
    if control.discard.load(Ordering::Acquire) {
        // Dropping `file` and `dir` removes both.
        return Err(IpcError::new("cancelled", "setup capture discarded"));
    }
    file.persist(&path)
        .map_err(|e| IpcError::from(Error::io_at(&path, e.error)))?;
    let dir = dir.keep();
    if active.as_ref().is_some_and(|c| Arc::ptr_eq(c, control)) {
        active.take();
    }
    drop(active);
    reclaim_after(dir, UNCLAIMED_GRACE);
    Ok(json!({"path":path,"sample_rate":16000}))
}

pub(crate) fn permissions(shared: &Shared, request: Option<&str>) -> Result<Value, IpcError> {
    if let Some(permission) = request {
        if !matches!(permission, "microphone" | "system_audio") {
            return Err(IpcError::new("invalid_input", "unknown capture permission"));
        }
        let session = lock(&shared.session);
        let _slot = shared.admit_capture(&session, "permission request")?;
        drop(session);
        let devices = lock(&shared.devices).clone();
        let status = devices
            .request_permission(permission)
            .map_err(|e| IpcError::new("io", e.to_string()))?;
        lock(&shared.permission_results).insert(permission.into(), status);
    }
    let (access, changed) = shared.refresh_capture_access();
    if changed {
        shared.publish_state();
    }
    let status = |permission| match permission {
        broscribe_ipc::CapturePermission::Granted => "granted",
        broscribe_ipc::CapturePermission::Denied => "denied",
        broscribe_ipc::CapturePermission::NotDetermined => "not_determined",
        broscribe_ipc::CapturePermission::Restricted => "restricted",
        broscribe_ipc::CapturePermission::Unsupported => "unsupported",
        broscribe_ipc::CapturePermission::Unknown => "unknown",
    };
    Ok(json!({"microphone":status(access.microphone),"system_audio":status(access.system_audio)}))
}

pub(crate) fn capture_access(shared: &Shared) -> broscribe_ipc::CaptureAccess {
    let devices = lock(&shared.devices).clone();
    let status = |permission: &str| {
        let queried = match devices.permission_status(permission) {
            Ok(status) => status,
            Err(broscribe_core::Error::Unsupported(_)) => "unsupported".into(),
            Err(_) => "unknown".into(),
        };
        let reported = if queried == "not_determined" {
            lock(&shared.permission_results)
                .get(permission)
                .cloned()
                .unwrap_or(queried)
        } else {
            queried
        };
        broscribe_ipc::CapturePermission::from(reported.as_str())
    };
    broscribe_ipc::CaptureAccess {
        microphone: status("microphone"),
        system_audio: status("system_audio"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_dir(root: &Path, name: &str, age: Duration, now: SystemTime) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("sample.wav"), b"RIFF").unwrap();
        filetime::set_file_mtime(&dir, filetime::FileTime::from_system_time(now - age)).unwrap();
        dir
    }

    #[test]
    fn reclaim_removes_abandoned_samples_but_keeps_fresh_and_active_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let old = Duration::from_secs(600);
        let gone = sample_dir(tmp.path(), "sample-gone-req-Ab12Cd", old, now);
        let fresh = sample_dir(tmp.path(), "sample-fresh-Ef34Gh", Duration::ZERO, now);
        let active = sample_dir(
            tmp.path(),
            &format!("sample-Ij56Kl{}", helper_sample_session_key("live.one:2")),
            old,
            now,
        );
        let other = sample_dir(tmp.path(), "unrelated", old, now);
        reclaim_in(tmp.path(), Some("live.one:2"), ABANDONED_AFTER, now);
        assert!(!gone.exists());
        assert!(fresh.exists());
        assert!(active.exists());
        assert!(other.exists());
    }

    #[test]
    fn reclaim_removes_aged_legacy_samples_without_a_session() {
        let tmp = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let aged = sample_dir(tmp.path(), "sample-Ab12Cd", Duration::from_secs(600), now);
        let fresh = sample_dir(tmp.path(), "sample-Ef34Gh", Duration::ZERO, now);
        reclaim_in(tmp.path(), Some("r-0123456789abcdef"), ABANDONED_AFTER, now);
        assert!(!aged.exists());
        assert!(fresh.exists());
        // With no capture active, legacy names are reclaimed by age alone too.
        let aged = sample_dir(tmp.path(), "sample-Ij56Kl", Duration::from_secs(600), now);
        reclaim_in(tmp.path(), None, ABANDONED_AFTER, now);
        assert!(!aged.exists());
        assert!(fresh.exists());
    }

    #[test]
    fn restart_removes_every_sample_whatever_its_age() {
        let tmp = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let old = Duration::from_secs(600);
        let aged = sample_dir(tmp.path(), "sample-r-0123456789abcdef-Ab12Cd", old, now);
        let legacy = sample_dir(tmp.path(), "sample-Ef34Gh", old, now);
        let fresh = sample_dir(
            tmp.path(),
            "sample-r-fedcba9876543210-Ij56Kl",
            Duration::ZERO,
            now,
        );
        let other = sample_dir(tmp.path(), "unrelated", Duration::ZERO, now);
        remove_all_samples(tmp.path());
        assert!(!aged.exists());
        assert!(!legacy.exists());
        assert!(!fresh.exists());
        assert!(other.exists());
    }

    #[test]
    fn session_samples_match_whole_session_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let mine = sample_dir(
            tmp.path(),
            &format!("sample-Ab12Cd{}", helper_sample_session_key("req")),
            Duration::ZERO,
            now,
        );
        let longer = sample_dir(
            tmp.path(),
            &format!("sample-Ef34Gh{}", helper_sample_session_key("req-two")),
            Duration::ZERO,
            now,
        );
        remove_session_samples(tmp.path(), "req");
        assert!(!mine.exists());
        assert!(longer.exists());
    }

    #[tokio::test]
    async fn unclaimed_sample_is_removed_after_grace() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = sample_dir(
            tmp.path(),
            "sample-req-Ab12Cd",
            Duration::ZERO,
            SystemTime::now(),
        );
        reclaim_after(dir.clone(), Duration::from_millis(10));
        tokio::time::timeout(Duration::from_secs(5), async {
            while dir.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    // ---- level events ----

    type Levels = (Option<String>, SetupLevel, Option<SetupLevel>);

    fn levels_of(sub: &crate::bus::Subscriber) -> Vec<Levels> {
        std::iter::from_fn(|| sub.try_next())
            .map(|event| match event {
                Event::SetupLevels {
                    session,
                    me,
                    others,
                } => (session, me, others),
                other => panic!("unexpected event {other:?}"),
            })
            .collect()
    }

    /// At most one event per 100 ms of `elapsed`, plus the first.
    fn assert_throttled(levels: &[Levels], elapsed: Duration) {
        assert!(levels.len() >= 2, "{levels:?}");
        assert!(
            levels.len() as u128 <= elapsed.as_millis() / 100 + 1,
            "{} events in {elapsed:?}",
            levels.len()
        );
    }

    #[test]
    fn meter_throttles_and_skips_unchanged_levels() {
        let bus = EventBus::new(64);
        let sub = bus.subscribe();
        let interval = Duration::from_millis(100);
        let mut meter = LevelMeter::on(bus.clone(), Some("s".into()), interval, false);
        let t0 = Instant::now();
        meter.tick(t0);
        assert!(levels_of(&sub).is_empty(), "nothing heard yet");
        meter.me(&[0.5, -0.5]);
        meter.tick(t0);
        let first = levels_of(&sub);
        assert_eq!(
            first,
            vec![(
                Some("s".into()),
                SetupLevel {
                    peak_dbfs: -6.0,
                    rms_dbfs: -6.0
                },
                None
            )]
        );
        meter.me(&[0.1]);
        meter.tick(t0 + interval / 2);
        assert!(levels_of(&sub).is_empty(), "within the interval");
        meter.tick(t0 + interval);
        let second = levels_of(&sub);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].1.peak_dbfs, -20.0);
        meter.me(&[0.1]);
        meter.tick(t0 + interval * 3);
        assert!(levels_of(&sub).is_empty(), "unchanged levels");
        meter.me(&[0.0]);
        meter.tick(t0 + interval * 4);
        assert_eq!(levels_of(&sub)[0].1, SILENT);
    }

    struct NoEngine;
    impl broscribe_ipc::CaptureEngine for NoEngine {
        fn start(
            &self,
            _: broscribe_ipc::SessionConfig,
            _: broscribe_ipc::CaptureOutputs,
        ) -> broscribe_core::Result<Box<dyn broscribe_ipc::SessionHandle>> {
            Err(Error::Internal("no recording in this test".into()))
        }
        fn recover_orphans(
            &self,
            _: &Path,
            _: &Path,
        ) -> broscribe_core::Result<broscribe_ipc::OrphanRecovery> {
            Ok(broscribe_ipc::OrphanRecovery::default())
        }
    }

    /// A microphone delivering 10 ms of changing level every 10 ms until stopped.
    struct StreamingMic;
    struct Stream(Arc<AtomicBool>);
    impl broscribe_platform::CaptureSession for Stream {
        fn sample_rate(&self) -> u32 {
            16_000
        }
        fn failure(&self) -> Option<String> {
            None
        }
        fn stop(self: Box<Self>) -> broscribe_platform::Result<()> {
            Ok(())
        }
    }
    impl Drop for Stream {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    impl broscribe_platform::MicCapture for StreamingMic {
        fn devices(&self) -> broscribe_platform::Result<Vec<broscribe_platform::AudioDevice>> {
            Ok(Vec::new())
        }
        fn start(
            &self,
            _: Option<&str>,
            mut sink: broscribe_platform::MicSink,
        ) -> broscribe_platform::Result<Box<dyn broscribe_platform::CaptureSession>> {
            let stopped = Arc::new(AtomicBool::new(false));
            let stop = stopped.clone();
            std::thread::spawn(move || {
                let mut n = 0u16;
                while !stop.load(Ordering::Acquire) {
                    n = n.wrapping_add(1);
                    let amplitude = 0.05 * f32::from(1 + n % 19);
                    sink(broscribe_platform::MicChunk::new(
                        vec![amplitude; 160],
                        16_000,
                    ));
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
            Ok(Box::new(Stream(stopped)))
        }
    }

    fn daemon(tmp: &Path) -> crate::Daemon {
        let daemon = crate::Daemon::new(
            crate::DaemonConfig::new(
                tmp.join("library"),
                tmp.join("data"),
                tmp.join("cache"),
                crate::WorkerCommand::new("/bin/false"),
            ),
            Arc::new(NoEngine),
            Arc::new(broscribe_platform::fakes::FakeProcessProbe::new(
                std::process::id(),
            )),
            Arc::new(
                broscribe_core::maintenance::ModeLease::acquire(
                    &tmp.join("data"),
                    broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
                )
                .unwrap(),
            ),
        )
        .unwrap();
        daemon.set_setup_devices(
            Arc::new(broscribe_platform::devices::FakeDeviceInfo::default()),
            Arc::new(StreamingMic),
        );
        // Captures are admitted once startup recovery finished.
        daemon.finish_startup();
        daemon
    }

    #[test]
    fn sample_capture_publishes_throttled_levels_for_its_session_and_none_after() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path());
        let sub = d.subscribe();
        let started = Instant::now();
        capture(&d.shared, "levels-sample", 0.6, "enrollment").unwrap();
        let elapsed = started.elapsed();
        let levels = levels_of(&sub);
        assert_throttled(&levels, elapsed);
        assert!(levels.iter().all(|(session, me, others)| {
            session.as_deref() == Some("levels-sample")
                && me.peak_dbfs > -120.0
                && me.rms_dbfs <= me.peak_dbfs
                && others.is_none()
        }));
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(sub.try_next(), None);
    }

    #[test]
    fn stopped_sample_capture_publishes_no_more_levels() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path());
        let sub = d.subscribe();
        let capturing = d.clone();
        let worker = std::thread::spawn(move || {
            capture(&capturing.shared, "stopped-sample", 30.0, "enrollment")
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let first = loop {
            if let Some(event) = sub.try_next() {
                break event;
            }
            assert!(Instant::now() < deadline, "no level event while capturing");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(
            &first,
            Event::SetupLevels { session: Some(s), .. } if s == "stopped-sample"
        ));
        stop(&d.shared, "stopped-sample", true).unwrap();
        assert_eq!(worker.join().unwrap().unwrap_err().code, "cancelled");
        levels_of(&sub);
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(sub.try_next(), None);
    }

    /// A discard landing after capture ended but before publication wins:
    /// nothing persists. One landing after publication removes the sample.
    #[test]
    fn discard_racing_publication_leaves_no_sample() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path());
        let root = capture_root(&d.shared);
        let samples = [0.25f32; 160];
        let register = |id: &str| {
            let control = Arc::new(Control {
                session: id.into(),
                ..Default::default()
            });
            *lock(&d.shared.sample) = Some(control.clone());
            control
        };
        let discarded = broscribe_ipc::RequestId::random().to_string();
        let control = register(&discarded);
        stop(&d.shared, &discarded, true).unwrap();
        let err = publish(&d.shared, &control, &samples).unwrap_err();
        assert_eq!(err.code, "cancelled");
        assert!(sample_dirs(&root, SystemTime::now()).is_empty());
        lock(&d.shared.sample).take();

        let late = broscribe_ipc::RequestId::random().to_string();
        let control = register(&late);
        let published = publish(&d.shared, &control, &samples).unwrap();
        let path = PathBuf::from(published["path"].as_str().unwrap());
        assert!(path.exists());
        assert!(lock(&d.shared.sample).is_none(), "control still registered");
        stop(&d.shared, &late, true).unwrap();
        assert!(!path.exists());
    }

    /// Hears a changing level and plays a steady −6 dBFS test sound.
    struct MeteredCheck;
    impl crate::audio_check::AudioCheck for MeteredCheck {
        fn run(
            &self,
            seconds: f64,
            _: &Path,
            _: Option<&str>,
            levels: &mut dyn FnMut(&[f32], &[f32]),
        ) -> broscribe_core::Result<broscribe_dsp::AudioCheckReport> {
            let started = Instant::now();
            let mut n = 0u16;
            while started.elapsed().as_secs_f64() < seconds {
                n = n.wrapping_add(1);
                levels(&[], &[0.5; 160]);
                levels(&[0.01 * f32::from(1 + n % 7); 160], &[]);
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(broscribe_dsp::AudioCheckReport {
                delay_ms: Some(12.5),
                leakage_db: Some(-30.0),
                correlation_peak: 0.9,
                noise_floor_dbfs: -70.0,
                recommendation: broscribe_dsp::EchoHandling::ResidueGating,
                bluetooth_call_profile: false,
            })
        }
    }

    #[test]
    fn audio_check_publishes_me_and_others_levels_and_reports_the_setup() {
        let tmp = tempfile::tempdir().unwrap();
        let d = daemon(tmp.path());
        d.set_audio_check(Arc::new(MeteredCheck));
        let sub = d.subscribe();
        let started = Instant::now();
        let result = crate::audio_check::run(&d.shared, 0.6).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(result["setup"], "speakers_mic");
        let levels = levels_of(&sub);
        assert_throttled(&levels, elapsed);
        assert!(levels.iter().all(|(session, me, others)| {
            session.is_none()
                && me.peak_dbfs > -120.0
                && *others
                    == Some(SetupLevel {
                        peak_dbfs: -6.0,
                        rms_dbfs: -6.0,
                    })
        }));
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(sub.try_next(), None);
    }
}
