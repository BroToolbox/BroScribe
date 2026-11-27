use crate::timed::{HostClock, TimedRing};
use broscribe_platform::devices::collect_devices;
use broscribe_platform::{
    AudioDevice, CaptureSession, Error, MicCapture, MicChunk, MicSink, MicUsageMonitor, MicUser,
    Result, SystemAudioCapture,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};

pub struct WinMicCapture;
pub struct WinMicUsageMonitor;
#[derive(Default)]
pub struct WinSystemAudioCapture {
    excluded: Mutex<Vec<u32>>,
}
struct State {
    stop: AtomicBool,
    failure: Mutex<Option<String>>,
}
impl State {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false),
            failure: Mutex::new(None),
        })
    }
    fn fail(&self, e: impl std::fmt::Display) {
        *self.failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
        self.stop.store(true, Ordering::Release);
    }
}
struct Session {
    state: Arc<State>,
    thread: Option<JoinHandle<()>>,
    rate: u32,
    name: String,
}
impl Session {
    fn finish(&mut self) -> Result<()> {
        self.state.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.join()
                .map_err(|_| Error::Internal("capture worker panicked".into()))?;
        }
        Ok(())
    }
}
impl CaptureSession for Session {
    fn sample_rate(&self) -> u32 {
        self.rate
    }
    fn device_name(&self) -> Option<String> {
        Some(self.name.clone())
    }
    fn failure(&self) -> Option<String> {
        self.state
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn stop(mut self: Box<Self>) -> Result<()> {
        self.finish()
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}
fn start_worker(
    name: &str,
    run: impl FnOnce(Arc<State>, mpsc::SyncSender<Result<(u32, String)>>) -> Result<()> + Send + 'static,
) -> Result<Box<dyn CaptureSession>> {
    let state = State::new();
    let worker_state = state.clone();
    let (tx, rx) = mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            if let Err(e) = run(worker_state.clone(), tx.clone()) {
                let _ = tx.try_send(Err(crate::error("starting audio capture", &e)));
                worker_state.fail(e);
            }
        })?;
    match rx.recv() {
        Ok(Ok((rate, name))) => Ok(Box::new(Session {
            state,
            thread: Some(thread),
            rate,
            name,
        })),
        result => {
            state.stop.store(true, Ordering::Release);
            let _ = thread.join();
            Err(match result {
                Ok(Err(e)) => e,
                _ => Error::Internal("capture worker ended before startup".into()),
            })
        }
    }
}
fn device_name(d: &cpal::Device) -> String {
    d.description()
        .map(|d| d.name().to_owned())
        .unwrap_or_else(|_| "Microphone".into())
}
impl MicCapture for WinMicCapture {
    /// Every input through the shared collector: a device whose identifier
    /// cannot be read is left out (logged by position) rather than failing the
    /// list, so a pinned microphone beside it stays selectable; only a listing
    /// where no device is readable fails. Rates are optional facts: a device
    /// whose formats cannot be read right now is listed without them.
    fn devices(&self) -> Result<Vec<AudioDevice>> {
        let host = cpal::default_host();
        let default = host.default_input_device().and_then(|d| d.id().ok());
        let listed = host
            .input_devices()
            .map_err(|e| crate::error("listing microphones", e))?;
        collect_devices("microphones", listed, |d| -> Result<_> {
            let id = d
                .id()
                .map_err(|e| crate::error("microphone identifier", e))?;
            let mut rates = Vec::new();
            for range in d.supported_input_configs().into_iter().flatten() {
                rates.extend([range.min_sample_rate(), range.max_sample_rate()]);
                for rate in [16000, 24000, 44100, 48000, 96000] {
                    if rate >= range.min_sample_rate() && rate <= range.max_sample_rate() {
                        rates.push(rate);
                    }
                }
            }
            rates.sort_unstable();
            rates.dedup();
            Ok(Some(AudioDevice {
                id: id.to_string(),
                name: device_name(&d),
                is_default: default.as_ref() == Some(&id),
                sample_rates: rates,
            }))
        })
    }
    fn start(&self, device: Option<&str>, mut sink: MicSink) -> Result<Box<dyn CaptureSession>> {
        let selected = device.map(str::to_owned);
        start_worker("broscribe-wasapi-mic", move |state, ready| {
            let _com = crate::Com::new()?;
            let host = cpal::default_host();
            let device = if let Some(id) = selected {
                host.input_devices()
                    .map_err(|e| crate::error("listing microphones", e))?
                    .find(|d| d.id().is_ok_and(|v| v.to_string() == id))
            } else {
                host.default_input_device()
            }
            .ok_or_else(|| Error::NotFound("microphone".into()))?;
            let supported = device
                .default_input_config()
                .map_err(|e| microphone_error("default microphone format", e))?;
            let config: cpal::StreamConfig = supported.into();
            let rate = config.sample_rate;
            let ring = Arc::new(TimedRing::new());
            let clock = HostClock::new()?;
            macro_rules! stream {
                ($t:ty) => {
                    mic_stream::<$t>(&device, &config, ring.clone(), state.clone())?
                };
            }
            let stream = match supported.sample_format() {
                cpal::SampleFormat::I8 => stream!(i8),
                cpal::SampleFormat::I16 => stream!(i16),
                cpal::SampleFormat::I24 => stream!(cpal::I24),
                cpal::SampleFormat::I32 => stream!(i32),
                cpal::SampleFormat::I64 => stream!(i64),
                cpal::SampleFormat::U8 => stream!(u8),
                cpal::SampleFormat::U16 => stream!(u16),
                cpal::SampleFormat::U24 => stream!(cpal::U24),
                cpal::SampleFormat::U32 => stream!(u32),
                cpal::SampleFormat::U64 => stream!(u64),
                cpal::SampleFormat::F32 => stream!(f32),
                cpal::SampleFormat::F64 => stream!(f64),
                format => {
                    return Err(Error::Unsupported(format!(
                        "microphone sample representation {format:?}"
                    )));
                }
            };
            stream
                .play()
                .map_err(|e| crate::error("starting microphone", e))?;
            ready
                .send(Ok((rate, device_name(&device))))
                .map_err(|e| crate::error("capture startup", e))?;
            while !state.stop.load(Ordering::Acquire) {
                if ring.dropped() != 0 {
                    return Err(Error::Internal(
                        "microphone delivery overrun; segment stopped instead of losing samples"
                            .into(),
                    ));
                }
                let mut samples = Vec::new();
                while let Some(time) = ring.pop(&mut samples) {
                    let mut chunk = MicChunk::new(std::mem::take(&mut samples), rate);
                    chunk.host_time = clock.instant(time);
                    sink(chunk);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            drop(stream);
            let mut tail = Vec::new();
            while let Some(time) = ring.pop(&mut tail) {
                let mut chunk = MicChunk::new(std::mem::take(&mut tail), rate);
                chunk.host_time = clock.instant(time);
                sink(chunk);
            }
            Ok(())
        })
    }
}
fn mic_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    ring: Arc<TimedRing>,
    state: Arc<State>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = usize::from(config.channels);
    let rate = config.sample_rate;
    device
        .build_input_stream(
            *config,
            move |data: &[T], info: &cpal::InputCallbackInfo| {
                let time =
                    (info.timestamp().capture.as_nanos() / 100).min(u128::from(u64::MAX)) as u64;
                ring.push(data.len() / channels, time, rate, |frame| {
                    data[frame * channels..(frame + 1) * channels]
                        .iter()
                        .map(|v| v.to_sample::<f32>())
                        .sum::<f32>()
                        / channels as f32
                });
            },
            move |e| state.fail(e),
            None,
        )
        .map_err(|e| microphone_error("building microphone stream", e))
}

fn microphone_error(operation: &'static str, error: cpal::Error) -> Error {
    if error.kind() == cpal::ErrorKind::PermissionDenied {
        Error::Service {
            code: "microphone_privacy",
            message: "Windows denied microphone access".into(),
        }
    } else {
        crate::error(operation, error)
    }
}
impl SystemAudioCapture for WinSystemAudioCapture {
    fn set_excluded_pids(&self, pids: &[u32]) -> Result<()> {
        let mut roots = vec![std::process::id()];
        roots.extend_from_slice(pids);
        roots.sort_unstable();
        roots.dedup();
        // A process-loopback activation excludes one process tree, not a union.
        // Require an actual helper root rather than excluding an unrelated app.
        let root = roots
            .iter()
            .copied()
            .find(|root| roots.iter().all(|pid| descendant(*pid, *root)))
            .ok_or_else(|| {
                Error::InvalidInput(
                    "WASAPI exclusions must belong to one BroScribe helper process tree".into(),
                )
            })?;
        *self.excluded.lock().unwrap_or_else(|e| e.into_inner()) = vec![root];
        Ok(())
    }
    fn start(&self, mut sink: MicSink) -> Result<Box<dyn CaptureSession>> {
        let root = self
            .excluded
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .first()
            .copied()
            .unwrap_or_else(std::process::id);
        start_worker("broscribe-process-loopback", move |state, ready| {
            let _com = crate::Com::new()?;
            let mut client = wasapi::AudioClient::new_application_loopback_client(root, false)
                .map_err(|e| {
                    crate::error("WASAPI process loopback (Windows build 20348+ required)", e)
                })?;
            let rate = 48000;
            let format =
                wasapi::WaveFormat::new(32, 32, wasapi::SampleType::Float, rate as usize, 2, None);
            client
                .initialize_client(
                    &format,
                    wasapi::Direction::Capture,
                    wasapi::StreamMode::EventsShared {
                        autoconvert: true,
                        buffer_duration_hns: 0,
                    },
                    None,
                )
                .map_err(|e| crate::error("initializing process loopback", e))?;
            let event = client
                .set_get_eventhandle()
                .map_err(|e| crate::error("loopback event handle", e))?;
            let capture = client
                .get_audiocaptureclient()
                .map_err(|e| crate::error("process capture client", e))?;
            let mut bytes = vec![0u8; 64 * 1024];
            let clock = HostClock::new()?;
            let mut seen = false;
            client
                .start_stream()
                .map_err(|e| crate::error("starting loopback", e))?;
            ready
                .send(Ok((
                    rate,
                    "WASAPI system output (helper tree excluded)".into(),
                )))
                .map_err(|e| crate::error("loopback startup", e))?;
            let result = (|| {
                while !state.stop.load(Ordering::Acquire) {
                    loop {
                        let frames = capture
                            .get_next_packet_size()
                            .map_err(|e| crate::error("loopback packet", e))?
                            .unwrap_or(0);
                        if frames == 0 {
                            break;
                        }
                        let needed = frames as usize * 8;
                        if bytes.len() < needed {
                            bytes.resize(needed, 0);
                        }
                        let (frames, info) = capture
                            .read_from_device(&mut bytes)
                            .map_err(|e| crate::error("reading process loopback", e))?;
                        if info.flags.data_discontinuity && seen {
                            return Err(Error::Internal(
                                "WASAPI discontinuity; restarting audio segment".into(),
                            ));
                        }
                        let mono: Vec<f32> = bytes[..frames as usize * 8]
                            .as_chunks::<8>()
                            .0
                            .iter()
                            .map(|b| {
                                (f32::from_le_bytes(b[..4].try_into().expect("four bytes"))
                                    + f32::from_le_bytes(b[4..].try_into().expect("four bytes")))
                                    * 0.5
                            })
                            .collect();
                        let mut chunk = MicChunk::new(mono, rate);
                        chunk.host_time = if info.flags.timestamp_error {
                            None
                        } else {
                            clock.instant(info.timestamp)
                        };
                        sink(chunk);
                        seen = true;
                    }
                    if let Err(e) = event.wait_for_event(100)
                        && !matches!(e, wasapi::WasapiError::EventTimeout)
                    {
                        return Err(crate::error("waiting for process loopback", e));
                    }
                }
                Ok(())
            })();
            client
                .stop_stream()
                .map_err(|e| crate::error("stopping loopback", e))?;
            result
        })
    }
}
fn descendant(mut pid: u32, root: u32) -> bool {
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return pid == root;
        };
        let snapshot = crate::native::OwnedHandle(snapshot);
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut parents = std::collections::HashMap::new();
        if Process32FirstW(snapshot.0, &mut entry).is_ok() {
            loop {
                parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                if Process32NextW(snapshot.0, &mut entry).is_err() {
                    break;
                }
            }
        }
        for _ in 0..parents.len() + 1 {
            if pid == root {
                return true;
            }
            let Some(parent) = parents.get(&pid) else {
                return false;
            };
            if *parent == pid {
                return false;
            }
            pid = *parent;
        }
        false
    }
}
impl MicUsageMonitor for WinMicUsageMonitor {
    fn mic_users(&self) -> Result<Vec<MicUser>> {
        let _com = crate::Com::new()?;
        let enumerator = wasapi::DeviceEnumerator::new()
            .map_err(|e| crate::error("audio endpoint enumerator", e))?;
        let devices = enumerator
            .get_device_collection(wasapi::Direction::Capture)
            .map_err(|e| crate::error("capture endpoints", e))?;
        let mut users = std::collections::BTreeMap::new();
        for i in 0..devices
            .get_nbr_devices()
            .map_err(|e| crate::error("capture endpoint count", e))?
        {
            let device = devices
                .get_device_at_index(i)
                .map_err(|e| crate::error("capture endpoint", e))?;
            let sessions = device
                .get_iaudiosessionmanager()
                .and_then(|m| m.get_audiosessionenumerator())
                .map_err(|e| crate::error("IAudioSessionManager2", e))?;
            for j in 0..sessions
                .get_count()
                .map_err(|e| crate::error("audio sessions", e))?
            {
                let session = sessions
                    .get_session(j)
                    .map_err(|e| crate::error("audio session", e))?;
                if session
                    .get_state()
                    .map_err(|e| crate::error("audio session state", e))?
                    != wasapi::SessionState::Active
                {
                    continue;
                }
                let pid = session
                    .get_process_id()
                    .map_err(|e| crate::error("microphone process", e))?;
                if pid == 0 {
                    continue;
                }
                users.insert(
                    pid,
                    MicUser {
                        pid,
                        bundle_id: crate::native::process_path(pid)
                            .and_then(|p| crate::native::app_identity(&p)),
                        ..Default::default()
                    },
                );
            }
        }
        let mut users: Vec<MicUser> = users.into_values().collect();
        attribute_helpers(&mut users);
        Ok(users)
    }
}

