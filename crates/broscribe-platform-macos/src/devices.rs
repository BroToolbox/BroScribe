//! Core Audio device UIDs, routing facts and current stream formats. This
//! enumerates hardware properties only; it never opens a recording stream.

use std::ffi::c_void;
use std::ptr::NonNull;

use broscribe_platform::devices::collect_devices;
use broscribe_platform::{DeviceDetails, DeviceInfo, DeviceTransport, Error, Result};
use objc2_core_audio::*;
use objc2_core_audio_types::{AudioStreamBasicDescription, AudioValueRange, AudioValueTranslation};
use objc2_core_foundation::{CFRetained, CFString};

#[derive(Default)]
pub struct MacDeviceInfo;

type NativeResult<T> = std::result::Result<T, i32>;

impl DeviceInfo for MacDeviceInfo {
    fn devices(&self) -> Result<Vec<DeviceDetails>> {
        let system = kAudioObjectSystemObject as AudioObjectID;
        let ids = array::<u32>(
            system,
            kAudioHardwarePropertyDevices,
            kAudioObjectPropertyScopeGlobal,
        )
        .map_err(native_error)?;
        let input = default_device(kAudioHardwarePropertyDefaultInputDevice);
        let output = default_device(kAudioHardwarePropertyDefaultOutputDevice);
        collect(&ids, |id| details(id, input, output)).map_err(native_error)
    }

    fn permission_status(&self, permission: &str) -> Result<String> {
        match permission {
            "microphone" => {
                let media = objc2_foundation::NSString::from_str("soun");
                let status: isize = unsafe {
                    objc2::msg_send![objc2::class!(AVCaptureDevice), authorizationStatusForMediaType: &*media]
                };
                Ok(match status {
                    0 => "not_determined",
                    3 => "granted",
                    _ => "denied",
                }
                .into())
            }
            "system_audio" => Ok(audio_capture_tcc()
                .map_or("not_determined", |tcc| tcc.status())
                .into()),
            _ => Err(Error::Unsupported("unknown capture permission".into())),
        }
    }

    fn request_permission(&self, permission: &str) -> Result<String> {
        if permission == "microphone" {
            let media = objc2_foundation::NSString::from_str("soun");
            let (send, receive) = std::sync::mpsc::sync_channel(1);
            let completion = block2::RcBlock::new(move |granted: objc2::runtime::Bool| {
                let _ = send.send(granted.as_bool());
            });
            unsafe {
                let _: () = objc2::msg_send![objc2::class!(AVCaptureDevice),
                    requestAccessForMediaType: &*media, completionHandler: &*completion];
            }
            receive
                .recv_timeout(std::time::Duration::from_secs(120))
                .map(|granted| if granted { "granted" } else { "denied" }.into())
                .map_err(|_| Error::Internal("microphone permission request timed out".into()))
        } else if permission == "system_audio" {
            if let Some(tcc) = audio_capture_tcc() {
                let status = tcc.status();
                if status != "not_determined" {
                    return Ok(status.into());
                }
                return tcc.request();
            }
            // Without the TCC query, starting a tap still raises the prompt,
            // but the answer stays unknown: never report a guessed grant.
            use broscribe_platform::SystemAudioCapture;
            let session = crate::tap::MacSystemAudioCapture::default().start(Box::new(|_| {}))?;
            session.stop()?;
            Ok("not_determined".into())
        } else {
            Err(Error::Unsupported("unknown capture permission".into()))
        }
    }
}

/// The TCC service process taps are authorized under ("System Audio
/// Recording Only" in Privacy & Security).
const AUDIO_CAPTURE_SERVICE: &str = "kTCCServiceAudioCapture";
const TCC_FRAMEWORK: &std::ffi::CStr =
    c"/System/Library/PrivateFrameworks/TCC.framework/Versions/A/TCC";

type Preflight = unsafe extern "C" fn(*const CFString, *const c_void) -> i32;
type Request = unsafe extern "C" fn(
    *const CFString,
    *const c_void,
    *const block2::Block<dyn Fn(objc2::runtime::Bool)>,
);

