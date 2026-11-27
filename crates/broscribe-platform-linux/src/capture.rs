use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::rc::Rc;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::audio_ring::{
    CAPACITY, ClockAnchor, Mixer, RATE, TimedRing, deliver_timed, scaled_ticks,
};
use broscribe_platform::devices::collect_devices;
use broscribe_platform::{
    AudioDevice, CaptureSession, DeviceDetails, DeviceInfo, DeviceTransport, Error, MicCapture,
    MicSink, MicUsageMonitor, MicUser, Result, SystemAudioCapture,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;

const TICK: Duration = Duration::from_millis(10);
type Failure = Arc<Mutex<Option<String>>>;
fn fail(failure: &Failure, message: impl Into<String>) {
    let mut slot = failure.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        *slot = Some(message.into());
    }
}

struct Session {
    rate: u32,
    name: String,
    stop: Arc<AtomicBool>,
    failure: Failure,
    thread: Option<JoinHandle<()>>,
}
impl Session {
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            fail(&self.failure, "audio capture worker panicked");
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.shutdown();
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
        self.failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    fn stop(mut self: Box<Self>) -> Result<()> {
        self.shutdown();
        match self.failure() {
            Some(message) => Err(crate::platform_error("capturing audio", message)),
            None => Ok(()),
        }
    }
}

fn host() -> cpal::Host {
    cpal::host_from_id(cpal::HostId::PipeWire).unwrap_or_else(|_| cpal::default_host())
}
#[derive(Default)]
pub struct LinuxMicCapture;
impl MicCapture for LinuxMicCapture {
    /// Every input through the shared collector: a device whose identifier
    /// cannot be read is left out (logged by position) rather than failing the
    /// list, so a pinned microphone beside it stays selectable; only a listing
    /// where no device is readable fails. Rates are optional facts: a device
    /// whose formats cannot be read right now is listed without them.
    fn devices(&self) -> Result<Vec<AudioDevice>> {
        let host = host();
        let default = host.default_input_device().and_then(|d| d.id().ok());
        let listed = host
            .input_devices()
            .map_err(|e| crate::platform_error("listing Linux microphones", e))?;
        collect_devices("microphones", listed, |device| -> Result<_> {
            let id = device
                .id()
                .map_err(|e| crate::platform_error("reading microphone id", e))?;
            if id == cpal::DeviceId::new(cpal::HostId::Alsa, "null") {
                return Ok(None);
            }
            let mut sample_rates = BTreeSet::new();
            for range in device.supported_input_configs().into_iter().flatten() {
                sample_rates.insert(range.min_sample_rate());
                sample_rates.insert(range.max_sample_rate());
                for rate in [8000, 16000, 24000, 32000, 44100, 48000, 96000] {
                    if range.contains_rate(rate) {
                        sample_rates.insert(rate);
                    }
                }
            }
            Ok(Some(AudioDevice {
                is_default: default.as_ref() == Some(&id),
                id: id.to_string(),
                name: device.to_string(),
                sample_rates: sample_rates.into_iter().collect(),
            }))
        })
    }
    fn start(&self, device: Option<&str>, mut sink: MicSink) -> Result<Box<dyn CaptureSession>> {
        let device_id = device.map(str::to_owned);
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let thread_stop = stop.clone();
        let thread_failure = failure.clone();
        let (ready, started) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new().name("broscribe-mic-delivery".into()).spawn(move || {
            let setup = (|| -> Result<_> {
                let host = match device_id.as_deref() {
                    Some(id) => { let id: cpal::DeviceId = id.parse().map_err(|e| crate::platform_error("parsing microphone id", e))?; cpal::host_from_id(id.host()).map_err(|e| crate::platform_error("opening microphone host", e))? }
                    None => host(),
                };
                let device = match device_id {
                    Some(id) => host.device_by_id(&id.parse().map_err(|e| crate::platform_error("parsing microphone id", e))?).ok_or_else(|| Error::NotFound("selected microphone".into()))?,
                    None => host.default_input_device().ok_or_else(|| Error::NotFound("default microphone".into()))?,
                };
                let format = device.default_input_config().map_err(|e| crate::platform_error("reading microphone format", e))?;
                let rate = format.sample_rate(); let channels = format.channels() as usize;
                if channels == 0 || rate == 0 { return Err(Error::InvalidInput("microphone reported an empty format".into())); }
                let ring = Arc::new(TimedRing::new((rate as usize).saturating_mul(2)));
                let failed = Arc::new(AtomicBool::new(false));
                let stream = match format.sample_format() {
                    cpal::SampleFormat::F32 => mic_stream::<f32>(&device, format.config(), ring.clone(), failed.clone()),
                    cpal::SampleFormat::F64 => mic_stream::<f64>(&device, format.config(), ring.clone(), failed.clone()),
                    cpal::SampleFormat::I8 => mic_stream::<i8>(&device, format.config(), ring.clone(), failed.clone()),
                    cpal::SampleFormat::I16 => mic_stream::<i16>(&device, format.config(), ring.clone(), failed.clone()),
                    cpal::SampleFormat::I24 => mic_stream::<cpal::I24>(&device, format.config(), ring.clone(), failed.clone()),
                    cpal::SampleFormat::I32 => mic_stream::<i32>(&device, format.config(), ring.clone(), failed.clone()),
                    cpal::SampleFormat::I64 => mic_stream::<i64>(&device, format.config(), ring.clone(), failed.clone()),
                    cpal::SampleFormat::U8 => mic_stream::<u8>(&device, format.config(), ring.clone(), failed.clone()),
                    cpal::SampleFormat::U16 => mic_stream::<u16>(&device, format.config(), ring.clone(), failed.clone()),
                    cpal::SampleFormat::U24 => mic_stream::<cpal::U24>(&device, format.config(), ring.clone(), failed.clone()),
                    cpal::SampleFormat::U32 => mic_stream::<u32>(&device, format.config(), ring.clone(), failed.clone()),
                    cpal::SampleFormat::U64 => mic_stream::<u64>(&device, format.config(), ring.clone(), failed.clone()),
                    other => return Err(Error::Unsupported(format!("CPAL microphone sample format {other}"))),
                }?;
                stream.play().map_err(|e| crate::platform_error("starting microphone", e))?;
                Ok((stream, ring, failed, rate, device.to_string()))
            })();
            let (stream, ring, failed, rate, name) = match setup { Ok(s) => s, Err(e) => { let _ = ready.send(Err(e)); return; } };
            if ready.send(Ok((rate, name))).is_err() { return; }
            // CPAL stamps each block with its capture instant on the stream's
            // own clock; pairing that clock with `Instant` on every pass maps
            // the stamps onto the recorder's timeline. Delivery time (worker
            // scheduling, stalls) never enters the timestamps.
            let anchor = |stream: &cpal::Stream| {
                ClockAnchor::new(stream.now().as_nanos() as i128, Instant::now())
            };
            let mut clock = anchor(&stream);
            while !thread_stop.load(Ordering::Acquire) && !failed.load(Ordering::Acquire) {
                clock = anchor(&stream);
                if let Err(error) = deliver_timed(&ring, rate, clock, &mut sink) {
                    fail(&thread_failure, error.to_string());
                    break;
                }
                if ring.dropped() != 0 { fail(&thread_failure, "microphone delivery overrun; capture stopped rather than losing audio silently"); break; }
                std::thread::sleep(TICK);
            }
            drop(stream); // stop the producer before the final drain
            if failed.load(Ordering::Acquire) { fail(&thread_failure, "microphone stream failed or device/format was lost"); }
            if let Err(error) = deliver_timed(&ring, rate, clock, &mut sink) {
                fail(&thread_failure, error.to_string());
            }
        })?;
        match started.recv() {
            Ok(Ok((rate, name))) => Ok(Box::new(Session {
                rate,
                name,
                stop,
                failure,
                thread: Some(thread),
            })),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(e) => {
                let _ = thread.join();
                Err(crate::platform_error("starting microphone worker", e))
            }
        }
    }
}
fn mic_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    ring: Arc<TimedRing>,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = config.channels as usize;
    device
        .build_input_stream::<T, _, _>(
            config,
            move |data: &[T], info: &cpal::InputCallbackInfo| {
                let captured = info
                    .timestamp()
                    .capture
                    .as_nanos()
                    .min(u128::from(u64::MAX)) as u64;
                ring.push(0, captured, data.len() / channels, |frame| {
                    data[frame * channels..(frame + 1) * channels]
                        .iter()
                        .map(|s| <f32 as cpal::FromSample<T>>::from_sample_(*s))
                        .sum::<f32>()
                        / channels as f32
                });
            },
            move |_| {
                failed.store(true, Ordering::Release);
            },
            None,
        )
        .map_err(|e| crate::platform_error("building microphone stream", e))
}

