//! Private Core Audio process tap (macOS 14.2+).
//!
//! IOProc and property listeners only touch preallocated atomic storage. The
//! worker owns HAL lifetime, exclusion refresh, host-clock conversion and sinks.
//! Route/format loss and a full ring end the segment rather than hiding a gap.

use std::ffi::{CStr, c_void};
use std::mem::{MaybeUninit, size_of};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use broscribe_platform::{CaptureSession, Error, MicChunk, MicSink, Result, SystemAudioCapture};
use objc2::rc::{Retained, autoreleasepool};
use objc2::{AnyThread, sel};
use objc2_core_audio::*;
use objc2_core_audio_types::{
    AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp, AudioTimeStampFlags,
    kAudioFormatFlagIsBigEndian, kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM,
};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_foundation::{NSArray, NSNumber, NSObjectProtocol, NSString, NSUUID};

const RING_SECONDS: usize = 2;
const BLOCK_CAPACITY: usize = 2_048;
const DRAIN_INTERVAL: Duration = Duration::from_millis(10);
const REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const OVERFLOW: u8 = 1;
const FORMAT_CHANGED: u8 = 2;
const ROUTE_CHANGED: u8 = 3;
const CLOCK_LOST: u8 = 4;

// Shared with the microphone edge: both tracks inhabit exactly one Instant
// clock, irrespective of which capture starts first. Bracket the native read
// to remove most of the anchor call's scheduling error.
static HOST_ANCHOR: LazyLock<(u64, Instant)> = LazyLock::new(|| {
    let before = Instant::now();
    let ticks = current_host_ticks();
    let after = Instant::now();
    (ticks, before + after.duration_since(before) / 2)
});

pub(crate) fn current_host_ticks() -> u64 {
    // SAFETY: reads the system's monotonic mach host clock, with no arguments.
    unsafe { AudioGetCurrentHostTime() }
}

pub(crate) fn host_instant(host_ticks: u64) -> Instant {
    let (anchor_ticks, anchor) = *HOST_ANCHOR;
    // SAFETY: converts a duration expressed in the same mach time base.
    let delta = Duration::from_nanos(unsafe {
        AudioConvertHostTimeToNanos(host_ticks.abs_diff(anchor_ticks))
    });
    if host_ticks >= anchor_ticks {
        anchor
            .checked_add(delta)
            .expect("HAL host time exceeds the monotonic clock range")
    } else {
        anchor
            .checked_sub(delta)
            .expect("HAL host time precedes the monotonic clock range")
    }
}

#[derive(Default)]
struct Exclusions {
    pids: Mutex<Vec<u32>>,
    generation: AtomicU64,
}

/// Captures other applications; the UI, daemon, workers and named BroScribe
/// helpers are excluded even if the caller did not supply their PIDs.
#[derive(Default)]
pub struct MacSystemAudioCapture {
    exclusions: Arc<Exclusions>,
}

impl MacSystemAudioCapture {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SystemAudioCapture for MacSystemAudioCapture {
    fn set_excluded_pids(&self, pids: &[u32]) -> Result<()> {
        let mut pids = pids.to_vec();
        pids.push(std::process::id());
        pids.sort_unstable();
        pids.dedup();
        let mut current = self
            .exclusions
            .pids
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *current != pids {
            *current = pids;
            self.exclusions.generation.fetch_add(1, Ordering::Release);
        }
        Ok(())
    }

    fn start(&self, sink: MicSink) -> Result<Box<dyn CaptureSession>> {
        // Initialize once on the control thread, never from a native callback.
        let _ = *HOST_ANCHOR;
        let native = autoreleasepool(|_| NativeTap::open(&self.exclusions))?;
        let sample_rate = native.sample_rate;
        let device_name = native.device_name.clone();
        let ring = native.ring.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let worker = {
            let stop = stop.clone();
            let failure = failure.clone();
            let exclusions = self.exclusions.clone();
            std::thread::Builder::new()
                .name("broscribe-system-audio".into())
                .spawn(move || run_worker(native, exclusions, sink, stop, failure))
                .map_err(|source| Error::Io {
                    context: "starting the system-audio worker".into(),
                    source,
                })?
        };
        Ok(Box::new(MacTapSession {
            worker: Some(worker),
            stop,
            failure,
            ring,
            sample_rate,
            device_name,
        }))
    }
}

struct Block {
    frames: AtomicUsize,
    host_ticks: AtomicU64,
}

/// SPSC sample storage plus a descriptor per IO cycle. Publishing a descriptor
/// publishes its samples and timestamp together; overflow never splits a block.
struct TapRing {
    samples: Box<[AtomicU32]>,
    blocks: Box<[Block]>,
    sample_write: AtomicUsize,
    sample_read: AtomicUsize,
    block_write: AtomicUsize,
    block_read: AtomicUsize,
    failure: AtomicU8,
    refresh: AtomicBool,
    updating: AtomicBool,
}

impl TapRing {
    fn new(sample_rate: u32) -> Self {
        Self {
            samples: (0..sample_rate as usize * RING_SECONDS)
                .map(|_| AtomicU32::new(0))
                .collect(),
            blocks: (0..BLOCK_CAPACITY)
                .map(|_| Block {
                    frames: AtomicUsize::new(0),
                    host_ticks: AtomicU64::new(0),
                })
                .collect(),
            sample_write: AtomicUsize::new(0),
            sample_read: AtomicUsize::new(0),
            block_write: AtomicUsize::new(0),
            block_read: AtomicUsize::new(0),
            failure: AtomicU8::new(0),
            refresh: AtomicBool::new(false),
            updating: AtomicBool::new(false),
        }
    }