/// macOS exposes no public query for process-tap authorization; TCC's
/// preflight/request pair answers it without guessing. Resolved at run time so
/// a missing symbol degrades to "not_determined" (*verify* on each new macOS).
struct AudioCaptureTcc {
    preflight: Preflight,
    request: Request,
}

static TCC: std::sync::LazyLock<Option<AudioCaptureTcc>> = std::sync::LazyLock::new(|| {
    // SAFETY: dlopen/dlsym with NUL-terminated names; the handle is never
    // closed, so resolved function pointers stay valid for the process.
    unsafe {
        let handle = libc::dlopen(TCC_FRAMEWORK.as_ptr(), libc::RTLD_LAZY);
        if handle.is_null() {
            return None;
        }
        let preflight = libc::dlsym(handle, c"TCCAccessPreflight".as_ptr());
        let request = libc::dlsym(handle, c"TCCAccessRequest".as_ptr());
        if preflight.is_null() || request.is_null() {
            return None;
        }
        Some(AudioCaptureTcc {
            preflight: std::mem::transmute::<*mut c_void, Preflight>(preflight),
            request: std::mem::transmute::<*mut c_void, Request>(request),
        })
    }
});

fn audio_capture_tcc() -> Option<&'static AudioCaptureTcc> {
    TCC.as_ref()
}

impl AudioCaptureTcc {
    fn status(&self) -> &'static str {
        let service = CFString::from_str(AUDIO_CAPTURE_SERVICE);
        // SAFETY: a valid CFString service name and no options dictionary.
        tcc_preflight_status(unsafe { (self.preflight)(&*service, std::ptr::null()) })
    }

    /// Shows the system prompt and returns the user's actual answer.
    fn request(&self) -> Result<String> {
        let service = CFString::from_str(AUDIO_CAPTURE_SERVICE);
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let completion = block2::RcBlock::new(move |granted: objc2::runtime::Bool| {
            let _ = send.send(granted.as_bool());
        });
        // SAFETY: TCC copies the block before returning and calls it once.
        unsafe { (self.request)(&*service, std::ptr::null(), &*completion) };
        receive
            .recv_timeout(std::time::Duration::from_secs(120))
            .map(|granted| if granted { "granted" } else { "denied" }.into())
            .map_err(|_| Error::Internal("system audio permission request timed out".into()))
    }
}

/// TCC preflight result: 0 authorized, 1 denied, anything else unanswered.
fn tcc_preflight_status(code: i32) -> &'static str {
    match code {
        0 => "granted",
        1 => "denied",
        _ => "not_determined",
    }
}

fn native_error(status: i32) -> Error {
    Error::Internal(format!("Core Audio device properties: OSStatus {status}"))
}

/// The system's default device for `selector`; an unreadable default marks
/// no device as default rather than failing every device's facts.
fn default_device(selector: AudioObjectPropertySelector) -> Option<AudioObjectID> {
    match value::<u32>(
        kAudioObjectSystemObject as AudioObjectID,
        selector,
        kAudioObjectPropertyScopeGlobal,
    ) {
        Ok(id) => (id != kAudioObjectUnknown).then_some(id),
        Err(status) => {
            tracing::warn!(status, "default audio device unreadable");
            None
        }
    }
}

/// Facts for every listed device through the shared collector: a device
/// whose identity or channel facts cannot be read is left out (logged by its
/// position, never by name) instead of failing the whole list, and one that
/// vanished mid-enumeration (`kAudioHardwareBadObjectError`) is skipped
/// silently. When devices exist but none could be read, the error is
/// returned rather than an empty list claiming there are none.
fn collect(
    ids: &[AudioObjectID],
    mut read: impl FnMut(AudioObjectID) -> NativeResult<DeviceDetails>,
) -> NativeResult<Vec<DeviceDetails>> {
    collect_devices("audio devices", ids.iter().copied(), |id| match read(id) {
        Ok(device) => Ok(Some(device)),
        #[allow(non_upper_case_globals)] // Apple's SDK constant spelling.
        Err(kAudioHardwareBadObjectError) => Ok(None),
        Err(status) => Err(status),
    })
}

