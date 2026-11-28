//! Request fields shared by the command routers.

use super::*;

pub(super) fn switch(enabled: bool) -> SwitchArg {
    if enabled {
        SwitchArg::On
    } else {
        SwitchArg::Off
    }
}

pub(super) fn on_off(enabled: bool) -> OnOff {
    if enabled { OnOff::On } else { OnOff::Off }
}

pub(super) fn session(id: String) -> SessionArg {
    SessionArg { session: Some(id) }
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum AppSetting {
    OpenAtLogin,
    StatusItem,
    PanelCompact,
    HideControlsFromScreenSharing,
    LiveIdentity,
    UncalibratedMeSuggestions,
}

impl AppSetting {
    pub(super) fn command(self, state: SwitchArg) -> Command {
        let action = match self {
            Self::OpenAtLogin => AppCmd::OpenAtLogin { state },
            Self::StatusItem => AppCmd::StatusItem { state },
            Self::PanelCompact => AppCmd::PanelCompact { state },
            Self::HideControlsFromScreenSharing => AppCmd::HideControlsFromScreenSharing { state },
            Self::LiveIdentity => AppCmd::LiveIdentity { state },
            Self::UncalibratedMeSuggestions => AppCmd::UncalibratedMeSuggestions { state },
        };
        Command::App { action }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum CompletableStep {
    MeetingApps,
    Summaries,
    Assistant,
}

impl CompletableStep {
    pub(super) fn into_cli(self) -> SetupCmd {
        match self {
            Self::MeetingApps => SetupCmd::MeetingApps {
                app: None,
                policy: None,
            },
            Self::Summaries => SetupCmd::Summaries,
            Self::Assistant => SetupCmd::Finish,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum GroupingPreset {
    Standard,
    Conservative,
    Custom {
        /// Suggest grouping at this percentage (0–100). Omit to disable suggestions.
        suggest_at: Option<f32>,
        /// Automatically group at this percentage (0–100). Omit to disable automatic grouping.
        auto_merge_at: Option<f32>,
    },
}

impl GroupingPreset {
    pub(super) fn into_cli(self) -> SpeakerGroupingCmd {
        match self {
            Self::Standard => SpeakerGroupingCmd::Standard,
            Self::Conservative => SpeakerGroupingCmd::Conservative,
            Self::Custom {
                suggest_at,
                auto_merge_at,
            } => SpeakerGroupingCmd::Custom {
                suggest_at,
                auto_merge_at,
            },
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum AudioSetup {
    Headphones,
    SpeakerphoneAec,
    SpeakersMic,
    BluetoothCall,
    HybridRoom,
    InPerson,
}

impl From<AudioSetup> for broscribe_dsp::Setup {
    fn from(setup: AudioSetup) -> Self {
        match setup {
            AudioSetup::Headphones => Self::Headphones,
            AudioSetup::SpeakerphoneAec => Self::SpeakerphoneAec,
            AudioSetup::SpeakersMic => Self::SpeakersMic,
            AudioSetup::BluetoothCall => Self::BluetoothCall,
            AudioSetup::HybridRoom => Self::HybridRoom,
            AudioSetup::InPerson => Self::InPerson,
        }
    }
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Fingerprint {
    /// Track of the segment from the saved transcript.
    pub(super) track: String,
    /// Exact start offset in seconds.
    pub(super) start: f64,
    /// Exact end offset in seconds.
    pub(super) end: f64,
    /// Lowercase hexadecimal SHA-256 of the segment text.
    pub(super) text_sha256: String,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TranscriptFence {
    /// Speaker namespace shown in the meeting record.
    pub(super) speaker_namespace: String,
    /// Grouping revision shown in the meeting record.
    pub(super) grouping_revision: String,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TemplateBaseline {
    /// Template ID from the displayed definition.
    pub(super) id: String,
    /// Displayed name.
    pub(super) name: String,
    /// Displayed description.
    pub(super) description: String,
    /// Displayed instructions.
    pub(super) body: String,
    /// Whether this is a built-in template.
    pub(super) builtin: bool,
    /// Whether the built-in template has a user override.
    pub(super) overridden: bool,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum CalendarRoute {
    Own {
        /// ID from the account's calendar catalog.
        calendar_id: String,
    },
    RecipientShared {
        /// Shared calendar ID from the catalog.
        calendar_id: String,
    },
    Delegated {
        /// Delegating mailbox from the catalog.
        mailbox: String,
        /// Named calendar, or omit for the mailbox's default calendar.
        calendar_id: Option<String>,
    },
    Group {
        /// Group ID from the catalog.
        group_id: String,
    },
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct EnrollVoiceInput {
    /// A saved meeting where you speak. Requires speaker_id.
    pub(super) meeting_id: Option<String>,
    /// Your speaker in the saved meeting.
    pub(super) speaker_id: Option<String>,
    /// Capture the passage and confirmation from the microphone.
    #[serde(default)]
    pub(super) microphone: bool,
    /// Microphone capture duration in seconds (up to 45).
    pub(super) seconds: Option<f64>,
    /// Optionally pin the input device ID for this enrollment.
    pub(super) device: Option<String>,
    /// Keep a local copy of the passage reading in the person profile.
    #[serde(default)]
    pub(super) keep_reading: bool,
    /// Fence the write to this setup voice session if the app owns one.
    pub(super) session_id: Option<String>,
    /// Passage language (BCP 47).
    pub(super) language: Option<String>,
    /// Display name for the Me person.
    pub(super) name: Option<String>,
    /// E-mail addresses for the Me person.
    #[serde(default)]
    pub(super) emails: Vec<String>,
    /// Record the enrollment. Without it the tool only returns the plan.
    #[serde(default)]
    pub(super) confirm: bool,
}

impl EnrollVoiceInput {
    /// What a confirmed call would do, returned while `confirm` is false.
    pub(super) fn plan(&self) -> Value {
        let source = if let Some(meeting) = &self.meeting_id {
            json!({"kind": "meeting", "meeting_id": meeting, "speaker_id": self.speaker_id})
        } else {
            json!({"kind": "microphone", "seconds": self.seconds, "device": self.device})
        };
        json!({
            "confirmed": false,
            "plan": {
                "source": source,
                "keeps_reading": self.keep_reading,
                "name": self.name,
                "emails": self.emails,
            },
            "next": "Nothing was recorded or saved. Call enroll_voice again with confirm: true to enroll this voice as yours.",
        })
    }

    pub(super) fn validate(&self) -> Result<()> {
        let meeting = self.meeting_id.is_some();
        let mic = self.microphone;
        if meeting == mic
            || meeting != self.speaker_id.is_some()
            || (!mic && self.seconds.is_some())
            || (meeting && self.device.is_some())
            || self.seconds.is_some_and(|s| !(1.0..=45.0).contains(&s))
        {
            return Err(Error::InvalidInput(
                "choose one enrollment source and supply options that apply to it".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn into_voice(self) -> VoiceArgs {
        VoiceArgs {
            mic: self.microphone,
            from: self.meeting_id,
            speaker: self.speaker_id,
            device: self.device,
            seconds: self.seconds.unwrap_or(30.0),
            session: self.session_id,
            keep_as_enrollment: self.keep_reading,
            lang: self.language,
            name: self.name,
            emails: self.emails,
            ..Default::default()
        }
    }
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct PersonRevisionInput {
    /// Revision of the complete profile.
    pub(super) revision: String,
    /// Name field hash.
    pub(super) name: String,
    /// Aliases field hash.
    pub(super) aliases: String,
    /// E-mail addresses field hash.
    pub(super) emails: String,
    /// Notes field hash.
    pub(super) notes: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct MeetingInput {
    /// Meeting ID (m-…).
    pub(super) meeting_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct PersonInput {
    /// Person ID, e-mail, alias or name.
    pub(super) person: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SpeakerInput {
    /// Meeting ID (m-…).
    pub(super) meeting_id: String,
    /// Speaker shown in the meeting (S1, S2, …).
    pub(super) speaker_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ProviderInput {
    /// Provider ID from list_providers.
    pub(super) id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TemplateInputId {
    /// Template ID from list_templates.
    pub(super) template_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct IntegrationFilter {
    /// Limit to this calendar integration.
    pub(super) integration: Option<IntegrationArg>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SessionInput {
    /// Exact recording session ID from get_recording_status or get_daemon_status.
    pub(super) session_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct OptionalMeetingInput {
    /// Limit to this meeting ID.
    pub(super) meeting_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ModelHealthInput {
    /// Also run the live models on a synthetic fixture.
    #[serde(default)]
    pub(super) live: bool,
    /// Profile for the live check. Defaults to active.
    pub(super) profile: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct FindTranscriptInput {
    /// Meeting ID (m-…).
    pub(super) meeting_id: String,
    /// Literal text or speaker label to find.
    pub(super) query: String,
    /// Limit to this speaker or person.
    pub(super) speaker: Option<String>,
    /// Start time in seconds or HH:MM:SS.
    pub(super) from: Option<String>,
    /// End time in seconds or HH:MM:SS.
    pub(super) to: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ReaderDumpInput {
    /// Running app ID. Supply exactly one of app or pid.
    pub(super) app: Option<String>,
    /// Running process ID. Supply exactly one of pid or app.
    pub(super) pid: Option<u32>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct PassageInput {
    /// Passage language (BCP 47), defaulting to the configured language.
    pub(super) language: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct CalendarCatalogInput {
    /// Calendar integration.
    pub(super) integration: IntegrationArg,
    /// Exact corporate account ID. Required for Microsoft.
    pub(super) account: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LoginAttemptInput {
    /// Calendar integration.
    pub(super) integration: IntegrationArg,
    /// Caller-chosen attempt ID used when signing in.
    pub(super) attempt: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct DirectoryInput {
    /// Name or e-mail to look up.
    pub(super) query: String,
    /// Limit the search to one integration.
    pub(super) integration: Option<IntegrationArg>,
    /// Exact qualified corporate account (provider/account ID).
    pub(super) account: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AppSettingInput {
    /// Name of the desktop or live setting.
    pub(super) setting: AppSetting,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct StartRecordingInput {
    /// Detected meeting app ID, if applicable.
    pub(super) app: Option<String>,
    /// User-supplied title.
    pub(super) title: Option<String>,
    /// Override the detected audio setup.
    pub(super) setup: Option<AudioSetup>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct BookmarkRecordingInput {
    /// Exact session ID from the recording or agent status.
    pub(super) session_id: String,
    /// Optional bookmark label.
    pub(super) label: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct NoteRecordingInput {
    /// Exact session ID from the recording or agent status.
    pub(super) session_id: String,
    /// Note text to save.
    pub(super) text: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LangRecordingInput {
    /// Exact session ID from the recording or agent status.
    pub(super) session_id: String,
    /// Language tag (BCP 47).
    pub(super) language: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TagRecordingInput {
    /// Exact session ID from the recording or agent status.
    pub(super) session_id: String,
    /// Tag changes (+tag or tag adds, -tag removes).
    #[serde(default)]
    pub(super) changes: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ParticipantRecordingInput {
    /// Exact session ID from the recording or agent status.
    pub(super) session_id: String,
    /// Confirm or remove this participant.
    pub(super) action: ParticipantArg,
    /// Person ID, registered e-mail or name.
    pub(super) person: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TitleRecordingInput {
    /// Exact session ID from the recording or agent status.
    pub(super) session_id: String,
    /// One nonempty line.
    pub(super) title: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AutoStopRecordingInput {
    /// Exact session ID from the recording or agent status.
    pub(super) session_id: String,
    /// Whether to stop and save automatically.
    pub(super) enabled: bool,
    /// Freshly detected app ID. Requires enabled.
    pub(super) app: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct RecordingPromptInput {
    /// Pending prompt ID from list_recording_prompts.
    pub(super) prompt_id: String,
    /// Record or ignore the detected meeting.
    pub(super) action: RecordingAnswer,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LiveAttributeInput {
    /// Exact session ID from the recording or agent status.
    pub(super) session_id: String,
    /// Turn from get_live_preview.
    pub(super) turn_id: String,
    /// The turn revision.
    pub(super) revision: u64,
    /// The identity epoch of that turn.
    pub(super) epoch: u64,
    /// The turn’s evidence receipt.
    pub(super) evidence: String,
    /// Name or person reference for a manual naming.
    pub(super) name: Option<String>,
    /// Person ID of the suggestion to confirm.
    pub(super) confirm: Option<String>,
    /// Clear a previously applied live name.
    #[serde(default)]
    pub(super) clear: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LiveDismissInput {
    /// Meeting containing the reviewed line.
    pub(super) meeting_id: String,
    /// Decision from list_live_speaker_reviews.
    pub(super) decision: u64,
    /// Exact revision key from the same item.
    pub(super) review_key: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TranscriptAttributeInput {
    /// Meeting containing the segments.
    pub(super) meeting_id: String,
    /// Fingerprints of the exact segments as reviewed.
    #[serde(default)]
    pub(super) fingerprints: Vec<Fingerprint>,
    /// Person ID, e-mail or name. Omit when clearing.
    pub(super) person: Option<String>,
    /// Restore the speaker attribution for these lines.
    #[serde(default)]
    pub(super) clear: bool,
    /// Optional namespace and grouping revision from the displayed meeting.
    pub(super) fence: Option<TranscriptFence>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ReprocessInput {
    /// Existing meeting ID.
    pub(super) meeting_id: String,
    /// Model profile. Defaults to active.
    pub(super) profile: Option<String>,
    /// Recognise speech again while preserving speaker diarization.
    #[serde(default)]
    pub(super) asr_only: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ImportInput {
    /// Recording file or folder. Supply either path or manifest.
    pub(super) path: Option<PathBuf>,
    /// Inline BroScribe import manifest JSON object.
    pub(super) manifest: Option<serde_json::Map<String, Value>>,
    /// Optional title for a recording.
    pub(super) title: Option<String>,
    /// Start time (local date/time or RFC 3339).
    pub(super) date: Option<String>,
    /// Participant names and e-mail addresses.
    #[serde(default)]
    pub(super) participants: Vec<String>,
    /// Copy or reference. An omitted setting uses the configured handling unless it is Move.
    pub(super) handling: Option<McpHandling>,
    /// Language tag (BCP 47), or omit for the configured choice.
    pub(super) language: Option<String>,
    /// Detect the language instead of using the configured default.
    #[serde(default)]
    pub(super) auto_language: bool,
}

impl ImportInput {
    pub(super) fn into_args(self, dry_run: bool) -> Result<ImportArgs> {
        if self.path.is_some() == self.manifest.is_some() {
            return Err(Error::InvalidInput(
                "supply exactly one of path or an inline manifest".into(),
            ));
        }
        if self.auto_language && self.language.is_some() {
            return Err(Error::InvalidInput(
                "choose language or auto_language".into(),
            ));
        }
        if self.manifest.is_some()
            && (self.language.is_some()
                || self.auto_language
                || self.date.is_some()
                || self.title.is_some()
                || !self.participants.is_empty())
        {
            return Err(Error::InvalidInput(
                "supply the title, date, language and participants inside the manifest".into(),
            ));
        }
        Ok(ImportArgs {
            dry_run,
            path: self.path,
            manifest: None,
            manifest_content: self.manifest.map(Value::Object),
            handling: self.handling.map(Into::into),
            lang: self.language,
            auto_language: self.auto_language,
            title: self.title,
            date: self.date,
            participants: self.participants,
            spec_stdin: false,
            enrich: false,
        })
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum McpHandling {
    Copy,
    Reference,
}

impl From<McpHandling> for HandlingArg {
    fn from(handling: McpHandling) -> Self {
        match handling {
            McpHandling::Copy => Self::Copy,
            McpHandling::Reference => Self::Reference,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct MeetingClipInput {
    /// Meeting containing the audio.
    pub(super) meeting_id: String,
    /// Start time in seconds or HH:MM:SS.
    pub(super) from: String,
    /// End time in seconds or HH:MM:SS.
    pub(super) to: String,
    /// New destination WAV outside the library.
    pub(super) out: PathBuf,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct PersonClipInput {
    /// Person ID, e-mail, alias or name.
    pub(super) person: String,
    /// Sample ID from list_person_samples.
    pub(super) sample_id: String,
    /// New destination WAV outside the library.
    pub(super) out: PathBuf,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct UndoPersonInput {
    /// Person ID, e-mail, alias or name.
    pub(super) person: String,
    /// Undo ID returned by edit_person.
    pub(super) undo_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AppPermissionInput {
    /// Native app permission to request.
    pub(super) permission: AppPermissionArg,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SetAppSettingInput {
    /// Name of the desktop or live setting.
    pub(super) setting: AppSetting,
    /// Whether to enable the setting.
    pub(super) enabled: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TextSizeInput {
    /// Reading size from small through largest.
    pub(super) size: TextSizeArg,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ThemeInput {
    /// `system` (follow the system's appearance, the default), `light` or `dark`.
    pub(super) theme: ThemeArg,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct GroupingInput {
    /// Grouping preset, or custom percentages.
    pub(super) preset: GroupingPreset,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct DefaultLanguageInput {
    /// Language tag (BCP 47) or auto.
    pub(super) language: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct WordsInput {
    /// Up to 1,000 inline recognition hints, one line each, at most 256 UTF-8 bytes per hint.
    #[schemars(length(max = 1000))]
    pub(super) words: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct RemoveWordInput {
    /// The exact saved word or phrase from get_word_list.
    pub(super) word: String,
    /// Confirm removal of this saved term.
    #[serde(default)]
    pub(super) confirm: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SetHandlingInput {
    /// Copy or reference.
    pub(super) handling: McpHandling,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct EnableConsentReminderInput {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ToggleInput {
    /// Whether to enable this setting.
    pub(super) enabled: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct PinInput {
    /// Microphone ID from list_audio_devices.
    pub(super) device: Option<String>,
    /// Restore the system default.
    #[serde(default)]
    pub(super) clear: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct PullModelInput {
    /// Registry model ID.
    pub(super) id: String,
    /// Reinstall and verify even if already installed.
    #[serde(default)]
    pub(super) force: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct UseModelInput {
    /// Registry model ID.
    pub(super) id: String,
    /// Model role such as asr or speaker_embedding.
    pub(super) role: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ProfileInput {
    /// Registry profile ID. Omit with pull to use the active profile.
    pub(super) profile: Option<String>,
    /// Download the profile’s required models.
    #[serde(default)]
    pub(super) pull: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ContactsInput {
    /// Up to 1,000 inline contacts. Each line is limited to 256 UTF-8 bytes and each contact to 16 e-mail addresses.
    #[schemars(length(max = 1000))]
    pub(super) contacts: Vec<SetupContact>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SelfInput {
    /// Your display name.
    pub(super) name: Option<String>,
    /// E-mail addresses to add.
    #[serde(default)]
    pub(super) emails: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TrySetupInput {
    /// Use the model’s bundled example instead.
    #[serde(default)]
    pub(super) model_example: bool,
    /// Microphone capture duration. Maximum 25 seconds.
    pub(super) seconds: Option<f64>,
    /// Microphone device ID.
    pub(super) device: Option<String>,
    /// Language tag for the test.
    pub(super) language: Option<String>,
    /// Save the test recording as a meeting.
    #[serde(default)]
    pub(super) save_as_meeting: bool,
    /// Setup session, when the app owns the test.
    pub(super) session_id: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct DeferInput {
    /// True defers the assistant. False resumes it.
    pub(super) enabled: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct RecordingPermissionInput {
    /// Native audio permission to request. Omit to check current grants.
    pub(super) permission: Option<SetupAudioPermission>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AudioCheckInput {
    /// Complete the setup step and save any usable result.
    #[serde(default)]
    pub(super) save: bool,
    /// Override the recommended echo handling when saving.
    pub(super) echo_handling: Option<EchoHandlingArg>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SetupStepInput {
    /// Setup step to mark complete.
    pub(super) step: CompletableStep,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct NewTemplateInput {
    /// Template ID.
    pub(super) template_id: String,
    /// Display name.
    pub(super) name: String,
    /// What the template does.
    pub(super) description: Option<String>,
    /// Instructions to run on the transcript.
    pub(super) body: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct EditTemplateInput {
    /// Template ID.
    pub(super) template_id: String,
    /// Display name.
    pub(super) name: Option<String>,
    /// What the template does.
    pub(super) description: Option<String>,
    /// Instructions to run on the transcript.
    pub(super) body: Option<String>,
    /// Template definition returned when editing began.
    pub(super) baseline: Option<TemplateBaseline>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct TemplateAutoInput {
    /// Template to configure.
    pub(super) template_id: String,
    /// Whether to run this template after automatic summaries.
    pub(super) enabled: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct InstructionsInput {
    /// Summary instructions, up to 4,000 characters.
    pub(super) instructions: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ProviderConfigurationInput {
    /// HTTP provider ID.
    pub(super) id: String,
    /// Base URL without keys or credentials.
    pub(super) base_url: String,
    /// Model name accepted by the provider.
    pub(super) model: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct PersonPhotoInput {
    /// Person ID, e-mail, alias or name.
    pub(super) person: String,
    /// Exact qualified Microsoft account. Google is provider-wide.
    pub(super) account: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct CalendarClientInput {
    /// Public OAuth client ID.
    pub(super) client_id: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SystemCalendarsInput {
    /// Native calendar IDs to read.
    pub(super) calendar_ids: Vec<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SelectCalendarsInput {
    /// Remote calendar integration.
    pub(super) integration: IntegrationArg,
    /// Corporate account ID for Microsoft.
    pub(super) account: Option<String>,
    /// Exact account generation from list_calendars.
    pub(super) generation: u64,
    /// Exact selection generation from list_calendars.
    pub(super) selection_generation: u64,
    /// Routing-qualified Microsoft targets from the catalog.
    pub(super) targets: Option<Vec<CalendarRoute>>,
    /// Google calendar IDs from the catalog.
    pub(super) calendar_ids: Option<Vec<String>>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct RemoveCalendarInput {
    /// Corporate account ID.
    pub(super) account: String,
    /// Account generation returned by list_calendars.
    pub(super) generation: u64,
    /// Selection generation returned by list_calendars.
    pub(super) selection_generation: u64,
    /// Exact selected calendar routing target.
    pub(super) target: CalendarRoute,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SyncAccountInput {
    /// Microsoft corporate account ID.
    pub(super) account: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct PeopleQueryInput {
    /// Name, e-mail, alias or ID to match.
    pub(super) query: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct LibraryMeetingsInput {
    /// Title, participants, tags or transcript text.
    pub(super) query: Option<String>,
    /// Limit to this person.
    pub(super) person: Option<String>,
    /// Limit to this tag.
    pub(super) tag: Option<String>,
    /// Limit to this enrichment topic.
    pub(super) topic: Option<String>,
    /// Limit to this detected app ID.
    pub(super) app: Option<String>,
    /// Limit to imported or recorded meetings.
    pub(super) source: Option<MeetingSourceArg>,
    /// Date order for unranked results.
    pub(super) order: Option<MeetingOrderArg>,
    /// Start of the date range, e.g. 30d or an RFC 3339 timestamp.
    pub(super) since: Option<String>,
    /// End of the date range.
    pub(super) until: Option<String>,
    /// Maximum rows when not paging. Default 20, maximum 500.
    pub(super) limit: Option<usize>,
    /// Page size from 1 to 500. Incompatible with transcript matches.
    pub(super) page_size: Option<usize>,
    /// Continue from the returned cursor. Requires page_size.
    pub(super) cursor: Option<String>,
    /// Append the top transcript match per meeting after the metadata hits.
    #[serde(default)]
    pub(super) include_transcripts: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct DirectoryEmailInput {
    /// Local person to edit.
    pub(super) person: String,
    /// The directory match e-mail address to add.
    pub(super) email: String,
    /// Reviewed source generation: google:N or provider/account_id:N.
    pub(super) account_generation: String,
    /// Optional exact field revisions returned by get_person.
    pub(super) expected: Option<PersonRevisionInput>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct NoticeInput {
    /// Pending notice ID from agent status or events.
    pub(super) notice_id: String,
    /// Action ID shown on that notice.
    pub(super) action: String,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ExportFileInput {
    /// Meeting to export.
    pub(super) meeting_id: String,
    /// Export format (md, txt or json).
    pub(super) format: ExportFormatArg,
    /// Requested sections, or omit for the defaults.
    #[serde(default)]
    pub(super) include: Vec<String>,
    /// Omit timestamps when the format supports them.
    #[serde(default)]
    pub(super) no_timestamps: bool,
    /// New destination outside the library.
    pub(super) out: PathBuf,
}