    fn fail(&self, reason: u8) {
        let _ = self
            .failure
            .compare_exchange(0, reason, Ordering::Release, Ordering::Relaxed);
    }

    // SAFETY: `data`, unless null (digital silence), holds `frames` readable
    // native-endian mono f32 samples. Only the IOProc calls this producer.
    unsafe fn push(&self, data: *const f32, frames: usize, host_ticks: u64) {
        let sw = self.sample_write.load(Ordering::Relaxed);
        let sr = self.sample_read.load(Ordering::Acquire);
        let bw = self.block_write.load(Ordering::Relaxed);
        let br = self.block_read.load(Ordering::Acquire);
        let free = self.samples.len() - sw.wrapping_sub(sr).min(self.samples.len());
        if frames > free || bw.wrapping_sub(br) >= self.blocks.len() {
            self.fail(OVERFLOW);
            return;
        }
        for i in 0..frames {
            let value = if data.is_null() {
                0.0
            } else {
                // SAFETY: the IOProc validated the native buffer byte count.
                unsafe { data.add(i).read_unaligned() }
            };
            self.samples[sw.wrapping_add(i) % self.samples.len()]
                .store(value.to_bits(), Ordering::Relaxed);
        }
        let block = &self.blocks[bw % self.blocks.len()];
        block.frames.store(frames, Ordering::Relaxed);
        block.host_ticks.store(host_ticks, Ordering::Relaxed);
        self.sample_write
            .store(sw.wrapping_add(frames), Ordering::Relaxed);
        self.block_write
            .store(bw.wrapping_add(1), Ordering::Release);
    }

