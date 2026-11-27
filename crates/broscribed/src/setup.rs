//! Resolve setup once at capture admission and persist it in SessionConfig.
//! Classification is advisory: only an invalid explicit setup refuses a
//! start; any other failure is logged and the rules' fallback is used.
use broscribe_core::config::Config;
use broscribe_dsp::{DeviceDescriptor, EchoHandling, Setup, SetupInput, SetupRules, Transport};
use broscribe_ipc::IpcError;
use broscribe_platform::{DeviceDetails, DeviceInfo, MicCapture};
use std::path::Path;

/// The microphone capture opens for `config`: the pinned one while it is
/// connected, the default input (`None`) otherwise. The recorder's own rule
/// ([`broscribe_capture::session::pinned_input`]), shared by recording,
/// setup classification, helper samples (enrollment) and audio checks.
pub(crate) fn capture_mic(mic: &dyn MicCapture, config: &Config) -> Option<String> {
    broscribe_capture::session::pinned_input(mic, config.audio.pinned_mic.as_deref()).map(|d| d.id)
}

/// What the daemon stores in the session at record start.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub setup: String,
    /// The audio check's echo handling for the current device pair.
    pub echo_handling: Option<String>,
    /// The rules' answer without a remote meeting, for the final job to use
    /// when the session shows no remote meeting (design §5). Only for an
    /// auto-detected setup whose remote context is unconfirmed.
    pub in_person_setup: Option<String>,
}

/// Admission never infers in person from the absence of a meeting app: an
/// auto-detected setup is classified as remote. When `remote_confirmed` is
/// false (a manual start with no app), the in-person answer is resolved too.
/// `mic_device` is the input capture opens ([`capture_mic`]); `None`: the
/// default input.
pub(crate) fn resolve(
    devices: &dyn DeviceInfo,
    data_dir: &Path,
    config: &Config,
    explicit: Option<&str>,
    remote_confirmed: bool,
    mic_device: Option<&str>,
) -> Result<Resolved, IpcError> {
    let explicit = explicit
        .map(|setup| {
            setup
                .parse::<Setup>()
                .map_err(|e| IpcError::new("invalid_input", e.to_string()))
        })
        .transpose()?;
    let input = match mic_device {
        // The pinned microphone capture opens, not the default input.
        Some(id) => devices.devices().map(|all| {
            let input = all.into_iter().find(|d| d.id == id && d.input_channels > 0);
            if input.is_none() {
                tracing::warn!(
                    "setup detection: the pinned microphone is not described; using unknown input metadata"
                );
            }
            input
        }),
        None => devices.default_input(),
    };
    let pair = match (input, devices.default_output()) {
        (Ok(input), Ok(output)) => Some((input, output)),
        (Err(error), _) | (_, Err(error)) => {
            tracing::warn!(%error, "setup detection: listing devices failed; using the rules' fallback");
            None
        }
    };
    let (input, output) = pair.unwrap_or((None, None));
    let echo_handling = match (&input, &output) {
        (Some(input), Some(output)) => config
            .audio_check_for(&input.id, &output.id, output.output_data_source.as_deref())
            .and_then(
                |handling| match serde_json::from_value::<EchoHandling>(handling.into()) {
                    Ok(_) => Some(handling.to_owned()),
                    Err(error) => {
                        tracing::warn!(%error, handling, "ignoring an invalid saved echo handling");
                        None
                    }
                },
            ),
        _ => None,
    };
    let (setup, in_person_setup) = match explicit {
        Some(setup) => (setup, None),
        None => {
            let rules = rules(data_dir);
            let remote = detect(&input, &output, &rules, config, true);
            let in_person = (!remote_confirmed)
                .then(|| detect(&input, &output, &rules, config, false).to_string());
            (remote, in_person)
        }
    };
    Ok(Resolved {
        setup: setup.to_string(),
        echo_handling,
        in_person_setup,
    })
}

