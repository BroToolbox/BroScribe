//! Synthetic tests at the daemon's CaptureEngine boundary.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use broscribe_capture::{
    Recorder,
    journal::{self, Track},
    timeline::{Timeline, TimelineOptions},
};
use broscribe_ipc::{
    CaptureEngine, CaptureEvents, CaptureNotice, CaptureOutputs, CaptureWarning, LiveFrame,
    LiveSink, SessionConfig, SessionHandle, TrackState,
};
use broscribe_platform::{
    AudioDevice, CaptureSession, MicCapture, MicChunk, MicSink, Result, SystemAudioCapture,
    fakes::FakeSystemInfo, priority::FakeProcessPriority,
};

#[derive(Default, Clone)]
struct Source(Arc<Mutex<Option<MicSink>>>);
impl Source {
    fn open(&self, sink: MicSink) -> Box<dyn CaptureSession> {
        *self.0.lock().unwrap() = Some(sink);
        Box::new(Running(self.clone()))
    }
    fn push(&self, at: Instant, samples: Vec<f32>) {
        let mut chunk = MicChunk::new(samples, 16_000);
        chunk.host_time = Some(at);
        self.0.lock().unwrap().as_mut().unwrap()(chunk);
    }
}
struct Running(Source);
impl Drop for Running {
    fn drop(&mut self) {
        self.0.0.lock().unwrap().take();
    }
}
impl CaptureSession for Running {
    fn sample_rate(&self) -> u32 {
        16_000
    }
    fn failure(&self) -> Option<String> {
        None
    }
    fn stop(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}
impl MicCapture for Source {
    fn devices(&self) -> Result<Vec<AudioDevice>> {
        Ok(Vec::new())
    }
    fn start(&self, _: Option<&str>, sink: MicSink) -> Result<Box<dyn CaptureSession>> {
        Ok(self.open(sink))
    }
}
impl SystemAudioCapture for Source {
    fn set_excluded_pids(&self, _: &[u32]) -> Result<()> {
        Ok(())
    }
    fn start(&self, sink: MicSink) -> Result<Box<dyn CaptureSession>> {
        Ok(self.open(sink))
    }
}
#[derive(Default)]
struct Preview {
    frames: Mutex<Vec<LiveFrame>>,
    notices: Mutex<Vec<CaptureNotice>>,
}
impl LiveSink for Preview {
    fn push(&self, frame: LiveFrame) {
        self.frames.lock().unwrap().push(frame);
    }
}
impl CaptureEvents for Preview {
    fn notice(&self, notice: CaptureNotice) {
        self.notices.lock().unwrap().push(notice);
    }
}
fn config(root: &std::path::Path) -> SessionConfig {
    SessionConfig {
        session_id: "synthetic-session".into(),
        journal_root: root.into(),
        library_root: root.canonicalize().unwrap(),
        started_at: chrono::Utc::now().fixed_offset(),
        title: None,
        app: None,
        language: Some("en".into()),
        setup: Some("headphones".into()),
        tags: Vec::new(),
        title_detected: false,
        echo_handling: None,
        in_person_setup: None,
        mic_device: None,
    }
}

#[test]
fn process_now_preserves_post_cutoff_audio_and_all_session_metadata() {
    let root = tempfile::tempdir().unwrap();
    let mic = Arc::new(Source::default());
    let system = Arc::new(Source::default());
    let preview = Arc::new(Preview::default());
    let recorder = Recorder::new(
        mic.clone(),
        system.clone(),
        Arc::new(FakeSystemInfo::default()),
        Arc::new(FakeProcessPriority::default()),
    );
    let mut session = recorder
        .start(
            config(root.path()),
            CaptureOutputs {
                live: preview.clone(),
                events: preview.clone(),
            },
        )
        .unwrap();
    let first = Instant::now();
    mic.push(first, vec![0.5; 1600]);
    system.push(first, vec![0.125; 1600]);
    // Short audio remains in the open chunk, so the live path cannot see it.
    session.bookmark(Some("Synthetic marker")).unwrap();
    assert!(preview.frames.lock().unwrap().is_empty());
    let cutoff = session.snapshot_cutoff().unwrap();
    assert_eq!(cutoff.chunks.get(&broscribe_ipc::Track::Mic), Some(&0));
    assert_eq!(cutoff.chunks.get(&broscribe_ipc::Track::System), Some(&0));
    let dir = root.path().join("synthetic-session");
    let before = journal::load_manifest(&dir).unwrap();
    let provisional = Timeline::plan(
        &dir,
        &before,
        &TimelineOptions {
            cutoff: Some(journal::Cutoff {
                mic: Some(0),
                system: Some(0),
            }),
            sample_rate: Some(16_000),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        provisional
            .read_track(Track::Mic)
            .unwrap()
            .iter()
            .filter(|v| **v == 0.5)
            .count(),
        1600
    );
    mic.push(first + Duration::from_millis(100), vec![0.75; 1600]);
    system.push(first + Duration::from_millis(100), vec![0.25; 1600]);
    session.set_language("pt").unwrap();
    session.note("Invented test note").unwrap();
    session.add_tags(&["synthetic".into()]).unwrap();
    session
        .participant("p-synthetic-alex", "Synthetic Alex", true)
        .unwrap();
    assert_eq!(session.status().participants, ["p-synthetic-alex"]);
    session
        .participant("p-synthetic-alex", "Synthetic Alex", false)
        .unwrap();
    assert!(session.status().participants.is_empty());
    session
        .participant("p-synthetic-blair", "Synthetic Blair", true)
        .unwrap();
    assert_eq!(session.status().participants, ["p-synthetic-blair"]);
    session.pause().unwrap();
    session.resume().unwrap();
    let stopped = session.stop().unwrap();
    let manifest = journal::load_manifest(&stopped.journal_dir).unwrap();
    assert_eq!(manifest.state, journal::SessionState::Stopped);
    assert_eq!(manifest.bookmarks.len(), 1);
    assert_eq!(manifest.notes.len(), 1);
    assert_eq!(manifest.languages[0].language, "pt");
    assert_eq!(manifest.meta.tags, ["synthetic"]);
    assert_eq!(manifest.participants.len(), 2);
    assert_eq!(manifest.participants[0].person_ref, "p-synthetic-alex");
    assert!(manifest.participants[0].removed);
    assert!(manifest.participants[0].name.is_none());
    assert_eq!(manifest.participants[1].person_ref, "p-synthetic-blair");
    assert!(!manifest.participants[1].removed);
    assert_eq!(
        manifest.participants[1].name.as_deref(),
        Some("Synthetic Blair")
    );
    assert_eq!(manifest.participants[1].evidence, ["user"]);
    assert_eq!(manifest.pauses.len(), 1);
    assert!(manifest.pauses[0].to_ns.is_some());
    let timeline = Timeline::plan(
        &dir,
        &manifest,
        &TimelineOptions {
            sample_rate: Some(16_000),
            ..Default::default()
        },
    )
    .unwrap();
    let audio = timeline.read_track(Track::Mic).unwrap();
    assert_eq!(audio.iter().filter(|v| **v == 0.5).count(), 1600);
    assert_eq!(audio.iter().filter(|v| **v == 0.75).count(), 1600);
    assert!(
        dir.exists(),
        "processing never implicitly deletes the journal"
    );
    let frames = preview.frames.lock().unwrap();
    assert_eq!(
        frames
            .iter()
            .filter(|f| f.track == broscribe_ipc::Track::Mic)
            .map(|f| f.samples.len())
            .sum::<usize>(),
        3200
    );
}

#[test]
fn disk_guard_rejects_start_without_opening_devices() {
    let root = tempfile::tempdir().unwrap();
    let mic = Arc::new(Source::default());
    let system = Arc::new(Source::default());
    let info = FakeSystemInfo {
        free_disk_bytes: 1,
        ..Default::default()
    };
    let recorder = Recorder::new(
        mic.clone(),
        system.clone(),
        Arc::new(info),
        Arc::new(FakeProcessPriority::default()),
    );
    let preview = Arc::new(Preview::default());
    assert!(
        recorder
            .start(
                config(root.path()),
                CaptureOutputs {
                    live: preview.clone(),
                    events: preview
                }
            )
            .is_err()
    );
    assert!(mic.0.lock().unwrap().is_none());
    assert!(system.0.lock().unwrap().is_none());
    assert!(!root.path().join("synthetic-session").exists());
}

#[test]
fn dropping_handle_closes_a_recoverable_journal() {
    let root = tempfile::tempdir().unwrap();
    let mic = Arc::new(Source::default());
    let system = Arc::new(Source::default());
    let recorder = Recorder::new(
        mic.clone(),
        system,
        Arc::new(FakeSystemInfo::default()),
        Arc::new(FakeProcessPriority::default()),
    );
    let preview = Arc::new(Preview::default());
    let session = recorder
        .start(
            config(root.path()),
            CaptureOutputs {
                live: preview.clone(),
                events: preview,
            },
        )
        .unwrap();
    mic.push(Instant::now(), vec![0.25; 1600]);
    drop(session);
    // A corrupt journal next to it is reported, never hides it.
    let corrupt = root.path().join("corrupt-session");
    std::fs::create_dir(&corrupt).unwrap();
    std::fs::write(corrupt.join(journal::MANIFEST_FILE), b"not json").unwrap();
    let recovery = recorder
        .recover_orphans(root.path(), &root.path().canonicalize().unwrap())
        .unwrap();
    assert_eq!(recovery.recovered.len(), 1);
    assert_eq!(recovery.recovered[0].session_id, "synthetic-session");
    let manifest = journal::load_manifest(&recovery.recovered[0].journal_dir).unwrap();
    assert_eq!(manifest.mic.chunks[0].frames, Some(1600));
    assert_eq!(manifest.state, journal::SessionState::Stopped);
    assert_eq!(recovery.failed.len(), 1);
    let failed = &recovery.failed[0];
    assert_eq!(failed.session_id, "corrupt-session");
    assert_eq!(failed.journal_dir, corrupt);
    assert!(failed.corrupt);
    assert!(!failed.code.is_empty());
    assert_eq!(
        std::fs::read(corrupt.join(journal::MANIFEST_FILE)).unwrap(),
        b"not json",
        "a corrupt journal is preserved byte for byte"
    );
    // An unreadable journal root is an error, not an empty sweep.
    let not_a_root = root.path().join("not-a-root");
    std::fs::write(&not_a_root, b"").unwrap();
    assert!(recorder.recover_orphans(&not_a_root, root.path()).is_err());
}

/// A device whose current session can be failed and whose next starts can
/// be refused, like a microphone lost mid-meeting.
#[derive(Default)]
struct Flaky {
    sink: Mutex<Option<MicSink>>,
    epoch: AtomicUsize,
    failed: AtomicBool,
    refuse: AtomicUsize,
    restart_held: Mutex<bool>,
    restart_ready: std::sync::Condvar,
}
struct FlakySession(Arc<Flaky>, usize);
struct RestartHold(Arc<Flaky>);
impl Drop for RestartHold {
    fn drop(&mut self) {
        *self.0.restart_held.lock().unwrap() = false;
        self.0.restart_ready.notify_all();
    }
}
impl Flaky {
    fn hold_restarts(self: &Arc<Self>) -> RestartHold {
        *self.restart_held.lock().unwrap() = true;
        RestartHold(self.clone())
    }
    fn push(&self, samples: Vec<f32>) {
        if let Some(sink) = self.sink.lock().unwrap().as_mut() {
            let mut chunk = MicChunk::new(samples, 16_000);
            chunk.host_time = Some(Instant::now());
            sink(chunk);
        }
    }
    fn fail(&self) {
        self.failed.store(true, Ordering::SeqCst);
        self.sink.lock().unwrap().take();
    }
    fn open(self: &Arc<Self>, sink: MicSink) -> Result<Box<dyn CaptureSession>> {
        if self
            .refuse
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(broscribe_platform::Error::Unsupported(
                "synthetic device is gone".into(),
            ));
        }
        let held = self
            .restart_ready
            .wait_while(self.restart_held.lock().unwrap(), |held| *held)
            .unwrap();
        drop(held);
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.failed.store(false, Ordering::SeqCst);
        *self.sink.lock().unwrap() = Some(sink);
        Ok(Box::new(FlakySession(self.clone(), epoch)))
    }
}
impl CaptureSession for FlakySession {
    fn sample_rate(&self) -> u32 {
        16_000
    }
    fn failure(&self) -> Option<String> {
        (self.0.epoch.load(Ordering::SeqCst) == self.1 && self.0.failed.load(Ordering::SeqCst))
            .then(|| "synthetic device lost".to_owned())
    }
    fn stop(self: Box<Self>) -> Result<()> {
        if self.0.epoch.load(Ordering::SeqCst) == self.1 {
            self.0.sink.lock().unwrap().take();
        }
        Ok(())
    }
}
struct FlakyMic(Arc<Flaky>);
impl MicCapture for FlakyMic {
    fn devices(&self) -> Result<Vec<AudioDevice>> {
        Ok(Vec::new())
    }
    fn start(&self, _: Option<&str>, sink: MicSink) -> Result<Box<dyn CaptureSession>> {
        self.0.open(sink)
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn warnings(preview: &Preview) -> Vec<CaptureWarning> {
    preview
        .notices
        .lock()
        .unwrap()
        .iter()
        .filter_map(|n| match n {
            CaptureNotice::Warning(w) => Some(w.clone()),
            _ => None,
        })
        .collect()
}

/// The mic failing (and refusing its first restart) is reported as typed
/// `TrackDown` warnings and in health, while the system track keeps
/// capturing and journaling; the restart reports `TrackResumed`.
#[test]
fn a_failed_mic_reports_track_down_then_resumed_while_system_records() {
    let root = tempfile::tempdir().unwrap();
    let mic = Arc::new(Flaky::default());
    let system = Arc::new(Source::default());
    let preview = Arc::new(Preview::default());
    let mut session = Recorder::new(
        Arc::new(FlakyMic(mic.clone())),
        system.clone(),
        Arc::new(FakeSystemInfo::default()),
        Arc::new(FakeProcessPriority::default()),
    )
    .start(
        config(root.path()),
        CaptureOutputs {
            live: preview.clone(),
            events: preview.clone(),
        },
    )
    .unwrap();
    mic.push(vec![0.25; 160]);
    system.push(Instant::now(), vec![0.5; 160]);
    // A successful reconnect waits until the failed attempt is observed.
    // The guard releases it on failure too, so teardown cannot hang.
    let restart = mic.hold_restarts();
    mic.refuse.store(1, Ordering::SeqCst);
    mic.fail();
    let mut system_frames = 160u64;
    let mut down = None;
    wait_until("the mic waiting after a refused restart", || {
        {
            let notices = preview.notices.lock().unwrap();
            assert!(
                notices
                    .iter()
                    .all(|n| !matches!(n, CaptureNotice::Stopped { .. })),
                "capture stopped before reconnect: {notices:?}"
            );
        }
        let health = session.health();
        let m = &health.tracks[0];
        let waiting =
            matches!(m.state, TrackState::Waiting | TrackState::Restarting) && m.failures >= 2;
        if waiting {
            down = Some(health);
        }
        waiting
    });
    let health = down.unwrap();
    assert_eq!(health.tracks.len(), 2);
    assert_eq!(health.tracks[0].track, broscribe_ipc::Track::Mic);
    assert_eq!(health.tracks[1].track, broscribe_ipc::Track::System);
    assert_eq!(health.tracks[1].state, TrackState::Capturing);
    assert_eq!(health.tracks[1].failures, 0);
    assert!(health.tracks[0].last_error.is_some());
    // A bounded block during the failure must be durable before reconnect;
    // callback-queue overflow is covered by a separate backpressure test.
    system.push(Instant::now(), vec![0.5; 160]);
    system_frames += 160;
    let cutoff = session.snapshot_cutoff().unwrap();
    let system_cutoff = cutoff.chunks[&broscribe_ipc::Track::System];
    drop(restart);
    // The notice follows the health update; wait for it rather than race it.
    let mic_down = |w: &CaptureWarning| {
        matches!(
            w,
            CaptureWarning::TrackDown {
                track: broscribe_ipc::Track::Mic,
                retry_in_s,
                ..
            } if *retry_in_s >= 0.0
        )
    };
    wait_until("the failure and the refused restart warnings", || {
        warnings(&preview).iter().filter(|w| mic_down(w)).count() >= 2
    });
    wait_until("the mic resumed warning", || {
        {
            let notices = preview.notices.lock().unwrap();
            assert!(
                notices
                    .iter()
                    .all(|n| !matches!(n, CaptureNotice::Stopped { .. })),
                "capture stopped during reconnect: {notices:?}"
            );
        }
        warnings(&preview).iter().any(|w| {
            matches!(
                w,
                CaptureWarning::TrackResumed { track: broscribe_ipc::Track::Mic, gap_s }
                    if *gap_s > 0.0
            )
        })
    });
    assert_eq!(session.health().tracks[0].state, TrackState::Capturing);
    assert_eq!(
        warnings(&preview).iter().filter(|w| mic_down(w)).count(),
        2,
        "the failure and the refused restart only"
    );
    system.push(Instant::now(), vec![0.5; 160]);
    system_frames += 160;
    let stopped = session.stop().unwrap();
    let manifest = journal::load_manifest(&stopped.journal_dir).unwrap();
    assert_eq!(manifest.stop_reason.as_deref(), Some("user"));
    let frames = manifest
        .system
        .chunks
        .iter()
        .filter_map(|c| c.frames)
        .sum::<u64>();
    assert_eq!(frames, system_frames, "the system track kept journaling");
    let before_reconnect: u64 = manifest
        .system
        .chunks
        .iter()
        .filter(|c| c.seq <= system_cutoff)
        .filter_map(|c| c.frames)
        .sum();
    assert_eq!(
        before_reconnect, 320,
        "healthy audio was durable before reconnect"
    );
    assert_eq!(manifest.system.dropped_frames, 0);
}

/// Free space in the warning band warns once, as typed `DiskLow`, before the
/// protective stop.
#[test]
fn low_disk_warns_once_before_the_protective_stop() {
    /// Start-up and two checks in the warning band, then below the stop.
    #[derive(Default)]
    struct Filling(AtomicUsize);
    impl broscribe_platform::SystemInfo for Filling {
        fn chip(&self) -> String {
            String::new()
        }
        fn memory_bytes(&self) -> u64 {
            0
        }
        fn free_disk_bytes(&self, _: &std::path::Path) -> Result<u64> {
            Ok(if self.0.fetch_add(1, Ordering::AcqRel) < 3 {
                800 << 20
            } else {
                1
            })
        }
        fn cpu_cores(&self) -> usize {
            1
        }
    }
    let root = tempfile::tempdir().unwrap();
    let preview = Arc::new(Preview::default());
    let session = Recorder::new(
        Arc::new(Source::default()),
        Arc::new(Source::default()),
        Arc::new(Filling::default()),
        Arc::new(FakeProcessPriority::default()),
    )
    .start(
        config(root.path()),
        CaptureOutputs {
            live: preview.clone(),
            events: preview.clone(),
        },
    )
    .unwrap();
    wait_until("the protective stop", || {
        preview
            .notices
            .lock()
            .unwrap()
            .iter()
            .any(|n| matches!(n, CaptureNotice::Stopped { .. }))
    });
    let notices = preview.notices.lock().unwrap().clone();
    let low: Vec<usize> = notices
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n, CaptureNotice::Warning(CaptureWarning::DiskLow { .. })))
        .map(|(i, _)| i)
        .collect();
    let stop = notices
        .iter()
        .position(|n| matches!(n, CaptureNotice::Stopped { reason } if reason == "disk_low"))
        .expect("stopped for disk_low");
    assert_eq!(low.len(), 1, "warned exactly once");
    assert!(low[0] < stop, "warned before the stop");
    let CaptureNotice::Warning(CaptureWarning::DiskLow {
        free_bytes,
        stop_below_bytes,
        stop_below_s,
        ..
    }) = &notices[low[0]]
    else {
        unreachable!()
    };
    assert_eq!(*free_bytes, 800 << 20);
    assert_eq!(*stop_below_bytes, 500 << 20);
    assert_eq!(*stop_below_s, 15.0 * 60.0);
    drop(session);
}

#[test]
fn delayed_paused_callback_never_reaches_the_journal() {
    let root = tempfile::tempdir().unwrap();
    let mic = Arc::new(Source::default());
    let recorder = Recorder::new(
        mic.clone(),
        Arc::new(Source::default()),
        Arc::new(FakeSystemInfo::default()),
        Arc::new(FakeProcessPriority::default()),
    );
    let preview = Arc::new(Preview::default());
    let mut session = recorder
        .start(
            config(root.path()),
            CaptureOutputs {
                live: preview.clone(),
                events: preview,
            },
        )
        .unwrap();
    session.pause().unwrap();
    let captured_during_pause = Instant::now();
    std::thread::sleep(Duration::from_millis(5));
    session.resume().unwrap();
    mic.push(captured_during_pause, vec![0.9; 16]);
    let cutoff = session.snapshot_cutoff().unwrap();
    assert!(cutoff.chunks.is_empty());
    let stopped = session.stop().unwrap();
    let manifest = journal::load_manifest(&stopped.journal_dir).unwrap();
    assert!(manifest.mic.chunks.is_empty());
}

#[test]
fn record_failed_device_start_removes_empty_journal() {
    struct Missing;
    impl SystemAudioCapture for Missing {
        fn set_excluded_pids(&self, _: &[u32]) -> Result<()> {
            Ok(())
        }
        fn start(&self, _: MicSink) -> Result<Box<dyn CaptureSession>> {
            Err(broscribe_platform::Error::Unsupported(
                "synthetic missing output".into(),
            ))
        }
    }
    let root = tempfile::tempdir().unwrap();
    let preview = Arc::new(Preview::default());
    let recorder = Recorder::new(
        Arc::new(Source::default()),
        Arc::new(Missing),
        Arc::new(FakeSystemInfo::default()),
        Arc::new(FakeProcessPriority::default()),
    );
    assert!(
        recorder
            .start(
                config(root.path()),
                CaptureOutputs {
                    live: preview.clone(),
                    events: preview.clone()
                }
            )
            .is_err()
    );
    assert!(!root.path().join("synthetic-session").exists());
    // A start that fails is not a stop: no `start_failed` stop notice.
    let notices = preview.notices.lock().unwrap();
    assert!(
        notices.is_empty(),
        "a failed start announces nothing: {notices:?}"
    );
}

/// A system-audio source whose start blocks until released, like a process
/// tap slow to open; released, it opens `source`, or fails with `refuse`.
#[derive(Default)]
struct HeldSystem {
    source: Source,
    refuse: bool,
    gate: Mutex<Gate>,
    changed: Condvar,
    opened: AtomicUsize,
}
#[derive(Default)]
struct Gate {
    entered: bool,
    released: bool,
}
/// Releases the held start when dropped, also when an assertion fails, so
/// no start stays blocked.
struct Release(Arc<HeldSystem>);
impl Drop for Release {
    fn drop(&mut self) {
        self.0
            .gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .released = true;
        self.0.changed.notify_all();
    }
}
impl HeldSystem {
    fn entered(&self) -> bool {
        self.gate.lock().unwrap().entered
    }
    fn opened(&self) -> usize {
        self.opened.load(Ordering::SeqCst)
    }
}
impl SystemAudioCapture for HeldSystem {
    fn set_excluded_pids(&self, _: &[u32]) -> Result<()> {
        Ok(())
    }
    fn start(&self, sink: MicSink) -> Result<Box<dyn CaptureSession>> {
        let mut gate = self.gate.lock().unwrap();
        gate.entered = true;
        drop(
            self.changed
                .wait_while(gate, |gate| !gate.released)
                .unwrap(),
        );
        if self.refuse {
            return Err(broscribe_platform::Error::Unsupported(
                "synthetic tap refused".into(),
            ));
        }
        let capture = self.source.open(sink);
        self.opened.fetch_add(1, Ordering::SeqCst);
        Ok(capture)
    }
}

/// Starts a session on its own thread, as the daemon waits for a start.
fn start_on_thread(
    root: &std::path::Path,
    mic: &Arc<Source>,
    system: &Arc<HeldSystem>,
    info: Arc<dyn broscribe_platform::SystemInfo>,
    preview: &Arc<Preview>,
) -> std::thread::JoinHandle<Result<Box<dyn SessionHandle>>> {
    let recorder = Recorder::new(
        mic.clone(),
        system.clone(),
        info,
        Arc::new(FakeProcessPriority::default()),
    );
    let outputs = CaptureOutputs {
        live: preview.clone(),
        events: preview.clone(),
    };
    let cfg = config(root);
    std::thread::spawn(move || recorder.start(cfg, outputs))
}

/// The live preview of `track`, every frame in order.
fn live_samples(preview: &Preview, track: broscribe_ipc::Track) -> Vec<f32> {
    preview
        .frames
        .lock()
        .unwrap()
        .iter()
        .filter(|frame| frame.track == track)
        .flat_map(|frame| frame.samples.iter().copied())
        .collect()
}

/// The first mic chunk, five seconds at 16 kHz, is closed in the manifest
/// on disk.
fn first_mic_chunk_closed(dir: &std::path::Path) -> bool {
    journal::load_manifest(dir).is_ok_and(|manifest| {
        manifest
            .mic
            .chunks
            .first()
            .is_some_and(|chunk| chunk.frames == Some(80_000))
    })
}

/// A system-audio start that takes long never holds up the microphone: what
/// it captures meanwhile is journaled and reaches the live preview before
/// the start returns, and the recording keeps every sample.
#[test]
fn a_slow_system_start_never_stalls_microphone_journaling() {
    let root = tempfile::tempdir().unwrap();
    let mic = Arc::new(Source::default());
    let system = Arc::new(HeldSystem::default());
    let preview = Arc::new(Preview::default());
    let release = Release(system.clone());
    let start = start_on_thread(
        root.path(),
        &mic,
        &system,
        Arc::new(FakeSystemInfo::default()),
        &preview,
    );
    wait_until("the system start", || system.entered());
    // Six seconds, more than one chunk, in 60 callbacks of 100 ms: the
    // bounded audio queue holds them all whenever the writer drains it.
    let pushed: Vec<f32> = (0..96_000).map(|i| (i % 4_000) as f32 / 4_000.0).collect();
    let first = Instant::now();
    for (n, block) in (0u64..).zip(pushed.chunks(1_600)) {
        mic.push(first + Duration::from_millis(100 * n), block.to_vec());
    }
    let dir = root.path().join("synthetic-session");
    wait_until("the first mic chunk, durable and live", || {
        first_mic_chunk_closed(&dir)
            && live_samples(&preview, broscribe_ipc::Track::Mic).len() >= 80_000
    });
    assert!(!start.is_finished(), "the start waits for the system track");
    assert_eq!(
        live_samples(&preview, broscribe_ipc::Track::Mic),
        pushed[..80_000],
        "the closed chunk reached the preview while the system track opened"
    );
    assert!(
        preview.notices.lock().unwrap().is_empty(),
        "nothing is announced before the start returns"
    );
    drop(release);
    let session = start.join().unwrap().unwrap();
    system.source.push(Instant::now(), vec![0.5; 160]);
    let stopped = session.stop().unwrap();
    let manifest = journal::load_manifest(&stopped.journal_dir).unwrap();
    assert_eq!(manifest.stop_reason.as_deref(), Some("user"));
    let mic_audio: Vec<f32> = manifest
        .mic
        .chunks
        .iter()
        .flat_map(|chunk| journal::read_chunk(&stopped.journal_dir, Track::Mic, chunk).unwrap())
        .collect();
    assert_eq!(mic_audio, pushed, "every microphone sample, in order");
    assert_eq!(manifest.mic.dropped_frames, 0);
    assert_eq!(manifest.system.dropped_frames, 0);
    assert_eq!(manifest.mic.segments.len(), 1);
    assert_eq!(manifest.mic.segments[0].generation, 0);
    assert_eq!(
        manifest
            .system
            .chunks
            .iter()
            .filter_map(|chunk| chunk.frames)
            .sum::<u64>(),
        160,
        "the system track records once open"
    );
    assert_eq!(live_samples(&preview, broscribe_ipc::Track::Mic), pushed);
    let notices = preview.notices.lock().unwrap();
    assert!(
        notices.iter().all(|n| !matches!(
            n,
            CaptureNotice::Stopped { .. }
                | CaptureNotice::Warning(
                    CaptureWarning::TrackDown { .. } | CaptureWarning::TrackResumed { .. }
                )
        )),
        "a first start resumes nothing: {notices:?}"
    );
}

/// A preview-only overflow while the peer still opens belongs to the
/// admitted session, never to a pending or rejected start.
#[test]
fn startup_preview_gaps_wait_for_admission_and_rejected_starts_announce_nothing() {
    struct HeldPreview {
        entered: AtomicBool,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl broscribe_platform::priority::ProcessPriority for HeldPreview {
        fn set_current_thread(
            &self,
            class: broscribe_platform::priority::PriorityClass,
        ) -> Result<()> {
            if class == broscribe_platform::priority::PriorityClass::Utility {
                self.entered.store(true, Ordering::Release);
                let _ = self.release.lock().unwrap().recv();
            }
            Ok(())
        }
        fn set_current_process(
            &self,
            _: broscribe_platform::priority::PriorityClass,
        ) -> Result<()> {
            Ok(())
        }
    }
    for refuse in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mic = Arc::new(Source::default());
        let system = Arc::new(HeldSystem {
            refuse,
            ..Default::default()
        });
        let preview = Arc::new(Preview::default());
        let system_release = Release(system.clone());
        // Dropping this sender also releases the worker on assertion failure.
        let (preview_release, released) = std::sync::mpsc::channel();
        let priority = Arc::new(HeldPreview {
            entered: AtomicBool::new(false),
            release: Mutex::new(released),
        });
        let recorder = Recorder::new(
            mic.clone(),
            system.clone(),
            Arc::new(FakeSystemInfo::default()),
            priority.clone(),
        );
        let outputs = CaptureOutputs {
            live: preview.clone(),
            events: preview.clone(),
        };
        let cfg = config(root.path());
        let start = std::thread::spawn(move || recorder.start(cfg, outputs));
        wait_until("the system and preview workers held", || {
            system.entered() && priority.entered.load(Ordering::Acquire)
        });
        let first = Instant::now();
        // Sixteen seconds in 160 callbacks fits the recording queue, but
        // three closed chunks overflow the unchanged ten-second preview.
        for n in 0..160 {
            mic.push(first + Duration::from_millis(100 * n), vec![0.25; 1_600]);
        }
        let dir = root.path().join("synthetic-session");
        wait_until("three durable mic chunks", || {
            journal::load_manifest(&dir)
                .is_ok_and(|m| m.mic.chunks.iter().filter(|c| c.frames.is_some()).count() >= 3)
        });
        drop(preview_release);
        wait_until("the retained preview audio delivered", || {
            live_samples(&preview, broscribe_ipc::Track::Mic).len() >= 160_000
        });
        assert!(!start.is_finished());
        assert!(
            preview.notices.lock().unwrap().is_empty(),
            "a pending start publishes no event even when preview audio was dropped"
        );
        drop(system_release);
        if refuse {
            assert!(start.join().unwrap().is_err());
            assert!(!dir.exists());
            assert!(preview.notices.lock().unwrap().is_empty());
        } else {
            let session = start.join().unwrap().unwrap();
            wait_until("the admitted preview gap", || {
                preview.notices.lock().unwrap().iter().any(|n| {
                    matches!(
                        n,
                        CaptureNotice::PreviewGap {
                            track: broscribe_ipc::Track::Mic,
                            ..
                        }
                    )
                })
            });
            let stopped = session.stop().unwrap();
            let manifest = journal::load_manifest(&stopped.journal_dir).unwrap();
            assert_eq!(manifest.mic.dropped_frames, 0);
            assert_eq!(
                manifest
                    .mic
                    .chunks
                    .iter()
                    .filter_map(|c| c.frames)
                    .sum::<u64>(),
                256_000,
                "preview overflow never drops recorded audio"
            );
            assert_eq!(
                preview
                    .notices
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|n| matches!(
                        n,
                        CaptureNotice::PreviewGap {
                            track: broscribe_ipc::Track::Mic,
                            ..
                        }
                    ))
                    .count(),
                1,
                "the deferred gap is announced exactly once after admission"
            );
        }
    }
}