    fn pop(&self, sample_rate: u32) -> Option<MicChunk> {
        let br = self.block_read.load(Ordering::Relaxed);
        if br == self.block_write.load(Ordering::Acquire) {
            return None;
        }
        let block = &self.blocks[br % self.blocks.len()];
        let frames = block.frames.load(Ordering::Relaxed);
        let host_ticks = block.host_ticks.load(Ordering::Relaxed);
        let sr = self.sample_read.load(Ordering::Relaxed);
        let samples = (0..frames)
            .map(|i| {
                f32::from_bits(
                    self.samples[sr.wrapping_add(i) % self.samples.len()].load(Ordering::Relaxed),
                )
            })
            .collect();
        self.sample_read
            .store(sr.wrapping_add(frames), Ordering::Release);
        self.block_read.store(br.wrapping_add(1), Ordering::Release);
        let mut chunk = MicChunk::new(samples, sample_rate);
        chunk.host_time = Some(host_instant(host_ticks));
        Some(chunk)
    }
}

unsafe extern "C-unwind" fn io_proc(
    _: AudioObjectID,
    _: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    input_time: NonNull<AudioTimeStamp>,
    _: NonNull<AudioBufferList>,
    _: NonNull<AudioTimeStamp>,
    context: *mut c_void,
) -> i32 {
    // SAFETY: a dedicated Arc reference remains alive until all callbacks have
    // been unregistered. Core Audio owns the buffer/timestamp for this call.
    let ring = unsafe { &*context.cast::<TapRing>() };
    // Only the exclusion property write gates IO; a pending process-list scan
    // (`refresh`) never drops audio.
    if ring.failure.load(Ordering::Acquire) != 0 || ring.updating.load(Ordering::Acquire) {
        return 0;
    }
    // SAFETY: the HAL supplies valid, read-only IOProc arguments.
    let input = unsafe { input.as_ref() };
    if input.mNumberBuffers == 0 {
        return 0;
    }
    if input.mNumberBuffers != 1 {
        ring.fail(FORMAT_CHANGED);
        return 0;
    }
    let buffer = &input.mBuffers[0];
    if buffer.mNumberChannels != 1 || buffer.mDataByteSize % size_of::<f32>() as u32 != 0 {
        ring.fail(FORMAT_CHANGED);
        return 0;
    }
    let frames = buffer.mDataByteSize as usize / size_of::<f32>();
    if frames == 0 {
        return 0;
    }
    // SAFETY: the input timestamp describes the first captured frame, not the
    // callback's scheduling time (inNow).
    let timestamp = unsafe { input_time.as_ref() };
    if !timestamp
        .mFlags
        .contains(AudioTimeStampFlags::HostTimeValid)
    {
        ring.fail(CLOCK_LOST);
        return 0;
    }
    // SAFETY: mono f32 format was checked before IO started; byte size checked
    // above. A null mData with a positive byte size is HAL digital silence.
    unsafe { ring.push(buffer.mData.cast::<f32>(), frames, timestamp.mHostTime) };
    0
}

#[allow(non_upper_case_globals)] // Apple's property selectors retain their SDK names.
unsafe extern "C-unwind" fn property_changed(
    _: AudioObjectID,
    count: u32,
    addresses: NonNull<AudioObjectPropertyAddress>,
    context: *mut c_void,
) -> i32 {
    // SAFETY: the HAL provides `count` addresses; the retained callback context
    // outlives listener removal. No allocation, lock or HAL query here.
    let ring = unsafe { &*context.cast::<TapRing>() };
    let addresses = unsafe { std::slice::from_raw_parts(addresses.as_ptr(), count as usize) };
    for address in addresses {
        if address.mSelector == kAudioHardwarePropertyProcessObjectList {
            // Schedule an ungated scan; only an actual property update gates IO.
            ring.refresh.store(true, Ordering::Release);
        } else if matches!(
            address.mSelector,
            kAudioHardwarePropertyDefaultOutputDevice
                | kAudioDevicePropertyNominalSampleRate
                | kAudioDevicePropertyDeviceIsAlive
                | kAudioTapPropertyFormat
        ) {
            ring.fail(ROUTE_CHANGED);
        }
    }
    0
}

#[derive(PartialEq, Eq)]
struct ExcludedProcesses {
    objects: Vec<AudioObjectID>,
    bundles: Vec<String>,
}

fn own_bundle(bundle: &str) -> bool {
    const PREFIX: &str = "org.broscribe.";
    bundle
        .get(..PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(PREFIX))
}

fn own_executable(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    let mut path = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: `path` is a writable buffer of the advertised size.
    let len = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
    if len <= 0 {
        return false;
    }
    let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
    let Ok(path) = std::str::from_utf8(&path[..end]) else {
        return false;
    };
    let Some(name) = Path::new(path).file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name == "broscribe" || name == "broscribed" || name.starts_with("broscribe-")
}

fn excluded_processes(exclusions: &Exclusions) -> Result<ExcludedProcesses> {
    let explicit = exclusions
        .pids
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let objects = read_array(system_object(), kAudioHardwarePropertyProcessObjectList)?;
    let mut excluded = ExcludedProcesses {
        objects: Vec::new(),
        bundles: Vec::new(),
    };
    for object in objects {
        // A client may disappear between listing it and querying its PID.
        let Ok(pid) = read_value::<i32>(object, kAudioProcessPropertyPID) else {
            continue;
        };
        let Ok(pid) = u32::try_from(pid) else {
            continue;
        };
        let bundle = read_string(object, kAudioProcessPropertyBundleID);
        if pid == std::process::id()
            || explicit.binary_search(&pid).is_ok()
            || bundle.as_deref().is_some_and(own_bundle)
            || own_executable(pid)
        {
            excluded.objects.push(object);
            // Do not expand a caller-supplied exclusion to unrelated processes
            // sharing its bundle; automatic bundle exclusion is only our app.
            if let Some(bundle) = bundle.filter(|bundle| own_bundle(bundle)) {
                excluded.bundles.push(bundle);
            }
        }
    }
    excluded.objects.sort_unstable();
    excluded.bundles.sort_unstable();
    excluded.bundles.dedup();
    Ok(excluded)
}

fn tap_description(
    excluded: &ExcludedProcesses,
    uuid: Option<&str>,
) -> Result<Retained<CATapDescription>> {
    let objects: Vec<_> = excluded
        .objects
        .iter()
        .copied()
        .map(NSNumber::new_u32)
        .collect();
    let objects = NSArray::from_retained_slice(&objects);
    // SAFETY: NSNumber values are AudioObjectIDs, not PIDs. A global mono
    // mixdown leaves playback untouched and yields the trait's mono track.
    let description = unsafe {
        CATapDescription::initMonoGlobalTapButExcludeProcesses(CATapDescription::alloc(), &objects)
    };
    unsafe {
        description.setName(&NSString::from_str("BroScribe system audio"));
        description.setPrivate(true);
        description.setMuteBehavior(CATapMuteBehavior::Unmuted);
    }
    if let Some(uuid) = uuid {
        let uuid = NSUUID::from_string(&NSString::from_str(uuid))
            .ok_or_else(|| Error::Internal("invalid Core Audio tap UUID".into()))?;
        // SAFETY: preserve the existing tap identity during description updates.
        unsafe { description.setUUID(&uuid) };
    }
    // Bundle matching closes the start/relaunch window on OS versions offering
    // it. Older versions still get process-list notifications plus polling.
    if description.respondsToSelector(sel!(setBundleIDs:)) {
        let bundles: Vec<_> = excluded
            .bundles
            .iter()
            .map(|s| NSString::from_str(s))
            .collect();
        let bundles = NSArray::from_retained_slice(&bundles);
        // SAFETY: runtime selector availability checked above.
        unsafe { description.setBundleIDs(&bundles) };
    }
    Ok(description)
}

struct NativeTap {
    tap: AudioObjectID,
    device: AudioObjectID,
    io_proc: AudioDeviceIOProcID,
    started: bool,
    listeners: Vec<(AudioObjectID, AudioObjectPropertyAddress)>,
    // One strong reference belongs to HAL callbacks; release only after every
    // registration is removed. Store its address as an integer so this owner
    // can move to the worker without an unsafe Send implementation.
    callback_ref: usize,
    ring: Arc<TapRing>,
    excluded: ExcludedProcesses,
    generation: u64,
    uuid: String,
    sample_rate: u32,
    format: AudioStreamBasicDescription,
    output: AudioObjectID,
    device_name: String,
    closed: bool,
}

impl NativeTap {
    fn open(exclusions: &Exclusions) -> Result<Self> {
        let generation = exclusions.generation.load(Ordering::Acquire);
        let excluded = excluded_processes(exclusions)?;
        let description = tap_description(&excluded, None)?;
        let mut tap = 0;
        // SAFETY: description and out parameter are valid for the synchronous call.
        check(
            unsafe { AudioHardwareCreateProcessTap(Some(&description), &mut tap) },
            "creating system-audio process tap (macOS 14.2+ and System Audio Recording permission required)",
        )?;
        let format_result = read_value::<AudioStreamBasicDescription>(tap, kAudioTapPropertyFormat)
            .and_then(|format| capture_rate(&format).map(|rate| (format, rate)));
        let (format, sample_rate) = match format_result {
            Ok(format) => format,
            Err(error) => {
                // SAFETY: no aggregate, callback or listener references this tap yet.
                let _ = unsafe { AudioHardwareDestroyProcessTap(tap) };
                return Err(error);
            }
        };
        // From here on the owner unwinds every partially-created HAL resource.
        let mut native = Self {
            tap,
            device: 0,
            io_proc: None,
            started: false,
            listeners: Vec::new(),
            callback_ref: 0,
            ring: Arc::new(TapRing::new(sample_rate)),
            excluded,
            generation,
            uuid: unsafe { description.UUID().UUIDString() }.to_string(),
            sample_rate,
            format,
            output: 0,
            device_name: String::new(),
            closed: false,
        };
        native.output = read_value(system_object(), kAudioHardwarePropertyDefaultOutputDevice)?;
        if native.output == 0 {
            return Err(Error::Unsupported("no default audio output device".into()));
        }
        native.device_name = read_string(native.output, kAudioObjectPropertyName)
            .map(|name| format!("System audio — {name}"))
            .unwrap_or_else(|| "System audio".into());
        let description = aggregate_description(&native.uuid);
        // SAFETY: dictionary contains the documented CF keys/value types.
        check(
            unsafe {
                AudioHardwareCreateAggregateDevice(
                    description.as_opaque(),
                    NonNull::from(&mut native.device),
                )
            },
            "creating private system-audio aggregate",
        )?;
        let aggregate_rate: f64 = read_value(native.device, kAudioDevicePropertyNominalSampleRate)?;
        if aggregate_rate != f64::from(native.sample_rate) {
            let rate = f64::from(native.sample_rate);
            let address = address(kAudioDevicePropertyNominalSampleRate);
            // SAFETY: set the new, private aggregate's clock to its tap format;
            // no public/default device is reconfigured.
            check(
                unsafe {
                    AudioObjectSetPropertyData(
                        native.device,
                        NonNull::from(&address),
                        0,
                        std::ptr::null(),
                        size_of::<f64>() as u32,
                        NonNull::from(&rate).cast(),
                    )
                },
                "matching private aggregate to the tap sample rate",
            )?;
        }
        native.callback_ref = Arc::into_raw(native.ring.clone()) as usize;
        // SAFETY: callback context has its own retained Arc, and the IOProc
        // signature/mono format are checked before starting it.
        check(
            unsafe {
                AudioDeviceCreateIOProcID(
                    native.device,
                    Some(io_proc),
                    native.context(),
                    NonNull::from(&mut native.io_proc),
                )
            },
            "registering system-audio IOProc",
        )?;
        for (object, selector) in [
            (system_object(), kAudioHardwarePropertyDefaultOutputDevice),
            (system_object(), kAudioHardwarePropertyProcessObjectList),
            (native.output, kAudioDevicePropertyNominalSampleRate),
            (native.output, kAudioDevicePropertyDeviceIsAlive),
            (native.device, kAudioDevicePropertyNominalSampleRate),
            (native.device, kAudioDevicePropertyDeviceIsAlive),
            (native.tap, kAudioTapPropertyFormat),
        ] {
            let address = address(selector);
            // SAFETY: context remains retained through listener removal.
            check(
                unsafe {
                    AudioObjectAddPropertyListener(
                        object,
                        NonNull::from(&address),
                        Some(property_changed),
                        native.context(),
                    )
                },
                "observing system-audio route",
            )?;
            native.listeners.push((object, address));
        }
        native.poll_route()?;
        native.refresh_exclusions(exclusions)?;
        check(
            unsafe { AudioDeviceStart(native.device, native.io_proc) },
            "starting system-audio tap",
        )?;
        native.started = true;
        Ok(native)
    }

