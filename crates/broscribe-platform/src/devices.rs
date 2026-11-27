//! Audio device facts for setup detection (design §5) and the audio check.

use std::sync::Mutex;

use crate::Result;

/// How a device is connected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceTransport {
    BuiltIn,
    Usb,
    Bluetooth,
    BluetoothLe,
    Aggregate,
    Virtual,
    Hdmi,
    DisplayPort,
    AirPlay,
    Thunderbolt,
    Pci,
    FireWire,
    Avb,
    ContinuityCapture,
    Unknown,
}

impl DeviceTransport {
    /// Stable lower-case name (`built_in`, `usb`, `bluetooth`…) used in rules
    /// tables and JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BuiltIn => "built_in",
            Self::Usb => "usb",
            Self::Bluetooth => "bluetooth",
            Self::BluetoothLe => "bluetooth_le",
            Self::Aggregate => "aggregate",
            Self::Virtual => "virtual",
            Self::Hdmi => "hdmi",
            Self::DisplayPort => "display_port",
            Self::AirPlay => "airplay",
            Self::Thunderbolt => "thunderbolt",
            Self::Pci => "pci",
            Self::FireWire => "firewire",
            Self::Avb => "avb",
            Self::ContinuityCapture => "continuity_capture",
            Self::Unknown => "unknown",
        }
    }

    pub fn is_bluetooth(self) -> bool {
        matches!(self, Self::Bluetooth | Self::BluetoothLe)
    }
}

/// One audio device as the system reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceDetails {
    /// Platform identifier, stable across reconnects (Core Audio device UID).
    pub id: String,
    pub name: String,
    pub manufacturer: String,
    pub transport: DeviceTransport,
    /// Selected data source of the output side, if the device has several
    /// (built-in output: "Internal Speakers" or "Headphones").
    pub output_data_source: Option<String>,
    /// Selected data source of the input side ("Internal Microphone"…).
    pub input_data_source: Option<String>,
    /// Current nominal sample rate (Hz); 0 when unknown.
    pub nominal_rate: u32,
    /// Supported nominal rates (Hz), ascending.
    pub available_rates: Vec<u32>,
    pub input_channels: u32,
    pub output_channels: u32,
    pub is_default_input: bool,
    pub is_default_output: bool,
}

impl DeviceDetails {
    /// A Bluetooth output running at a call-profile rate (≤ 16 kHz): the
    /// audio check warns about it (plan M4 §4).
    pub fn is_bluetooth_call_profile(&self) -> bool {
        self.transport.is_bluetooth()
            && self.output_channels > 0
            && self.nominal_rate > 0
            && self.nominal_rate <= 16_000
    }
}

/// Lists audio devices with the properties setup detection needs.
pub trait DeviceInfo: Send + Sync {
    /// Every input and output device.
    fn devices(&self) -> Result<Vec<DeviceDetails>>;

    /// The system's default input device.
    fn default_input(&self) -> Result<Option<DeviceDetails>> {
        Ok(self.devices()?.into_iter().find(|d| d.is_default_input))
    }

    /// The system's default output device.
    fn default_output(&self) -> Result<Option<DeviceDetails>> {
        Ok(self.devices()?.into_iter().find(|d| d.is_default_output))
    }

    /// Non-prompting capture authorization status, when the OS exposes it.
    fn permission_status(&self, _permission: &str) -> Result<String> {
        Err(crate::Error::Unsupported(
            "capture permission status".into(),
        ))
    }

    /// Requests authorization after the caller has explained the permission.
    fn request_permission(&self, _permission: &str) -> Result<String> {
        Err(crate::Error::Unsupported(
            "capture permission request".into(),
        ))
    }
}

/// Reads each device a backend listed, keeping the readable ones (NA28).
///
/// One device that cannot be read never hides the others (a pinned
/// microphone among them): it is left out and logged by its position in the
/// listing, never by name or error text, which can carry the name. `Ok(None)`
/// is a device that vanished mid-enumeration, skipped silently. When devices
/// were listed but none could be read, the first error is returned rather
/// than an empty list claiming there are none; an empty listing, or one whose
/// devices all vanished, is a true empty list. `list` names the listing in
/// the log ("microphones", "audio devices").
pub fn collect_devices<D, T, E>(
    list: &'static str,
    listed: impl IntoIterator<Item = D>,
    mut read: impl FnMut(D) -> std::result::Result<Option<T>, E>,
) -> std::result::Result<Vec<T>, E> {
    let listed = listed.into_iter();
    let mut devices = Vec::with_capacity(listed.size_hint().0);
    let mut first_error = None;
    for (index, device) in listed.enumerate() {
        match read(device) {
            Ok(Some(device)) => devices.push(device),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(list, device = index, "audio device unreadable; skipped");
                first_error.get_or_insert(error);
            }
        }
    }
    match first_error {
        Some(error) if devices.is_empty() => Err(error),
        _ => Ok(devices),
    }
}

/// Test double: returns whatever `devices` holds.
#[derive(Default)]
pub struct FakeDeviceInfo {
    pub devices: Mutex<Vec<DeviceDetails>>,
    pub permissions: Mutex<std::collections::BTreeMap<String, String>>,
}

