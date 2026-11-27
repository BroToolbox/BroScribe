//! Fakes for every platform trait, used in tests across the workspace.

use std::collections::{HashMap, HashSet};
use std::f32::consts::TAU;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::player::{
    PREPARE_TIMEOUT, PlaybackPreparation, PlaybackRequests, PreparedPlayback, ProbedPlayback,
    check_range, needs_playable_cache,
};
use crate::system::check_web_url;
use crate::{
    AnnouncementPriority, Announcer, AppPaths, AppUiReader, AudioDevice, AudioPlayer, Autostart,
    AxChange, AxChangeCallback, AxLimits, AxNode, AxObservation, AxTree, BrowserLauncher,
    CaptureSession, ChangeCallback, Error, LibraryWatcher, MediaDecoder, MediaInfo, MenuItemSpec,
    MicCapture, MicChunk, MicSink, MicUsageMonitor, MicUser, Notifier, PcmSink, PlayerState,
    ProcessProbe, Result, SecretStore, StatusClickSender, StatusItem, SystemAudioCapture,
    SystemInfo, WatchHandle, WindowInfo, WindowInspector,
};

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Decodes every path to the same samples. `failing` decoders emit `chunks`
/// chunks first and then fail, to exercise the chain's fallback rules.
pub struct FakeDecoder {
    name: String,
    sample_rate: u32,
    samples: Vec<f32>,
    fail_after_chunks: Option<usize>,
}

impl FakeDecoder {
    /// Samples per `samples` call.
    pub const CHUNK: usize = 4096;

    pub fn new(name: &str, sample_rate: u32, samples: Vec<f32>) -> Self {
        Self {
            name: name.into(),
            sample_rate,
            samples,
            fail_after_chunks: None,
        }
    }

    /// Emits `chunks` chunks of silence at 16 kHz (with `format` first when
    /// `chunks > 0`), then fails with `Error::Decode`.
    pub fn failing(name: &str, chunks: usize) -> Self {
        Self {
            name: name.into(),
            sample_rate: 16_000,
            samples: vec![0.0; chunks * Self::CHUNK],
            fail_after_chunks: Some(chunks),
        }
    }

    fn info(&self) -> MediaInfo {
        MediaInfo {
            sample_rate: self.sample_rate,
            channels: 1,
            duration_s: Some(self.samples.len() as f64 / f64::from(self.sample_rate.max(1))),
            created_at: None,
            audio_tracks: 1,
            container: "fake".into(),
            tracks: vec![crate::TrackInfo {
                index: 0,
                channels: 1,
                sample_rate: self.sample_rate,
                ..Default::default()
            }],
        }
    }
}

impl MediaDecoder for FakeDecoder {
    fn name(&self) -> &str {
        &self.name
    }

    fn probe(&self, path: &Path) -> Result<MediaInfo> {
        match self.fail_after_chunks {
            Some(_) => Err(Error::Decode(format!(
                "{}: cannot probe {}",
                self.name,
                path.display()
            ))),
            None => Ok(self.info()),
        }
    }

    fn decode(&self, path: &Path, sink: &mut dyn PcmSink) -> Result<MediaInfo> {
        let fail = |n: usize| -> Result<MediaInfo> {
            Err(Error::Decode(format!(
                "{}: failed after {n} chunks of {}",
                self.name,
                path.display()
            )))
        };
        if self.fail_after_chunks == Some(0) {
            return fail(0);
        }
        sink.format(self.sample_rate)?;
        for chunk in self.samples.chunks(Self::CHUNK) {
            sink.samples(chunk)?;
        }
        match self.fail_after_chunks {
            Some(n) => fail(n),
            None => Ok(self.info()),
        }
    }
}

/// Collects decoded audio.
#[derive(Debug, Default)]
pub struct CollectSink {
    pub sample_rate: Option<u32>,
    pub format_calls: u32,
    pub samples: Vec<f32>,
}

impl PcmSink for CollectSink {
    fn format(&mut self, sample_rate: u32) -> Result<()> {
        self.format_calls += 1;
        self.sample_rate = Some(sample_rate);
        Ok(())
    }

    fn samples(&mut self, mono: &[f32]) -> Result<()> {
        if self.sample_rate.is_none() {
            return Err(Error::Internal("samples before format".into()));
        }
        self.samples.extend_from_slice(mono);
        Ok(())
    }
}

/// Folders under one root: `data/`, `cache/`, `library/`.
pub struct FakePaths {
    pub root: PathBuf,
}

impl FakePaths {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl AppPaths for FakePaths {
    fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }
    fn cache_dir(&self) -> PathBuf {
        self.root.join("cache")
    }
    fn documents_dir(&self) -> PathBuf {
        self.root.join("Documents")
    }
    fn default_library_dir(&self) -> PathBuf {
        self.root.join("library")
    }
}

/// Reports the current PID and a configurable set of other live PIDs.
pub struct FakeProcessProbe {
    pub current: u32,
    pub alive: Mutex<HashSet<u32>>,
}

impl FakeProcessProbe {
    pub fn new(current: u32) -> Self {
        Self {
            current,
            alive: Mutex::new(HashSet::from([current])),
        }
    }

    pub fn set_alive(&self, pid: u32, alive: bool) {
        let mut set = lock(&self.alive);
        if alive {
            set.insert(pid);
        } else {
            set.remove(&pid);
        }
    }
}

impl ProcessProbe for FakeProcessProbe {
    fn is_alive(&self, pid: u32) -> bool {
        lock(&self.alive).contains(&pid)
    }
    fn current_pid(&self) -> u32 {
        self.current
    }
}

/// System-audio capture that delivers `samples` as one chunk on `start`;
/// the session then holds the sink until it is stopped.
pub struct FakeCapture {
    pub sample_rate: u32,
    pub samples: Vec<f32>,
    /// The latest `set_excluded_pids` list.
    pub excluded: Mutex<Vec<u32>>,
    /// Sessions stopped (explicitly or by drop).
    pub stopped: Arc<AtomicUsize>,
}