/// The system start refused after a long open fails the start: the
/// microphone that opened is stopped, nothing journaled meanwhile is kept,
/// and nothing is announced.
#[test]
fn a_system_start_refused_after_a_long_open_keeps_nothing() {
    let root = tempfile::tempdir().unwrap();
    let mic = Arc::new(Source::default());
    let system = Arc::new(HeldSystem {
        refuse: true,
        ..Default::default()
    });
    let preview = Arc::new(Preview::default());
    let release = Release(system.clone());
    let start = start_on_thread(
        root.path(),
        &mic,
        &system,
        Arc::new(FakeSystemInfo::default()),
        &preview,
    );
    wait_until("the system start", || system.entered());
    let first = Instant::now();
    for n in 0..60 {
        mic.push(first + Duration::from_millis(100 * n), vec![0.25; 1_600]);
    }
    let dir = root.path().join("synthetic-session");
    wait_until("the first mic chunk durable", || {
        first_mic_chunk_closed(&dir)
    });
    drop(release);
    assert!(start.join().unwrap().is_err());
    assert!(!dir.exists(), "the failed start's journal is gone");
    assert!(
        mic.0.lock().unwrap().is_none(),
        "the microphone that opened is stopped"
    );
    assert_eq!(system.opened(), 0);
    let notices = preview.notices.lock().unwrap();
    assert!(
        notices.is_empty(),
        "a failed start announces nothing: {notices:?}"
    );
}