#[derive(Default)]
pub struct LinuxDeviceInfo;
impl DeviceInfo for LinuxDeviceInfo {
    /// Every device through the shared collector: one whose identifier or
    /// description cannot be read is left out (logged by position) rather than
    /// failing the list; only a listing where no device is readable fails.
    fn devices(&self) -> Result<Vec<DeviceDetails>> {
        let host = host();
        let input = host.default_input_device().and_then(|d| d.id().ok());
        let output = host.default_output_device().and_then(|d| d.id().ok());
        let listed = host
            .devices()
            .map_err(|e| crate::platform_error("listing audio devices", e))?;
        collect_devices("audio devices", listed, |device| -> Result<_> {
            let id = device
                .id()
                .map_err(|e| crate::platform_error("reading audio device id", e))?;
            let description = device
                .description()
                .map_err(|e| crate::platform_error("reading audio device description", e))?;
            let input_format = device.default_input_config().ok();
            let output_format = device.default_output_config().ok();
            let input_channels = input_format
                .as_ref()
                .map(|f| f.channels() as u32)
                .unwrap_or(0);
            let output_channels = output_format
                .as_ref()
                .map(|f| f.channels() as u32)
                .unwrap_or(0);
            let nominal_rate = input_format
                .as_ref()
                .or(output_format.as_ref())
                .map(|f| f.sample_rate())
                .unwrap_or(0);
            let mut rates = BTreeSet::new();
            for format in device
                .supported_input_configs()
                .into_iter()
                .flatten()
                .chain(device.supported_output_configs().into_iter().flatten())
            {
                rates.insert(format.min_sample_rate());
                rates.insert(format.max_sample_rate());
                for rate in [8000, 16000, 24000, 32000, 44100, 48000, 96000] {
                    if format.contains_rate(rate) {
                        rates.insert(rate);
                    }
                }
            }
            let transport = match description.interface_type() {
                cpal::InterfaceType::BuiltIn => DeviceTransport::BuiltIn,
                cpal::InterfaceType::Usb => DeviceTransport::Usb,
                cpal::InterfaceType::Bluetooth => DeviceTransport::Bluetooth,
                cpal::InterfaceType::Pci => DeviceTransport::Pci,
                cpal::InterfaceType::FireWire => DeviceTransport::FireWire,
                cpal::InterfaceType::Thunderbolt => DeviceTransport::Thunderbolt,
                cpal::InterfaceType::Hdmi => DeviceTransport::Hdmi,
                cpal::InterfaceType::DisplayPort => DeviceTransport::DisplayPort,
                cpal::InterfaceType::Virtual => DeviceTransport::Virtual,
                cpal::InterfaceType::Aggregate => DeviceTransport::Aggregate,
                _ => DeviceTransport::Unknown,
            };
            Ok(Some(DeviceDetails {
                id: id.to_string(),
                name: description.name().into(),
                manufacturer: description.manufacturer().unwrap_or_default().into(),
                transport,
                output_data_source: None,
                input_data_source: None,
                nominal_rate,
                available_rates: rates.into_iter().collect(),
                input_channels,
                output_channels,
                is_default_input: input.as_ref() == Some(&id),
                is_default_output: output.as_ref() == Some(&id),
            }))
        })
    }
}

