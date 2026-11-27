//! Audio setup detection (design §5) from input/output device info.
//!
//! The rules are data: an embedded default table (`setup_rules.toml`) that a
//! user file can extend (user rules are tried first). The first matching rule
//! wins. Per-device-pair and per-meeting overrides live in settings and are
//! applied by the caller before detection is consulted.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::DspError;

/// Audio setup of a recording (design §5), stored on the meeting as its
/// snake_case name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Setup {
    /// The mic hears only the user.
    Headphones,
    /// A speakerphone with built-in echo cancellation: small residue.
    SpeakerphoneAec,
    /// Speakers + mic without device AEC: strong echo.
    SpeakersMic,
    /// Bluetooth headset in call (narrowband) mode.
    BluetoothCall,
    /// Colleagues next to the user share the mic.
    HybridRoom,
    /// Everyone is in the room (or a phone on speaker): single track.
    InPerson,
}

impl Setup {
    /// Every setup, in table order.
    pub const ALL: [Self; 6] = [
        Self::Headphones,
        Self::SpeakerphoneAec,
        Self::SpeakersMic,
        Self::BluetoothCall,
        Self::HybridRoom,
        Self::InPerson,
    ];

    /// Stored name (`meeting.json` `setup`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Headphones => "headphones",
            Self::SpeakerphoneAec => "speakerphone_aec",
            Self::SpeakersMic => "speakers_mic",
            Self::BluetoothCall => "bluetooth_call",
            Self::HybridRoom => "hybrid_room",
            Self::InPerson => "in_person",
        }
    }

    /// Setups where other people legitimately speak into the mic, so mic
    /// speech that is not the user is never dropped (design §5).
    pub fn is_room(self) -> bool {
        matches!(self, Self::HybridRoom | Self::InPerson)
    }
}

impl fmt::Display for Setup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Setup {
    type Err = DspError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|v| v.as_str() == s)
            .ok_or_else(|| DspError::Rules(format!("unknown setup {s:?}")))
    }
}

/// Device transport (Core Audio `kAudioDevicePropertyTransportType` and the
/// equivalents elsewhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
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

/// What detection knows about one device.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DeviceDescriptor {
    /// Stable device id (Core Audio UID); equal ids mean one physical device.
    #[serde(default)]
    pub uid: Option<String>,
    /// Display name.
    pub name: String,
    pub transport: Option<Transport>,
    /// Current data source name (e.g. "Headphones" on built-in output).
    #[serde(default)]
    pub data_source: Option<String>,
    /// Nominal sample rate.
    #[serde(default)]
    pub sample_rate_hz: Option<u32>,
}

/// Inputs to detection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SetupInput {
    pub input: Option<DeviceDescriptor>,
    pub output: Option<DeviceDescriptor>,
    /// A remote meeting (meeting app detected or system track expected);
    /// `false` for in-person recordings.
    pub remote_meeting: bool,
}

/// One rule; every present condition must hold. Name and data-source matches
/// are case-insensitive substring matches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupRule {
    pub setup: Setup,
    /// Why the rule exists (shown in `doctor`/setup step).
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub remote_meeting: Option<bool>,
    #[serde(default)]
    pub input_transport: Option<Vec<Transport>>,
    #[serde(default)]
    pub output_transport: Option<Vec<Transport>>,
    #[serde(default)]
    pub input_name_contains: Option<Vec<String>>,
    #[serde(default)]
    pub output_name_contains: Option<Vec<String>>,
    #[serde(default)]
    pub output_data_source_contains: Option<Vec<String>>,
    #[serde(default)]
    pub input_max_rate_hz: Option<u32>,
    #[serde(default)]
    pub output_max_rate_hz: Option<u32>,
    /// Input and output are the same physical device.
    #[serde(default)]
    pub same_device: Option<bool>,
}

