//! User configuration: `<data_dir>/config.toml`.
//!
//! Every write goes through one per-data-directory OS file lock
//! (`<data_dir>/config.lock`), whatever library the config names, and is a
//! compare-and-swap on the persisted [`Config::revision()`]: [`Config::save`]
//! refuses a snapshot older than the file (`config_stale`) instead of
//! overwriting someone else's change, and [`Config::update`] edits a fresh
//! read under the same lock.
//!
//! Lock order: the config lock is a leaf. A holder of the library, queue or
//! session lock may take it; while it is held (inside an
//! [`Config::update`] closure) nothing takes another lock, touches a
//! library, runs a model or uses the network.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

use crate::schema::Handling;
use crate::{Error, Result, fsutil};

pub const CONFIG_FILE: &str = "config.toml";
/// The data directory's config write lock (empty file).
pub const CONFIG_LOCK_FILE: &str = "config.lock";
/// Error code: the snapshot being saved is older than `config.toml`.
pub const CONFIG_STALE: &str = "config_stale";
/// Error code: another writer held the config lock for the whole wait.
pub const CONFIG_BUSY: &str = "config_busy";
/// How long a config write waits for the lock before `config_busy`.
pub const CONFIG_LOCK_WAIT: Duration = Duration::from_secs(5);

/// Minimum detected speech before an optional enrollment confirmation is compared.
pub const CONFIRM_MIN_SPEECH_S: f64 = 8.0;
/// Elapsed-time guidance; detected speech is measured only after capture stops.
pub const CONFIRM_TARGET_CAPTURE_S: u64 = 15;
/// Shared capture cap; the CLI records for this long, while the app can stop early.
pub const CONFIRM_MAX_CAPTURE_S: u64 = 25;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Persisted write counter: missing file or a file written before it
    /// existed reads as 0; each successful write stores the next value.
    /// Maintained only by [`Config::save`] / [`Config::update`].
    revision: u64,
    /// Library folder; the platform default when unset.
    pub library: Option<PathBuf>,
    /// BCP 47 tag used when an import names no language.
    pub default_language: Option<String>,
    /// Show a consent reminder in the recording panel; no effect on recording policy.
    pub consent_reminder: bool,
    pub import_handling: Handling,
    /// Terms passed to models that accept hotwords.
    pub hotwords: Vec<String>,
    pub profile: Profile,
    /// Naming thresholds per speaker-embedding model ID (design §8.3).
    pub identity: BTreeMap<String, Thresholds>,
    /// Explicit grouping choices. Their thresholds live in `identity` and
    /// take precedence over the grouping floor fitted by calibration.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    speaker_grouping_presets: BTreeMap<String, SpeakerGroupingPreset>,
    pub setup: SetupProgress,
    /// What happens to recordings the library owns (design §7.2).
    pub retention: Retention,
    /// When enrichment runs on its own (plan §9.8).
    pub auto_enrich: AutoEnrich,
    /// The built-in summary's saved instructions (`[enrichment]`).
    #[serde(skip_serializing_if = "EnrichmentConfig::is_unset")]
    pub enrichment: EnrichmentConfig,
    /// Meeting templates (`[templates]`).
    #[serde(skip_serializing_if = "TemplatesConfig::is_unset")]
    pub templates: TemplatesConfig,
    /// Enrichment and remote-embedding providers by registry provider ID;
    /// a provider absent here is disabled.
    pub providers: BTreeMap<String, ProviderConfig>,
    /// User overrides of meeting-app recording policies.
    pub detect: DetectConfig,
    /// Per-device-pair recording setup overrides.
    pub audio_setup_overrides: Vec<AudioSetupOverride>,
    /// Integration settings are interpreted by broscribe-integrations.
    /// Preserve provider-specific and future keys on unrelated config saves.
    #[serde(skip_serializing_if = "toml::Table::is_empty")]
    pub integration: toml::Table,
    /// Echo handling chosen by the audio check (design §4.1 step 5), per
    /// device pair; the daemon applies it to recordings on the same pair.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub audio_checks: Vec<AudioCheckChoice>,
    /// Capture devices (`[audio]`).
    #[serde(skip_serializing_if = "AudioConfig::is_unset")]
    pub audio: AudioConfig,
    /// Agent access through MCP (`[mcp]`).
    #[serde(skip_serializing_if = "McpConfig::is_unset")]
    pub mcp: McpConfig,
    /// App behaviour (`[app]`).
    #[serde(skip_serializing_if = "AppConfig::is_unset")]
    pub app: AppConfig,
    /// Recording controls presentation (`[panel]`).
    #[serde(skip_serializing_if = "PanelConfig::is_unset")]
    pub panel: PanelConfig,
    /// What runs alongside a recording's live transcript (`[live]`).
    #[serde(skip_serializing_if = "LiveConfig::is_unset")]
    pub live: LiveConfig,
    /// Reading presentation (`[appearance]`).
    #[serde(skip_serializing_if = "AppearanceConfig::is_unset")]
    pub appearance: AppearanceConfig,
    /// Export picker preferences.
    #[serde(skip_serializing_if = "ExportConfig::is_unset")]
    pub export: ExportConfig,
    /// The signed updater's overrides (`[updates]`); validated by
    /// `broscribe-updates` before use.
    #[serde(skip_serializing_if = "UpdateSettings::is_unset")]
    pub updates: UpdateSettings,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportConfig {
    pub last_dir: Option<PathBuf>,
}

impl ExportConfig {
    fn is_unset(&self) -> bool {
        self.last_dir.is_none()
    }
}

/// `[enrichment]`: the user's default instructions for the built-in summary.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EnrichmentConfig {
    /// What summaries should focus on (emphasis, language, length); placed
    /// in a delimited preferences block of the prompt, never able to change
    /// the reply's schema. At most [`INSTRUCTIONS_MAX_CHARS`] characters.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

impl EnrichmentConfig {
    fn is_unset(&self) -> bool {
        self.instructions.is_none()
    }
}

/// `[templates]`: which templates run on their own after enrichment.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TemplatesConfig {
    /// Template IDs run after each automatic enrichment, only when
    /// `auto_enrich` and a consented provider permit it. Empty by default.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub auto: Vec<String>,
}

impl TemplatesConfig {
    fn is_unset(&self) -> bool {
        self.auto.is_empty()
    }
}

/// Longest user instructions accepted for the summary or a template run.
pub const INSTRUCTIONS_MAX_CHARS: usize = 4_000;

/// Checks user instructions: at most [`INSTRUCTIONS_MAX_CHARS`] characters
/// and no control characters other than newlines and tabs. `invalid_input`
/// otherwise. An empty text is valid (no instructions).
pub fn validate_instructions(text: &str) -> Result<()> {
    if text.chars().count() > INSTRUCTIONS_MAX_CHARS {
        return Err(Error::InvalidInput(format!(
            "instructions are longer than {INSTRUCTIONS_MAX_CHARS} characters"
        )));
    }
    if text
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(Error::InvalidInput(
            "instructions contain control characters".into(),
        ));
    }
    Ok(())
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// UID of the microphone recordings use while it is connected (`audio
    /// pin`); the system default input otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_mic: Option<String>,
}

impl AudioConfig {
    fn is_unset(&self) -> bool {
        self.pinned_mic.is_none()
    }
}