/// The built-in table preceded by the user's `setup_rules.toml`, if valid.
fn rules(data_dir: &Path) -> SetupRules {
    let rules = SetupRules::builtin();
    let path = data_dir.join("setup_rules.toml");
    match std::fs::read_to_string(&path) {
        Ok(text) => match SetupRules::from_toml_str(&text) {
            Ok(user) => return rules.with_user_rules(user),
            Err(error) => tracing::warn!(
                %error,
                path = %path.display(),
                "ignoring invalid setup rules; using the built-in table"
            ),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            %error,
            path = %path.display(),
            "reading setup rules failed; using the built-in table"
        ),
    }
    rules
}

fn detect(
    input: &Option<DeviceDetails>,
    output: &Option<DeviceDetails>,
    rules: &SetupRules,
    config: &Config,
    remote_meeting: bool,
) -> Setup {
    // A per-pair override describes a remote meeting on those devices; an
    // in-person recording on the same pair is still in person.
    if remote_meeting
        && let Some(pair) = config.audio_setup_overrides.iter().find(|pair| {
            input.as_ref().is_some_and(|d| d.id == pair.input_uid)
                && output.as_ref().is_some_and(|d| d.id == pair.output_uid)
        })
    {
        match pair.setup.parse::<Setup>() {
            Ok(setup) => return setup,
            Err(error) => {
                tracing::warn!(%error, "ignoring an invalid audio_setup_overrides entry")
            }
        }
    }
    rules
        .detect(&SetupInput {
            input: input.as_ref().map(|d| descriptor(d, true)),
            output: output.as_ref().map(|d| descriptor(d, false)),
            remote_meeting,
        })
        .setup
}