impl SetupRule {
    fn matches(&self, input: &SetupInput) -> bool {
        let inp = input.input.as_ref();
        let out = input.output.as_ref();
        if let Some(remote) = self.remote_meeting
            && remote != input.remote_meeting
        {
            return false;
        }
        if let Some(t) = &self.input_transport
            && !inp
                .and_then(|d| d.transport)
                .is_some_and(|tr| t.contains(&tr))
        {
            return false;
        }
        if let Some(t) = &self.output_transport
            && !out
                .and_then(|d| d.transport)
                .is_some_and(|tr| t.contains(&tr))
        {
            return false;
        }
        if let Some(p) = &self.input_name_contains
            && !inp.is_some_and(|d| contains_any(&d.name, p))
        {
            return false;
        }
        if let Some(p) = &self.output_name_contains
            && !out.is_some_and(|d| contains_any(&d.name, p))
        {
            return false;
        }
        if let Some(p) = &self.output_data_source_contains
            && !out
                .and_then(|d| d.data_source.as_deref())
                .is_some_and(|s| contains_any(s, p))
        {
            return false;
        }
        if let Some(max) = self.input_max_rate_hz
            && !inp.and_then(|d| d.sample_rate_hz).is_some_and(|r| r <= max)
        {
            return false;
        }
        if let Some(max) = self.output_max_rate_hz
            && !out.and_then(|d| d.sample_rate_hz).is_some_and(|r| r <= max)
        {
            return false;
        }
        if let Some(same) = self.same_device {
            let is_same = match (inp, out) {
                (Some(i), Some(o)) => match (&i.uid, &o.uid) {
                    (Some(a), Some(b)) => a == b,
                    _ => i.name == o.name,
                },
                _ => false,
            };
            if is_same != same {
                return false;
            }
        }
        true
    }
}

fn contains_any(haystack: &str, needles: &[String]) -> bool {
    let h = haystack.to_lowercase();
    needles.iter().any(|n| h.contains(&n.to_lowercase()))
}

/// Detection result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupDetection {
    pub setup: Setup,
    /// Index of the matching rule in [`SetupRules::rules`]; `None` for the
    /// table's fallback.
    pub rule_index: Option<usize>,
    pub note: Option<String>,
}

/// An ordered rules table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupRules {
    pub version: u32,
    /// Setup when no rule matches.
    pub fallback: Setup,
    #[serde(default, rename = "rule")]
    pub rules: Vec<SetupRule>,
}

/// The embedded default table.
pub const BUILTIN_RULES_TOML: &str = include_str!("setup_rules.toml");

impl SetupRules {
    /// Parses a table (`version = 1`).
    pub fn from_toml_str(text: &str) -> Result<Self, DspError> {
        let rules: Self = toml::from_str(text).map_err(|e| DspError::Rules(e.to_string()))?;
        if rules.version != 1 {
            return Err(DspError::Rules(format!(
                "unsupported version {}",
                rules.version
            )));
        }
        Ok(rules)
    }

    /// The embedded default table.
    pub fn builtin() -> Self {
        Self::from_toml_str(BUILTIN_RULES_TOML)
            .unwrap_or_else(|e| panic!("embedded setup_rules.toml is invalid: {e}"))
    }

    /// `user` rules tried before these; `user`'s fallback wins.
    pub fn with_user_rules(mut self, user: Self) -> Self {
        let mut rules = user.rules;
        rules.append(&mut self.rules);
        Self {
            version: 1,
            fallback: user.fallback,
            rules,
        }
    }