/// `[mcp]`: whether `broscribe mcp` serves agents.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// Unset means off. Connecting AI tools requires explicit opt-in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl McpConfig {
    fn is_unset(&self) -> bool {
        self.enabled.is_none()
    }
}

/// `[app]`: desktop app behaviour.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// The agent opens the menu-bar app (no window) when it starts at
    /// login; unset means on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open_at_login: Option<bool>,
    /// Show the macOS menu-bar item. Unset means on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_status_item: Option<bool>,
    /// Exclude Recording controls from supported screen-capture APIs.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hide_controls_from_screen_sharing: bool,
}

impl AppConfig {
    fn is_unset(&self) -> bool {
        self.open_at_login.is_none()
            && self.show_status_item.is_none()
            && !self.hide_controls_from_screen_sharing
    }
}

/// `[panel]`: recording controls presentation.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PanelConfig {
    pub compact: bool,
}

impl PanelConfig {
    fn is_unset(&self) -> bool {
        !self.compact
    }
}

/// `[live]`: what runs alongside a recording's live transcript.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveConfig {
    /// Live speaker identification while recording (provisional voices and
    /// suggested names); unset means on. Turning it off never stops the
    /// capture or the live transcript, and the final pass identifies
    /// speakers as always.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<bool>,
    /// Explicit opt-in (off by default) to show the user's own person as a
    /// live *suggestion* when the speaker model has no calibrated
    /// thresholds. It never names automatically, enrolls a voice or changes
    /// any threshold; the user still confirms each suggestion.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub allow_uncalibrated_me_suggestions: bool,
    /// Final lines from one diarized speaker needed to confirm a live name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promotion_min_lines: Option<std::num::NonZeroUsize>,
}

impl LiveConfig {
    fn is_unset(&self) -> bool {
        *self == Self::default()
    }

    pub fn promotion_min_lines(&self) -> usize {
        self.promotion_min_lines
            .map_or(Self::MIN_PROMOTION_LINES, std::num::NonZeroUsize::get)
            .max(Self::MIN_PROMOTION_LINES)
    }

    pub const MIN_PROMOTION_LINES: usize = 2;
}

/// `[appearance]`: how BroScribe's windows look.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    /// Reading size of transcripts, summaries and panels; theme colours are
    /// unaffected.
    pub reading_size: ReadingSize,
    /// Light, dark, or the system's appearance (the default).
    pub theme: ThemeChoice,
}

impl AppearanceConfig {
    fn is_unset(&self) -> bool {
        self.reading_size == ReadingSize::default() && self.theme == ThemeChoice::default()
    }
}

/// Whether BroScribe's windows are light, dark, or follow the system:
/// `system` (default), `light` or `dark`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeChoice {
    /// Follow the system's light or dark appearance.
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    /// Every choice, in the order a control offers them.
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark];

    /// The saved name (`system`, `light`, `dark`).
    pub fn name(self) -> &'static str {
        match self {
            ThemeChoice::System => "system",
            ThemeChoice::Light => "light",
            ThemeChoice::Dark => "dark",
        }
    }
}

/// A reading-size step: `small`, `standard` (default), `large`, `larger`
/// or `largest`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadingSize {
    Small,
    #[default]
    Standard,
    Large,
    Larger,
    Largest,
}

impl ReadingSize {
    /// Every step, smallest first (the order a size control offers them).
    pub const ALL: [ReadingSize; 5] = [
        ReadingSize::Small,
        ReadingSize::Standard,
        ReadingSize::Large,
        ReadingSize::Larger,
        ReadingSize::Largest,
    ];

    /// Factor applied to the theme's base text size: 87.5, 100, 125, 150
    /// and 200 % (ACC-31: the largest step reaches 200 %). The saved names
    /// never changed, so a `config.toml` written before the 200 % steps
    /// loads unchanged and its step simply draws at the new factor.
    pub fn scale(self) -> f32 {
        match self {
            ReadingSize::Small => 0.875,
            ReadingSize::Standard => 1.0,
            ReadingSize::Large => 1.25,
            ReadingSize::Larger => 1.5,
            ReadingSize::Largest => 2.0,
        }
    }

    /// The next larger step, if any.
    pub fn larger(self) -> Option<Self> {
        let i = Self::ALL.iter().position(|s| *s == self)?;
        Self::ALL.get(i + 1).copied()
    }

    /// The next smaller step, if any.
    pub fn smaller(self) -> Option<Self> {
        let i = Self::ALL.iter().position(|s| *s == self)?;
        i.checked_sub(1).map(|i| Self::ALL[i])
    }
}

/// The built-in stable release envelope on GitHub Releases.
pub const RELEASE_FEED: &str =
    "https://github.com/BroToolbox/BroScribe/releases/latest/download/broscribe-update.json";

/// Overrides for the signed updater. Checks are on by default. A custom
/// feed must explicitly pin its release keys. The native backend supplies
/// its built-in signer pin, if any. `enabled = false` turns checks off.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateSettings {
    pub enabled: bool,
    /// HTTPS URL of the signed release envelope. Absent uses the built-in feed.
    pub feed_url: Option<String>,
    /// Release channel the manifest must name.
    pub channel: String,
    /// Pinned Ed25519 public keys, 64 lowercase hex digits each. A key
    /// rotation is an explicit edit of this list.
    pub trusted_keys: Vec<String>,
    /// Pinned code signer where the OS verifies signatures: the Apple team
    /// identifier on macOS, the lowercase hex SHA-256 fingerprint of the
    /// Authenticode leaf certificate on Windows. Must be absent on Linux,
    /// which has no OS code-signing facility.
    pub signer: Option<String>,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            feed_url: None,
            channel: "stable".into(),
            trusted_keys: Vec::new(),
            signer: None,
        }
    }
}

impl UpdateSettings {
    pub fn effective_feed(&self) -> &str {
        self.feed_url.as_deref().unwrap_or(RELEASE_FEED)
    }

    pub fn effective_keys(&self) -> impl Iterator<Item = &str> {
        let built_in = include_str!("../release-keys.txt")
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'));
        let use_built_in = self.trusted_keys.is_empty() && self.effective_feed() == RELEASE_FEED;
        built_in
            .filter(move |_| use_built_in)
            .chain(self.trusted_keys.iter().map(String::as_str))
    }

    /// Whether source and trust follow the compiled defaults. The check
    /// preference is independent of the source.
    pub fn uses_built_in_source(&self) -> bool {
        self.effective_feed() == RELEASE_FEED
            && self.channel == "stable"
            && self.trusted_keys.is_empty()
            && self.signer.is_none()
    }