    fn context(&self) -> *mut c_void {
        self.callback_ref as *mut c_void
    }

    fn refresh_exclusions(&mut self, exclusions: &Exclusions) -> Result<()> {
        self.ring.refresh.store(false, Ordering::Release);
        let result = (|| {
            let generation = exclusions.generation.load(Ordering::Acquire);
            let excluded = excluded_processes(exclusions)?;
            if excluded != self.excluded {
                let description = tap_description(&excluded, Some(&self.uuid))?;
                let ptr: *const CATapDescription = &*description;
                let address = address(kAudioTapPropertyDescription);
                self.ring.updating.store(true, Ordering::Release);
                // SAFETY: the property takes one CATapDescription object
                // pointer; HAL copies it synchronously, retaining its identity.
                check(
                    unsafe {
                        AudioObjectSetPropertyData(
                            self.tap,
                            NonNull::from(&address),
                            0,
                            std::ptr::null(),
                            size_of::<*const CATapDescription>() as u32,
                            NonNull::from(&ptr).cast(),
                        )
                    },
                    "refreshing own-process tap exclusions",
                )?;
                self.excluded = excluded;
            }
            self.generation = generation;
            Ok(())
        })();
        if result.is_ok() {
            self.ring.updating.store(false, Ordering::Release);
        }
        // On failure, keep callbacks gated until the worker closes this segment.
        result
    }