impl FakeCapture {
    pub fn new(sample_rate: u32, samples: Vec<f32>) -> Self {
        Self {
            sample_rate,
            samples,
            excluded: Mutex::new(Vec::new()),
            stopped: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl SystemAudioCapture for FakeCapture {
    fn set_excluded_pids(&self, pids: &[u32]) -> Result<()> {
        *lock(&self.excluded) = pids.to_vec();
        Ok(())
    }

    fn start(&self, mut sink: MicSink) -> Result<Box<dyn CaptureSession>> {
        sink(MicChunk::new(self.samples.clone(), self.sample_rate));
        Ok(Box::new(FakeCaptureSession {
            sample_rate: self.sample_rate,
            failure: None,
            sink: Some(sink),
            stopped: Some(self.stopped.clone()),
        }))
    }
}

/// A microphone that plays a sine wave. `start` delivers `chunks` chunks of
/// `chunk_len` samples to the sink before returning; the sink then stays alive
/// until the session stops, unless `failure` is set, in which case it is
/// dropped at once and the session reports that failure (a lost device).
pub struct FakeMicCapture {
    pub devices: Vec<AudioDevice>,
    pub sample_rate: u32,
    pub frequency_hz: f32,
    pub amplitude: f32,
    pub chunk_len: usize,
    pub chunks: usize,
    pub failure: Option<String>,
    /// The `device` argument of every `start`.
    pub started: Mutex<Vec<Option<String>>>,
    /// Sessions stopped (explicitly or by drop).
    pub stopped: Arc<AtomicUsize>,
}

impl FakeMicCapture {
    /// One default device `fake-mic` at `sample_rate`; 440 Hz at amplitude
    /// 0.5, ten 100 ms chunks.
    pub fn new(sample_rate: u32) -> Self {
        Self {
            devices: vec![AudioDevice {
                id: "fake-mic".into(),
                name: "Fake microphone".into(),
                is_default: true,
                sample_rates: vec![sample_rate],
            }],
            sample_rate,
            frequency_hz: 440.0,
            amplitude: 0.5,
            chunk_len: (sample_rate / 10).max(1) as usize,
            chunks: 10,
            failure: None,
            started: Mutex::new(Vec::new()),
            stopped: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl MicCapture for FakeMicCapture {
    fn devices(&self) -> Result<Vec<AudioDevice>> {
        Ok(self.devices.clone())
    }

    fn start(&self, device: Option<&str>, mut sink: MicSink) -> Result<Box<dyn CaptureSession>> {
        if let Some(id) = device
            && !self.devices.iter().any(|d| d.id == id)
        {
            return Err(Error::NotFound(format!("audio input device {id}")));
        }
        if device.is_none() && self.devices.is_empty() {
            return Err(Error::NotFound("no audio input device".into()));
        }
        lock(&self.started).push(device.map(str::to_owned));
        let step = TAU * self.frequency_hz / self.sample_rate as f32;
        let mut n = 0usize;
        for _ in 0..self.chunks {
            let samples = (0..self.chunk_len)
                .map(|i| self.amplitude * (step * (n + i) as f32).sin())
                .collect();
            n += self.chunk_len;
            sink(MicChunk::new(samples, self.sample_rate));
        }
        let sink = if self.failure.is_some() {
            drop(sink);
            None
        } else {
            Some(sink)
        };
        Ok(Box::new(FakeCaptureSession {
            sample_rate: self.sample_rate,
            failure: self.failure.clone(),
            sink,
            stopped: Some(self.stopped.clone()),
        }))
    }
}

struct FakeCaptureSession {
    sample_rate: u32,
    failure: Option<String>,
    sink: Option<MicSink>,
    stopped: Option<Arc<AtomicUsize>>,
}

impl FakeCaptureSession {
    fn finish(&mut self) {
        self.sink = None;
        if let Some(stopped) = self.stopped.take() {
            stopped.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl CaptureSession for FakeCaptureSession {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn failure(&self) -> Option<String> {
        self.failure.clone()
    }
    fn stop(mut self: Box<Self>) -> Result<()> {
        self.finish();
        Ok(())
    }
}

impl Drop for FakeCaptureSession {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Plays nothing; tracks position on a manual clock ([`advance`](Self::advance)).
/// Preparations go through the real request bookkeeping (cancellation,
/// supersession, ownership) with a probe that touches no file.
pub struct FakeAudioPlayer {
    /// Lower-case extensions treated as natively playable.
    pub native_extensions: Vec<String>,
    /// Length of every file.
    pub duration_s: f64,
    /// Every playback started: `(path, start_s, end_s)`.
    pub played: Vec<(PathBuf, f64, Option<f64>)>,
    /// Real time each preparation's probe takes (cancellation ends it early).
    pub probe_delay: Duration,
    /// Seconds of [`advance`](Self::advance) a started playback spends
    /// `Loading` before its position moves.
    pub loading_s: f64,
    /// Files the fake opens but cannot play whole: their preparation fails
    /// with [`PLAYBACK_NEEDS_CACHE`](crate::player::PLAYBACK_NEEDS_CACHE).
    pub needs_cache: Vec<PathBuf>,
    requests: PlaybackRequests,
    loaded: bool,
    playing: bool,
    loading_left_s: f64,
    position_s: f64,
    rate: f32,
    end_s: Option<f64>,
}

impl FakeAudioPlayer {
    pub fn new(duration_s: f64) -> Self {
        Self {
            native_extensions: ["m4a", "mp3", "wav", "aac", "caf", "aif", "aiff", "flac"]
                .map(String::from)
                .to_vec(),
            duration_s,
            played: Vec::new(),
            probe_delay: Duration::ZERO,
            loading_s: 0.0,
            needs_cache: Vec::new(),
            requests: PlaybackRequests::new(),
            loaded: false,
            playing: false,
            loading_left_s: 0.0,
            position_s: 0.0,
            rate: 1.0,
            end_s: None,
        }
    }

    /// Moves the clock forward; the position moves only while playing, once
    /// loading is over.
    pub fn advance(&mut self, seconds: f64) {
        if self.playing {
            let loading = seconds.min(self.loading_left_s);
            self.loading_left_s -= loading;
            let rate = if self.end_s.is_some() { 1.0 } else { self.rate };
            self.position_s += (seconds - loading) * f64::from(rate);
        }
    }

    /// Whether a preparation was issued and neither played nor superseded.
    pub fn is_preparing(&self) -> bool {
        self.requests.is_pending()
    }

    fn limit(&self) -> f64 {
        self.end_s
            .map_or(self.duration_s, |end| end.min(self.duration_s))
    }
}

impl AudioPlayer for FakeAudioPlayer {
    fn can_play_natively(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
            self.native_extensions
                .iter()
                .any(|n| n.eq_ignore_ascii_case(e))
        })
    }

    fn prepare(&mut self, path: &Path) -> Result<PlaybackPreparation> {
        if !self.can_play_natively(path) {
            return Err(Error::Unsupported(format!(
                "playback of {}",
                path.display()
            )));
        }
        let delay = self.probe_delay;
        let duration_s = self.duration_s;
        let needs_cache = self.needs_cache.iter().any(|p| p == path);
        Ok(self.requests.begin(
            path,
            PREPARE_TIMEOUT,
            Box::new(move |path: &Path, cancel: &std::sync::atomic::AtomicBool| {
                let until = Instant::now() + delay;
                while Instant::now() < until && !cancel.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                if needs_cache {
                    return Err(needs_playable_cache(format!(
                        "{} cannot be played whole",
                        path.display()
                    )));
                }
                Ok(ProbedPlayback {
                    tracks: vec![0],
                    duration_s: Some(duration_s),
                })
            }),
        ))
    }

    fn play_prepared(
        &mut self,
        prepared: PreparedPlayback,
        start_s: f64,
        end_s: Option<f64>,
    ) -> Result<()> {
        check_range(start_s, end_s)?;
        self.requests.accept(&prepared)?;
        self.played
            .push((prepared.path().to_path_buf(), start_s, end_s));
        self.loaded = true;
        self.playing = true;
        self.loading_left_s = self.loading_s.max(0.0);
        self.position_s = start_s.min(self.duration_s);
        self.end_s = end_s;
        Ok(())
    }

    fn set_rate(&mut self, rate: f32) -> Result<()> {
        crate::player::check_rate(rate)?;
        self.rate = rate;
        Ok(())
    }

    fn pause(&mut self) {
        self.playing = false;
    }

    fn resume(&mut self) -> Result<()> {
        if !self.loaded {
            return Err(Error::InvalidInput("nothing to resume".into()));
        }
        self.playing = true;
        Ok(())
    }

    fn stop(&mut self) {
        self.requests.supersede();
        self.loaded = false;
        self.playing = false;
        self.loading_left_s = 0.0;
        self.end_s = None;
    }

    fn position_s(&self) -> Option<f64> {
        self.loaded.then(|| self.position_s.min(self.limit()))
    }

    fn is_playing(&self) -> bool {
        self.playing
    }

    fn poll(&mut self) -> PlayerState {
        if !self.loaded {
            return PlayerState::Stopped;
        }
        let limit = self.limit();
        if self.position_s >= limit {
            self.stop();
            return PlayerState::Finished { position_s: limit };
        }
        let position_s = self.position_s;
        if !self.playing {
            PlayerState::Paused { position_s }
        } else if self.loading_left_s > 0.0 {
            PlayerState::Loading { position_s }
        } else {
            PlayerState::Playing { position_s }
        }
    }
}

type FakeWatches = Mutex<Vec<(u64, PathBuf, ChangeCallback)>>;

/// Reports changes only when [`trigger`](Self::trigger)ed.
#[derive(Default)]
pub struct FakeLibraryWatcher {
    watches: Arc<FakeWatches>,
    next_id: AtomicU64,
}

impl FakeLibraryWatcher {
    /// Calls every active watch on `root` with `paths` (on this thread, while
    /// holding the watch list); returns how many were called.
    pub fn trigger(&self, root: &Path, paths: Vec<PathBuf>) -> usize {
        let watches = lock(&self.watches);
        let mut called = 0;
        for (_, watched, on_change) in watches.iter() {
            if watched == root {
                on_change(paths.clone());
                called += 1;
            }
        }
        called
    }

    /// Active watches.
    pub fn active(&self) -> usize {
        lock(&self.watches).len()
    }
}

impl LibraryWatcher for FakeLibraryWatcher {
    fn watch(&self, root: &Path, on_change: ChangeCallback) -> Result<Box<dyn WatchHandle>> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        lock(&self.watches).push((id, root.to_path_buf(), on_change));
        Ok(Box::new(FakeWatchHandle {
            id,
            watches: self.watches.clone(),
        }))
    }
}

struct FakeWatchHandle {
    id: u64,
    watches: Arc<FakeWatches>,
}

impl WatchHandle for FakeWatchHandle {
    fn stop(self: Box<Self>) {}
}

impl Drop for FakeWatchHandle {
    fn drop(&mut self) {
        lock(&self.watches).retain(|(id, _, _)| *id != self.id);
    }
}

/// Fixed machine facts (defaults: a 10-core chip with 16 GiB and 100 GiB free).
#[derive(Clone, Debug)]
pub struct FakeSystemInfo {
    pub chip: String,
    pub memory_bytes: u64,
    pub free_disk_bytes: u64,
    pub cpu_cores: usize,
    /// Known application aliases. `None` models unavailable discovery.
    pub installed_applications: Option<HashSet<String>>,
}

impl Default for FakeSystemInfo {
    fn default() -> Self {
        Self {
            chip: "Fake Chip".into(),
            memory_bytes: 16 << 30,
            free_disk_bytes: 100 << 30,
            cpu_cores: 10,
            installed_applications: None,
        }
    }
}

impl SystemInfo for FakeSystemInfo {
    fn application_installed(&self, identities: &[String]) -> Option<bool> {
        self.installed_applications.as_ref().map(|installed| {
            identities
                .iter()
                .any(|identity| installed.contains(identity))
        })
    }

    fn chip(&self) -> String {
        self.chip.clone()
    }
    fn memory_bytes(&self) -> u64 {
        self.memory_bytes
    }
    fn free_disk_bytes(&self, _path: &Path) -> Result<u64> {
        Ok(self.free_disk_bytes)
    }
    fn cpu_cores(&self) -> usize {
        self.cpu_cores
    }
}

/// Reports whatever the test last [`set`](Self::set).
#[derive(Default)]
pub struct FakeMicUsageMonitor {
    users: Mutex<Vec<MicUser>>,
}

impl FakeMicUsageMonitor {
    pub fn new(users: Vec<MicUser>) -> Self {
        Self {
            users: Mutex::new(users),
        }
    }

    pub fn set(&self, users: Vec<MicUser>) {
        *lock(&self.users) = users;
    }
}

impl MicUsageMonitor for FakeMicUsageMonitor {
    fn mic_users(&self) -> Result<Vec<MicUser>> {
        Ok(lock(&self.users).clone())
    }
}

#[derive(Default)]
pub struct FakeWindowInspector {
    pub windows: Vec<WindowInfo>,
}

impl WindowInspector for FakeWindowInspector {
    fn windows(&self) -> Result<Vec<WindowInfo>> {
        Ok(self.windows.clone())
    }
}

type FakeObserver = (u64, u32, Vec<AxChange>, AxChangeCallback);

/// Serves hand-written trees per pid and delivers [`FakeAppUiReader::emit`]
/// notifications to registered observers on the calling thread.
#[derive(Default)]
pub struct FakeAppUiReader {
    pub trees: Mutex<HashMap<u32, AxTree>>,
    /// The Accessibility permission is withheld: every read fails.
    pub denied: bool,
    /// Apps supporting `AXManualAccessibility`, with its current value.
    pub manual: Mutex<HashMap<u32, bool>>,
    /// Every `set_manual_accessibility` call, in order.
    pub manual_calls: Mutex<Vec<(u32, bool)>>,
    pub trust_requests: AtomicUsize,
    observers: Arc<Mutex<Vec<FakeObserver>>>,
    next_observer: AtomicU64,
}

impl FakeAppUiReader {
    pub fn set_tree(&self, pid: u32, tree: AxTree) {
        lock(&self.trees).insert(pid, tree);
    }

    /// Delivers `change` from `pid` to its observers; returns how many ran.
    pub fn emit(&self, pid: u32, change: AxChange) -> usize {
        let observers = lock(&self.observers);
        let mut n = 0;
        for (_, p, changes, callback) in observers.iter() {
            if *p == pid && changes.contains(&change) {
                callback(change);
                n += 1;
            }
        }
        n
    }

    pub fn observer_count(&self) -> usize {
        lock(&self.observers).len()
    }

    fn check_trusted(&self) -> Result<()> {
        if self.denied {
            Err(Error::Unsupported(
                "accessibility permission not granted".into(),
            ))
        } else {
            Ok(())
        }
    }
}

struct FakeAxObservation {
    id: u64,
    observers: Arc<Mutex<Vec<FakeObserver>>>,
}

impl AxObservation for FakeAxObservation {}

impl Drop for FakeAxObservation {
    fn drop(&mut self) {
        lock(&self.observers).retain(|(id, ..)| *id != self.id);
    }
}

impl AppUiReader for FakeAppUiReader {
    fn is_trusted(&self) -> bool {
        !self.denied
    }

    fn request_trust(&self) -> bool {
        self.trust_requests.fetch_add(1, Ordering::SeqCst);
        !self.denied
    }

    fn snapshot(&self, pid: u32, limits: AxLimits) -> Result<AxTree> {
        self.check_trusted()?;
        let tree = lock(&self.trees)
            .get(&pid)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("no accessible app with pid {pid}")))?;
        let mut budget = limits.max_nodes;
        let mut incomplete = tree.incomplete;
        let root = cap_ax_node(
            &tree.root,
            0,
            limits.max_depth,
            &mut budget,
            &mut incomplete,
        );
        Ok(AxTree { root, incomplete })
    }

    fn observe(
        &self,
        pid: u32,
        changes: &[AxChange],
        callback: AxChangeCallback,
    ) -> Result<Box<dyn AxObservation>> {
        self.check_trusted()?;
        let id = self.next_observer.fetch_add(1, Ordering::SeqCst);
        lock(&self.observers).push((id, pid, changes.to_vec(), callback));
        Ok(Box::new(FakeAxObservation {
            id,
            observers: Arc::clone(&self.observers),
        }))
    }

    fn manual_accessibility(&self, pid: u32) -> Result<Option<bool>> {
        self.check_trusted()?;
        Ok(lock(&self.manual).get(&pid).copied())
    }

    fn set_manual_accessibility(&self, pid: u32, enabled: bool) -> Result<()> {
        self.check_trusted()?;
        let mut manual = lock(&self.manual);
        let Some(value) = manual.get_mut(&pid) else {
            return Err(Error::Unsupported(format!(
                "pid {pid} has no AXManualAccessibility"
            )));
        };
        *value = enabled;
        lock(&self.manual_calls).push((pid, enabled));
        Ok(())
    }
}

/// Copies `node` within the depth and node budget, like a real snapshot,
/// recording which limit cut the tree short.
fn cap_ax_node(
    node: &AxNode,
    depth: usize,
    max_depth: usize,
    budget: &mut usize,
    incomplete: &mut crate::AxIncomplete,
) -> AxNode {
    *budget = budget.saturating_sub(1);
    let mut out = AxNode {
        children: Vec::new(),
        ..node.clone()
    };
    for child in &node.children {
        if depth + 1 > max_depth {
            incomplete.depth_cap = true;
            break;
        }
        if *budget == 0 {
            incomplete.node_cap = true;
            break;
        }
        out.children
            .push(cap_ax_node(child, depth + 1, max_depth, budget, incomplete));
    }
    out
}

#[derive(Default)]
pub struct FakeSecretStore {
    pub secrets: Mutex<HashMap<String, String>>,
}

impl SecretStore for FakeSecretStore {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(lock(&self.secrets).get(key).cloned())
    }
    fn set(&self, key: &str, value: &str) -> Result<()> {
        lock(&self.secrets).insert(key.into(), value.into());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<()> {
        lock(&self.secrets).remove(key);
        Ok(())
    }
}

/// Process trees without OS containment: the leader alone is killed and
/// reaped through [`std::process::Child`]; calls are counted for tests.
#[derive(Default)]
pub struct FakeProcessTreeControl {
    pub prepared: AtomicUsize,
    pub attached: AtomicUsize,
    /// Makes the next `attach` fail (the caller must then reap the child).
    pub fail_attach: std::sync::atomic::AtomicBool,
    kills: Arc<AtomicUsize>,
}

impl FakeProcessTreeControl {
    /// Trees killed so far (each at most once).
    pub fn kills(&self) -> usize {
        self.kills.load(Ordering::SeqCst)
    }
}

impl crate::ProcessTreeControl for FakeProcessTreeControl {
    fn prepare(&self, _command: &mut std::process::Command) -> Result<()> {
        self.prepared.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn attach(&self, _child: &std::process::Child) -> Result<Box<dyn crate::ProcessTreeHandle>> {
        if self.fail_attach.swap(false, Ordering::SeqCst) {
            return Err(Error::Internal(
                "synthetic process-tree attach failure".into(),
            ));
        }
        self.attached.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeProcessTree {
            kills: Arc::clone(&self.kills),
            killed: false,
        }))
    }
}

struct FakeProcessTree {
    kills: Arc<AtomicUsize>,
    killed: bool,
}

impl crate::ProcessTreeHandle for FakeProcessTree {
    fn leader_exited(&mut self, child: &mut std::process::Child) -> Result<bool> {
        child
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|e| Error::io("checking a fake process tree", e))
    }
    fn kill_tree(&mut self, child: &mut std::process::Child) -> Result<()> {
        if !self.killed {
            self.killed = true;
            self.kills.fetch_add(1, Ordering::SeqCst);
        }
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
        Ok(())
    }
}

/// Fixed TLS policy; `None` behaves like an unreadable OS store.
#[derive(Default)]
pub struct FakeTrustStore {
    pub policy: Mutex<Option<crate::TlsTrust>>,
    pub reads: AtomicUsize,
}

impl crate::TrustStore for FakeTrustStore {
    fn tls_trust(&self) -> Result<crate::TlsTrust> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        match lock(&self.policy).clone() {
            Some(policy) if policy.root_count() != 0 => Ok(policy),
            _ => Err(Error::Unsupported(
                "synthetic trust store has no root certificates".into(),
            )),
        }
    }
}

/// Records opened URLs; refuses non-web URLs like the real launchers.
#[derive(Default)]
pub struct FakeBrowserLauncher {
    pub opened: Mutex<Vec<String>>,
}

impl BrowserLauncher for FakeBrowserLauncher {
    fn open(&self, url: &str) -> Result<()> {
        check_web_url(url)?;
        lock(&self.opened).push(url.into());
        Ok(())
    }
}

pub struct FakeSystemSettings {
    pub host: crate::HostPlatform,
    pub supported: Vec<crate::SettingsPane>,
    pub opened: Mutex<Vec<crate::SettingsPane>>,
    pub notifications_permission: Option<&'static str>,
}

impl crate::SystemSettingsOpener for FakeSystemSettings {
    fn host_platform(&self) -> crate::HostPlatform {
        self.host
    }

    fn notifications_permission(&self) -> Option<&'static str> {
        self.notifications_permission
    }

    fn supports(&self, pane: crate::SettingsPane) -> bool {
        self.supported.contains(&pane)
    }

    fn open(&self, pane: crate::SettingsPane) -> Result<()> {
        if !self.supports(pane) {
            return Err(Error::Unsupported(
                "system settings pane unavailable".into(),
            ));
        }
        lock(&self.opened).push(pane);
        Ok(())
    }
}

#[derive(Default)]
pub struct FakeAutostart {
    pub enabled: Mutex<bool>,
    pub login_registration_path: Option<PathBuf>,
    pub login_job: Mutex<crate::LoginJob>,
    pub login_job_profile: Mutex<Option<bool>>,
    pub agent_status: Mutex<Option<crate::AgentServiceStatus>>,
    pub agent_install_location_advisory: std::sync::atomic::AtomicBool,
    pub starts: AtomicUsize,
    pub stops: AtomicUsize,
}

impl Autostart for FakeAutostart {
    fn agent_install_location_advisory(&self) -> bool {
        self.agent_install_location_advisory.load(Ordering::SeqCst)
    }