    fn is_unset(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioCheckChoice {
    pub input_uid: String,
    pub output_uid: String,
    /// Built-in outputs keep one UID for speakers and headphones; the data
    /// source tells them apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_data_source: Option<String>,
    /// `none`, `residue_gating` or `full_aec`.
    pub echo_handling: String,
}

impl Config {
    /// Records the audio check's echo handling for this device pair,
    /// replacing an earlier choice for the same pair.
    pub fn set_audio_check(&mut self, choice: AudioCheckChoice) {
        self.audio_checks.retain(|c| {
            (&c.input_uid, &c.output_uid, &c.output_data_source)
                != (
                    &choice.input_uid,
                    &choice.output_uid,
                    &choice.output_data_source,
                )
        });
        self.audio_checks.push(choice);
    }

    /// The saved echo handling for this device pair, if the audio check ran on it.
    pub fn audio_check_for(
        &self,
        input_uid: &str,
        output_uid: &str,
        output_data_source: Option<&str>,
    ) -> Option<&str> {
        self.audio_checks
            .iter()
            .find(|c| {
                c.input_uid == input_uid
                    && c.output_uid == output_uid
                    && c.output_data_source.as_deref() == output_data_source
            })
            .map(|c| c.echo_handling.as_str())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DetectConfig {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub policies: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioSetupOverride {
    pub input_uid: String,
    pub output_uid: String,
    pub setup: String,
}

/// Recording retention (design §7.2). In TOML: `[retention]` with
/// `policy = "keep_forever" | "delete_immediately" | "delete_after_days"`,
/// plus `wait_until_named` or `days`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "snake_case")]
pub enum Retention {
    #[default]
    KeepForever,
    /// Delete once the final transcript and identity pass are done;
    /// optionally only after every speaker is named.
    DeleteImmediately {
        #[serde(default)]
        wait_until_named: bool,
    },
    /// Delete `days` days after the meeting's start.
    DeleteAfterDays { days: u32 },
}

impl Retention {
    pub const MAX_DAYS: u32 = 3_650;

    /// Parses the CLI spelling: `keep`, `processed`, `processed-and-named`, or
    /// `days:<N>` (1 through [`Self::MAX_DAYS`]).
    pub fn parse_cli(value: &str) -> Result<Self> {
        let trimmed = value.trim();
        match trimmed {
            "keep" => Ok(Self::KeepForever),
            "processed" => Ok(Self::DeleteImmediately {
                wait_until_named: false,
            }),
            "processed-and-named" => Ok(Self::DeleteImmediately {
                wait_until_named: true,
            }),
            other => {
                if let Some(days) = other.strip_prefix("days:")
                    && !days.is_empty()
                    && days.bytes().all(|b| b.is_ascii_digit())
                    && let Ok(days) = days.parse::<u32>()
                    && (1..=Self::MAX_DAYS).contains(&days)
                {
                    return Ok(Self::DeleteAfterDays { days });
                }
                Err(Error::InvalidInput(format!(
                    "unknown retention policy {value:?} (expected keep, processed, \
                     processed-and-named, or days:<N> with N from 1 to {})",
                    Self::MAX_DAYS
                )))
            }
        }
    }

    /// Canonical CLI spelling accepted by [`Self::parse_cli`].
    pub fn cli_value(self) -> String {
        match self {
            Self::KeepForever => "keep".to_owned(),
            Self::DeleteImmediately {
                wait_until_named: false,
            } => "processed".to_owned(),
            Self::DeleteImmediately {
                wait_until_named: true,
            } => "processed-and-named".to_owned(),
            Self::DeleteAfterDays { days } => format!("days:{days}"),
        }
    }

    /// Whether switching from `current` to `self` could delete recordings that
    /// `current` would keep (SET-01): meeting recordings or the kept voice
    /// reading (`people/*/enrollment/*.wav`).
    ///
    /// - `KeepForever` never deletes more than any policy.
    /// - `processed` deletes every processed meeting recording, and the voice
    ///   reading once its voiceprints exist, so `processed-and-named` deletes
    ///   no more. `days:<N>` deletes no more meetings but deletes the voice
    ///   reading by age, voiceprints or not, so it can delete more.
    /// - `days:<new>` is stricter than `days:<old>` when `new < old`.
    /// - Moving between `processed-and-named` and `days:<N>` in either
    ///   direction can catch recordings the other kept (old unnamed meetings
    ///   or an unconfirmed reading under `days`, recent named meetings under
    ///   `processed-and-named`).
    pub fn may_delete_more_than(self, current: Self) -> bool {
        if self == current {
            return false;
        }
        match (current, self) {
            (_, Self::KeepForever) => false,
            (
                Self::DeleteImmediately {
                    wait_until_named: false,
                },
                Self::DeleteImmediately { .. },
            ) => false,
            (Self::DeleteAfterDays { days: old }, Self::DeleteAfterDays { days: new }) => new < old,
            _ => true,
        }
    }
}

/// `new`: enrich single imports when a provider is enabled; `off`: only on
/// request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoEnrich {
    #[default]
    New,
    Off,
}

/// One provider's user settings. Requests go only to providers that are
/// enabled and consented ([`ProviderConfig::is_consented`]).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub enabled: bool,
    /// When the user accepted the provider's consent text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consented_at: Option<DateTime<FixedOffset>>,
    /// Model override (required for `openai-compatible-http`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Endpoint for HTTP providers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

impl ProviderConfig {
    /// Enabled and consented: the only state in which requests may be sent.
    pub fn is_consented(&self) -> bool {
        self.enabled && self.consented_at.is_some()
    }
}

/// Loose BCP 47 check: `en`, `pt-BR`, `zh-Hant-TW`.
pub fn valid_language_tag(tag: &str) -> bool {
    let mut parts = tag.split('-');
    let Some(first) = parts.next() else {
        return false;
    };
    (2..=3).contains(&first.len())
        && first.chars().all(|c| c.is_ascii_alphabetic())
        && parts.all(|p| (1..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// Longest per-run or configured model name accepted.
pub const MODEL_NAME_MAX_CHARS: usize = 200;

/// Checks a model name before it reaches a provider's argv or request:
/// 1–[`MODEL_NAME_MAX_CHARS`] characters, no whitespace or control
/// characters, and no leading `-` (it could read as a flag). `invalid_input`
/// otherwise.
pub fn validate_model_name(model: &str) -> Result<()> {
    let chars = model.chars().count();
    let reason = if chars == 0 {
        Some("is empty")
    } else if chars > MODEL_NAME_MAX_CHARS {
        Some("is longer than 200 characters")
    } else if model.chars().any(|c| c.is_whitespace() || c.is_control()) {
        Some("contains whitespace or control characters")
    } else if model.starts_with('-') {
        Some("starts with '-'")
    } else {
        None
    };
    match reason {
        Some(r) => Err(Error::InvalidInput(format!("model name {r}"))),
        None => Ok(()),
    }
}

/// Active model per role (design §8.3 `[profile.default]`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub vad: String,
    pub asr: String,
    pub diarization: String,
    pub speaker_embedding: String,
    /// Text-embedding model ID, or a provider ID for remote embeddings.
    pub text_embedding: String,
    /// Enrichment providers in fallback order; empty: every LLM provider
    /// that is enabled with recorded consent. With none enabled and
    /// consented, enrichment is off.
    pub llm: Vec<String>,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            vad: "silero-vad".into(),
            asr: "parakeet-tdt-0.6b-v3".into(),
            diarization: "pyannote-segmentation-3.0".into(),
            speaker_embedding: "nemo-titanet-large".into(),
            text_embedding: "potion-multilingual-128m".into(),
            llm: Vec::new(),
        }
    }
}

/// Cosine-score thresholds for one embedding model; unset means "not calibrated".
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Thresholds {
    pub auto_name: Option<f32>,
    pub suggest: Option<f32>,
    pub margin: Option<f32>,
    pub merge: Option<f32>,
    /// Same-meeting speakers at least this similar are grouped
    /// automatically (a reversible overlay, design §6.4); pairs in
    /// [`merge`](Self::merge)`..auto_merge` are only suggested. Unset: no
    /// automatic grouping. Never calibrated; set explicitly.
    pub auto_merge: Option<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerGroupingPreset {
    Standard,
    Conservative,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpeakerGrouping {
    pub preset: SpeakerGroupingPreset,
    pub merge: Option<f32>,
    pub auto_merge: Option<f32>,
}

impl SpeakerGrouping {
    pub const STANDARD: Self = Self {
        preset: SpeakerGroupingPreset::Standard,
        merge: Some(0.5),
        auto_merge: Some(0.8),
    };

    pub const CONSERVATIVE: Self = Self {
        preset: SpeakerGroupingPreset::Conservative,
        merge: Some(0.5401943),
        auto_merge: Some(0.9),
    };

    /// The UI and CLI take percentages. Missing values disable that action.
    pub fn custom(merge_percent: Option<f32>, auto_merge_percent: Option<f32>) -> Result<Self> {
        for percent in [merge_percent, auto_merge_percent].into_iter().flatten() {
            if !(0.0..=100.0).contains(&percent) {
                return Err(Error::InvalidInput(
                    "Enter a percentage from 0 to 100 for each threshold.".into(),
                ));
            }
        }
        if let (Some(merge), Some(auto_merge)) = (merge_percent, auto_merge_percent)
            && auto_merge < merge
        {
            return Err(Error::InvalidInput(
                "The automatic merge threshold must be at least the suggestion threshold.".into(),
            ));
        }
        Ok(Self {
            preset: SpeakerGroupingPreset::Custom,
            merge: merge_percent.map(|v| v / 100.0),
            auto_merge: auto_merge_percent.map(|v| v / 100.0),
        })
    }
}

impl Thresholds {
    /// Every threshold must be a finite number: scores (`auto_name`,
    /// `suggest`, `merge`, `auto_merge`) at least −1 (cosines; above 1 never
    /// matches, which turns that step off) and `margin` at least 0. NaN is an
    /// error (a NaN `merge` would merge every compatible pair). `auto_merge`
    /// is at least `merge` when both are set.
    pub fn validate(&self) -> std::result::Result<(), String> {
        let fields = [
            ("auto_name", self.auto_name, -1.0),
            ("suggest", self.suggest, -1.0),
            ("merge", self.merge, -1.0),
            ("auto_merge", self.auto_merge, -1.0),
            ("margin", self.margin, 0.0),
        ];
        for (name, value, min) in fields {
            if let Some(v) = value
                && !(v.is_finite() && v >= min)
            {
                return Err(format!("{name} = {v} must be a finite number ≥ {min}"));
            }
        }
        if let (Some(auto), Some(merge)) = (self.auto_merge, self.merge)
            && auto < merge
        {
            return Err(format!("auto_merge = {auto} must be ≥ merge = {merge}"));
        }
        Ok(())
    }

    /// `self` with `previous`'s `auto_merge`, which calibration never fits:
    /// kept while it is still ≥ the new `merge`, else dropped (no automatic
    /// grouping until it is set again).
    pub fn keep_auto_merge(mut self, previous: Option<&Thresholds>) -> Self {
        self.auto_merge = previous
            .and_then(|p| p.auto_merge)
            .filter(|a| self.merge.is_none_or(|m| *a >= m));
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SetupProgress {
    pub completed_steps: Vec<String>,
    /// The user asked to start the app without showing the assistant.
    pub defer_assistant: bool,
    /// The user's own name (`setup --name`): names a new Me person.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The user's own e-mail addresses (`setup --email`): manifest people and
    /// participants with one of them are the user.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub emails: Vec<String>,
    /// The voice-enrollment session in progress in the setup assistant, so
    /// it can resume after a restart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice_session: Option<String>,
}

impl Config {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(CONFIG_FILE)
    }

    /// Reads the config; a missing file gives the defaults (revision 0).
    /// Needs no lock: writes replace the file atomically.
    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = Self::path(data_dir);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(Error::io_at(&path, e)),
        };
        let config: Self = toml::from_str(&text)
            .map_err(|e| Error::InvalidInput(format!("{}: {e}", path.display())))?;
        config.validate(&path)?;
        Ok(config)
    }

    /// The persisted revision this value was read at or last saved as.
    /// Higher is newer; a reader can drop any config older than one it
    /// already applied.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Saves this snapshot as the next revision, under the data
    /// directory's config lock, only if `config.toml` is still at
    /// [`Config::revision()`]; `self` takes the new revision on success.
    /// A newer file fails with `config_stale` and is left untouched (so is
    /// `self`): reload, reapply the change and save again, or use
    /// [`Config::update`]. An unreadable or invalid file is never
    /// overwritten. `Config::default()` stands for a missing or
    /// pre-revision file (revision 0).
    pub fn save(&mut self, data_dir: &Path) -> Result<()> {
        let _lock = ConfigLock::acquire(data_dir)?;
        let current = Self::load(data_dir)?.revision;
        if current != self.revision {
            return Err(Error::Service {
                code: CONFIG_STALE,
                message: format!(
                    "settings changed since they were read (revision {} is now {current}); reload them and try again",
                    self.revision
                ),
            });
        }
        self.write_locked(data_dir)
    }

    /// Reads the current config under the data directory's config lock,
    /// lets `edit` change it and saves it as the next revision before the
    /// lock is released; returns what was saved and `edit`'s value. An
    /// error from `edit` saves nothing; an edit that changes nothing writes
    /// nothing and keeps the revision.
    ///
    /// The lock is not reentrant: `edit` must not save or update the config
    /// itself, take any other lock, write a library, run a model or use the
    /// network. Prepare such work before calling.
    pub fn update<T>(
        data_dir: &Path,
        edit: impl FnOnce(&mut Config) -> Result<T>,
    ) -> Result<(Config, T)> {
        let _lock = ConfigLock::acquire(data_dir)?;
        let fresh = Self::load(data_dir)?;
        let mut edited = fresh.clone();
        let value = edit(&mut edited)?;
        if edited != fresh {
            edited.write_locked(data_dir)?;
        }
        Ok((edited, value))
    }

    /// Checks what [`Config::load`] would refuse, so no write leaves an
    /// unreadable file.
    fn validate(&self, path: &Path) -> Result<()> {
        for (model, t) in &self.identity {
            t.validate().map_err(|e| {
                Error::InvalidInput(format!("{}: [identity.{model:?}] {e}", path.display()))
            })?;
        }
        Ok(())
    }

    /// Writes `self` as the next revision; the caller holds the config lock
    /// and has checked the file's revision.
    fn write_locked(&mut self, data_dir: &Path) -> Result<()> {
        let path = Self::path(data_dir);
        self.validate(&path)?;
        let previous = self.revision;
        self.revision = previous
            .checked_add(1)
            .ok_or_else(|| Error::Internal("config revision overflow".into()))?;
        let written = toml::to_string(self)
            .map_err(|e| Error::Internal(format!("serialising config: {e}")))
            .and_then(|text| fsutil::write_atomic(&path, text.as_bytes()));
        if written.is_err() {
            self.revision = previous;
        }
        written
    }

    /// Thresholds for `model`, if any were calibrated or set.
    pub fn thresholds(&self, model: &str) -> Option<&Thresholds> {
        self.identity.get(model)
    }

    pub fn speaker_grouping(&self, model: &str) -> SpeakerGrouping {
        let thresholds = self.thresholds(model).copied().unwrap_or_default();
        let custom = SpeakerGrouping {
            preset: SpeakerGroupingPreset::Custom,
            merge: thresholds.merge,
            auto_merge: thresholds.auto_merge,
        };
        if self.speaker_grouping_presets.get(model) == Some(&SpeakerGroupingPreset::Custom) {
            return custom;
        }
        [SpeakerGrouping::STANDARD, SpeakerGrouping::CONSERVATIVE]
            .into_iter()
            .find(|preset| preset.merge == custom.merge && preset.auto_merge == custom.auto_merge)
            .unwrap_or(custom)
    }

    pub fn set_speaker_grouping(&mut self, model: &str, grouping: SpeakerGrouping) {
        self.speaker_grouping_presets
            .insert(model.to_owned(), grouping.preset);
        let thresholds = self.identity.entry(model.to_owned()).or_default();
        thresholds.merge = grouping.merge;
        thresholds.auto_merge = grouping.auto_merge;
    }

    /// Saves a calibration while preserving explicit grouping choices and
    /// existing values that already match a named preset.
    pub fn set_calibrated_thresholds(&mut self, model: &str, mut fitted: Thresholds) -> Thresholds {
        let grouping = self.speaker_grouping(model);
        if grouping.preset != SpeakerGroupingPreset::Custom
            || self.speaker_grouping_presets.contains_key(model)
        {
            fitted.merge = grouping.merge;
            fitted.auto_merge = grouping.auto_merge;
        } else {
            fitted = fitted.keep_auto_merge(self.thresholds(model));
        }
        self.identity.insert(model.to_owned(), fitted);
        fitted
    }

    /// The settings of provider `id`, if any.
    pub fn provider(&self, id: &str) -> Option<&ProviderConfig> {
        self.providers.get(id)
    }

    /// Whether `broscribe mcp` may serve agents (`[mcp] enabled`, off when unset).
    pub fn mcp_enabled(&self) -> bool {
        self.mcp.enabled.unwrap_or(false)
    }

    /// Whether the agent opens the menu-bar app at start (`[app]
    /// open_at_login`, on when unset).
    pub fn open_at_login(&self) -> bool {
        self.app.open_at_login.unwrap_or(true)
    }

    pub fn show_status_item(&self) -> bool {
        self.app.show_status_item.unwrap_or(true)
    }

    /// Whether live speaker identification runs while recording (`[live]
    /// identity`, on when unset).
    pub fn live_identity_enabled(&self) -> bool {
        self.live.identity.unwrap_or(true)
    }

    /// Whether the live preview may suggest the user's own person while the
    /// speaker model is uncalibrated (`[live]
    /// allow_uncalibrated_me_suggestions`, off when unset). Suggestions
    /// only: never automatic naming or enrollment.
    pub fn allow_uncalibrated_me_suggestions(&self) -> bool {
        self.live.allow_uncalibrated_me_suggestions
    }
}

/// The data directory's config write lock: an OS lock on its own open
/// handle of `<data_dir>/config.lock`, so writers in this process and in
/// others exclude one another. Released on drop.
struct ConfigLock {
    file: File,
}

impl ConfigLock {
    /// Creates `data_dir` if needed and takes the lock, polling for up to
    /// [`CONFIG_LOCK_WAIT`], else `config_busy`.
    fn acquire(data_dir: &Path) -> Result<Self> {
        const POLL: Duration = Duration::from_millis(20);
        std::fs::create_dir_all(data_dir).map_err(|e| Error::io_at(data_dir, e))?;
        let path = data_dir.join(CONFIG_LOCK_FILE);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| Error::io_at(&path, e))?;
        let deadline = Instant::now() + CONFIG_LOCK_WAIT;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { file }),
                Err(TryLockError::WouldBlock) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(Error::Service {
                            code: CONFIG_BUSY,
                            message: format!(
                                "another writer is changing the settings in {}",
                                data_dir.display()
                            ),
                        });
                    }
                    std::thread::sleep(POLL.min(deadline - now));
                }
                Err(TryLockError::Error(e)) => return Err(Error::io_at(&path, e)),
            }
        }
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speaker_grouping_persists_per_model_and_survives_calibration() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        let model = config.profile.speaker_embedding.clone();
        let existing = config.identity.entry(model.clone()).or_default();
        existing.merge = SpeakerGrouping::CONSERVATIVE.merge;
        existing.auto_merge = SpeakerGrouping::CONSERVATIVE.auto_merge;
        existing.auto_name = Some(0.9);
        assert_eq!(
            config.speaker_grouping(&model),
            SpeakerGrouping::CONSERVATIVE
        );
        let calibration = Thresholds {
            auto_name: Some(0.85),
            merge: Some(0.95),
            ..Thresholds::default()
        };
        let mut legacy = config.clone();
        legacy.set_calibrated_thresholds(&model, calibration);
        assert_eq!(
            legacy.speaker_grouping(&model),
            SpeakerGrouping::CONSERVATIVE
        );