    fn poll_route(&self) -> Result<()> {
        let output: AudioObjectID =
            read_value(system_object(), kAudioHardwarePropertyDefaultOutputDevice)?;
        let format: AudioStreamBasicDescription = read_value(self.tap, kAudioTapPropertyFormat)?;
        let rate: f64 = read_value(self.device, kAudioDevicePropertyNominalSampleRate)?;
        let alive: u32 = read_value(self.device, kAudioDevicePropertyDeviceIsAlive)?;
        let output_alive: u32 = read_value(self.output, kAudioDevicePropertyDeviceIsAlive)?;
        if output != self.output
            || format != self.format
            || rate != f64::from(self.sample_rate)
            || alive == 0
            || output_alive == 0
        {
            return Err(Error::Internal(
                "system-audio route or format changed; restart the capture segment".into(),
            ));
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let mut first_error = None;
        let mut callbacks_removed = true;
        for (object, address) in self.listeners.drain(..) {
            let status = unsafe {
                AudioObjectRemovePropertyListener(
                    object,
                    NonNull::from(&address),
                    Some(property_changed),
                    self.callback_ref as *mut c_void,
                )
            };
            callbacks_removed &= status == 0;
            remember_error(&mut first_error, status, "removing system-audio listener");
        }
        if self.started {
            let status = unsafe { AudioDeviceStop(self.device, self.io_proc) };
            remember_error(&mut first_error, status, "stopping system-audio IO");
        }
        if self.io_proc.is_some() {
            let status = unsafe { AudioDeviceDestroyIOProcID(self.device, self.io_proc) };
            callbacks_removed &= status == 0;
            remember_error(
                &mut first_error,
                status,
                "unregistering system-audio IOProc",
            );
        }
        if self.device != 0 {
            remember_error(
                &mut first_error,
                unsafe { AudioHardwareDestroyAggregateDevice(self.device) },
                "destroying private system-audio aggregate",
            );
        }
        if self.tap != 0 {
            remember_error(
                &mut first_error,
                unsafe { AudioHardwareDestroyProcessTap(self.tap) },
                "destroying system-audio tap",
            );
        }
        if self.callback_ref != 0 && callbacks_removed {
            // SAFETY: precisely the strong reference handed to HAL, now with no
            // remaining callback registrations. If unregistering failed, retain
            // that bounded allocation rather than risk a dangling HAL context.
            unsafe { drop(Arc::from_raw(self.callback_ref as *const TapRing)) };
        }
        self.callback_ref = 0;
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for NativeTap {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

fn aggregate_description(uuid: &str) -> CFRetained<CFDictionary<CFString, CFType>> {
    let uid = CFString::from_str(&format!("org.broscribe.capture.{uuid}"));
    let name = CFString::from_str("BroScribe private system audio");
    let tap_uid = CFString::from_str(uuid);
    let one = CFNumber::new_i32(1);
    let enabled = CFBoolean::new(true);
    let sub_keys = [
        cf_key(kAudioSubTapUIDKey),
        cf_key(kAudioSubTapDriftCompensationKey),
    ];
    let sub_tap = CFDictionary::<CFString, CFType>::from_slices(
        &[&sub_keys[0], &sub_keys[1]],
        &[tap_uid.as_ref(), one.as_ref()],
    );
    let taps = CFArray::from_objects(&[&*sub_tap]);
    let keys = [
        cf_key(kAudioAggregateDeviceUIDKey),
        cf_key(kAudioAggregateDeviceNameKey),
        cf_key(kAudioAggregateDeviceIsPrivateKey),
        cf_key(kAudioAggregateDeviceTapAutoStartKey),
        cf_key(kAudioAggregateDeviceTapListKey),
    ];
    CFDictionary::from_slices(
        &[&keys[0], &keys[1], &keys[2], &keys[3], &keys[4]],
        &[
            uid.as_ref(),
            name.as_ref(),
            enabled.as_ref(),
            enabled.as_ref(),
            taps.as_ref(),
        ],
    )
}

fn cf_key(key: &CStr) -> CFRetained<CFString> {
    // All Core Audio's dictionary keys are static ASCII constants.
    CFString::from_str(key.to_str().expect("Core Audio dictionary key is ASCII"))
}

fn capture_rate(format: &AudioStreamBasicDescription) -> Result<u32> {
    if format.mFormatID != kAudioFormatLinearPCM
        || format.mFormatFlags & kAudioFormatFlagIsFloat == 0
        || format.mFormatFlags & kAudioFormatFlagIsPacked == 0
        || format.mFormatFlags & kAudioFormatFlagIsBigEndian != 0
        || format.mChannelsPerFrame != 1
        || format.mBitsPerChannel != 32
        || format.mBytesPerFrame != 4
        || format.mFramesPerPacket != 1
        || !format.mSampleRate.is_finite()
        || format.mSampleRate < 1.0
        || format.mSampleRate > 768_000.0
        || format.mSampleRate.fract() != 0.0
    {
        return Err(Error::Unsupported(format!(
            "unsupported mono system-audio tap format: {format:?}"
        )));
    }
    Ok(format.mSampleRate as u32)
}

fn run_worker(
    mut native: NativeTap,
    exclusions: Arc<Exclusions>,
    mut sink: MicSink,
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
) -> Result<()> {
    if let Err(error) = broscribe_platform::ProcessPriority::set_current_thread(
        &crate::priority::MacProcessPriority,
        broscribe_platform::PriorityClass::Capture,
    ) {
        tracing::warn!(%error, "system-audio worker priority unavailable");
    }
    let mut next_refresh = Instant::now();
    loop {
        if stop.load(Ordering::Acquire) || native.ring.failure.load(Ordering::Acquire) != 0 {
            break;
        }
        let now = Instant::now();
        if now >= next_refresh
            || native.ring.refresh.load(Ordering::Acquire)
            || exclusions.generation.load(Ordering::Acquire) != native.generation
        {
            let result = autoreleasepool(|_| {
                native.poll_route()?;
                native.refresh_exclusions(&exclusions)
            });
            if let Err(error) = result {
                *failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.to_string());
                native.ring.fail(ROUTE_CHANGED);
                break;
            }
            next_refresh = now + REFRESH_INTERVAL;
        }
        // Bound each drain even if the sink takes time: check stop/failure and
        // refresh the exclusion list between batches, not just after emptying.
        let ready = native
            .ring
            .block_write
            .load(Ordering::Acquire)
            .wrapping_sub(native.ring.block_read.load(Ordering::Relaxed));
        for _ in 0..ready {
            if let Some(chunk) = native.ring.pop(native.sample_rate) {
                sink(chunk);
            }
        }
        std::thread::sleep(DRAIN_INTERVAL);
    }
    // Halt callbacks before the final drain, so stop returns only after the
    // final successfully published block has reached the durable consumer.
    let result = native.close();
    while let Some(chunk) = native.ring.pop(native.sample_rate) {
        sink(chunk);
    }
    if let Err(error) = &result {
        let mut current = failure.lock().unwrap_or_else(|e| e.into_inner());
        if current.is_none() {
            *current = Some(error.to_string());
        }
    }
    result
}

struct MacTapSession {
    worker: Option<JoinHandle<Result<()>>>,
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<String>>>,
    ring: Arc<TapRing>,
    sample_rate: u32,
    device_name: String,
}

impl MacTapSession {
    fn finish(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| Error::Internal("system-audio worker panicked".into()))??;
        }
        Ok(())
    }
}

impl CaptureSession for MacTapSession {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn device_name(&self) -> Option<String> {
        Some(self.device_name.clone())
    }

    fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .or_else(|| {
                let reason = match self.ring.failure.load(Ordering::Acquire) {
                    OVERFLOW => "system-audio ring overflow; restart the capture segment",
                    FORMAT_CHANGED => {
                        "system-audio callback format changed; restart the capture segment"
                    }
                    ROUTE_CHANGED => {
                        "system-audio route/rate/tap changed; restart the capture segment"
                    }
                    CLOCK_LOST => {
                        "system-audio input host timestamp lost; restart the capture segment"
                    }
                    _ => return None,
                };
                Some(reason.into())
            })
    }

    fn stop(mut self: Box<Self>) -> Result<()> {
        self.finish()
    }
}

impl Drop for MacTapSession {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn system_object() -> AudioObjectID {
    kAudioObjectSystemObject as AudioObjectID
}

fn address(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn read_value<T: Copy>(object: AudioObjectID, selector: AudioObjectPropertySelector) -> Result<T> {
    let address = address(selector);
    let mut value = MaybeUninit::<T>::uninit();
    let mut size = size_of::<T>() as u32;
    // SAFETY: the caller chooses the property's documented fixed-size C type;
    // the supplied storage is exactly `size` bytes and checked before reading.
    check(
        unsafe {
            AudioObjectGetPropertyData(
                object,
                NonNull::from(&address),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::from(&mut value).cast(),
            )
        },
        "reading system-audio property",
    )?;
    if size as usize != size_of::<T>() {
        return Err(Error::Internal(
            "unexpected system-audio property size".into(),
        ));
    }
    // SAFETY: successful HAL read initialized precisely one T.
    Ok(unsafe { value.assume_init() })
}

fn read_array(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
) -> Result<Vec<AudioObjectID>> {
    let address = address(selector);
    let mut size = 0;
    check(
        unsafe {
            AudioObjectGetPropertyDataSize(
                object,
                NonNull::from(&address),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
            )
        },
        "listing Core Audio process objects (macOS 14.2+ required)",
    )?;
    if !(size as usize).is_multiple_of(size_of::<AudioObjectID>()) {
        return Err(Error::Internal(
            "invalid Core Audio process-list size".into(),
        ));
    }
    let mut objects = vec![0; size as usize / size_of::<AudioObjectID>()];
    if !objects.is_empty() {
        check(
            unsafe {
                AudioObjectGetPropertyData(
                    object,
                    NonNull::from(&address),
                    0,
                    std::ptr::null(),
                    NonNull::from(&mut size),
                    NonNull::new(objects.as_mut_ptr().cast()).expect("nonempty process list"),
                )
            },
            "reading Core Audio process objects",
        )?;
        objects.truncate(size as usize / size_of::<AudioObjectID>());
    }
    Ok(objects)
}

fn read_string(object: AudioObjectID, selector: AudioObjectPropertySelector) -> Option<String> {
    let raw: usize = read_value(object, selector).ok()?;
    let ptr = NonNull::new(raw as *mut CFString)?;
    // SAFETY: Core Audio's CFString properties transfer a +1 reference.
    let string = unsafe { CFRetained::from_raw(ptr) }.to_string();
    (!string.is_empty()).then_some(string)
}

fn check(status: i32, context: &str) -> Result<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(Error::Internal(format!("{context}: OSStatus {status}")))
    }
}

fn remember_error(first: &mut Option<Error>, status: i32, context: &str) {
    if first.is_none() {
        *first = check(status, context).err();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::Command;

    /// Feeds one IO cycle of `frames` constant samples through the real IOProc.
    fn deliver(ring: &TapRing, frames: usize, host_ticks: u64) {
        let samples = vec![0.25f32; frames];
        // SAFETY: plain C structs; every field the IOProc reads is set below.
        let mut list: AudioBufferList = unsafe { std::mem::zeroed() };
        list.mNumberBuffers = 1;
        list.mBuffers[0].mNumberChannels = 1;
        list.mBuffers[0].mDataByteSize = (frames * size_of::<f32>()) as u32;
        list.mBuffers[0].mData = samples.as_ptr().cast_mut().cast();
        let mut time: AudioTimeStamp = unsafe { std::mem::zeroed() };
        time.mHostTime = host_ticks;
        time.mFlags = AudioTimeStampFlags::HostTimeValid;
        let mut out: AudioBufferList = unsafe { std::mem::zeroed() };
        unsafe {
            io_proc(
                0,
                NonNull::from(&time),
                NonNull::from(&list),
                NonNull::from(&time),
                NonNull::from(&mut out),
                NonNull::from(&time),
                (ring as *const TapRing).cast_mut().cast(),
            );
        }
    }

    #[test]
    fn process_list_listener_requests_scan_without_gating_audio() {
        let ring = TapRing::new(48_000);
        let mut property = address(kAudioHardwarePropertyProcessObjectList);
        unsafe {
            property_changed(
                0,
                1,
                NonNull::from(&mut property),
                (&ring as *const TapRing).cast_mut().cast(),
            );
        }
        assert!(ring.refresh.load(Ordering::Acquire));
        assert!(!ring.updating.load(Ordering::Acquire));
        // A pending scan must not drop the IO cycle.
        deliver(&ring, 480, 1_000);
        let chunk = ring
            .pop(48_000)
            .expect("audio kept while a scan is pending");
        assert_eq!(chunk.samples_mono_f32.len(), 480);
        assert_eq!(ring.failure.load(Ordering::Acquire), 0);
        // Only the exclusion property write gates IO.
        ring.updating.store(true, Ordering::Release);
        deliver(&ring, 480, 2_000);
        assert!(ring.pop(48_000).is_none());
        ring.updating.store(false, Ordering::Release);
        deliver(&ring, 480, 3_000);
        assert!(ring.pop(48_000).is_some());
    }
    #[test]
    #[ignore = "hardware/TCC: run only on an otherwise silent system; plays a generated 3 s tone, never meeting audio"]
    fn captures_three_seconds_of_synthetic_system_audio() {
        // afplay is deliberately an included, non-BroScribe process. All audio
        // must be synthetic for this opt-in hardware test; nothing is persisted
        // outside its automatically removed temporary directory.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synthetic.wav");
        let rate = 48_000u32;
        let frames = rate * 3;
        let mut wav = std::fs::File::create(&path).unwrap();
        wav.write_all(b"RIFF").unwrap();
        wav.write_all(&(36 + frames * 2).to_le_bytes()).unwrap();
        wav.write_all(b"WAVEfmt ").unwrap();
        wav.write_all(&16u32.to_le_bytes()).unwrap();
        wav.write_all(&1u16.to_le_bytes()).unwrap();
        wav.write_all(&1u16.to_le_bytes()).unwrap();
        wav.write_all(&rate.to_le_bytes()).unwrap();
        wav.write_all(&(rate * 2).to_le_bytes()).unwrap();
        wav.write_all(&2u16.to_le_bytes()).unwrap();
        wav.write_all(&16u16.to_le_bytes()).unwrap();
        wav.write_all(b"data").unwrap();
        wav.write_all(&(frames * 2).to_le_bytes()).unwrap();
        for frame in 0..frames {
            let sample = ((f64::from(frame) * 440.0 * std::f64::consts::TAU / f64::from(rate))
                .sin()
                * 4_000.0) as i16;
            wav.write_all(&sample.to_le_bytes()).unwrap();
        }
        drop(wav);
        let (tx, rx) = std::sync::mpsc::channel();
        let capture = MacSystemAudioCapture::new();
        capture.set_excluded_pids(&[std::process::id()]).unwrap();
        let session = capture
            .start(Box::new(move |chunk| {
                let _ = tx.send(chunk);
            }))
            .unwrap();
        let sample_rate = session.sample_rate();
        let mut player = Command::new("/usr/bin/afplay").arg(&path).spawn().unwrap();
        assert!(player.wait().unwrap().success());
        assert!(session.failure().is_none());
        session.stop().unwrap();
        let chunks: Vec<_> = rx.into_iter().collect();
        assert!(!chunks.is_empty());
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.sample_rate == sample_rate && chunk.host_time.is_some())
        );
        assert!(
            chunks
                .windows(2)
                .all(|pair| pair[0].host_time <= pair[1].host_time)
        );
        let total: usize = chunks
            .iter()
            .map(|chunk| chunk.samples_mono_f32.len())
            .sum();
        assert!(
            total >= sample_rate as usize * 2,
            "fewer than 2 s captured from the 3 s signal"
        );
        assert!(
            chunks.iter().any(|chunk| chunk.level_peak > 0.001),
            "generated tone was not captured"
        );
    }
}