fn details(
    id: AudioObjectID,
    input: Option<AudioObjectID>,
    output: Option<AudioObjectID>,
) -> NativeResult<DeviceDetails> {
    let global = kAudioObjectPropertyScopeGlobal;
    Ok(DeviceDetails {
        id: string(id, kAudioDevicePropertyDeviceUID, global)?,
        name: string(id, kAudioObjectPropertyName, global)?,
        manufacturer: string(id, kAudioObjectPropertyManufacturer, global).unwrap_or_default(),
        // An unreadable transport stays unknown; it never hides the device.
        transport: value(id, kAudioDevicePropertyTransportType, global)
            .map_or(DeviceTransport::Unknown, transport),
        output_data_source: data_source(id, kAudioObjectPropertyScopeOutput),
        input_data_source: data_source(id, kAudioObjectPropertyScopeInput),
        nominal_rate: rate(
            value::<f64>(id, kAudioDevicePropertyNominalSampleRate, global).unwrap_or_default(),
        ),
        available_rates: available_rates(
            &array::<AudioValueRange>(id, kAudioDevicePropertyAvailableNominalSampleRates, global)
                .unwrap_or_default(),
        ),
        input_channels: channels(id, kAudioObjectPropertyScopeInput)?,
        output_channels: channels(id, kAudioObjectPropertyScopeOutput)?,
        is_default_input: input == Some(id),
        is_default_output: output == Some(id),
    })
}