/// Points windowless audio helpers (Chromium's audio service, WebView2) at
/// the process owning the app's windows, the one UI Automation reads.
fn attribute_helpers(users: &mut [MicUser]) {
    if users.is_empty() {
        return;
    }
    let table = process_table();
    let owners: std::collections::HashSet<u32> = crate::ui::top_windows(None)
        .map(|windows| windows.into_iter().map(|(_, pid)| pid).collect())
        .unwrap_or_default();
    let parent_valid = |child: u32, parent: u32| match (created_at(child), created_at(parent)) {
        (Some(child), Some(parent)) => parent <= child,
        _ => true,
    };
    for user in users {
        if let Some((pid, exe)) = crate::process_tree::responsible_process(
            user.pid,
            &table,
            |pid| owners.contains(&pid),
            parent_valid,
        ) {
            user.responsible_pid = Some(pid);
            user.responsible_bundle_id = Some(exe);
        }
    }
}

/// Parent and executable name of every process.
fn process_table() -> std::collections::HashMap<u32, crate::process_tree::Process> {
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    let mut table = std::collections::HashMap::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return table;
        };
        let snapshot = crate::native::OwnedHandle(snapshot);
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot.0, &mut entry).is_ok() {
            loop {
                let name = &entry.szExeFile;
                let len = name.iter().position(|c| *c == 0).unwrap_or(name.len());
                table.insert(
                    entry.th32ProcessID,
                    crate::process_tree::Process {
                        parent: entry.th32ParentProcessID,
                        exe: String::from_utf16_lossy(&name[..len]),
                    },
                );
                if Process32NextW(snapshot.0, &mut entry).is_err() {
                    break;
                }
            }
        }
    }
    table
}

/// Creation time (FILETIME ticks) of a running process.
fn created_at(pid: u32) -> Option<u64> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let handle = crate::native::OwnedHandle(
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?,
        );
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user).ok()?;
        Some((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
}