    fn agent_status(&self) -> Result<crate::AgentServiceStatus> {
        use crate::AgentServiceStatus::{NotInstalled, Stopped};
        Ok(lock(&self.agent_status).unwrap_or(if self.is_enabled()? {
            Stopped
        } else {
            NotInstalled
        }))
    }
    fn is_enabled(&self) -> Result<bool> {
        Ok(*lock(&self.enabled))
    }
    fn set_enabled(&self, enabled: bool) -> Result<()> {
        let registration =
            self.lock_login_registration(crate::LoginRegistrationGuard::DEFAULT_WAIT)?;
        self.set_enabled_with_registration(enabled, &registration)
    }
    fn set_enabled_with_registration(
        &self,
        enabled: bool,
        _: &crate::LoginRegistrationGuard,
    ) -> Result<()> {
        *lock(&self.enabled) = enabled;
        Ok(())
    }
    fn start(&self) -> Result<()> {
        let registration =
            self.lock_login_registration(crate::LoginRegistrationGuard::DEFAULT_WAIT)?;
        self.start_with_registration(&registration)
    }
    fn start_with_registration(&self, _: &crate::LoginRegistrationGuard) -> Result<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn stop(&self) -> Result<()> {
        let registration =
            self.lock_login_registration(crate::LoginRegistrationGuard::DEFAULT_WAIT)?;
        self.stop_login_job(&registration)
    }
    fn stop_login_job(&self, _: &crate::LoginRegistrationGuard) -> Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn lock_login_registration(&self, wait: Duration) -> Result<crate::LoginRegistrationGuard> {
        self.login_registration_path.as_deref().map_or_else(
            || Ok(crate::LoginRegistrationGuard::default()),
            |path| crate::LoginRegistrationGuard::acquire(path, wait),
        )
    }
    fn login_job(&self, _: &std::path::Path) -> crate::LoginJob {
        *lock(&self.login_job)
    }
    fn login_job_matches(&self, _: &std::path::Path) -> Option<bool> {
        *lock(&self.login_job_profile)
    }
}

#[derive(Default)]
pub struct FakeAgentInstanceProbe {
    pub held: std::sync::atomic::AtomicBool,
}

impl crate::AgentInstanceProbe for FakeAgentInstanceProbe {
    fn is_held(&self, _: &std::path::Path) -> std::io::Result<bool> {
        Ok(self.held.load(Ordering::SeqCst))
    }
}

type NotificationCallback = Arc<dyn Fn(crate::system::NotificationAction) + Send + Sync>;

/// Records notifications as `(title, body)`.
#[derive(Clone, Default)]
pub struct FakeNotifier {
    pub sent: Arc<Mutex<Vec<(String, String)>>>,
    pub authorization: Arc<Mutex<crate::system::NotificationAuthorization>>,
    pub authorization_answer: Arc<Mutex<Option<crate::system::NotificationAuthorization>>>,
    pub authorization_requests: Arc<AtomicUsize>,
    pub delivery: Arc<Mutex<Option<crate::system::NotificationDelivery>>>,
    pub actions: Arc<Mutex<Vec<crate::system::Notification>>>,
    callbacks: Arc<Mutex<std::collections::BTreeMap<String, NotificationCallback>>>,
}

impl FakeNotifier {
    pub fn act(&self, id: &str, action: &str) -> bool {
        let callback = lock(&self.callbacks).get(id).cloned();
        if let Some(callback) = callback {
            callback(crate::system::NotificationAction {
                id: id.into(),
                action: action.into(),
            });
            true
        } else {
            false
        }
    }
}

impl Notifier for FakeNotifier {
    fn authorization(&self) -> crate::system::NotificationAuthorization {
        *lock(&self.authorization)
    }