fn address(
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn value<T: Copy>(
    id: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> NativeResult<T> {
    let mut out = std::mem::MaybeUninit::<T>::uninit();
    let mut size = std::mem::size_of::<T>() as u32;
    // SAFETY: output is aligned writable storage with its exact byte capacity.
    let status = unsafe {
        AudioObjectGetPropertyData(
            id,
            NonNull::from(&address(selector, scope)),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(out.as_mut_ptr())
                .expect("stack storage")
                .cast(),
        )
    };
    if status != 0 {
        return Err(status);
    }
    if size as usize != std::mem::size_of::<T>() {
        return Err(kAudioHardwareBadPropertySizeError);
    }
    // SAFETY: the successful property read initialized exactly one T.
    Ok(unsafe { out.assume_init() })
}

fn array<T: Copy>(
    id: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> NativeResult<Vec<T>> {
    let address = address(selector, scope);
    let mut size = 0;
    // SAFETY: the address and size out pointer are valid; no qualifier.
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            id,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 {
        return Err(status);
    }
    let element_size = std::mem::size_of::<T>();
    if !(size as usize).is_multiple_of(element_size) {
        return Err(kAudioHardwareBadPropertySizeError);
    }
    let capacity = size as usize / element_size;
    let mut out = Vec::<T>::with_capacity(capacity);
    if capacity == 0 {
        return Ok(out);
    }
    // SAFETY: the vector has capacity for the reported number of bytes. Core
    // Audio fails rather than exceeding the supplied capacity if it grows.
    let status = unsafe {
        AudioObjectGetPropertyData(
            id,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(out.as_mut_ptr().cast::<c_void>()).expect("nonempty vector"),
        )
    };
    if status != 0 {
        return Err(status);
    }
    if !(size as usize).is_multiple_of(element_size) || size as usize > capacity * element_size {
        return Err(kAudioHardwareBadPropertySizeError);
    }
    // SAFETY: Core Audio initialized precisely this many complete T values.
    unsafe {
        out.set_len(size as usize / element_size);
    }
    Ok(out)
}

fn string(
    id: AudioObjectID,
    selector: AudioObjectPropertySelector,
    scope: AudioObjectPropertyScope,
) -> NativeResult<String> {
    let raw = value::<usize>(id, selector, scope)?;
    let pointer = NonNull::new(raw as *mut CFString).ok_or(kAudioHardwareIllegalOperationError)?;
    // SAFETY: Core Audio CFString-valued properties transfer a +1 reference.
    Ok(unsafe { CFRetained::from_raw(pointer) }.to_string())
}

fn data_source(id: AudioObjectID, scope: AudioObjectPropertyScope) -> Option<String> {
    let mut source = value::<u32>(id, kAudioDevicePropertyDataSource, scope).ok()?;
    let mut name: *mut CFString = std::ptr::null_mut();
    let mut translation = AudioValueTranslation {
        mInputData: NonNull::from(&mut source).cast(),
        mInputDataSize: std::mem::size_of::<u32>() as u32,
        mOutputData: NonNull::from(&mut name).cast(),
        mOutputDataSize: std::mem::size_of::<*mut CFString>() as u32,
    };
    let mut size = std::mem::size_of::<AudioValueTranslation>() as u32;
    // SAFETY: translation contains live, correctly sized input/output storage.
    let status = unsafe {
        AudioObjectGetPropertyData(
            id,
            NonNull::from(&address(
                kAudioDevicePropertyDataSourceNameForIDCFString,
                scope,
            )),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut translation).cast(),
        )
    };
    if status != 0 {
        return None;
    }
    // SAFETY: a successful CFString translation transfers a +1 reference.
    let name = unsafe { CFRetained::from_raw(NonNull::new(name)?) }.to_string();
    (!name.is_empty()).then_some(name)
}

fn channels(id: AudioObjectID, scope: AudioObjectPropertyScope) -> NativeResult<u32> {
    let mut channels = 0u32;
    for stream in array::<u32>(id, kAudioDevicePropertyStreams, scope)? {
        let format = value::<AudioStreamBasicDescription>(
            stream,
            kAudioStreamPropertyVirtualFormat,
            kAudioObjectPropertyScopeGlobal,
        )?;
        channels = channels
            .checked_add(format.mChannelsPerFrame)
            .ok_or(kAudioHardwareBadPropertySizeError)?;
    }
    Ok(channels)
}

fn rate(rate: f64) -> u32 {
    if rate.is_finite() && rate > 0.0 && rate <= u32::MAX as f64 {
        rate.round() as u32
    } else {
        0
    }
}

fn available_rates(ranges: &[AudioValueRange]) -> Vec<u32> {
    // DeviceDetails represents discrete rates. Preserve range endpoints and
    // include standard audio rates inside continuous ranges, without expanding
    // a continuous hardware range into millions of integer rates.
    const STANDARD: &[u32] = &[
        8_000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 44_100, 48_000, 64_000, 88_200,
        96_000, 176_400, 192_000, 352_800, 384_000,
    ];
    let mut rates = Vec::new();
    for range in ranges {
        if !range.mMinimum.is_finite()
            || !range.mMaximum.is_finite()
            || range.mMinimum > range.mMaximum
        {
            continue;
        }
        rates.extend(
            [rate(range.mMinimum), rate(range.mMaximum)]
                .into_iter()
                .filter(|rate| *rate > 0),
        );
        rates.extend(
            STANDARD.iter().copied().filter(|rate| {
                (*rate as f64) >= range.mMinimum && (*rate as f64) <= range.mMaximum
            }),
        );
    }
    rates.sort_unstable();
    rates.dedup();
    rates
}

#[allow(non_upper_case_globals)] // Apple's SDK constants retain their native names.
fn transport(value: u32) -> DeviceTransport {
    match value {
        kAudioDeviceTransportTypeBuiltIn => DeviceTransport::BuiltIn,
        kAudioDeviceTransportTypeUSB => DeviceTransport::Usb,
        kAudioDeviceTransportTypeBluetooth => DeviceTransport::Bluetooth,
        kAudioDeviceTransportTypeBluetoothLE => DeviceTransport::BluetoothLe,
        kAudioDeviceTransportTypeAggregate => DeviceTransport::Aggregate,
        kAudioDeviceTransportTypeVirtual => DeviceTransport::Virtual,
        kAudioDeviceTransportTypeHDMI => DeviceTransport::Hdmi,
        kAudioDeviceTransportTypeDisplayPort => DeviceTransport::DisplayPort,
        kAudioDeviceTransportTypeAirPlay => DeviceTransport::AirPlay,
        kAudioDeviceTransportTypeThunderbolt => DeviceTransport::Thunderbolt,
        kAudioDeviceTransportTypePCI => DeviceTransport::Pci,
        kAudioDeviceTransportTypeFireWire => DeviceTransport::FireWire,
        kAudioDeviceTransportTypeAVB => DeviceTransport::Avb,
        // Retain the legacy transport value for older Continuity devices.
        #[allow(deprecated)]
        kAudioDeviceTransportTypeContinuityCaptureWired
        | kAudioDeviceTransportTypeContinuityCaptureWireless
        | kAudioDeviceTransportTypeContinuityCapture => DeviceTransport::ContinuityCapture,
        _ => DeviceTransport::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_audio_permission_reports_tcc_state_not_a_cached_grant() {
        assert_eq!(tcc_preflight_status(0), "granted");
        assert_eq!(tcc_preflight_status(1), "denied");
        assert_eq!(tcc_preflight_status(2), "not_determined");
        assert_eq!(tcc_preflight_status(-1), "not_determined");
        // The status is TCC's non-prompting answer when the private symbols
        // resolve on this macOS (no prompt, no tap started), and never a
        // guessed grant when they do not.
        let status = MacDeviceInfo.permission_status("system_audio").unwrap();
        match audio_capture_tcc() {
            Some(tcc) => assert_eq!(status, tcc.status()),
            None => assert_eq!(status, "not_determined"),
        }
    }

    fn synthetic(id: AudioObjectID) -> DeviceDetails {
        DeviceDetails {
            id: format!("synthetic-{id}"),
            name: format!("Synthetic {id}"),
            manufacturer: String::new(),
            transport: DeviceTransport::Unknown,
            output_data_source: None,
            input_data_source: None,
            nominal_rate: 0,
            available_rates: Vec::new(),
            input_channels: 1,
            output_channels: 0,
            is_default_input: false,
            is_default_output: false,
        }
    }

    #[test]
    fn one_unreadable_device_does_not_hide_the_others() {
        let read = |id: AudioObjectID| match id {
            2 => Err(kAudioHardwareUnspecifiedError),
            3 => Err(kAudioHardwareBadObjectError),
            id => Ok(synthetic(id)),
        };
        let devices = collect(&[1, 2, 3, 4], read).unwrap();
        assert_eq!(
            devices.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            ["synthetic-1", "synthetic-4"]
        );
        // Devices exist but none is readable: an error, not "no devices".
        assert_eq!(collect(&[2, 3], read), Err(kAudioHardwareUnspecifiedError));
        // Devices that all vanished, or none at all, are a true empty list.
        assert_eq!(collect(&[3], read), Ok(Vec::new()));
        assert_eq!(collect(&[], read), Ok(Vec::new()));
    }

    #[test]
    fn rates_preserve_discrete_values_and_continuous_endpoints() {
        let rates = available_rates(&[
            AudioValueRange {
                mMinimum: 44_100.0,
                mMaximum: 48_000.0,
            },
            AudioValueRange {
                mMinimum: 16_000.0,
                mMaximum: 16_000.0,
            },
            AudioValueRange {
                mMinimum: f64::NAN,
                mMaximum: 96_000.0,
            },
        ]);
        assert_eq!(rates, [16_000, 44_100, 48_000]);
        assert_eq!(rate(f64::INFINITY), 0);
        assert_eq!(
            transport(kAudioDeviceTransportTypeBluetoothLE),
            DeviceTransport::BluetoothLe
        );
        assert_eq!(transport(u32::MAX), DeviceTransport::Unknown);
    }

    #[test]
    #[ignore = "enumerates this Mac's audio hardware; no recording or device names logged"]
    fn native_device_facts() {
        for device in MacDeviceInfo.devices().unwrap() {
            assert!(!device.id.is_empty());
            assert!(!device.name.is_empty());
            assert!(
                device
                    .available_rates
                    .windows(2)
                    .all(|rates| rates[0] < rates[1])
            );
        }
    }
}