    /// First matching rule, else the fallback.
    pub fn detect(&self, input: &SetupInput) -> SetupDetection {
        match self
            .rules
            .iter()
            .enumerate()
            .find(|(_, r)| r.matches(input))
        {
            Some((i, r)) => SetupDetection {
                setup: r.setup,
                rule_index: Some(i),
                note: r.note.clone(),
            },
            None => SetupDetection {
                setup: self.fallback,
                rule_index: None,
                note: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, t: Transport, rate: u32) -> DeviceDescriptor {
        DeviceDescriptor {
            uid: Some(name.to_owned()),
            name: name.to_owned(),
            transport: Some(t),
            data_source: None,
            sample_rate_hz: Some(rate),
        }
    }

    fn detect(input: Option<DeviceDescriptor>, output: Option<DeviceDescriptor>) -> Setup {
        SetupRules::builtin()
            .detect(&SetupInput {
                input,
                output,
                remote_meeting: true,
            })
            .setup
    }

    #[test]
    fn setup_names_round_trip() {
        for s in Setup::ALL {
            assert_eq!(s.as_str().parse::<Setup>().unwrap(), s);
            #[derive(Serialize)]
            struct Wrapped {
                s: Setup,
            }
            let text = toml::to_string(&Wrapped { s }).unwrap();
            assert_eq!(text.trim(), format!("s = \"{}\"", s.as_str()));
        }
    }

    #[test]
    fn builtin_speakers_and_mic() {
        let mic = dev("MacBook Pro Microphone", Transport::BuiltIn, 48_000);
        let spk = dev("MacBook Pro Speakers", Transport::BuiltIn, 48_000);
        assert_eq!(detect(Some(mic), Some(spk)), Setup::SpeakersMic);
    }

    #[test]
    fn builtin_output_with_headphone_data_source() {
        let mic = dev("MacBook Pro Microphone", Transport::BuiltIn, 48_000);
        let mut out = dev("External Headphones", Transport::BuiltIn, 48_000);
        out.name = "Built-in Output".into();
        out.data_source = Some("Headphones".into());
        assert_eq!(detect(Some(mic), Some(out)), Setup::Headphones);
    }

    #[test]
    fn bluetooth_call_profile_vs_music_profile() {
        let bt_in = dev("Test Buds", Transport::Bluetooth, 16_000);
        let bt_out = dev("Test Buds", Transport::Bluetooth, 16_000);
        assert_eq!(detect(Some(bt_in), Some(bt_out)), Setup::BluetoothCall);
        let mic = dev("MacBook Pro Microphone", Transport::BuiltIn, 48_000);
        let bt_music = dev("Test Buds", Transport::Bluetooth, 48_000);
        assert_eq!(detect(Some(mic.clone()), Some(bt_music)), Setup::Headphones);
        let bt_speaker = dev("Desk Bluetooth Speaker", Transport::Bluetooth, 48_000);
        assert_eq!(detect(Some(mic), Some(bt_speaker)), Setup::SpeakersMic);
    }

    #[test]
    fn usb_speakerphone_and_usb_headset() {
        let wave = dev("Insta360 Wave", Transport::Usb, 48_000);
        assert_eq!(
            detect(Some(wave.clone()), Some(wave)),
            Setup::SpeakerphoneAec
        );
        let generic = dev("Conference Speakerphone 20", Transport::Usb, 48_000);
        assert_eq!(
            detect(Some(generic.clone()), Some(generic)),
            Setup::SpeakerphoneAec
        );
        let headset = dev("USB Audio Device", Transport::Usb, 48_000);
        assert_eq!(
            detect(Some(headset.clone()), Some(headset)),
            Setup::Headphones
        );
    }

    #[test]
    fn monitor_speakers_and_unknown_fall_back_to_speakers_mic() {
        let mic = dev("MacBook Pro Microphone", Transport::BuiltIn, 48_000);
        let hdmi = dev("Studio Display", Transport::DisplayPort, 48_000);
        assert_eq!(detect(Some(mic.clone()), Some(hdmi)), Setup::SpeakersMic);
        assert_eq!(detect(Some(mic), None), Setup::SpeakersMic);
    }

    #[test]
    fn not_remote_is_in_person() {
        let rules = SetupRules::builtin();
        let mic = dev("MacBook Pro Microphone", Transport::BuiltIn, 48_000);
        let out = rules.detect(&SetupInput {
            input: Some(mic),
            output: None,
            remote_meeting: false,
        });
        assert_eq!(out.setup, Setup::InPerson);
    }

    #[test]
    fn user_rules_come_first() {
        let user = SetupRules::from_toml_str(
            r#"
            version = 1
            fallback = "speakers_mic"
            [[rule]]
            setup = "hybrid_room"
            input_name_contains = ["Room Array"]
            "#,
        )
        .unwrap();
        let rules = SetupRules::builtin().with_user_rules(user);
        let mic = dev("Room Array Mic", Transport::Usb, 48_000);
        let out = dev("Room Array Mic", Transport::Usb, 48_000);
        let d = rules.detect(&SetupInput {
            input: Some(mic),
            output: Some(out),
            remote_meeting: true,
        });
        assert_eq!(d.setup, Setup::HybridRoom);
        assert_eq!(d.rule_index, Some(0));
    }

    #[test]
    fn rejects_unknown_fields_and_versions() {
        assert!(SetupRules::from_toml_str("version = 2\nfallback = \"headphones\"").is_err());
        assert!(
            SetupRules::from_toml_str(
                "version = 1\nfallback = \"headphones\"\n[[rule]]\nsetup = \"headphones\"\nbogus = 1"
            )
            .is_err()
        );
    }
}