#[derive(Clone, Default)]
struct Node {
    properties: HashMap<String, String>,
    running: bool,
}
struct NodeBinding {
    _listener: pw::node::NodeListener,
    _node: pw::node::Node,
}
struct ClientBinding {
    _listener: pw::client::ClientListener,
    _client: pw::client::Client,
}
struct MetadataBinding {
    _listener: pw::metadata::MetadataListener,
    _metadata: pw::metadata::Metadata,
}
struct Graph {
    revision: Rc<Cell<u64>>,
    nodes: Rc<RefCell<HashMap<u32, Node>>>,
    clients: Rc<RefCell<HashMap<u32, HashMap<String, String>>>>,
    _client_bindings: Rc<RefCell<HashMap<u32, ClientBinding>>>,
    links: Rc<RefCell<HashMap<u32, (u32, u32)>>>,
    default_sink: Rc<RefCell<Option<String>>>,
    _bindings: Rc<RefCell<HashMap<u32, NodeBinding>>>,
    _metadata: Rc<RefCell<HashMap<u32, MetadataBinding>>>,
    _listener: pw::registry::Listener,
}
impl Graph {
    fn new(registry: &pw::registry::RegistryRc) -> Self {
        let nodes = Rc::new(RefCell::new(HashMap::<u32, Node>::new()));
        let revision = Rc::new(Cell::new(0u64));
        let changed = revision.clone();
        let removed = revision.clone();
        let clients = Rc::new(RefCell::new(HashMap::<u32, HashMap<String, String>>::new()));
        let client_bindings = Rc::new(RefCell::new(HashMap::new()));
        let c = clients.clone();
        let cb = client_bindings.clone();
        let cr = clients.clone();
        let cbr = client_bindings.clone();
        let links = Rc::new(RefCell::new(HashMap::new()));
        let default_sink = Rc::new(RefCell::new(None));
        let bindings = Rc::new(RefCell::new(HashMap::new()));
        let metadata = Rc::new(RefCell::new(HashMap::new()));
        let n = nodes.clone();
        let l = links.clone();
        let d = default_sink.clone();
        let b = bindings.clone();
        let m = metadata.clone();
        let r = registry.clone();
        let nr = nodes.clone();
        let lr = links.clone();
        let br = bindings.clone();
        let mr = metadata.clone();
        let listener = registry
            .add_listener_local()
            .global(move |global| {
                let empty;
                let properties = match global.props {
                    Some(properties) => properties,
                    None => {
                        empty = pw::properties::PropertiesBox::new();
                        empty.as_ref()
                    }
                };
                let id = global.id;
                match global.type_ {
                    pw::types::ObjectType::Client => {
                        c.borrow_mut().insert(
                            id,
                            properties
                                .iter()
                                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                                .collect(),
                        );
                        if let Ok(client) = r.bind::<pw::client::Client, _>(global) {
                            let clients = c.clone();
                            let changed = changed.clone();
                            let listener = client
                                .add_listener_local()
                                .info(move |info| {
                                    if let Some(properties) = info.props() {
                                        clients.borrow_mut().entry(id).or_default().extend(
                                            properties
                                                .iter()
                                                .map(|(k, v)| (k.to_owned(), v.to_owned())),
                                        );
                                        changed.set(changed.get().wrapping_add(1));
                                    }
                                })
                                .register();
                            cb.borrow_mut().insert(
                                id,
                                ClientBinding {
                                    _listener: listener,
                                    _client: client,
                                },
                            );
                        }
                    }
                    pw::types::ObjectType::Node => {
                        let props = properties
                            .iter()
                            .map(|(k, v)| (k.to_owned(), v.to_owned()))
                            .collect();
                        n.borrow_mut().insert(
                            id,
                            Node {
                                properties: props,
                                running: false,
                            },
                        );
                        if let Ok(node) = r.bind::<pw::node::Node, _>(global) {
                            let nodes = n.clone();
                            let changed = changed.clone();
                            let listener = node
                                .add_listener_local()
                                .info(move |info| {
                                    if let Some(node) = nodes.borrow_mut().get_mut(&id) {
                                        node.running =
                                            matches!(info.state(), pw::node::NodeState::Running);
                                        if let Some(props) = info.props() {
                                            node.properties.extend(
                                                props
                                                    .iter()
                                                    .map(|(k, v)| (k.to_owned(), v.to_owned())),
                                            );
                                        }
                                        changed.set(changed.get().wrapping_add(1));
                                    }
                                })
                                .register();
                            b.borrow_mut().insert(
                                id,
                                NodeBinding {
                                    _listener: listener,
                                    _node: node,
                                },
                            );
                        }
                    }
                    pw::types::ObjectType::Link => {
                        if let (Some(output), Some(input)) = (
                            properties
                                .get("link.output.node")
                                .and_then(|v| v.parse().ok()),
                            properties
                                .get("link.input.node")
                                .and_then(|v| v.parse().ok()),
                        ) {
                            l.borrow_mut().insert(id, (output, input));
                        }
                    }
                    pw::types::ObjectType::Metadata
                        if properties.get("metadata.name") == Some("default") =>
                    {
                        if let Ok(metadata) = r.bind::<pw::metadata::Metadata, _>(global) {
                            let default_sink = d.clone();
                            let listener = metadata
                                .add_listener_local()
                                .property(move |_, key, _, value| {
                                    if key == Some("default.audio.sink") {
                                        *default_sink.borrow_mut() = value
                                            .and_then(|v| {
                                                serde_json::from_str::<serde_json::Value>(v).ok()
                                            })
                                            .and_then(|v| {
                                                v.get("name")
                                                    .and_then(|n| n.as_str())
                                                    .map(str::to_owned)
                                            });
                                    }
                                    0
                                })
                                .register();
                            m.borrow_mut().insert(
                                id,
                                MetadataBinding {
                                    _listener: listener,
                                    _metadata: metadata,
                                },
                            );
                        }
                    }
                    _ => {}
                }
                changed.set(changed.get().wrapping_add(1));
            })
            .global_remove(move |id| {
                nr.borrow_mut().remove(&id);
                lr.borrow_mut().remove(&id);
                br.borrow_mut().remove(&id);
                mr.borrow_mut().remove(&id);
                cr.borrow_mut().remove(&id);
                cbr.borrow_mut().remove(&id);
                removed.set(removed.get().wrapping_add(1));
            })
            .register();
        Self {
            revision,
            nodes,
            clients,
            _client_bindings: client_bindings,
            links,
            default_sink,
            _bindings: bindings,
            _metadata: metadata,
            _listener: listener,
        }
    }
}
fn process_property<'a>(
    node: &'a Node,
    clients: &'a HashMap<u32, HashMap<String, String>>,
    key: &str,
) -> Option<&'a str> {
    node.properties.get(key).map(String::as_str).or_else(|| {
        let client_id = node.properties.get("client.id")?.parse::<u32>().ok()?;
        clients.get(&client_id)?.get(key).map(String::as_str)
    })
}
fn roundtrip(
    loop_: &pw::loop_::Loop,
    core: &pw::core::Core,
    error: &Rc<RefCell<Option<String>>>,
) -> Result<()> {
    let done = Rc::new(Cell::new(false));
    let finished = done.clone();
    let pending = core
        .sync(0)
        .map_err(|e| crate::platform_error("synchronizing PipeWire graph", e))?;
    let _listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                finished.set(true);
            }
        })
        .register();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !done.get() {
        if let Some(error) = error.borrow().as_ref() {
            return Err(crate::platform_error("reading PipeWire graph", error));
        }
        if Instant::now() >= deadline {
            return Err(crate::platform_error(
                "reading PipeWire graph",
                "server timed out",
            ));
        }
        loop_.iterate(pw::loop_::Timeout::Finite(TICK));
    }
    Ok(())
}