    fn request_authorization(&self) -> crate::system::NotificationAuthorization {
        self.authorization_requests.fetch_add(1, Ordering::SeqCst);
        if let Some(answer) = *lock(&self.authorization_answer) {
            *lock(&self.authorization) = answer;
        }
        self.authorization()
    }

    fn notify(&self, title: &str, body: &str) -> Result<()> {
        lock(&self.sent).push((title.into(), body.into()));
        Ok(())
    }

    fn notify_actionable(
        &self,
        notification: crate::system::Notification,
        callback: NotificationCallback,
    ) -> Result<crate::system::NotificationDelivery> {
        lock(&self.callbacks).insert(notification.id.clone(), callback);
        lock(&self.actions).push(notification);
        Ok(lock(&self.delivery).unwrap_or(crate::system::NotificationDelivery::Denied))
    }

    fn withdraw(&self, id: &str) -> Result<()> {
        lock(&self.callbacks).remove(id);
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct FakeJumpList(pub Arc<Mutex<Vec<Vec<crate::shell::DockItem>>>>);

impl crate::shell::JumpList for FakeJumpList {
    fn replace(&self, items: &[crate::shell::DockItem]) -> Result<()> {
        lock(&self.0).push(items.to_vec());
        Ok(())
    }
}

/// Records what the app shows; [`click`](Self::click) plays a menu click.
#[derive(Debug)]
pub struct FakeStatusItem {
    pub title: String,
    pub icon: crate::ui::StatusIcon,
    pub menu: Vec<MenuItemSpec>,
    pub badge_count: usize,
    pub taskbar_states: Arc<Mutex<Vec<crate::TaskbarState>>>,
    clicks: StatusClickSender,
}

impl FakeStatusItem {
    pub fn new(clicks: StatusClickSender) -> Self {
        Self {
            title: String::new(),
            icon: crate::ui::StatusIcon::Idle,
            menu: Vec::new(),
            badge_count: 0,
            taskbar_states: Arc::default(),
            clicks,
        }
    }

    /// The text the menu bar exposes for tooltips and assistive technology.
    pub fn display_title(&self) -> String {
        self.title.clone()
    }

    /// Sends `id` on the click channel if the menu has that item enabled.
    pub fn click(&self, id: &str) -> bool {
        let clickable = self
            .menu
            .iter()
            .any(|item| item.id == id && item.enabled && !item.is_separator());
        clickable && self.clicks.unbounded_send(id.to_owned()).is_ok()
    }

    /// Sends the enabled default menu item's id on the click channel, if any.
    /// Never falls back to the first enabled item.
    pub fn activate(&self) -> bool {
        let Some(item) = crate::ui::default_menu_item(&self.menu) else {
            return false;
        };
        self.clicks.unbounded_send(item.id.clone()).is_ok()
    }

    /// Secondary activation (Linux middle-click) follows the same rule as
    /// [`activate`](Self::activate).
    pub fn secondary_activate(&self) -> bool {
        self.activate()
    }
}

impl StatusItem for FakeStatusItem {
    fn set_title(&mut self, title: &str) -> Result<()> {
        self.title = title.into();
        Ok(())
    }
    fn set_icon(&mut self, icon: crate::ui::StatusIcon) -> Result<()> {
        self.icon = icon;
        Ok(())
    }
    fn set_menu(&mut self, items: Vec<MenuItemSpec>) -> Result<()> {
        self.menu = items;
        Ok(())
    }
    fn set_badge_count(&mut self, n: usize) -> Result<()> {
        self.badge_count = n;
        Ok(())
    }

    fn set_taskbar_state(
        &mut self,
        _window: &dyn crate::overlay::HasWindowHandle,
        state: crate::TaskbarState,
    ) -> Result<()> {
        lock(&self.taskbar_states).push(state);
        Ok(())
    }
}

/// What [`FakeRecordingOverlay`] was asked to do, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayCall {
    PresentNew,
    PresentExisting,
    SetCaptureExcluded(bool),
    FocusForUserRequest,
    ReturnUserFocus,
    /// A presence was dropped (its panel went away).
    Released,
}

/// Records presentations without touching any window; `capability` says
/// which branch the app takes. `fail_new` / `fail_existing` make the
/// matching presentation fail (after recording the call), as a native
/// adapter that cannot configure the panel would.
pub struct FakeRecordingOverlay {
    pub capability: crate::overlay::OverlayCapability,
    pub calls: Arc<Mutex<Vec<OverlayCall>>>,
    /// The placement each new panel was presented with, in order.
    pub placements: Arc<Mutex<Vec<crate::overlay::OverlayPlacement>>>,
    pub fail_new: std::sync::atomic::AtomicBool,
    pub fail_existing: std::sync::atomic::AtomicBool,
    pub standard_presentations: AtomicUsize,
    pub fail_standard: std::sync::atomic::AtomicBool,
    pub capture_exclusion: std::sync::atomic::AtomicBool,
    pub fail_capture_exclusion: std::sync::atomic::AtomicBool,
    pub beside_main_window: std::sync::atomic::AtomicBool,
}

impl FakeRecordingOverlay {
    pub fn new(capability: crate::overlay::OverlayCapability) -> Self {
        Self {
            capability,
            calls: Arc::default(),
            placements: Arc::default(),
            fail_new: false.into(),
            fail_existing: false.into(),
            standard_presentations: 0.into(),
            fail_standard: false.into(),
            capture_exclusion: false.into(),
            fail_capture_exclusion: false.into(),
            beside_main_window: false.into(),
        }
    }

