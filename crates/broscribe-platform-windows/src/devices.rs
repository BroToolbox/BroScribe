use broscribe_platform::devices::collect_devices;
use broscribe_platform::{DeviceDetails, DeviceInfo, DeviceTransport, Result};
use cpal::traits::{DeviceTrait, HostTrait};
pub struct WinDeviceInfo;
impl DeviceInfo for WinDeviceInfo {
    /// Every endpoint through the shared collector: one whose identifier or
    /// description cannot be read is left out (logged by position) rather than
    /// failing the list; only a listing where no device is readable fails.
    fn devices(&self) -> Result<Vec<DeviceDetails>> {
        let host = cpal::default_host();
        let default_input = host.default_input_device().and_then(|d| d.id().ok());
        let default_output = host.default_output_device().and_then(|d| d.id().ok());
        let listed = host
            .devices()
            .map_err(|e| crate::error("enumerating WASAPI devices", e))?;
        collect_devices("audio devices", listed, |device| -> Result<_> {
            let id = device
                .id()
                .map_err(|e| crate::error("audio device identifier", e))?;
            let description = device
                .description()
                .map_err(|e| crate::error("audio device properties", e))?;
            let input = device.default_input_config().ok();
            let output = device.default_output_config().ok();
            let mut rates = Vec::new();
            if let Ok(configs) = device.supported_input_configs() {
                for config in configs {
                    rates.extend([config.min_sample_rate(), config.max_sample_rate()]);
                }
            }
            if let Ok(configs) = device.supported_output_configs() {
                for config in configs {
                    rates.extend([config.min_sample_rate(), config.max_sample_rate()]);
                }
            }
            rates.sort_unstable();
            rates.dedup();
            let transport = if description.device_type() == cpal::DeviceType::Virtual {
                DeviceTransport::Virtual
            } else {
                match description.interface_type() {
                    cpal::InterfaceType::BuiltIn => DeviceTransport::BuiltIn,
                    cpal::InterfaceType::Usb => DeviceTransport::Usb,
                    cpal::InterfaceType::Bluetooth => DeviceTransport::Bluetooth,
                    cpal::InterfaceType::Pci => DeviceTransport::Pci,
                    cpal::InterfaceType::FireWire => DeviceTransport::FireWire,
                    cpal::InterfaceType::Thunderbolt => DeviceTransport::Thunderbolt,
                    cpal::InterfaceType::Hdmi => DeviceTransport::Hdmi,
                    _ => DeviceTransport::Unknown,
                }
            };
            Ok(Some(DeviceDetails {
                id: id.to_string(),
                name: description.name().into(),
                manufacturer: description.manufacturer().unwrap_or_default().into(),
                transport,
                output_data_source: output.as_ref().map(|_| description.name().into()),
                input_data_source: input.as_ref().map(|_| description.name().into()),
                nominal_rate: input
                    .as_ref()
                    .or(output.as_ref())
                    .map(|c| c.sample_rate())
                    .unwrap_or(0),
                available_rates: rates,
                input_channels: input.map(|c| u32::from(c.channels())).unwrap_or(0),
                output_channels: output.map(|c| u32::from(c.channels())).unwrap_or(0),
                is_default_input: default_input.as_ref() == Some(&id),
                is_default_output: default_output.as_ref() == Some(&id),
            }))
        })
    }
}