/// The graph snapshot is also the seam used by synthetic PipeWire tests.
fn mic_users_from_graph(
    nodes: &HashMap<u32, Node>,
    links: &HashMap<u32, (u32, u32)>,
    clients: &HashMap<u32, HashMap<String, String>>,
) -> Vec<MicUser> {
    let mut users = HashMap::new();
    for (id, node) in nodes {
        if !node.running
            || node.properties.get("media.class").map(String::as_str) != Some("Stream/Input/Audio")
            || node
                .properties
                .get("stream.capture.sink")
                .is_some_and(|v| v == "true")
        {
            continue;
        }
        let microphone_link = links.values().any(|(source, target)| {
            target == id
                && nodes.get(source).is_some_and(|n| {
                    n.properties
                        .get("media.class")
                        .is_some_and(|v| v.starts_with("Audio/Source"))
                        && n.properties.get("device.class").map(String::as_str) != Some("monitor")
                        && !n
                            .properties
                            .get("node.name")
                            .is_some_and(|v| v.ends_with(".monitor"))
                })
        });
        if !microphone_link {
            continue;
        }
        // Anonymous capture nodes are not attributable; they must not hide
        // other apps that PipeWire can identify in the same snapshot.
        let Some(pid) = process_property(node, clients, "application.process.id")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|pid| *pid != 0)
        else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        users.entry(pid).or_insert_with(|| MicUser {
            pid,
            bundle_id: process_property(node, clients, "application.id")
                .or_else(|| process_property(node, clients, "application.process.binary"))
                .map(str::to_owned),
            responsible_pid: None,
            responsible_bundle_id: None,
        });
    }
    let mut users: Vec<_> = users.into_values().collect();
    users.sort_by_key(|u| u.pid);
    users
}

#[derive(Default)]
pub struct LinuxMicUsageMonitor;
impl MicUsageMonitor for LinuxMicUsageMonitor {
    fn mic_users(&self) -> Result<Vec<MicUser>> {
        // PipeWire objects remain on this worker/calling thread, not GPUI.
        pw::init();
        let loop_ = pw::main_loop::MainLoopRc::new(None)
            .map_err(|e| crate::platform_error("opening PipeWire loop", e))?;
        let context = pw::context::ContextRc::new(&loop_, None)
            .map_err(|e| crate::platform_error("opening PipeWire context", e))?;
        let core = context
            .connect_rc(None)
            .map_err(|e| crate::platform_error("connecting PipeWire", e))?;
        let error = Rc::new(RefCell::new(None));
        let error_slot = error.clone();
        let _errors = core
            .add_listener_local()
            .error(move |_, _, _, message| *error_slot.borrow_mut() = Some(message.to_owned()))
            .register();
        let registry = core
            .get_registry_rc()
            .map_err(|e| crate::platform_error("opening PipeWire registry", e))?;
        let graph = Graph::new(&registry);
        roundtrip(loop_.loop_(), &core, &error)?;
        roundtrip(loop_.loop_(), &core, &error)?; // node info from bound proxies
        let nodes = graph.nodes.borrow();
        let links = graph.links.borrow();
        let clients = graph.clients.borrow();
        let mut users = mic_users_from_graph(&nodes, &links, &clients);
        attribute_helpers(&mut users, crate::process_tree::proc_info);
        Ok(users)
    }
}

/// Points browser/Electron audio helpers at their application process (the
/// one the accessibility reader can find).
fn attribute_helpers(
    users: &mut [MicUser],
    info: impl Fn(u32) -> Option<crate::process_tree::ProcessInfo> + Copy,
) {
    for user in users {
        if let Some((pid, binary)) = crate::process_tree::responsible_process(user.pid, info) {
            user.responsible_pid = Some(pid);
            user.responsible_bundle_id = binary;
        }
    }
}

/// Playback streams recorded through per-application monitors (node id →
/// monitor target): every `Stream/Output/Audio` node except forwarding
/// outputs ([`forwards_audio`]), BroScribe's own processes (`own`) and the
/// caller's `excluded` pids (sorted). A node without a process identity is
/// recorded, unless the caller excluded processes: then its audio might be
/// one of them.
fn capture_targets(
    nodes: &HashMap<u32, Node>,
    clients: &HashMap<u32, HashMap<String, String>>,
    excluded: &[u32],
    mut own: impl FnMut(u32) -> bool,
) -> Result<HashMap<u32, String>> {
    let mut targets = HashMap::new();
    for (id, node) in nodes {
        if node.properties.get("media.class").map(String::as_str) != Some("Stream/Output/Audio")
            || forwards_audio(node)
        {
            continue;
        }
        let pid = process_property(node, clients, "application.process.id")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|pid| *pid != 0);
        match pid {
            Some(pid) if excluded.binary_search(&pid).is_ok() || own(pid) => continue,
            Some(_) => {}
            None if excluded.is_empty() => {}
            None => {
                return Err(crate::platform_error(
                    "enforcing system-audio exclusions",
                    "PipeWire did not expose a playback node's process identity",
                ));
            }
        }
        targets.insert(
            *id,
            node.properties
                .get("object.serial")
                .cloned()
                .unwrap_or_else(|| id.to_string()),
        );
    }
    Ok(targets)
}