        let custom = SpeakerGrouping::custom(Some(50.0), Some(80.0)).unwrap();
        config.set_speaker_grouping(&model, custom);
        assert_eq!(config.thresholds(&model).unwrap().auto_name, Some(0.9));
        config.save(dir.path()).unwrap();
        let mut config = Config::load(dir.path()).unwrap();
        assert_eq!(config.speaker_grouping(&model), custom);
        assert_eq!(config.speaker_grouping("other").auto_merge, None);

        let fitted = config.set_calibrated_thresholds(&model, calibration);
        assert_eq!(fitted.auto_name, Some(0.85));
        assert_eq!(config.speaker_grouping(&model), custom);
        assert!(SpeakerGrouping::custom(Some(80.0), Some(79.9)).is_err());
        assert!(SpeakerGrouping::custom(None, Some(f32::NAN)).is_err());
    }

    #[test]
    fn model_names_are_validated() {
        for ok in ["gpt-5", "sonnet", "x-ai/grok-4.1", &"m".repeat(200)] {
            validate_model_name(ok).unwrap();
        }
        for bad in ["", "-x", "a b", "a\tb", "a\u{7}b", "a\nb", &"m".repeat(201)] {
            let e = validate_model_name(bad).unwrap_err();
            assert_eq!(e.code(), "invalid_input", "{bad:?}");
        }
    }

    #[test]
    fn integration_settings_survive_unrelated_config_save() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(Config::path(dir.path()), "[integration.google]\nclient_id = 'synthetic-client'\nfuture_setting = true\n[detect.policies]\nteams = 'ask'\n").unwrap();
        let mut config = Config::load(dir.path()).unwrap();
        config.default_language = Some("pt-BR".into());
        config.save(dir.path()).unwrap();
        let saved = Config::load(dir.path()).unwrap();
        assert_eq!(
            saved.integration["google"]["client_id"].as_str(),
            Some("synthetic-client")
        );
        assert_eq!(
            saved.integration["google"]["future_setting"].as_bool(),
            Some(true)
        );
        assert_eq!(saved.detect.policies["teams"], "ask");
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let c = Config::load(dir.path()).unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.profile.speaker_embedding, "nemo-titanet-large");
        assert_eq!(c.import_handling, Handling::Copy);
    }

    #[test]
    fn mcp_defaults_off_and_open_at_login_defaults_on() {
        let dir = tempfile::tempdir().unwrap();
        let mut defaults = Config::load(dir.path()).unwrap();
        assert!(!defaults.mcp_enabled());
        assert!(defaults.open_at_login());
        defaults.save(dir.path()).unwrap();
        let text = std::fs::read_to_string(Config::path(dir.path())).unwrap();
        assert!(!text.contains("[mcp]") && !text.contains("[app]"), "{text}");

        let mut config = defaults;
        config.mcp.enabled = Some(false);
        config.app.open_at_login = Some(false);
        config.save(dir.path()).unwrap();
        let saved = Config::load(dir.path()).unwrap();
        assert!(!saved.mcp_enabled());
        assert!(!saved.open_at_login());
        assert_eq!(saved, config);

        std::fs::write(
            Config::path(dir.path()),
            "[mcp]\nenabled = true\n[app]\nopen_at_login = true\n",
        )
        .unwrap();
        let explicit = Config::load(dir.path()).unwrap();
        assert_eq!(explicit.mcp.enabled, Some(true));
        assert_eq!(explicit.app.open_at_login, Some(true));
    }

    #[test]
    fn live_identity_defaults_on_round_trips_and_rejects_non_booleans() {
        let dir = tempfile::tempdir().unwrap();
        let mut defaults = Config::load(dir.path()).unwrap();
        assert!(defaults.live_identity_enabled());
        defaults.save(dir.path()).unwrap();
        let text = std::fs::read_to_string(Config::path(dir.path())).unwrap();
        assert!(!text.contains("[live]"), "{text}");

        let mut config = defaults;
        config.live.identity = Some(false);
        config.save(dir.path()).unwrap();
        let saved = Config::load(dir.path()).unwrap();
        assert!(!saved.live_identity_enabled());
        assert_eq!(saved, config);

        std::fs::write(Config::path(dir.path()), "[live]\nidentity = \"off\"\n").unwrap();
        assert_eq!(
            Config::load(dir.path()).unwrap_err().code(),
            "invalid_input"
        );
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let mut c = Config {
            library: Some("/tmp/lib".into()),
            default_language: Some("pt-BR".into()),
            import_handling: Handling::Reference,
            hotwords: vec!["Kubernetes".into()],
            ..Config::default()
        };
        c.identity.insert(
            "nemo-titanet-large".into(),
            Thresholds {
                auto_name: Some(0.7),
                suggest: Some(0.5),
                margin: Some(0.1),
                merge: None,
                auto_merge: Some(0.9),
            },
        );
        c.setup.completed_steps.push("language".into());
        c.setup.voice_session = Some("v-1".into());
        c.retention = Retention::DeleteImmediately {
            wait_until_named: true,
        };
        c.auto_enrich = AutoEnrich::Off;
        c.profile.llm = vec!["claude-cli".into()];
        c.providers.insert(
            "claude-cli".into(),
            ProviderConfig {
                enabled: true,
                consented_at: Some(
                    DateTime::parse_from_rfc3339("2026-11-14T10:00:00-03:00").unwrap(),
                ),
                model: None,
                base_url: None,
            },
        );
        c.save(&data).unwrap();
        assert_eq!(Config::load(&data).unwrap(), c);
        assert!(c.provider("claude-cli").unwrap().is_consented());
        assert!(c.provider("codex-cli").is_none());
    }

    #[test]
    fn retention_toml_shapes() {
        let dir = tempfile::tempdir().unwrap();
        for (text, want) in [
            ("", Retention::KeepForever),
            (
                "[retention]\npolicy = \"keep_forever\"\n",
                Retention::KeepForever,
            ),
            (
                "[retention]\npolicy = \"delete_immediately\"\n",
                Retention::DeleteImmediately {
                    wait_until_named: false,
                },
            ),
            (
                "[retention]\npolicy = \"delete_after_days\"\ndays = 30\n",
                Retention::DeleteAfterDays { days: 30 },
            ),
        ] {
            std::fs::write(Config::path(dir.path()), text).unwrap();
            let c = Config::load(dir.path()).unwrap();
            assert_eq!(c.retention, want, "{text}");
            assert_eq!(c.auto_enrich, AutoEnrich::New);
        }
        std::fs::write(
            Config::path(dir.path()),
            "[retention]\npolicy = \"delete_after_days\"\n",
        )
        .unwrap();
        assert_eq!(
            Config::load(dir.path()).unwrap_err().code(),
            "invalid_input"
        );
    }

    #[test]
    fn retention_cli_round_trip_and_strictness_matrix() {
        for (raw, want) in [
            ("keep", Retention::KeepForever),
            (
                "processed",
                Retention::DeleteImmediately {
                    wait_until_named: false,
                },
            ),
            (
                "processed-and-named",
                Retention::DeleteImmediately {
                    wait_until_named: true,
                },
            ),
            ("days:1", Retention::DeleteAfterDays { days: 1 }),
            ("days:30", Retention::DeleteAfterDays { days: 30 }),
        ] {
            assert_eq!(Retention::parse_cli(raw).unwrap(), want);
            assert_eq!(want.cli_value(), raw);
        }
        for bad in [
            "",
            "forever",
            "keep_forever",
            "days:",
            "days:0",
            "days:-5",
            "days:+5",
            "days:abc",
            "days:999999999999",
        ] {
            assert_eq!(
                Retention::parse_cli(bad).unwrap_err().code(),
                "invalid_input",
                "{bad}"
            );
        }

        let keep = Retention::KeepForever;
        let processed = Retention::DeleteImmediately {
            wait_until_named: false,
        };
        let named = Retention::DeleteImmediately {
            wait_until_named: true,
        };
        let days30 = Retention::DeleteAfterDays { days: 30 };
        let days7 = Retention::DeleteAfterDays { days: 7 };
        let days60 = Retention::DeleteAfterDays { days: 60 };

        // Equal or looser transitions never need confirmation.
        for (saved, proposed) in [
            (keep, keep),
            (processed, keep),
            (processed, processed),
            (processed, named),
            (named, keep),
            (named, named),
            (days30, keep),
            (days30, days30),
            (days30, days60),
        ] {
            assert!(
                !proposed.may_delete_more_than(saved),
                "expected {saved:?} -> {proposed:?} not to delete more"
            );
        }

        // Stricter or incomparable transitions can delete more recordings.
        for (saved, proposed) in [
            (keep, processed),
            (keep, named),
            (keep, days30),
            (named, processed),
            (named, days30),
            // `days` deletes a voice reading by age, voiceprints or not;
            // `processed` keeps an unconfirmed one.
            (processed, days30),
            (days30, days7),
            (days30, processed),
            (days30, named),
        ] {
            assert!(
                proposed.may_delete_more_than(saved),
                "expected {saved:?} -> {proposed:?} to require confirmation"
            );
        }
    }

    #[test]
    fn unconsented_provider_is_not_usable() {
        let enabled_only = ProviderConfig {
            enabled: true,
            ..ProviderConfig::default()
        };
        assert!(!enabled_only.is_consented());
        let consented_but_off = ProviderConfig {
            consented_at: Some(crate::schema::now()),
            ..ProviderConfig::default()
        };
        assert!(!consented_but_off.is_consented());
    }

    #[test]
    fn partial_file_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            Config::path(dir.path()),
            "import_handling = \"move\"\n[profile]\nspeaker_embedding = \"nemo-titanet-small\"\n",
        )
        .unwrap();
        let c = Config::load(dir.path()).unwrap();
        assert_eq!(c.import_handling, Handling::Move);
        assert_eq!(c.profile.speaker_embedding, "nemo-titanet-small");
        assert_eq!(c.profile.asr, "parakeet-tdt-0.6b-v3");
    }

    #[test]
    fn nan_or_out_of_range_thresholds_are_rejected_at_load() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "merge = nan",
            "auto_name = -1.5",
            "margin = -0.1",
            "suggest = inf",
        ] {
            std::fs::write(
                Config::path(dir.path()),
                format!("[identity.\"nemo-titanet-large\"]\n{bad}\n"),
            )
            .unwrap();
            let err = Config::load(dir.path()).unwrap_err();
            assert_eq!(err.code(), "invalid_input", "{bad}");
            assert!(err.to_string().contains("nemo-titanet-large"), "{err}");
        }
        std::fs::write(
            Config::path(dir.path()),
            "[identity.\"nemo-titanet-large\"]\nauto_name = 0.7\nmargin = 0.1\nmerge = 0.8\n",
        )
        .unwrap();
        assert_eq!(
            Config::load(dir.path())
                .unwrap()
                .thresholds("nemo-titanet-large")
                .unwrap()
                .merge,
            Some(0.8)
        );
    }

    #[test]
    fn audio_check_choice_is_per_pair_and_data_source_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Config::default();
        c.integration
            .insert("synthetic".into(), toml::Value::Boolean(true));
        let choice = |source: Option<&str>, handling: &str| AudioCheckChoice {
            input_uid: "mic".into(),
            output_uid: "out".into(),
            output_data_source: source.map(str::to_owned),
            echo_handling: handling.into(),
        };
        c.set_audio_check(choice(Some("Speakers"), "residue_gating"));
        c.set_audio_check(choice(Some("Headphones"), "none"));
        c.set_audio_check(choice(Some("Speakers"), "full_aec"));
        assert_eq!(c.audio_checks.len(), 2);
        c.save(dir.path()).unwrap();
        let loaded = Config::load(dir.path()).unwrap();
        assert_eq!(loaded, c);
        assert_eq!(
            loaded.audio_check_for("mic", "out", Some("Speakers")),
            Some("full_aec")
        );
        assert_eq!(
            loaded.audio_check_for("mic", "out", Some("Headphones")),
            Some("none")
        );
        assert_eq!(loaded.audio_check_for("mic", "out", None), None);
        assert_eq!(
            loaded.audio_check_for("other", "out", Some("Speakers")),
            None
        );
    }

    fn hotwords(config: &Config) -> Vec<String> {
        let mut words = config.hotwords.clone();
        words.sort();
        words
    }

    #[test]
    fn concurrent_writers_with_different_libraries_lose_no_change() {
        use std::sync::{Arc, Barrier};

        let data = tempfile::tempdir().unwrap();
        let libraries = tempfile::tempdir().unwrap();
        const WRITERS: usize = 8;
        let barrier = Arc::new(Barrier::new(WRITERS));
        let handles: Vec<_> = (0..WRITERS)
            .map(|i| {
                let data = data.path().to_path_buf();
                let library = libraries.path().join(format!("library-{i}"));
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    std::fs::create_dir_all(&library).unwrap();
                    // Each writer holds its own library's lock: the config
                    // lock belongs to the data directory, not to a library.
                    let _library =
                        crate::lock::LibraryLock::acquire(&library, Duration::ZERO).unwrap();
                    barrier.wait();
                    let word = format!("w{i}");
                    if i % 2 == 0 {
                        let (saved, ()) = Config::update(&data, |c| {
                            c.library = Some(library.clone());
                            c.hotwords.push(word);
                            Ok(())
                        })
                        .unwrap();
                        return saved.revision();
                    }
                    // Snapshot writers reload and reapply after a stale save.
                    for _ in 0..1000 {
                        let mut snapshot = Config::load(&data).unwrap();
                        snapshot.library = Some(library.clone());
                        snapshot.hotwords.push(word.clone());
                        match snapshot.save(&data) {
                            Ok(()) => return snapshot.revision(),
                            Err(e) => assert_eq!(e.code(), CONFIG_STALE),
                        }
                    }
                    panic!("writer {i} never saved");
                })
            })
            .collect();
        let mut revisions: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        revisions.sort_unstable();
        assert_eq!(revisions, (1..=WRITERS as u64).collect::<Vec<_>>());
        let saved = Config::load(data.path()).unwrap();
        assert_eq!(saved.revision(), WRITERS as u64);
        let mut want: Vec<String> = (0..WRITERS).map(|i| format!("w{i}")).collect();
        want.sort();
        assert_eq!(hotwords(&saved), want);
    }

    #[test]
    fn stale_snapshots_are_refused_without_touching_file_or_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = Config::path(dir.path());
        std::fs::write(
            &path,
            "hotwords = ['legacy']\n[integration.google]\nclient_id = 'synthetic-client'\n",
        )
        .unwrap();
        let mut first = Config::load(dir.path()).unwrap();
        let mut second = Config::load(dir.path()).unwrap();
        assert_eq!((first.revision(), second.revision()), (0, 0));

        first.default_language = Some("pt-BR".into());
        first.save(dir.path()).unwrap();
        assert_eq!(first.revision(), 1);
        let after_first = std::fs::read_to_string(&path).unwrap();

        second.hotwords.push("lost?".into());
        let err = second.save(dir.path()).unwrap_err();
        assert_eq!(err.code(), CONFIG_STALE);
        assert_eq!(second.revision(), 0, "a refused save keeps the snapshot");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), after_first);
        assert_eq!(
            Config::default().save(dir.path()).unwrap_err().code(),
            CONFIG_STALE,
            "a never-read default never overwrites a saved file"
        );

        let (updated, ()) = Config::update(dir.path(), |c| {
            c.hotwords.push("kept".into());
            Ok(())
        })
        .unwrap();
        assert_eq!(updated.revision(), 2);
        assert_eq!(
            first.save(dir.path()).unwrap_err().code(),
            CONFIG_STALE,
            "a snapshot older than a fresh update is stale too"
        );
        let saved = Config::load(dir.path()).unwrap();
        assert_eq!(saved, updated);
        assert_eq!(saved.default_language.as_deref(), Some("pt-BR"));
        assert_eq!(hotwords(&saved), ["kept", "legacy"]);
        assert_eq!(
            saved.integration["google"]["client_id"].as_str(),
            Some("synthetic-client")
        );
    }

    #[test]
    fn revisions_are_persisted_in_write_order_and_only_real_writes_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = Config::path(dir.path());
        let (unchanged, ()) = Config::update(dir.path(), |_| Ok(())).unwrap();
        assert_eq!(unchanged.revision(), 0);
        assert!(
            !path.exists(),
            "an edit that changes nothing writes nothing"
        );

        std::fs::write(&path, "hotwords = ['a']\n").unwrap();
        let mut legacy = Config::load(dir.path()).unwrap();
        assert_eq!(legacy.revision(), 0, "files without a revision read as 0");
        legacy.hotwords.push("b".into());
        legacy.save(dir.path()).unwrap();
        assert_eq!(Config::load(dir.path()).unwrap().revision(), 1);

        let (second, n) = Config::update(dir.path(), |c| {
            c.hotwords.push("c".into());
            Ok(c.hotwords.len())
        })
        .unwrap();
        assert_eq!((second.revision(), n), (2, 3));
        let (same, ()) = Config::update(dir.path(), |_| Ok(())).unwrap();
        assert_eq!(same.revision(), 2);

        let before = std::fs::read_to_string(&path).unwrap();
        let err = Config::update(dir.path(), |c| {
            c.hotwords.push("never".into());
            Err::<(), _>(Error::InvalidInput("synthetic refusal".into()))
        })
        .unwrap_err();
        assert_eq!(err.code(), "invalid_input");
        let err = Config::update(dir.path(), |c| {
            c.identity.insert(
                "nemo-titanet-large".into(),
                Thresholds {
                    merge: Some(f32::NAN),
                    ..Thresholds::default()
                },
            );
            Ok(())
        })
        .unwrap_err();
        assert_eq!(
            err.code(),
            "invalid_input",
            "no write leaves an unreadable file"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        let mut next = Config::load(dir.path()).unwrap();
        next.consent_reminder = true;
        next.save(dir.path()).unwrap();
        assert_eq!(next.revision(), 3);
        assert_eq!(Config::load(dir.path()).unwrap(), next);

        std::fs::write(&path, "revision = -1\n").unwrap();
        assert_eq!(
            Config::load(dir.path()).unwrap_err().code(),
            "invalid_input"
        );
        let malformed = "[live]\nidentity = \"off\"\n";
        std::fs::write(&path, malformed).unwrap();
        assert_eq!(
            Config::default().save(dir.path()).unwrap_err().code(),
            "invalid_input",
            "an unreadable file is never overwritten"
        );
        assert_eq!(
            Config::update(dir.path(), |_| Ok(())).unwrap_err().code(),
            "invalid_input"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), malformed);
    }

    #[test]
    fn unrelated_integration_and_update_settings_survive_fresh_edits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            Config::path(dir.path()),
            "[integration.microsoft-graph]\nclient_id = 'synthetic-client'\nfuture_setting = 3\n\
             [updates]\nenabled = true\nfeed_url = 'https://example.org/feed.json'\nchannel = 'beta'\n\
             trusted_keys = ['00']\n",
        )
        .unwrap();
        let (saved, ()) = Config::update(dir.path(), |c| {
            c.live.identity = Some(false);
            Ok(())
        })
        .unwrap();
        let loaded = Config::load(dir.path()).unwrap();
        assert_eq!(loaded, saved);
        assert!(!loaded.live_identity_enabled());
        let graph = &loaded.integration["microsoft-graph"];
        assert_eq!(graph["client_id"].as_str(), Some("synthetic-client"));
        assert_eq!(graph["future_setting"].as_integer(), Some(3));
        assert_eq!(
            loaded.updates,
            UpdateSettings {
                enabled: true,
                feed_url: Some("https://example.org/feed.json".into()),
                channel: "beta".into(),
                trusted_keys: vec!["00".into()],
                signer: None,
            }
        );
    }

    #[test]
    fn new_settings_default_off_round_trip_and_reject_malformed_values() {
        let dir = tempfile::tempdir().unwrap();
        let mut defaults = Config::load(dir.path()).unwrap();
        assert!(!defaults.allow_uncalibrated_me_suggestions());
        assert_eq!(defaults.appearance.reading_size, ReadingSize::Standard);
        assert_eq!(defaults.appearance.theme, ThemeChoice::System);
        let updates = &defaults.updates;
        assert!(updates.enabled);
        assert!(updates.uses_built_in_source());
        assert_eq!(updates.effective_feed(), RELEASE_FEED);
        assert!(updates.effective_keys().next().is_some());
        assert_eq!(updates.feed_url, None);
        assert!(updates.trusted_keys.is_empty());
        assert_eq!(updates.signer, None);
        assert_eq!(updates.channel, "stable");
        defaults.save(dir.path()).unwrap();
        let text = std::fs::read_to_string(Config::path(dir.path())).unwrap();
        for table in ["[live]", "[appearance]", "[updates]"] {
            assert!(!text.contains(table), "{text}");
        }

        let mut config = defaults;
        config.live.allow_uncalibrated_me_suggestions = true;
        config.appearance.reading_size = ReadingSize::Larger;
        config.appearance.theme = ThemeChoice::Dark;
        config.updates.enabled = false;
        config.updates.signer = Some("SYNTHETIC1".into());
        config.save(dir.path()).unwrap();
        let saved = Config::load(dir.path()).unwrap();
        assert_eq!(saved, config);
        assert!(saved.allow_uncalibrated_me_suggestions());
        assert!(
            saved.live_identity_enabled(),
            "the Me policy is separate from the identity switch"
        );

        for bad in [
            "[appearance]\nreading_size = \"huge\"\n",
            "[appearance]\ntheme = \"sepia\"\n",
            "[live]\nallow_uncalibrated_me_suggestions = \"yes\"\n",
            "[updates]\nenabled = \"true\"\n",
            "[updates]\ntrusted_key = ['00']\n",
        ] {
            std::fs::write(Config::path(dir.path()), bad).unwrap();
            assert_eq!(
                Config::load(dir.path()).unwrap_err().code(),
                "invalid_input",
                "{bad}"
            );
        }

        assert_eq!(ReadingSize::Standard.larger(), Some(ReadingSize::Large));
        assert_eq!(ReadingSize::Standard.smaller(), Some(ReadingSize::Small));
        assert_eq!(ReadingSize::Small.smaller(), None);
        assert_eq!(ReadingSize::Largest.larger(), None);
        assert!(
            ReadingSize::ALL
                .windows(2)
                .all(|pair| pair[0].scale() < pair[1].scale())
        );
    }

    /// ACC-31: the five steps keep their saved names and draw at 87.5, 100,
    /// 125, 150 and 200 %, so a config saved before the change loads as the
    /// same step and an update through `Config::update` keeps other tables.
    #[test]
    fn reading_size_steps_reach_200_percent_and_old_files_still_load() {
        assert_eq!(
            ReadingSize::ALL.map(ReadingSize::scale),
            [0.875, 1.0, 1.25, 1.5, 2.0]
        );
        let dir = tempfile::tempdir().unwrap();
        for (name, size) in [
            ("small", ReadingSize::Small),
            ("standard", ReadingSize::Standard),
            ("large", ReadingSize::Large),
            ("larger", ReadingSize::Larger),
            ("largest", ReadingSize::Largest),
        ] {
            std::fs::write(
                Config::path(dir.path()),
                format!(
                    "[appearance]\nreading_size = \"{name}\"\n\n[live]\nallow_uncalibrated_me_suggestions = true\n"
                ),
            )
            .unwrap();
            let loaded = Config::load(dir.path()).unwrap();
            assert_eq!(loaded.appearance.reading_size, size, "{name}");
            let (saved, _) = Config::update(dir.path(), |config| {
                config.appearance.reading_size = ReadingSize::Largest;
                Ok(())
            })
            .unwrap();
            assert_eq!(saved.appearance.reading_size, ReadingSize::Largest);
            assert!(saved.allow_uncalibrated_me_suggestions(), "{name}");
        }
    }
}