/// A start failing while the system track still opens (here, the disk
/// filling up) returns without waiting for the device; the capture that
/// opens late is stopped.
#[test]
fn a_start_failing_while_the_system_opens_does_not_wait_for_it() {
    /// Room until the peer has actually entered its held open.
    struct Filled(Arc<HeldSystem>);
    impl broscribe_platform::SystemInfo for Filled {
        fn chip(&self) -> String {
            String::new()
        }
        fn memory_bytes(&self) -> u64 {
            0
        }
        fn free_disk_bytes(&self, _: &std::path::Path) -> Result<u64> {
            Ok(if self.0.entered() { 1 } else { 100 << 30 })
        }
        fn cpu_cores(&self) -> usize {
            1
        }
    }
    let root = tempfile::tempdir().unwrap();
    let mic = Arc::new(Source::default());
    let system = Arc::new(HeldSystem::default());
    let preview = Arc::new(Preview::default());
    let release = Release(system.clone());
    let start = start_on_thread(
        root.path(),
        &mic,
        &system,
        Arc::new(Filled(system.clone())),
        &preview,
    );
    wait_until("the system start", || system.entered());
    wait_until("the failed start", || start.is_finished());
    assert!(start.join().unwrap().is_err());
    assert_eq!(system.opened(), 0, "the system start is still held");
    assert!(!root.path().join("synthetic-session").exists());
    assert!(
        mic.0.lock().unwrap().is_none(),
        "the microphone that opened is stopped"
    );
    drop(release);
    wait_until("the late system capture stopped", || {
        system.opened() == 1 && system.source.0.lock().unwrap().is_none()
    });
    let notices = preview.notices.lock().unwrap();
    assert!(
        notices.is_empty(),
        "a failed start announces nothing: {notices:?}"
    );
}

#[test]
fn meeting_app_seen_reaches_the_session_manifest_while_recording() {
    let root = tempfile::tempdir().unwrap();
    let recorder = Recorder::new(
        Arc::new(Source::default()),
        Arc::new(Source::default()),
        Arc::new(FakeSystemInfo::default()),
        Arc::new(FakeProcessPriority::default()),
    );
    let preview = Arc::new(Preview::default());
    let mut session = recorder
        .start(
            config(root.path()),
            CaptureOutputs {
                live: preview.clone(),
                events: preview,
            },
        )
        .unwrap();
    let dir = root.path().join("synthetic-session");
    assert!(dir.join(journal::MANIFEST_FILE).is_file());
    assert!(!journal::load_manifest(&dir).unwrap().meta.meeting_app_seen);
    session.mark_meeting_app_seen().unwrap();
    // On disk at once, so a crash before stop keeps the evidence.
    assert!(journal::load_manifest(&dir).unwrap().meta.meeting_app_seen);
    session.mark_meeting_app_seen().unwrap();
    let stopped = session.stop().unwrap();
    let manifest = journal::load_manifest(&stopped.journal_dir).unwrap();
    assert!(manifest.meta.meeting_app_seen);
}