impl FakeDeviceInfo {
    pub fn new(devices: Vec<DeviceDetails>) -> Self {
        Self {
            devices: Mutex::new(devices),
            permissions: Mutex::new(std::collections::BTreeMap::new()),
        }
    }

    /// Replaces the device list (e.g. to simulate plugging in headphones).
    pub fn set(&self, devices: Vec<DeviceDetails>) {
        *self.devices.lock().unwrap_or_else(|e| e.into_inner()) = devices;
    }

    /// A device with the given role flags and otherwise plain values.
    pub fn device(
        id: &str,
        transport: DeviceTransport,
        input: bool,
        output: bool,
    ) -> DeviceDetails {
        DeviceDetails {
            id: id.into(),
            name: id.into(),
            manufacturer: String::new(),
            transport,
            output_data_source: None,
            input_data_source: None,
            nominal_rate: 48_000,
            available_rates: vec![48_000],
            input_channels: u32::from(input),
            output_channels: if output { 2 } else { 0 },
            is_default_input: input,
            is_default_output: output,
        }
    }
}

impl DeviceInfo for FakeDeviceInfo {
    fn devices(&self) -> Result<Vec<DeviceDetails>> {
        Ok(self
            .devices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone())
    }

    fn permission_status(&self, permission: &str) -> Result<String> {
        Ok(self
            .permissions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(permission)
            .cloned()
            .unwrap_or_else(|| "not_determined".into()))
    }

    fn request_permission(&self, permission: &str) -> Result<String> {
        self.permissions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(permission.into(), "granted".into());
        Ok("granted".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioDevice, Error};

    /// One entry of a synthetic backend listing and what reading it gives.
    enum Slot {
        Mic(&'static str, bool),
        Unreadable(&'static str),
        Vanished,
    }

    fn read_mic(slot: &Slot) -> Result<Option<AudioDevice>> {
        match *slot {
            Slot::Mic(id, is_default) => Ok(Some(AudioDevice {
                id: id.into(),
                name: format!("Synthetic {id}"),
                is_default,
                sample_rates: vec![48_000],
            })),
            Slot::Unreadable(why) => Err(Error::Internal(why.into())),
            Slot::Vanished => Ok(None),
        }
    }

    fn mics(slots: &[Slot]) -> Result<Vec<AudioDevice>> {
        collect_devices("microphones", slots, read_mic)
    }

    #[test]
    fn an_unreadable_microphone_never_hides_the_pin_or_the_default() {
        let listed = mics(&[
            Slot::Mic("built-in", true),
            Slot::Unreadable("busy hw device"),
            Slot::Vanished,
            Slot::Mic("usb-pin", false),
            Slot::Unreadable("removed between listing and query"),
        ])
        .unwrap();
        assert_eq!(
            listed.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            ["built-in", "usb-pin"]
        );
        // The pin next to the bad entry is still listed, so it is opened
        // instead of falling back to the default microphone.
        let pin = listed.iter().find(|d| d.id == "usb-pin").unwrap();
        assert!(!pin.is_default);
        assert!(listed.iter().any(|d| d.id == "built-in" && d.is_default));
    }

    #[test]
    fn every_listed_device_unreadable_is_an_error_but_none_listed_is_empty() {
        // Devices exist but none is readable: the first error, not "no
        // devices".
        let error = mics(&[
            Slot::Unreadable("first"),
            Slot::Vanished,
            Slot::Unreadable("second"),
        ])
        .unwrap_err();
        assert!(
            matches!(&error, Error::Internal(why) if why == "first"),
            "{error:?}"
        );
        // Nothing listed, or everything vanished: a true empty list.
        assert!(mics(&[]).unwrap().is_empty());
        assert!(mics(&[Slot::Vanished, Slot::Vanished]).unwrap().is_empty());
    }

    #[test]
    fn device_facts_keep_readable_devices_and_their_defaults() {
        let read = |slot: (&str, bool)| match slot {
            ("bad", _) => Err(Error::Internal("unreadable description".into())),
            (id, input) => Ok(Some(FakeDeviceInfo::device(
                id,
                DeviceTransport::Usb,
                input,
                !input,
            ))),
        };
        let info = FakeDeviceInfo::new(
            collect_devices(
                "audio devices",
                [("headset", false), ("bad", true), ("mic", true)],
                read,
            )
            .unwrap(),
        );
        assert_eq!(info.default_input().unwrap().unwrap().id, "mic");
        assert_eq!(info.default_output().unwrap().unwrap().id, "headset");
        assert!(collect_devices("audio devices", [("bad", true)], read).is_err());
    }

    #[test]
    fn defaults_and_call_profile() {
        let mut headset = FakeDeviceInfo::device("bt", DeviceTransport::Bluetooth, false, true);
        headset.nominal_rate = 16_000;
        let mic = FakeDeviceInfo::device("mic", DeviceTransport::BuiltIn, true, false);
        let info = FakeDeviceInfo::new(vec![mic.clone(), headset.clone()]);
        assert_eq!(info.default_input().unwrap(), Some(mic));
        assert_eq!(info.default_output().unwrap(), Some(headset.clone()));
        assert!(headset.is_bluetooth_call_profile());
        headset.nominal_rate = 48_000;
        assert!(!headset.is_bluetooth_call_profile());
        assert_eq!(DeviceTransport::BluetoothLe.as_str(), "bluetooth_le");
    }
}