fn descriptor(device: &DeviceDetails, input: bool) -> DeviceDescriptor {
    let transport = match device.transport {
        broscribe_platform::DeviceTransport::BuiltIn => Transport::BuiltIn,
        broscribe_platform::DeviceTransport::Usb => Transport::Usb,
        broscribe_platform::DeviceTransport::Bluetooth => Transport::Bluetooth,
        broscribe_platform::DeviceTransport::BluetoothLe => Transport::BluetoothLe,
        broscribe_platform::DeviceTransport::Aggregate => Transport::Aggregate,
        broscribe_platform::DeviceTransport::Virtual => Transport::Virtual,
        broscribe_platform::DeviceTransport::Hdmi => Transport::Hdmi,
        broscribe_platform::DeviceTransport::DisplayPort => Transport::DisplayPort,
        broscribe_platform::DeviceTransport::AirPlay => Transport::AirPlay,
        broscribe_platform::DeviceTransport::Thunderbolt => Transport::Thunderbolt,
        broscribe_platform::DeviceTransport::Pci => Transport::Pci,
        broscribe_platform::DeviceTransport::FireWire => Transport::FireWire,
        broscribe_platform::DeviceTransport::Avb => Transport::Avb,
        broscribe_platform::DeviceTransport::ContinuityCapture => Transport::ContinuityCapture,
        broscribe_platform::DeviceTransport::Unknown => Transport::Unknown,
    };
    DeviceDescriptor {
        uid: Some(device.id.clone()),
        name: device.name.clone(),
        transport: Some(transport),
        data_source: if input {
            device.input_data_source.clone()
        } else {
            device.output_data_source.clone()
        },
        sample_rate_hz: (device.nominal_rate != 0).then_some(device.nominal_rate),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use broscribe_platform::{DeviceTransport, devices::FakeDeviceInfo};
    fn setup(
        devices: &dyn DeviceInfo,
        dir: &Path,
        config: &Config,
        explicit: Option<&str>,
        remote: bool,
    ) -> String {
        resolve(devices, dir, config, explicit, remote, None)
            .unwrap()
            .setup
    }
    struct FailingDevices;
    impl DeviceInfo for FailingDevices {
        fn devices(&self) -> broscribe_platform::Result<Vec<DeviceDetails>> {
            Err(broscribe_platform::Error::Internal(
                "Core Audio device properties: OSStatus -1".into(),
            ))
        }
    }
    #[test]
    fn record_setup_uses_devices_then_pair_override_then_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let mic = FakeDeviceInfo::device("mic", DeviceTransport::BuiltIn, true, false);
        let mut output = FakeDeviceInfo::device("out", DeviceTransport::BuiltIn, false, true);
        output.output_data_source = Some("Headphones".into());
        let devices = FakeDeviceInfo::new(vec![mic, output]);
        let mut config = Config::default();
        assert_eq!(
            setup(&devices, tmp.path(), &config, None, true),
            "headphones"
        );
        config
            .audio_setup_overrides
            .push(broscribe_core::config::AudioSetupOverride {
                input_uid: "mic".into(),
                output_uid: "out".into(),
                setup: "hybrid_room".into(),
            });
        assert_eq!(
            setup(&devices, tmp.path(), &config, None, true),
            "hybrid_room"
        );
        assert_eq!(
            setup(&devices, tmp.path(), &config, Some("in_person"), true),
            "in_person"
        );
        assert!(resolve(&devices, tmp.path(), &config, Some("bogus"), true, None).is_err());
    }

    #[test]
    fn missing_pinned_metadata_never_uses_another_microphones_override() {
        let tmp = tempfile::tempdir().unwrap();
        let devices = FakeDeviceInfo::new(vec![
            FakeDeviceInfo::device("default-mic", DeviceTransport::BuiltIn, true, false),
            FakeDeviceInfo::device("out", DeviceTransport::BuiltIn, false, true),
        ]);
        let mut config = Config::default();
        config
            .audio_setup_overrides
            .push(broscribe_core::config::AudioSetupOverride {
                input_uid: "default-mic".into(),
                output_uid: "out".into(),
                setup: "hybrid_room".into(),
            });
        let resolved = resolve(
            &devices,
            tmp.path(),
            &config,
            None,
            true,
            Some("unlisted-pinned-mic"),
        )
        .unwrap();
        assert_ne!(resolved.setup, "hybrid_room");
        assert_eq!(resolved.echo_handling, None);
    }
    #[test]
    fn record_setup_user_rules_precede_builtin() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("setup_rules.toml"), "version=1\nfallback='speakers_mic'\n[[rule]]\nsetup='hybrid_room'\ninput_name_contains=['array']\n").unwrap();
        let devices = FakeDeviceInfo::new(vec![FakeDeviceInfo::device(
            "array",
            DeviceTransport::Usb,
            true,
            false,
        )]);
        assert_eq!(
            setup(&devices, tmp.path(), &Config::default(), None, true),
            "hybrid_room"
        );
    }
    #[test]
    fn record_setup_errors_are_advisory_and_fall_back_to_builtin() {
        let tmp = tempfile::tempdir().unwrap();
        let speakers = || {
            FakeDeviceInfo::new(vec![
                FakeDeviceInfo::device("mic", DeviceTransport::BuiltIn, true, false),
                FakeDeviceInfo::device("out", DeviceTransport::BuiltIn, false, true),
            ])
        };
        // Malformed user rules: logged, built-in table used.
        std::fs::write(tmp.path().join("setup_rules.toml"), "version = [not toml").unwrap();
        assert_eq!(
            setup(&speakers(), tmp.path(), &Config::default(), None, true),
            "speakers_mic"
        );
        // Unreadable rules path (a directory): same.
        let unreadable = tempfile::tempdir().unwrap();
        std::fs::create_dir(unreadable.path().join("setup_rules.toml")).unwrap();
        assert_eq!(
            setup(
                &speakers(),
                unreadable.path(),
                &Config::default(),
                None,
                true
            ),
            "speakers_mic"
        );
        // Device enumeration error: the rules' fallback, not a refused start.
        let resolved = resolve(
            &FailingDevices,
            tmp.path(),
            &Config::default(),
            None,
            true,
            None,
        )
        .unwrap();
        assert_eq!(resolved.setup, "speakers_mic");
        assert_eq!(resolved.echo_handling, None);
        // An invalid pair override is skipped, not fatal.
        let mut config = Config::default();
        config
            .audio_setup_overrides
            .push(broscribe_core::config::AudioSetupOverride {
                input_uid: "mic".into(),
                output_uid: "out".into(),
                setup: "not_a_setup".into(),
            });
        assert_eq!(
            setup(&speakers(), tmp.path(), &config, None, true),
            "speakers_mic"
        );
    }
    #[test]
    fn record_setup_in_person_only_offered_for_unconfirmed_starts() {
        let tmp = tempfile::tempdir().unwrap();
        let devices = FakeDeviceInfo::new(vec![
            FakeDeviceInfo::device("mic", DeviceTransport::BuiltIn, true, false),
            FakeDeviceInfo::device("out", DeviceTransport::BuiltIn, false, true),
        ]);
        let mut config = Config::default();
        let resolved = |config: &Config, devices: &dyn DeviceInfo, explicit, confirmed| {
            let r = resolve(devices, tmp.path(), config, explicit, confirmed, None).unwrap();
            (r.setup, r.in_person_setup)
        };
        // Admission classifies an auto setup as remote either way.
        assert_eq!(
            resolved(&config, &devices, None, true),
            ("speakers_mic".into(), None)
        );
        assert_eq!(
            resolved(&config, &devices, None, false),
            ("speakers_mic".into(), Some("in_person".into()))
        );
        // An explicit setup always wins and offers no alternative.
        assert_eq!(
            resolved(&config, &devices, Some("headphones"), false),
            ("headphones".into(), None)
        );
        // A pair override describes remote meetings on those devices.
        config
            .audio_setup_overrides
            .push(broscribe_core::config::AudioSetupOverride {
                input_uid: "mic".into(),
                output_uid: "out".into(),
                setup: "headphones".into(),
            });
        assert_eq!(
            resolved(&config, &devices, None, false),
            ("headphones".into(), Some("in_person".into()))
        );
        assert_eq!(
            resolved(&config, &FailingDevices, None, false),
            ("speakers_mic".into(), Some("in_person".into()))
        );
    }
    #[test]
    fn record_setup_carries_audio_check_echo_handling_for_the_pair() {
        let tmp = tempfile::tempdir().unwrap();
        let mut output = FakeDeviceInfo::device("out", DeviceTransport::BuiltIn, false, true);
        output.output_data_source = Some("Speakers".into());
        let devices = FakeDeviceInfo::new(vec![
            FakeDeviceInfo::device("mic", DeviceTransport::BuiltIn, true, false),
            output,
        ]);
        let mut config = Config::default();
        let choice = |source: &str, handling: &str| broscribe_core::config::AudioCheckChoice {
            input_uid: "mic".into(),
            output_uid: "out".into(),
            output_data_source: Some(source.into()),
            echo_handling: handling.into(),
        };
        config.set_audio_check(choice("Headphones", "none"));
        let resolved =
            |config: &Config| resolve(&devices, tmp.path(), config, None, true, None).unwrap();
        assert_eq!(resolved(&config).echo_handling, None);
        config.set_audio_check(choice("Speakers", "residue_gating"));
        assert_eq!(
            resolved(&config).echo_handling.as_deref(),
            Some("residue_gating")
        );
        config.set_audio_check(choice("Speakers", "garbage"));
        assert_eq!(resolved(&config).echo_handling, None);
    }

    /// Classification describes the microphone capture opens: a connected
    /// pinned microphone, not the system default input (RT06).
    #[test]
    fn record_setup_classifies_the_pinned_microphone_capture_opens() {
        let tmp = tempfile::tempdir().unwrap();
        let mut pinned = FakeDeviceInfo::device("usb-mic", DeviceTransport::Usb, true, false);
        pinned.is_default_input = false;
        let mut output = FakeDeviceInfo::device("out", DeviceTransport::BuiltIn, false, true);
        output.output_data_source = Some("Speakers".into());
        let devices = FakeDeviceInfo::new(vec![
            FakeDeviceInfo::device("mic", DeviceTransport::BuiltIn, true, false),
            pinned,
            output,
        ]);
        let mut config = Config::default();
        config.set_audio_check(broscribe_core::config::AudioCheckChoice {
            input_uid: "usb-mic".into(),
            output_uid: "out".into(),
            output_data_source: Some("Speakers".into()),
            echo_handling: "residue_gating".into(),
        });
        let handling = |mic: Option<&str>| {
            resolve(&devices, tmp.path(), &config, None, true, mic)
                .unwrap()
                .echo_handling
        };
        assert_eq!(handling(Some("usb-mic")).as_deref(), Some("residue_gating"));
        // The default input has no saved check for this pair.
        assert_eq!(handling(None), None);
        // A pin capture would not open (not described) falls back to the
        // default input, as capture does.
        assert_eq!(handling(Some("unplugged")), None);
    }
}