    pub fn calls(&self) -> Vec<OverlayCall> {
        lock(&self.calls).clone()
    }

    pub fn placements(&self) -> Vec<crate::overlay::OverlayPlacement> {
        lock(&self.placements).clone()
    }
}

fn injected(flag: &std::sync::atomic::AtomicBool) -> Result<()> {
    if flag.load(Ordering::SeqCst) {
        Err(Error::Unsupported("injected overlay failure".into()))
    } else {
        Ok(())
    }
}

struct FakePresence(Arc<Mutex<Vec<OverlayCall>>>);
impl crate::overlay::OverlayPresence for FakePresence {}
impl Drop for FakePresence {
    fn drop(&mut self) {
        lock(&self.0).push(OverlayCall::Released);
    }
}

impl crate::overlay::RecordingOverlay for FakeRecordingOverlay {
    fn capability(&self) -> crate::overlay::OverlayCapability {
        self.capability
    }

    fn present_new(
        &self,
        window: &dyn raw_window_handle::HasWindowHandle,
        placement: crate::overlay::OverlayPlacement,
    ) -> Result<Box<dyn crate::overlay::OverlayPresence>> {
        if self.capability == crate::overlay::OverlayCapability::Standard {
            return crate::overlay::StandardOverlay.present_new(window, placement);
        }
        lock(&self.calls).push(OverlayCall::PresentNew);
        lock(&self.placements).push(placement);
        injected(&self.fail_new)?;
        Ok(Box::new(FakePresence(self.calls.clone())))
    }

    fn present_existing(&self, window: &dyn raw_window_handle::HasWindowHandle) -> Result<()> {
        if self.capability == crate::overlay::OverlayCapability::Standard {
            return crate::overlay::StandardOverlay.present_existing(window);
        }
        lock(&self.calls).push(OverlayCall::PresentExisting);
        injected(&self.fail_existing)
    }

    fn present_standard(&self, _window: &dyn raw_window_handle::HasWindowHandle) -> Result<()> {
        self.standard_presentations.fetch_add(1, Ordering::SeqCst);
        injected(&self.fail_standard)
    }

    fn default_frame(
        &self,
        visible: crate::overlay::OverlayRect,
        default: crate::overlay::OverlayRect,
        main: Option<crate::overlay::OverlayRect>,
        min_width: f64,
    ) -> crate::overlay::OverlayRect {
        if self.beside_main_window.load(Ordering::SeqCst) {
            crate::overlay::beside_main_window(visible, default, main, min_width)
        } else {
            default
        }
    }

    fn capture_exclusion_supported(&self) -> bool {
        self.capture_exclusion.load(Ordering::SeqCst)
    }

    fn set_capture_excluded(
        &self,
        _window: &dyn raw_window_handle::HasWindowHandle,
        excluded: bool,
    ) -> Result<crate::overlay::CaptureExclusion> {
        if !self.capture_exclusion_supported() {
            return Ok(crate::overlay::CaptureExclusion::Unsupported);
        }
        lock(&self.calls).push(OverlayCall::SetCaptureExcluded(excluded));
        injected(&self.fail_capture_exclusion)?;
        Ok(crate::overlay::CaptureExclusion::Applied)
    }

    fn focus_for_user_request(
        &self,
        _window: &dyn raw_window_handle::HasWindowHandle,
    ) -> Result<crate::overlay::OverlayFocus> {
        if self.capability == crate::overlay::OverlayCapability::NonactivatingPanel {
            lock(&self.calls).push(OverlayCall::FocusForUserRequest);
            Ok(crate::overlay::OverlayFocus::Native)
        } else {
            Ok(crate::overlay::OverlayFocus::ActivateWindow)
        }
    }

    fn return_user_focus(&self, _window: &dyn raw_window_handle::HasWindowHandle) -> Result<bool> {
        if self.capability != crate::overlay::OverlayCapability::NonactivatingPanel {
            return Ok(false);
        }
        lock(&self.calls).push(OverlayCall::ReturnUserFocus);
        Ok(true)
    }
}

std::thread_local! {
    static DIALOG_BUTTON_ORDER: std::cell::Cell<Option<crate::DialogButtonOrder>> =
        const { std::cell::Cell::new(None) };
}

/// Active thread-local [`crate::DialogButtonOrder`] override set by
/// [`FakeDialogButtonOrder`], if any.
pub fn dialog_button_order_override() -> Option<crate::DialogButtonOrder> {
    DIALOG_BUTTON_ORDER.with(std::cell::Cell::get)
}

/// Scoped fake for `NativePlatform::dialog_button_order` (SYS-151): overrides
/// the button order on the current thread until dropped, restoring the
/// previous override.
#[derive(Debug)]
pub struct FakeDialogButtonOrder {
    order: crate::DialogButtonOrder,
    previous: Option<crate::DialogButtonOrder>,
}

impl FakeDialogButtonOrder {
    pub fn new(order: crate::DialogButtonOrder) -> Self {
        let previous = DIALOG_BUTTON_ORDER.with(|cell| cell.replace(Some(order)));
        Self { order, previous }
    }

    pub fn order(&self) -> crate::DialogButtonOrder {
        self.order
    }
}

impl Drop for FakeDialogButtonOrder {
    fn drop(&mut self) {
        DIALOG_BUTTON_ORDER.with(|cell| cell.set(self.previous));
    }
}

/// Fake [`crate::DisplayAccessibility`] (ACC-32): the settings a test sets,
/// shared between clones, with a count of reads.
#[derive(Clone, Debug, Default)]
pub struct FakeDisplayAccessibility {
    state: Arc<Mutex<crate::DisplayPreferences>>,
    reads: Arc<AtomicUsize>,
}

impl FakeDisplayAccessibility {
    pub fn new(preferences: crate::DisplayPreferences) -> Self {
        let fake = Self::default();
        fake.set(preferences);
        fake
    }

    /// Changes what the next read returns, as a system setting change would.
    pub fn set(&self, preferences: crate::DisplayPreferences) {
        *lock(&self.state) = preferences;
    }

    /// How many times the settings were read.
    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl crate::DisplayAccessibility for FakeDisplayAccessibility {
    fn increase_contrast(&self) -> bool {
        self.preferences().increase_contrast
    }
    fn reduce_transparency(&self) -> bool {
        self.preferences().reduce_transparency
    }
    fn forced_colors(&self) -> bool {
        self.preferences().forced_colors
    }
    fn accent(&self, dark: bool) -> Option<u32> {
        self.preferences().accent(dark)
    }
    fn preferences(&self) -> crate::DisplayPreferences {
        self.reads.fetch_add(1, Ordering::SeqCst);
        *lock(&self.state)
    }
}

std::thread_local! {
    static STATUS_ICON_PLACE: std::cell::Cell<Option<crate::StatusIconPlace>> =
        const { std::cell::Cell::new(None) };
}

/// Active thread-local [`crate::StatusIconPlace`] override set by
/// [`FakeStatusIconPlace`], if any.
pub fn status_icon_place_override() -> Option<crate::StatusIconPlace> {
    STATUS_ICON_PLACE.with(std::cell::Cell::get)
}

/// Scoped fake for `NativePlatform::status_icon_place` (SHL-14): overrides
/// where the status icon lives on the current thread until dropped,
/// restoring the previous override.
#[derive(Debug)]
pub struct FakeStatusIconPlace {
    previous: Option<crate::StatusIconPlace>,
}

impl FakeStatusIconPlace {
    pub fn new(place: crate::StatusIconPlace) -> Self {
        let previous = STATUS_ICON_PLACE.with(|cell| cell.replace(Some(place)));
        Self { previous }
    }
}

impl Drop for FakeStatusIconPlace {
    fn drop(&mut self) {
        STATUS_ICON_PLACE.with(|cell| cell.set(self.previous));
    }
}

#[derive(Default)]
struct FakeChannelState {
    /// The listening side's sender and the id of the listener that set it.
    listener: Option<(u64, crate::HandoffSender)>,
    next_listener: u64,
    sent: Vec<crate::HandoffRequest>,
    fail_sends: bool,
}

/// In-memory [`crate::InstanceChannel`]: clones share one channel, so a
/// test's "second launch" reaches the "running app" that listens on it.
/// `send` waits for the app's answer like the real channel, so call it off
/// the UI loop (a thread) while the loop runs.
#[derive(Clone, Default)]
pub struct FakeInstanceChannel {
    state: Arc<Mutex<FakeChannelState>>,
}

impl FakeInstanceChannel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every request sent, delivered or not.
    pub fn sent(&self) -> Vec<crate::HandoffRequest> {
        lock(&self.state).sent.clone()
    }