/// Whether a playback stream forwards audio another stream already played:
/// the output side of a loopback, filter-chain, echo-cancel or combine
/// module (paired with its capture side through `node.link-group`) or a
/// virtual node. The original application streams are recorded instead;
/// recording both would double them and let excluded audio back in.
fn forwards_audio(node: &Node) -> bool {
    let set = |key: &str| node.properties.get(key).is_some_and(|v| !v.is_empty());
    let yes = |key: &str| node.properties.get(key).map(String::as_str) == Some("true");
    set("node.link-group") || yes("node.virtual") || yes("stream.is-loopback")
}

/// Key of the sink monitor in the worker's monitor map; per-app monitors
/// are keyed by their target's node id.
const CLOCK_MONITOR: u32 = u32::MAX;

/// Which failed monitors (`failed`, monitor keys) are reaped: per-app
/// monitors whose target node left the graph (`node.dont-reconnect` makes
/// PipeWire destroy the monitor of an ended stream). `Err` names the first
/// failure that ends the capture: the clock monitor, or a per-app monitor
/// whose target is still present.
fn reaped_monitors(
    failed: &[u32],
    target_present: impl Fn(u32) -> bool,
) -> std::result::Result<Vec<u32>, u32> {
    failed
        .iter()
        .map(|&id| {
            if id == CLOCK_MONITOR || target_present(id) {
                Err(id)
            } else {
                Ok(id)
            }
        })
        .collect()
}