    /// Makes every later send fail before it reaches the app, as a
    /// channel the second launch can't open would.
    pub fn fail_sends(&self, fail: bool) {
        lock(&self.state).fail_sends = fail;
    }

    pub fn is_listening(&self) -> bool {
        lock(&self.state).listener.is_some()
    }

    /// Hands `request` to the listening app without waiting for its
    /// answer, as a second launch's arrives; the answer comes on the
    /// returned receiver. For tests that drive the app's loop on the same
    /// thread.
    pub fn deliver(
        &self,
        request: crate::HandoffRequest,
    ) -> Result<std::sync::mpsc::Receiver<&'static str>> {
        let mut state = lock(&self.state);
        state.sent.push(request);
        let sender = state
            .listener
            .as_ref()
            .map(|(_, sender)| sender.clone())
            .ok_or_else(|| Error::NotFound("no running BroScribe listens".into()))?;
        let (handoff, answered) = crate::Handoff::new(request);
        sender
            .unbounded_send(handoff)
            .map_err(|_| Error::NotFound("the running BroScribe stopped listening".into()))?;
        Ok(answered)
    }
}

struct FakeHandoffListener {
    id: u64,
    state: Arc<Mutex<FakeChannelState>>,
}

impl crate::HandoffListener for FakeHandoffListener {}

impl Drop for FakeHandoffListener {
    fn drop(&mut self) {
        let mut state = lock(&self.state);
        if state
            .listener
            .as_ref()
            .is_some_and(|(id, _)| *id == self.id)
        {
            state.listener = None;
        }
    }
}

impl crate::InstanceChannel for FakeInstanceChannel {
    fn listen(&self, sender: crate::HandoffSender) -> Result<Box<dyn crate::HandoffListener>> {
        let mut state = lock(&self.state);
        if state.listener.is_some() {
            return Err(Error::AlreadyExists("another app listens here".into()));
        }
        state.next_listener += 1;
        let id = state.next_listener;
        state.listener = Some((id, sender));
        Ok(Box::new(FakeHandoffListener {
            id,
            state: self.state.clone(),
        }))
    }

    fn request(&self, request: crate::HandoffRequest, timeout: Duration) -> Result<String> {
        let sender = {
            let mut state = lock(&self.state);
            state.sent.push(request);
            if state.fail_sends {
                return Err(Error::Unsupported("injected handoff failure".into()));
            }
            state.listener.as_ref().map(|(_, sender)| sender.clone())
        };
        let sender =
            sender.ok_or_else(|| Error::NotFound("no running BroScribe listens".into()))?;
        Ok(crate::shell::answer_request(request.word(), &sender, timeout).to_owned())
    }
}

/// What [`FakeActivation`] was asked to do, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivationCall {
    Policy(crate::ActivationPolicy),
    Activate,
}

/// Records activation policy changes and activations instead of making
/// them (SHL-15).
#[derive(Default)]
pub struct FakeActivation {
    calls: Mutex<Vec<ActivationCall>>,
}

impl FakeActivation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn calls(&self) -> Vec<ActivationCall> {
        lock(&self.calls).clone()
    }

    /// The policy changes alone, in order.
    pub fn policies(&self) -> Vec<crate::ActivationPolicy> {
        lock(&self.calls)
            .iter()
            .filter_map(|call| match call {
                ActivationCall::Policy(policy) => Some(*policy),
                ActivationCall::Activate => None,
            })
            .collect()
    }

    pub fn clear(&self) {
        lock(&self.calls).clear();
    }
}

impl<Cx: ?Sized> crate::AppActivation<Cx> for FakeActivation {
    fn set_activation_policy(&self, policy: crate::ActivationPolicy, _cx: &mut Cx) {
        lock(&self.calls).push(ActivationCall::Policy(policy));
    }

    fn activate(&self, _cx: &mut Cx) {
        lock(&self.calls).push(ActivationCall::Activate);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn playback_rate_changes_full_audio_and_leaves_sample_ranges_at_normal_speed() {
        use crate::AudioPlayer as _;
        let mut player = super::FakeAudioPlayer::new(100.);
        let path = std::path::Path::new("synthetic.wav");
        player.set_rate(1.5).unwrap();
        player.play(path, 10., None).unwrap();
        player.advance(4.);
        assert_eq!(player.position_s(), Some(16.));
        player.pause();
        player.set_rate(0.75).unwrap();
        player.advance(4.);
        assert_eq!(player.position_s(), Some(16.));
        player.resume().unwrap();
        player.advance(4.);
        assert_eq!(player.position_s(), Some(19.));
        player.play(path, 20., Some(30.)).unwrap();
        player.set_rate(2.).unwrap();
        player.advance(2.);
        assert_eq!(player.position_s(), Some(22.));
        assert!(player.set_rate(f32::NAN).is_err());
    }

    use super::*;

    #[test]
    fn simple_fakes_behave() {
        let probe = FakeProcessProbe::new(42);
        assert!(probe.is_alive(42));
        probe.set_alive(7, true);
        assert!(probe.is_alive(7));
        probe.set_alive(7, false);
        assert!(!probe.is_alive(7));

        let secrets = FakeSecretStore::default();
        secrets.set("k", "v").unwrap();
        assert_eq!(secrets.get("k").unwrap().as_deref(), Some("v"));
        secrets.delete("k").unwrap();
        assert_eq!(secrets.get("k").unwrap(), None);

        let cap = FakeCapture::new(48_000, vec![0.1; 3]);
        cap.set_excluded_pids(&[7]).unwrap();
        assert_eq!(*lock(&cap.excluded), vec![7]);
        let (tx, rx) = std::sync::mpsc::channel();
        let sink: MicSink = Box::new(move |c: MicChunk| tx.send(c.samples_mono_f32.len()).unwrap());
        let session = SystemAudioCapture::start(&cap, sink).unwrap();
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![3]);
        session.stop().unwrap();
        assert_eq!(cap.stopped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn fake_mic_produces_a_sine_with_levels() {
        let mic = FakeMicCapture::new(16_000);
        assert_eq!(mic.devices().unwrap().len(), 1);
        let (tx, rx) = std::sync::mpsc::channel();
        let session = mic
            .start(Some("fake-mic"), Box::new(move |c| tx.send(c).unwrap()))
            .unwrap();
        assert_eq!(session.sample_rate(), 16_000);
        let chunks: Vec<MicChunk> = rx.try_iter().collect();
        assert_eq!(chunks.len(), 10);
        for c in &chunks {
            assert_eq!(c.samples_mono_f32.len(), 1_600);
            assert!((c.level_peak - 0.5).abs() < 0.01, "{}", c.level_peak);
            // A sine's RMS is its amplitude / sqrt(2).
            assert!(
                (c.level_rms - 0.5 / 2f32.sqrt()).abs() < 0.01,
                "{}",
                c.level_rms
            );
        }
        assert!(session.failure().is_none());
        // The sink lives until stop: the channel is still connected.
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        session.stop().unwrap();
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
        assert_eq!(mic.stopped.load(Ordering::SeqCst), 1);
        assert!(mic.start(Some("missing"), Box::new(|_| {})).is_err());
        assert_eq!(*lock(&mic.started), vec![Some("fake-mic".to_owned())]);
    }

    #[test]
    fn fake_mic_failure_drops_the_sink() {
        let mut mic = FakeMicCapture::new(8_000);
        mic.failure = Some("device lost".into());
        let (tx, rx) = std::sync::mpsc::channel();
        let session = mic
            .start(None, Box::new(move |c| tx.send(c).unwrap()))
            .unwrap();
        assert_eq!(rx.iter().count(), 10);
        assert_eq!(session.failure().as_deref(), Some("device lost"));
    }

    #[test]
    fn fake_player_tracks_position_and_end() {
        let mut p = FakeAudioPlayer::new(60.0);
        assert!(p.can_play_natively(Path::new("a.WAV")));
        assert!(!p.can_play_natively(Path::new("a.ogg")));
        assert!(p.play(Path::new("a.ogg"), 0.0, None).is_err());
        assert_eq!(p.poll(), PlayerState::Stopped);

        p.play(Path::new("a.m4a"), 10.0, Some(12.0)).unwrap();
        assert!(p.is_playing());
        p.advance(1.0);
        assert_eq!(p.poll(), PlayerState::Playing { position_s: 11.0 });
        p.pause();
        p.advance(5.0);
        assert_eq!(p.poll(), PlayerState::Paused { position_s: 11.0 });
        p.resume().unwrap();
        p.advance(1.5);
        assert_eq!(p.position_s(), Some(12.0));
        assert_eq!(p.poll(), PlayerState::Finished { position_s: 12.0 });
        assert_eq!(p.poll(), PlayerState::Stopped);
        assert_eq!(p.position_s(), None);
        assert!(p.resume().is_err());

        p.play(Path::new("a.mp3"), 59.0, None).unwrap();
        p.advance(2.0);
        assert_eq!(p.poll(), PlayerState::Finished { position_s: 60.0 });
        assert_eq!(p.played.len(), 2);
    }

    #[test]
    fn fake_player_prepares_off_thread_and_refuses_stale_results() {
        use crate::player::{PLAYBACK_CANCELLED, PLAYBACK_SUPERSEDED};

        let mut p = FakeAudioPlayer::new(10.0);
        p.probe_delay = Duration::from_secs(30);
        let first = p.prepare(Path::new("a.m4a")).unwrap();
        assert!(p.is_preparing());
        let first = std::thread::spawn(move || first.run());
        // A newer request supersedes the first while it is still probing.
        p.probe_delay = Duration::from_millis(20);
        let second = p.prepare(Path::new("b.m4a")).unwrap();
        let first = first.join().unwrap();
        assert_eq!(first.unwrap_err().code(), PLAYBACK_CANCELLED);
        let second = std::thread::spawn(move || second.run())
            .join()
            .unwrap()
            .unwrap();
        assert_eq!(second.duration_s(), Some(10.0));
        p.play_prepared(second, 2.0, Some(4.0)).unwrap();
        assert_eq!(p.played, [(PathBuf::from("b.m4a"), 2.0, Some(4.0))]);
        assert!(!p.is_preparing());

        // Stopping supersedes a finished preparation that was not played yet.
        p.probe_delay = Duration::ZERO;
        let stale = p.prepare(Path::new("c.m4a")).unwrap().run().unwrap();
        p.stop();
        assert_eq!(
            p.play_prepared(stale, 0.0, None).unwrap_err().code(),
            PLAYBACK_SUPERSEDED
        );
        assert_eq!(p.poll(), PlayerState::Stopped);

        // Another player's preparation is refused.
        let mut other = FakeAudioPlayer::new(10.0);
        let foreign = other.prepare(Path::new("d.m4a")).unwrap().run().unwrap();
        p.prepare(Path::new("d.m4a")).unwrap();
        assert_eq!(
            p.play_prepared(foreign, 0.0, None).unwrap_err().code(),
            "invalid_input"
        );
        assert!(p.is_preparing());
    }

    #[test]
    fn fake_player_drop_cancels_and_ranges_are_checked() {
        let mut p = FakeAudioPlayer::new(10.0);
        p.probe_delay = Duration::from_secs(30);
        let pending = p.prepare(Path::new("a.m4a")).unwrap();
        drop(p);
        let started = Instant::now();
        assert_eq!(
            pending.run().unwrap_err().code(),
            crate::player::PLAYBACK_CANCELLED
        );
        assert!(started.elapsed() < Duration::from_secs(5));

        let mut p = FakeAudioPlayer::new(10.0);
        for (start, end) in [(-1.0, None), (3.0, Some(3.0)), (f64::NAN, None)] {
            let prepared = p.prepare(Path::new("a.m4a")).unwrap().run().unwrap();
            assert_eq!(
                p.play_prepared(prepared, start, end).unwrap_err().code(),
                "invalid_input"
            );
        }
        assert!(p.played.is_empty());
    }

    #[test]
    fn fake_player_reports_loading_then_eof() {
        let mut p = FakeAudioPlayer::new(10.0);
        p.loading_s = 0.5;
        p.play(Path::new("a.wav"), 9.0, None).unwrap();
        assert_eq!(p.poll(), PlayerState::Loading { position_s: 9.0 });
        p.advance(0.75);
        assert_eq!(p.poll(), PlayerState::Playing { position_s: 9.25 });
        p.advance(5.0);
        assert_eq!(p.position_s(), Some(10.0));
        assert_eq!(p.poll(), PlayerState::Finished { position_s: 10.0 });
        assert_eq!(p.poll(), PlayerState::Stopped);
    }

    #[test]
    fn fake_player_sends_unplayable_recordings_to_the_cache() {
        let mut p = FakeAudioPlayer::new(10.0);
        p.needs_cache.push(PathBuf::from("two.m4a"));
        let error = p.play(Path::new("two.m4a"), 0.0, None).unwrap_err();
        assert_eq!(error.code(), crate::player::PLAYBACK_NEEDS_CACHE);
        assert_eq!(p.poll(), PlayerState::Stopped);
        p.play(Path::new("cache.wav"), 0.0, None).unwrap();
        assert_eq!(p.played, [(PathBuf::from("cache.wav"), 0.0, None)]);
    }

    #[test]
    fn fake_watcher_triggers_until_stopped() {
        let watcher = FakeLibraryWatcher::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let root = Path::new("/lib");
        let handle = watcher
            .watch(root, Box::new(move |paths| tx.send(paths).unwrap()))
            .unwrap();
        assert_eq!(watcher.active(), 1);
        assert_eq!(watcher.trigger(root, vec![root.join("people")]), 1);
        assert_eq!(rx.try_recv().unwrap(), vec![root.join("people")]);
        assert_eq!(watcher.trigger(Path::new("/other"), vec![]), 0);
        handle.stop();
        assert_eq!(watcher.active(), 0);
        assert_eq!(watcher.trigger(root, vec![root.join("people")]), 0);
    }

    #[test]
    fn fake_display_accessibility_reports_what_was_set_and_counts_reads() {
        use crate::DisplayAccessibility as _;
        let fake = FakeDisplayAccessibility::default();
        assert_eq!(fake.preferences(), crate::DisplayPreferences::default());
        let shared = fake.clone();
        shared.set(crate::DisplayPreferences {
            increase_contrast: true,
            reduce_transparency: true,
            forced_colors: false,
            light_accent: Some(0x007aff),
            dark_accent: Some(0x409cff),
        });
        assert!(fake.increase_contrast() && fake.reduce_transparency());
        assert!(!fake.forced_colors());
        assert_eq!(fake.accent(false), Some(0x007aff));
        assert_eq!(fake.accent(true), Some(0x409cff));
        assert_eq!(fake.reads(), 6);
    }

    #[test]
    fn fake_system_browser_and_status_item() {
        let info = FakeSystemInfo::default();
        assert_eq!(info.cpu_cores(), 10);
        assert_eq!(info.free_disk_bytes(Path::new("/")).unwrap(), 100 << 30);

        let browser = FakeBrowserLauncher::default();
        browser.open("https://example.org/licence").unwrap();
        assert!(browser.open("file:///etc/hosts").is_err());
        assert_eq!(lock(&browser.opened).len(), 1);

        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        let mut item = FakeStatusItem::new(tx);
        item.set_title("BroScribe · 3 speakers to review").unwrap();
        item.set_badge_count(3).unwrap();
        assert_eq!(item.display_title(), "BroScribe · 3 speakers to review");
        let mut off = MenuItemSpec::new("settings", "Settings…");
        off.enabled = false;
        item.set_menu(vec![
            MenuItemSpec::new("open-app", "Open BroScribe"),
            MenuItemSpec::separator(),
            off,
        ])
        .unwrap();
        assert!(item.click("open-app"));
        assert!(!item.click("settings"));
        assert!(!item.click(""));
        assert_eq!(rx.try_recv().unwrap(), "open-app");
        assert!(rx.try_recv().is_err());

        // Without an enabled default item, activation sends nothing.
        assert!(!item.activate());
        assert!(!item.secondary_activate());
        assert!(rx.try_recv().is_err());

        let mut open = MenuItemSpec::new("open-app", "Open BroScribe");
        open.default = true;
        item.set_menu(vec![
            MenuItemSpec::new("record-start", "Start recording"),
            open,
        ])
        .unwrap();
        assert!(item.activate());
        assert_eq!(rx.try_recv().unwrap(), "open-app");
        assert!(item.secondary_activate());
        assert_eq!(rx.try_recv().unwrap(), "open-app");
        assert!(rx.try_recv().is_err());

        assert_eq!(dialog_button_order_override(), None);
        {
            let outer = FakeDialogButtonOrder::new(crate::DialogButtonOrder::ActionFirst);
            assert_eq!(outer.order(), crate::DialogButtonOrder::ActionFirst);
            assert_eq!(
                dialog_button_order_override(),
                Some(crate::DialogButtonOrder::ActionFirst)
            );
            {
                let inner = FakeDialogButtonOrder::new(crate::DialogButtonOrder::CancelFirst);
                assert_eq!(inner.order(), crate::DialogButtonOrder::CancelFirst);
                assert_eq!(
                    dialog_button_order_override(),
                    Some(crate::DialogButtonOrder::CancelFirst)
                );
            }
            assert_eq!(
                dialog_button_order_override(),
                Some(crate::DialogButtonOrder::ActionFirst)
            );
        }
        assert_eq!(dialog_button_order_override(), None);
    }

    #[test]
    fn fake_application_discovery_and_install_location_are_configurable() {
        let mut system = FakeSystemInfo::default();
        let aliases = ["test.alias".into(), "test.application".into()];
        assert_eq!(system.application_installed(&aliases), None);
        system.installed_applications = Some(HashSet::new());
        assert_eq!(system.application_installed(&aliases), Some(false));
        system.installed_applications = Some(HashSet::from([aliases[1].clone()]));
        assert_eq!(system.application_installed(&aliases), Some(true));

        let autostart = FakeAutostart::default();
        assert!(!autostart.agent_install_location_advisory());
        autostart
            .agent_install_location_advisory
            .store(true, Ordering::SeqCst);
        assert!(autostart.agent_install_location_advisory());
    }

    #[test]
    fn fake_standard_overlay_records_presentations_and_can_fail() {
        use crate::overlay::{OverlayCapability, RecordingOverlay as _};
        use raw_window_handle::{HandleError, HasWindowHandle, WindowHandle};

        struct NoHandle;
        impl HasWindowHandle for NoHandle {
            fn window_handle(&self) -> std::result::Result<WindowHandle<'_>, HandleError> {
                Err(HandleError::Unavailable)
            }
        }

        let overlay = FakeRecordingOverlay::new(OverlayCapability::Standard);
        assert!(overlay.present_standard(&NoHandle).is_ok());
        overlay.fail_standard.store(true, Ordering::SeqCst);
        assert!(overlay.present_standard(&NoHandle).is_err());
        assert_eq!(overlay.standard_presentations.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn fake_announcer_records_non_empty_announcements_in_order() {
        let announcer = FakeAnnouncer::new();
        announcer.announce("Saving…", AnnouncementPriority::Medium);
        announcer.announce("Disk space is low.", AnnouncementPriority::High);
        announcer.announce("", AnnouncementPriority::Medium);
        assert_eq!(
            announcer.announcements(),
            vec![
                ("Saving…".to_owned(), AnnouncementPriority::Medium),
                ("Disk space is low.".to_owned(), AnnouncementPriority::High),
            ]
        );
        assert_eq!(announcer.take().len(), 2);
        assert!(announcer.announcements().is_empty());
    }

    #[test]
    fn fake_status_icon_place_is_scoped_and_restores_the_previous_one() {
        use crate::StatusIconPlace;
        assert_eq!(status_icon_place_override(), None);
        {
            let _outer = FakeStatusIconPlace::new(StatusIconPlace::SystemTray);
            {
                let _inner = FakeStatusIconPlace::new(StatusIconPlace::NoTrayHost);
                assert_eq!(
                    status_icon_place_override(),
                    Some(StatusIconPlace::NoTrayHost)
                );
            }
            assert_eq!(
                status_icon_place_override(),
                Some(StatusIconPlace::SystemTray)
            );
        }
        assert_eq!(status_icon_place_override(), None);
    }

    /// The "running app" side of a fake channel: answers every request
    /// with `done` on its own thread until the channel closes.
    fn answering_app(
        mut requests: crate::HandoffReceiver,
    ) -> std::thread::JoinHandle<Vec<crate::HandoffRequest>> {
        std::thread::spawn(move || {
            let mut handled = Vec::new();
            while let Some(handoff) = crate::shell::next_handoff_blocking(&mut requests) {
                handled.push(handoff.request);
                handoff.done();
            }
            handled
        })
    }

    #[test]
    fn fake_instance_channel_delivers_to_the_one_listener() {
        use crate::{HandoffRequest, InstanceChannel as _};
        let channel = FakeInstanceChannel::new();
        // Nobody listens yet: the second launch's request fails.
        assert!(matches!(
            channel.send(HandoffRequest::Show, Duration::from_secs(5)),
            Err(Error::NotFound(_))
        ));
        let (sender, requests) = crate::handoff_channel();
        let listener = channel.listen(sender).unwrap();
        assert!(channel.is_listening());
        let (other, _unused) = crate::handoff_channel();
        assert!(matches!(
            channel.clone().listen(other),
            Err(Error::AlreadyExists(_))
        ));
        let app = answering_app(requests);
        channel
            .clone()
            .send(HandoffRequest::Show, Duration::from_secs(5))
            .unwrap();
        channel.fail_sends(true);
        assert!(
            channel
                .send(HandoffRequest::Show, Duration::from_secs(5))
                .is_err()
        );
        channel.fail_sends(false);
        let answered = channel.deliver(HandoffRequest::Show).unwrap();
        answered
            .recv_timeout(Duration::from_secs(5))
            .expect("answered");
        drop(listener);
        assert!(!channel.is_listening());
        assert!(channel.deliver(HandoffRequest::Show).is_err());
        assert_eq!(app.join().unwrap(), [HandoffRequest::Show; 2]);
        assert!(
            channel
                .send(HandoffRequest::Show, Duration::from_secs(5))
                .is_err()
        );
        assert_eq!(channel.sent(), [HandoffRequest::Show; 6]);
    }

    #[test]
    fn fake_activation_records_policies_and_activations_in_order() {
        use crate::{ActivationPolicy, AppActivation as _};
        let fake = FakeActivation::new();
        fake.set_activation_policy(ActivationPolicy::Regular, &mut ());
        fake.activate(&mut ());
        fake.set_activation_policy(ActivationPolicy::Accessory, &mut ());
        assert_eq!(
            fake.calls(),
            [
                ActivationCall::Policy(ActivationPolicy::Regular),
                ActivationCall::Activate,
                ActivationCall::Policy(ActivationPolicy::Accessory),
            ]
        );
        assert_eq!(
            fake.policies(),
            [ActivationPolicy::Regular, ActivationPolicy::Accessory]
        );
        fake.clear();
        assert!(fake.calls().is_empty());
    }
}

/// Records assistive-technology announcements for tests.
#[derive(Clone, Debug, Default)]
pub struct FakeAnnouncer {
    announcements: Arc<Mutex<Vec<(String, AnnouncementPriority)>>>,
}

impl FakeAnnouncer {
    pub fn new() -> Self {
        Self::default()
    }

    /// All recorded `(text, priority)` pairs in arrival order.
    pub fn announcements(&self) -> Vec<(String, AnnouncementPriority)> {
        lock(&self.announcements).clone()
    }

    /// Drains and returns all recorded `(text, priority)` pairs.
    pub fn take(&self) -> Vec<(String, AnnouncementPriority)> {
        std::mem::take(&mut *lock(&self.announcements))
    }

    /// Clears recorded announcements.
    pub fn clear(&self) {
        lock(&self.announcements).clear();
    }
}

impl Announcer for FakeAnnouncer {
    fn announce(&self, text: &str, priority: AnnouncementPriority) {
        if !text.is_empty() {
            lock(&self.announcements).push((text.to_owned(), priority));
        }
    }
}

/// One scripted native file prompt result.
pub struct FakeImportPathPicker(pub Option<Vec<std::path::PathBuf>>);

impl crate::ui::ImportPathPicker for FakeImportPathPicker {
    fn pick(
        &self,
        _: crate::ui::ImportPathRequest,
    ) -> futures::future::LocalBoxFuture<
        'static,
        std::result::Result<Option<Vec<std::path::PathBuf>>, String>,
    > {
        Box::pin(futures::future::ready(Ok(self.0.clone())))
    }
}

pub struct FakeImportFileDrop(pub Vec<std::path::PathBuf>);

impl crate::ui::ImportFileDrop for FakeImportFileDrop {
    fn paths(&self) -> &[std::path::PathBuf] {
        &self.0
    }
}

/// File chooser and file manager on a test UI loop. Records both interactions.
#[derive(Default)]
pub struct FakeFileActions {
    pub selected: Option<PathBuf>,
    pub prompts: std::cell::RefCell<Vec<(PathBuf, String)>>,
    pub revealed: std::cell::RefCell<Vec<PathBuf>>,
}

impl<Ui> crate::paths::FileActions<Ui> for FakeFileActions {
    fn choose_save_path(
        &self,
        _: &Ui,
        directory: &std::path::Path,
        name: &str,
    ) -> crate::paths::SavePath {
        self.prompts
            .borrow_mut()
            .push((directory.to_owned(), name.to_owned()));
        Box::pin(std::future::ready(Ok(self.selected.clone())))
    }

    fn reveal(&self, _: &Ui, path: &std::path::Path) {
        self.revealed.borrow_mut().push(path.to_owned());
    }
}