#[derive(Default)]
pub struct LinuxSystemAudioCapture {
    excluded: Mutex<Vec<u32>>,
    generation: Arc<AtomicU64>,
}
impl SystemAudioCapture for LinuxSystemAudioCapture {
    fn set_excluded_pids(&self, pids: &[u32]) -> Result<()> {
        let mut next = pids.to_vec();
        next.sort_unstable();
        next.dedup();
        let mut current = self.excluded.lock().unwrap_or_else(|e| e.into_inner());
        if *current != next {
            *current = next;
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        Ok(())
    }
    fn start(&self, sink: MicSink) -> Result<Box<dyn CaptureSession>> {
        let guard = self.excluded.lock().unwrap_or_else(|e| e.into_inner());
        let excluded = guard.clone();
        let generation = self.generation.clone();
        let initial_generation = generation.load(Ordering::Acquire);
        drop(guard);
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let thread_stop = stop.clone();
        let thread_failure = failure.clone();
        let (ready, started) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("broscribe-pipewire-delivery".into())
            .spawn(move || {
                let result = system_worker(
                    sink,
                    &excluded,
                    &generation,
                    initial_generation,
                    &thread_stop,
                    &thread_failure,
                    &ready,
                );
                if let Err(error) = result {
                    fail(&thread_failure, error.to_string());
                    let _ = ready.try_send(Err(error));
                }
            })?;
        match started.recv() {
            Ok(Ok(())) => Ok(Box::new(Session {
                rate: RATE,
                name: "PipeWire system output".into(),
                stop,
                failure,
                thread: Some(thread),
            })),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(e) => {
                let _ = thread.join();
                Err(crate::platform_error("starting PipeWire capture", e))
            }
        }
    }
}
/// Whether playback processes are BroScribe's own ([`is_own_process`]
/// reads several procfs files per process and ancestor), remembered while
/// the process keeps playing. Graph revisions are frequent (every node state
/// change); a process that stops playing is forgotten at the next revision,
/// so a recycled PID is examined afresh.
///
/// [`is_own_process`]: crate::process_tree::is_own_process
struct OwnProcesses<F> {
    probe: F,
    known: HashMap<u32, bool>,
    seen: HashSet<u32>,
}
impl<F: Fn(u32) -> bool> OwnProcesses<F> {
    fn new(probe: F) -> Self {
        Self {
            probe,
            known: HashMap::new(),
            seen: HashSet::new(),
        }
    }
    fn is_own(&mut self, pid: u32) -> bool {
        self.seen.insert(pid);
        let probe = &self.probe;
        *self.known.entry(pid).or_insert_with(|| probe(pid))
    }
    /// Forgets the processes not asked about since the last call.
    fn end_revision(&mut self) {
        let seen = std::mem::take(&mut self.seen);
        self.known.retain(|pid, _| seen.contains(pid));
    }
}

struct CaptureData {
    ring: Arc<TimedRing>,
    format: spa::param::audio::AudioInfoRaw,
    failed: Arc<AtomicBool>,
    negotiated: Arc<AtomicBool>,
}
struct Monitor {
    // Drop listener before stream, and keep both on the PipeWire owner thread.
    _listener: pw::stream::StreamListener<CaptureData>,
    stream: pw::stream::StreamRc,
    ring: Arc<TimedRing>,
    failed: Arc<AtomicBool>,
    negotiated: Arc<AtomicBool>,
}
impl Monitor {
    fn new(
        core: &pw::core::CoreRc,
        target: Option<&str>,
        record: bool,
        clock: bool,
    ) -> Result<Self> {
        let mut props = properties! { "media.type" => "Audio", "media.category" => "Capture", "media.role" => "Production", "application.name" => "BroScribe", "stream.capture.sink" => "true", "node.passive" => "true", "node.dont-reconnect" => "true" };
        if clock {
            props.insert("node.always-process", "true");
        }
        if let Some(target) = target {
            props.insert("target.object", target);
        }
        let stream = pw::stream::StreamRc::new(core.clone(), "BroScribe system capture", props)
            .map_err(|e| crate::platform_error("creating PipeWire monitor stream", e))?;
        // The clock monitor keeps cycle metadata only: no PCM storage.
        let ring = Arc::new(TimedRing::new(if record { CAPACITY } else { 0 }));
        let failed = Arc::new(AtomicBool::new(false));
        let negotiated = Arc::new(AtomicBool::new(false));
        let data = CaptureData {
            ring: ring.clone(),
            format: Default::default(),
            failed: failed.clone(),
            negotiated: negotiated.clone(),
        };
        let listener = stream
            .add_local_listener_with_user_data(data)
            .state_changed(|_, data, old, state| {
                if matches!(state, pw::stream::StreamState::Error(_))
                    || (matches!(state, pw::stream::StreamState::Unconnected)
                        && !matches!(old, pw::stream::StreamState::Unconnected))
                {
                    data.failed.store(true, Ordering::Release);
                }
            })
            .param_changed(|_, data, id, parameter| {
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Some(parameter) = parameter else {
                    return;
                };
                if data.format.parse(parameter).is_err()
                    || data.format.rate() != RATE
                    || data.format.channels() != 1
                    || data.format.format() != spa::param::audio::AudioFormat::F32LE
                {
                    data.failed.store(true, Ordering::Release);
                } else {
                    data.negotiated.store(true, Ordering::Release);
                }
            })
            .process(|stream, data| {
                if !data.negotiated.load(Ordering::Acquire) {
                    return;
                }
                let Ok(time) = stream.time() else {
                    data.failed.store(true, Ordering::Release);
                    return;
                };
                // The cycle's graph-clock position only aligns streams; the
                // mixer places samples by count (audio_ring.rs). `ticks` are
                // rebased per stream; `now` is the driver's cycle time
                // (`clock.nsec`), shared by every stream of the graph.
                let rate = time.rate();
                let Some(ticks) = scaled_ticks(time.ticks(), rate.num, rate.denom) else {
                    data.failed.store(true, Ordering::Release);
                    return;
                };
                // The driver's cycle time places the stream on the session
                // timeline and stamps delivered chunks; a cycle without one
                // cannot be placed.
                let Ok(cycle_ns) = u64::try_from(time.now()) else {
                    data.failed.store(true, Ordering::Release);
                    return;
                };
                if cycle_ns == 0 {
                    data.failed.store(true, Ordering::Release);
                    return;
                }
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(data_buffer) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let offset = data_buffer.chunk().offset() as usize;
                let size = data_buffer.chunk().size() as usize;
                if data_buffer
                    .chunk()
                    .flags()
                    .contains(spa::buffer::ChunkFlags::CORRUPTED)
                    || !matches!(data_buffer.chunk().stride(), 0 | 4)
                {
                    data.failed.store(true, Ordering::Release);
                    return;
                }
                if let Some(bytes) = data_buffer.data() {
                    let end = offset.saturating_add(size);
                    if end > bytes.len() || !size.is_multiple_of(4) {
                        data.failed.store(true, Ordering::Release);
                        return;
                    }
                    let bytes = &bytes[offset..end];
                    data.ring.push(ticks, cycle_ns, bytes.len() / 4, |i| {
                        f32::from_le_bytes([
                            bytes[i * 4],
                            bytes[i * 4 + 1],
                            bytes[i * 4 + 2],
                            bytes[i * 4 + 3],
                        ])
                    });
                }
            })
            .register()
            .map_err(|e| crate::platform_error("registering PipeWire capture callbacks", e))?;
        let mut format = spa::param::audio::AudioInfoRaw::new();
        format.set_format(spa::param::audio::AudioFormat::F32LE);
        format.set_rate(RATE);
        format.set_channels(1);
        let object = spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: format.into(),
        };
        let bytes = spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &spa::pod::Value::Object(object),
        )
        .map_err(|e| crate::platform_error("serializing PipeWire audio format", e))?
        .0
        .into_inner();
        let parameter = spa::pod::Pod::from_bytes(&bytes)
            .ok_or_else(|| Error::Internal("invalid PipeWire audio parameter".into()))?;
        stream
            .connect(
                spa::utils::Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS,
                &mut [parameter],
            )
            .map_err(|e| crate::platform_error("connecting PipeWire monitor", e))?;
        Ok(Self {
            _listener: listener,
            stream,
            ring,
            failed,
            negotiated,
        })
    }
}
fn system_worker(
    mut sink: MicSink,
    excluded: &[u32],
    generation: &AtomicU64,
    initial_generation: u64,
    stop: &AtomicBool,
    failure: &Failure,
    ready: &mpsc::SyncSender<Result<()>>,
) -> Result<()> {
    pw::init();
    let loop_ = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| crate::platform_error("opening PipeWire capture loop", e))?;
    let context = pw::context::ContextRc::new(&loop_, None)
        .map_err(|e| crate::platform_error("opening PipeWire capture context", e))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| crate::platform_error("connecting PipeWire capture", e))?;
    let error = Rc::new(RefCell::new(None));
    let errors = error.clone();
    let _error_listener = core
        .add_listener_local()
        .error(move |_, _, _, message| *errors.borrow_mut() = Some(message.to_owned()))
        .register();
    let registry = core
        .get_registry_rc()
        .map_err(|e| crate::platform_error("opening capture graph", e))?;
    let graph = Graph::new(&registry);
    roundtrip(loop_.loop_(), &core, &error)?;
    roundtrip(loop_.loop_(), &core, &error)?;
    let initial_sink = graph.default_sink.borrow().clone();
    let mut monitors = HashMap::<u32, Monitor>::new();
    // The sink monitor supplies the graph clock; its PCM is never copied out
    // of PipeWire. Per-app monitors, not unmixing an already-mixed sink,
    // leave out BroScribe's own playback and the excluded processes.
    monitors.insert(
        CLOCK_MONITOR,
        Monitor::new(&core, initial_sink.as_deref(), false, true)?,
    );
    let self_pid = std::process::id();
    let mut own = OwnProcesses::new(|pid| {
        crate::process_tree::is_own_process(pid, self_pid, crate::process_tree::proc_info)
    });
    let mut mixer = Mixer::new(ClockAnchor::monotonic()?);
    let mut announced = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut graph_revision = u64::MAX;
    while !stop.load(Ordering::Acquire) {
        if generation.load(Ordering::Acquire) != initial_generation {
            return Err(crate::platform_error(
                "capturing system audio",
                "process exclusions changed; start a fresh capture segment",
            ));
        }
        if let Some(error) = error.borrow().as_ref() {
            return Err(crate::platform_error("capturing PipeWire graph", error));
        }
        if *graph.default_sink.borrow() != initial_sink {
            return Err(crate::platform_error(
                "capturing system audio",
                "default output route changed; start a fresh capture segment",
            ));
        }
        if graph.revision.get() != graph_revision {
            let targets = capture_targets(
                &graph.nodes.borrow(),
                &graph.clients.borrow(),
                excluded,
                |pid| own.is_own(pid),
            )?;
            own.end_revision();
            let removed: Vec<_> = monitors
                .keys()
                .copied()
                .filter(|id| *id != CLOCK_MONITOR && !targets.contains_key(id))
                .collect();
            for id in removed {
                if let Some(monitor) = monitors.remove(&id) {
                    reap(monitor, &mut mixer)?;
                }
            }
            for (id, target) in targets {
                if let std::collections::hash_map::Entry::Vacant(entry) = monitors.entry(id) {
                    entry.insert(Monitor::new(&core, Some(&target), true, false)?);
                }
            }
            graph_revision = graph.revision.get();
        }
        loop_.loop_().iterate(pw::loop_::Timeout::Finite(TICK));
        let failed = |monitors: &HashMap<u32, Monitor>| -> Vec<u32> {
            monitors
                .iter()
                .filter(|(_, m)| m.failed.load(Ordering::Acquire))
                .map(|(id, _)| *id)
                .collect()
        };
        let mut decision = reaped_monitors(&failed(&monitors), |id| {
            graph.nodes.borrow().contains_key(&id)
        });
        if matches!(decision, Err(id) if id != CLOCK_MONITOR) {
            // The target's removal may still be queued behind its monitor's
            // failure: settle the graph once before deciding.
            roundtrip(loop_.loop_(), &core, &error)?;
            decision = reaped_monitors(&failed(&monitors), |id| {
                graph.nodes.borrow().contains_key(&id)
            });
        }
        match decision {
            Ok(ended) => {
                for id in ended {
                    if let Some(monitor) = monitors.remove(&id) {
                        reap(monitor, &mut mixer)?;
                    }
                }
            }
            Err(_) => {
                return Err(crate::platform_error(
                    "capturing system audio",
                    "PipeWire stream failed or output/format was lost",
                ));
            }
        }
        if monitors.values().any(|m| m.ring.dropped() != 0) {
            return Err(crate::platform_error(
                "capturing system audio",
                "delivery overrun; capture stopped rather than losing audio silently",
            ));
        }
        let configured = monitors
            .values()
            .all(|m| m.negotiated.load(Ordering::Acquire));
        if !announced && configured {
            ready
                .send(Ok(()))
                .map_err(|e| crate::platform_error("starting system capture", e))?;
            announced = true;
        }
        if !announced && Instant::now() >= deadline {
            return Err(crate::platform_error(
                "starting system capture",
                "PipeWire format negotiation timed out",
            ));
        }
        deliver_mix(&monitors, &mut mixer, &mut sink, false)?;
    }
    // Disconnect all producers before draining their last samples.
    for monitor in monitors.values() {
        monitor
            .stream
            .disconnect()
            .map_err(|e| crate::platform_error("stopping PipeWire monitor", e))?;
    }
    deliver_mix(&monitors, &mut mixer, &mut sink, true)?;
    if !announced {
        fail(
            failure,
            "system capture stopped before its format was negotiated",
        );
    }
    Ok(())
}
/// Stops a per-app monitor and mixes in its last samples.
fn reap(monitor: Monitor, mixer: &mut Mixer) -> Result<()> {
    let _ = monitor.stream.disconnect();
    monitor.ring.drain(mixer, false)?;
    monitor.ring.ensure_drained()
}
fn deliver_mix(
    monitors: &HashMap<u32, Monitor>,
    mixer: &mut Mixer,
    sink: &mut MicSink,
    final_drain: bool,
) -> Result<()> {
    // The clock monitor is never audible (see `system_worker`).
    if let Some(clock) = monitors.get(&CLOCK_MONITOR) {
        clock.ring.drain(mixer, true)?;
    }
    for (id, monitor) in monitors {
        if *id != CLOCK_MONITOR {
            monitor.ring.drain(mixer, false)?;
            if final_drain {
                monitor.ring.ensure_drained()?;
            }
        }
    }
    mixer.deliver(sink, final_drain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anonymous_capture_stream_does_not_hide_identified_mic_users() {
        let source = Node {
            properties: HashMap::from([("media.class".into(), "Audio/Source".into())]),
            running: true,
        };
        let anonymous = Node {
            properties: HashMap::from([("media.class".into(), "Stream/Input/Audio".into())]),
            running: true,
        };
        let mut identified = anonymous.clone();
        identified
            .properties
            .insert("application.process.id".into(), "424242".into());
        identified
            .properties
            .insert("application.process.binary".into(), "zoom".into());
        let nodes = HashMap::from([(1, source), (2, anonymous), (3, identified)]);
        let links = HashMap::from([(10, (1, 2)), (11, (1, 3))]);
        let users = mic_users_from_graph(&nodes, &links, &HashMap::new());
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].pid, 424242);
        assert_eq!(users[0].bundle_id.as_deref(), Some("zoom"));
    }

    fn playback(pid: Option<&str>, serial: &str) -> Node {
        let mut properties = HashMap::from([
            ("media.class".to_owned(), "Stream/Output/Audio".to_owned()),
            ("object.serial".to_owned(), serial.to_owned()),
        ]);
        if let Some(pid) = pid {
            properties.insert("application.process.id".into(), pid.into());
        }
        Node {
            properties,
            running: true,
        }
    }

    #[test]
    fn own_playback_is_left_out_of_system_capture_by_default() {
        let mut sink = playback(None, "1");
        sink.properties
            .insert("media.class".into(), "Audio/Sink".into());
        let nodes = HashMap::from([
            (10, sink),
            (11, playback(Some("500"), "51")), // BroScribe's ffplay
            (12, playback(Some("600"), "61")), // the meeting app
            (13, playback(Some("700"), "71")), // excluded by the caller
            (14, playback(None, "81")),        // anonymous
        ]);
        let own = |pid| pid == 500;
        let targets = capture_targets(&nodes, &HashMap::new(), &[], own).unwrap();
        assert_eq!(
            targets,
            HashMap::from([
                (12, "61".to_owned()),
                (13, "71".to_owned()),
                (14, "81".to_owned())
            ])
        );
        // With explicit exclusions an anonymous stream cannot be attributed.
        assert!(capture_targets(&nodes, &HashMap::new(), &[700], own).is_err());
        let mut identified = nodes;
        identified.remove(&14);
        let targets = capture_targets(&identified, &HashMap::new(), &[700], own).unwrap();
        assert_eq!(targets, HashMap::from([(12, "61".to_owned())]));
    }

    /// An excluded player routed into a virtual sink reaches the speakers
    /// through the sink's forwarding output (a loopback/filter-chain stream
    /// owned by PipeWire): that output is not recorded, so the player's
    /// audio stays out and an app routed the same way is recorded once.
    #[test]
    fn forwarding_outputs_of_virtual_sinks_are_not_recorded() {
        let mut virtual_sink = playback(Some("50"), "2");
        virtual_sink
            .properties
            .insert("media.class".into(), "Audio/Sink".into());
        virtual_sink
            .properties
            .insert("node.virtual".into(), "true".into());
        let mut forward = playback(Some("50"), "3"); // PipeWire's own process
        forward
            .properties
            .insert("node.link-group".into(), "loopback-50-17".into());
        let mut filter = playback(Some("50"), "4");
        filter
            .properties
            .insert("node.link-group".into(), "filter-chain-50-18".into());
        let mut loopback = playback(Some("50"), "5");
        loopback
            .properties
            .insert("stream.is-loopback".into(), "true".into());
        let nodes = HashMap::from([
            (10, playback(Some("500"), "51")), // excluded player → virtual sink
            (11, playback(Some("600"), "61")), // meeting app → virtual sink
            (20, virtual_sink),
            (21, forward),
            (22, filter),
            (23, loopback),
        ]);
        let own = |pid| pid == 500;
        let targets = capture_targets(&nodes, &HashMap::new(), &[], own).unwrap();
        assert_eq!(targets, HashMap::from([(11, "61".to_owned())]));
    }

    /// A per-app monitor fails when its stream ends (`node.dont-reconnect`):
    /// it is reaped once the target left the graph, while a failed clock
    /// monitor, or one whose target is still present, ends the capture.
    #[test]
    fn monitors_of_ended_streams_are_reaped_not_fatal() {
        let present = |id| id == 12;
        assert_eq!(reaped_monitors(&[], present), Ok(vec![]));
        assert_eq!(reaped_monitors(&[11, 13], present), Ok(vec![11, 13]));
        assert_eq!(reaped_monitors(&[11, 12], present), Err(12));
        assert_eq!(
            reaped_monitors(&[11, CLOCK_MONITOR], present),
            Err(CLOCK_MONITOR)
        );
        // The sink monitor fails the capture even if its id were "gone".
        assert_eq!(
            reaped_monitors(&[CLOCK_MONITOR], |_| false),
            Err(CLOCK_MONITOR)
        );
    }

    #[test]
    fn browser_audio_helpers_are_attributed_to_the_browser() {
        use crate::process_tree::ProcessInfo;
        let info = |pid| {
            let (parent, binary, helper) = match pid {
                1000 => (900, "chrome", false),
                1001 => (1000, "chrome", true),
                3000 => (900, "zoom", false),
                _ => return None,
            };
            Some(ProcessInfo {
                parent,
                binary: Some(binary.into()),
                helper,
            })
        };
        let mut users = vec![
            MicUser {
                pid: 1001,
                bundle_id: Some("chrome".into()),
                ..Default::default()
            },
            MicUser {
                pid: 3000,
                bundle_id: Some("zoom".into()),
                ..Default::default()
            },
        ];
        attribute_helpers(&mut users, info);
        assert_eq!(users[0].responsible_pid, Some(1000));
        assert_eq!(users[0].responsible_bundle_id.as_deref(), Some("chrome"));
        assert_eq!(users[1].responsible_pid, None);
    }

    /// Each graph revision used to re-read procfs for every playing process.
    /// A process is examined once while it keeps playing; once it stops it is
    /// forgotten, so a recycled PID is examined again.
    #[test]
    fn own_process_checks_are_reused_while_the_process_plays() {
        let probes = std::cell::RefCell::new(Vec::new());
        let mut own = OwnProcesses::new(|pid| {
            probes.borrow_mut().push(pid);
            pid == 500
        });
        let nodes = HashMap::from([
            (11, playback(Some("500"), "51")),
            (12, playback(Some("600"), "61")),
        ]);
        for _ in 0..3 {
            let targets =
                capture_targets(&nodes, &HashMap::new(), &[], |pid| own.is_own(pid)).unwrap();
            own.end_revision();
            assert_eq!(targets, HashMap::from([(12, "61".to_owned())]));
        }
        let sorted = |probes: &[u32]| {
            let mut probes = probes.to_vec();
            probes.sort_unstable();
            probes
        };
        assert_eq!(sorted(probes.borrow().as_slice()), [500, 600]);
        let without_500 = HashMap::from([(12, playback(Some("600"), "61"))]);
        capture_targets(&without_500, &HashMap::new(), &[], |pid| own.is_own(pid)).unwrap();
        own.end_revision();
        capture_targets(&nodes, &HashMap::new(), &[], |pid| own.is_own(pid)).unwrap();
        own.end_revision();
        assert_eq!(sorted(probes.borrow().as_slice()), [500, 500, 600]);
    }
}
