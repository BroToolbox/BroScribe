//! Recording, library, settings and integration commands that change state.

use super::input::*;
use super::*;

#[tool_router(router = command_writes, vis = "pub(in crate::mcp)")]
impl Server {
    #[tool(
        description = "Start a new recording through the background agent.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn start_recording(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<StartRecordingInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Record {
                action: RecordCmd::Start {
                    app: p.app,
                    title: p.title,
                    setup: p.setup.map(Into::into),
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Pause capture. Requires the exact session.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn pause_recording(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SessionInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Record {
                action: RecordCmd::Pause {
                    session: session(p.session_id),
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Resume the paused recording. Requires the exact session.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn resume_recording(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SessionInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Record {
                action: RecordCmd::Resume {
                    session: session(p.session_id),
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Stop capture and queue saving and final processing. Requires the exact session.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn stop_recording(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SessionInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Record {
                action: RecordCmd::Stop {
                    session: session(p.session_id),
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Transcribe the recording so far while capture continues. Requires the exact session.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn process_recording_now(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SessionInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Record {
                action: RecordCmd::ProcessNow {
                    session: session(p.session_id),
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Optional bookmark label. Applies only to the named recording session.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn bookmark_recording(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<BookmarkRecordingInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Record {
                action: RecordCmd::Bookmark {
                    session: session(p.session_id),
                    label: p.label,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Note text to save. Applies only to the named recording session.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn note_recording(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<NoteRecordingInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Record {
                action: RecordCmd::Note {
                    session: session(p.session_id),
                    text: p.text,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Language tag (BCP 47). Applies only to the named recording session.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_recording_language(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<LangRecordingInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Record {
                action: RecordCmd::Lang {
                    session: session(p.session_id),
                    language: p.language,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Tag changes (+tag or tag adds, -tag removes). Applies only to the named recording session.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn tag_recording(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<TagRecordingInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Record {
                action: RecordCmd::Tag {
                    session: session(p.session_id),
                    changes: p.changes,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Confirm or remove a live participant without learning a voice.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_recording_participant(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ParticipantRecordingInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Record {
                action: RecordCmd::Participant {
                    action: p.action,
                    person: p.person,
                    session: Some(p.session_id),
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Rename the exact active or stopped recording before it is published.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_recording_title(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<TitleRecordingInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Record {
                action: RecordCmd::Title {
                    title: Some(p.title),
                    stdin: false,
                    session: Some(p.session_id),
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Set automatic stop for this session or link it to a detected call. Automatic stop saves the recording.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_recording_auto_stop(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<AutoStopRecordingInput>,
    ) -> CallToolResult {
        if !p.enabled && p.app.is_some() {
            return reply::<Value>(Err(Error::InvalidInput("app requires enabled".into())));
        }
        self.command_write(
            &ctx,
            Command::Record {
                action: RecordCmd::AutoStop {
                    session: Some(p.session_id),
                    on: p.enabled,
                    off: !p.enabled,
                    app: p.app,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Retry saving a stopped recording whose final job failed.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn retry_recording(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SessionInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Record {
                action: RecordCmd::Retry {
                    session: p.session_id,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Record or ignore a prompt that is still pending.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn answer_recording_prompt(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<RecordingPromptInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Record {
                action: RecordCmd::Answer {
                    id: p.prompt_id,
                    action: p.action,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Name, confirm or clear one live turn using the exact evidence shown in the live preview.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn attribute_live_turn(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<LiveAttributeInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Record {
                action: RecordCmd::LiveAttribute {
                    session: Some(p.session_id),
                    turn: p.turn_id,
                    revision: p.revision,
                    epoch: p.epoch,
                    evidence: p.evidence,
                    name: p.name,
                    name_stdin: false,
                    confirm: p.confirm,
                    clear: p.clear,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Leave an exact live review decision as transcribed. Refuses a changed item.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn dismiss_live_speaker_review(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<LiveDismissInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Speakers {
                action: SpeakersCmd::LiveDismiss {
                    meeting: p.meeting_id,
                    decision: p.decision,
                    review_key: p.review_key,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Correct or clear saved line attribution. Refuses stale segment fingerprints.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn attribute_transcript_lines(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<TranscriptAttributeInput>,
    ) -> CallToolResult {
        if p.fingerprints.is_empty() {
            return reply::<Value>(Err(Error::InvalidInput(
                "supply at least one fingerprint".into(),
            )));
        }
        self.command_write(
            &ctx,
            Command::Transcript {
                action: Some(TranscriptCmd::Attribute {
                    meeting: p.meeting_id,
                    segments: vec![],
                    fingerprints_stdin: true,
                    person: None,
                    clear: p.clear,
                }),
                args: TranscriptArgs::default(),
            },
            Some(
                json!({ "fingerprints": p.fingerprints, "person": p.person, "fence": p.fence })
                    .to_string(),
            ),
        )
        .await
    }

    #[tool(
        description = "Queue transcription or full reprocessing of an existing meeting.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn queue_reprocess_meeting(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ReprocessInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Reprocess(ReprocessArgs {
                meeting: p.meeting_id,
                profile: p.profile,
                asr_only: p.asr_only,
                queue: true,
            }),
            None,
        )
        .await
    }

    #[tool(
        description = "Import media from a file, folder or inline manifest using Copy or Reference and the configured processing policy.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn import_recording(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ImportInput>,
    ) -> CallToolResult {
        match p.into_args(false) {
            Ok(args) => self.command_write(&ctx, Command::Import(args), None).await,
            Err(error) => reply::<Value>(Err(error)),
        }
    }

    #[tool(
        description = "Reconcile or rebuild the derived search index, using library files as the source.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn repair_library_index(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Library {
                action: LibraryCmd::Repair {
                    only: vec![RepairScopeArg::Index],
                    dry_run: false,
                    yes: true,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Reconcile saved recording availability. A restored file must match its stored hash.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn check_recording_file(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<MeetingInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Meeting {
                action: MeetingCmd::CheckRecording {
                    meeting: p.meeting_id,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Prepare the playback cache of an available recording.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn prepare_meeting_audio(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<MeetingInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Meeting {
                action: MeetingCmd::PrepareAudio {
                    meeting: p.meeting_id,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Save an available meeting audio range as a WAV at a new visible .wav file outside the library and data folder.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn export_meeting_clip(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<MeetingClipInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Meeting {
                action: MeetingCmd::Clip {
                    meeting: p.meeting_id,
                    from: p.from,
                    to: p.to,
                    out: p.out,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Save a listed voice sample as a WAV at a new visible .wav file outside the library and data folder.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn export_person_clip(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<PersonClipInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::People {
                action: Some(PeopleCmd::Clip {
                    person: p.person,
                    sample: p.sample_id,
                    out: p.out,
                }),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Restore one person edit using its undo receipt. Refuses if another edit superseded it.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn undo_person_edit(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<UndoPersonInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::People {
                action: Some(PeopleCmd::UndoEdit {
                    person: p.person,
                    undo_id: p.undo_id,
                }),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Start the background recording agent and report its status.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn start_daemon(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Daemon {
                action: DaemonCmd::Start,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Register the background agent to start at login.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn install_daemon(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Daemon {
                action: DaemonCmd::Install,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Ask the running desktop app to show its main window.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn show_app(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::App {
                action: AppCmd::Show,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Request one OS permission through the running desktop app.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn request_app_permission(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<AppPermissionInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::App {
                action: AppCmd::Permissions {
                    request: Some(p.permission),
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Set one desktop or recording presentation setting.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_app_setting(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SetAppSettingInput>,
    ) -> CallToolResult {
        self.command_write(&ctx, p.setting.command(switch(p.enabled)), None)
            .await
    }

    #[tool(
        description = "Change the text size used by BroScribe windows.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_text_size(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<TextSizeInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Settings {
                action: SettingsCmd::TextSize {
                    value: Some(p.size),
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Make BroScribe's windows light, dark, or follow the system's appearance.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_theme(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ThemeInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Settings {
                action: SettingsCmd::Theme {
                    value: Some(p.theme),
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Apply a grouping preset or custom percentages for the active embedding model.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_speaker_grouping(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<GroupingInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Settings {
                action: SettingsCmd::SpeakerGrouping {
                    preset: Some(p.preset.into_cli()),
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Set the default meeting language or choose auto-detection.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_default_language(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<DefaultLanguageInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Settings {
                action: SettingsCmd::DefaultLanguage {
                    value: Some(p.language),
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Add inline recognition words, one entry per line.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn add_words(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<WordsInput>,
    ) -> CallToolResult {
        if p.words.len() > 1000 || p.words.iter().any(|word| !inline_line(word)) {
            return reply::<Value>(Err(Error::InvalidInput(
                "supply up to 1,000 single-line words of at most 256 UTF-8 bytes each".into(),
            )));
        }
        self.command_write(
            &ctx,
            Command::Setup {
                action: None,
                args: SetupArgs {
                    words: Some(p.words),
                    non_interactive: true,
                    ..Default::default()
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Set the default handling for imported recordings.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_import_handling(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SetHandlingInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Settings {
                action: SettingsCmd::ImportHandling {
                    value: Some(p.handling.into()),
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Turn on the recording consent reminder.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn enable_consent_reminder(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(_): Parameters<EnableConsentReminderInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Settings {
                action: SettingsCmd::ConsentReminder {
                    value: Some(OnOff::On),
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Set whether automatic summaries are permitted through consented providers.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_automatic_summaries(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ToggleInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Settings {
                action: SettingsCmd::AutomaticSummaries {
                    value: Some(on_off(p.enabled)),
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Pin a connected microphone, or restore the system default.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_microphone_pin(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<PinInput>,
    ) -> CallToolResult {
        if p.device.is_some() == p.clear {
            return reply::<Value>(Err(Error::InvalidInput(
                "supply exactly one of device or clear".into(),
            )));
        }
        self.command_write(
            &ctx,
            Command::Audio {
                action: AudioCmd::Pin {
                    device: p.device,
                    clear: p.clear,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Download a model and verify it before installation.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn pull_model(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<PullModelInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Models {
                action: ModelsCmd::Pull {
                    id: p.id,
                    allow_unverified_license: false,
                    force: p.force,
                    cancel_on_stdin_eof: false,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Select a model for one role. Changing the speaker embedding model can re-embed the library.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn use_model(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<UseModelInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Models {
                action: ModelsCmd::Use {
                    id: p.id,
                    role: p.role,
                    allow_unverified_license: false,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Select a model profile, download its models, or do both.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn configure_model_profile(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ProfileInput>,
    ) -> CallToolResult {
        if !p.pull && p.profile.is_none() {
            return reply::<Value>(Err(Error::InvalidInput("supply profile or pull".into())));
        }
        self.command_write(
            &ctx,
            Command::Setup {
                action: None,
                args: SetupArgs {
                    profile: p.profile,
                    pull_models: p.pull,
                    allow_unverified_license: false,
                    non_interactive: true,
                    ..Default::default()
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Add or update people from inline contact names and e-mails.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn import_contacts(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ContactsInput>,
    ) -> CallToolResult {
        if p.contacts.len() > 1000
            || p.contacts.iter().any(|contact| {
                contact
                    .name
                    .as_deref()
                    .is_some_and(|name| !inline_line(name))
                    || contact.emails.len() > 16
                    || contact.emails.iter().any(|email| !inline_line(email))
            })
        {
            return reply::<Value>(Err(Error::InvalidInput(
                "supply up to 1,000 contacts with up to 16 e-mails each, in single lines of at most 256 UTF-8 bytes".into(),
            )));
        }
        self.command_write(
            &ctx,
            Command::Setup {
                action: None,
                args: SetupArgs {
                    contacts: Some(p.contacts),
                    non_interactive: true,
                    ..Default::default()
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Create or update the local Me profile.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_self_person(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SelfInput>,
    ) -> CallToolResult {
        if p.name.is_none() && p.emails.is_empty() {
            return reply::<Value>(Err(Error::InvalidInput(
                "give a name or e-mail address".into(),
            )));
        }
        self.command_write(
            &ctx,
            Command::Setup {
                action: None,
                args: SetupArgs {
                    name: p.name,
                    emails: p.emails,
                    non_interactive: true,
                    ..Default::default()
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Enroll your voice from microphone capture or a saved meeting. Without confirm: true it only returns the plan (source and what is kept).",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn enroll_voice(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<EnrollVoiceInput>,
    ) -> CallToolResult {
        if let Err(error) = p.validate() {
            return reply::<Value>(Err(error));
        }
        if !p.confirm {
            return reply(Ok(p.plan()));
        }
        self.command_write(
            &ctx,
            Command::Setup {
                action: Some(Box::new(SetupCmd::Voice(p.into_voice()))),
                args: SetupArgs::default(),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Capture a two-second microphone sample and report its level.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn check_microphone(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Setup {
                action: Some(Box::new(SetupCmd::MicTest)),
                args: SetupArgs::default(),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Transcribe a short test from a microphone or model example. Optionally save it as a meeting.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn try_setup_transcription(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<TrySetupInput>,
    ) -> CallToolResult {
        if (p.model_example && (p.device.is_some() || p.save_as_meeting || p.seconds.is_some()))
            || p.seconds.is_some_and(|v| !(1.0..=25.0).contains(&v))
        {
            return reply::<Value>(Err(Error::InvalidInput(
                "choose model example or microphone with valid capture options".into(),
            )));
        }
        self.command_write(
            &ctx,
            Command::Setup {
                action: Some(Box::new(SetupCmd::Try(TryArgs {
                    seconds: p.seconds.unwrap_or(20.0),
                    from_file: None,
                    device: p.device,
                    lang: p.language,
                    save_as_meeting: p.save_as_meeting,
                    session: p.session_id,
                    model_example: p.model_example,
                }))),
                args: SetupArgs::default(),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Defer or resume the setup assistant without losing completed steps.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_setup_deferred(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<DeferInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Setup {
                action: Some(Box::new(if p.enabled {
                    SetupCmd::Defer
                } else {
                    SetupCmd::Resume
                })),
                args: SetupArgs::default(),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Check and record the setup permission step. Optionally request one native audio permission.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn setup_recording_access(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<RecordingPermissionInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Setup {
                action: Some(Box::new(SetupCmd::RecordingAccess {
                    request: p.permission,
                })),
                args: SetupArgs::default(),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Play a synthetic test signal and measure echo. Optionally save the result and echo handling.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn run_audio_check(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<AudioCheckInput>,
    ) -> CallToolResult {
        if p.echo_handling.is_some() && !p.save {
            return reply::<Value>(Err(Error::InvalidInput(
                "echo_handling requires save".into(),
            )));
        }
        self.command_write(
            &ctx,
            Command::AudioCheck {
                save: p.save,
                echo_handling: p.echo_handling,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Mark the meeting-app, summary or assistant step complete.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn complete_setup_step(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SetupStepInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Setup {
                action: Some(Box::new(p.step.into_cli())),
                args: SetupArgs::default(),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Create a reusable template, or duplicate one by supplying its definition under a new ID.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn create_template(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<NewTemplateInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Templates {
                action: TemplatesCmd::New {
                    id: p.template_id,
                    stdin: true,
                },
            },
            Some(
                json!({ "name": p.name, "description": p.description, "body": p.body }).to_string(),
            ),
        )
        .await
    }

    #[tool(
        description = "Change a template. Pass the displayed definition as a baseline to refuse conflicting edits.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn edit_template(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<EditTemplateInput>,
    ) -> CallToolResult {
        self.command_write(&ctx, Command::Templates { action: TemplatesCmd::Edit{ id: p.template_id, stdin: true } }, Some(json!({ "name": p.name, "description": p.description, "body": p.body, "baseline": p.baseline }).to_string())).await
    }

    #[tool(
        description = "Choose whether to run a template after automatic summaries.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_template_automatic(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<TemplateAutoInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Templates {
                action: TemplatesCmd::Auto {
                    action: if p.enabled {
                        TemplatesAutoCmd::Add { id: p.template_id }
                    } else {
                        TemplatesAutoCmd::Remove { id: p.template_id }
                    },
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Save instructions for future summaries. Pass an empty string to clear them.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_summary_instructions(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<InstructionsInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Enrich {
                action: Some(EnrichCmd::Instructions {
                    action: InstructionsCmd::Set { stdin: true },
                }),
                args: EnrichArgs::default(),
            },
            Some(p.instructions),
        )
        .await
    }

    #[tool(
        description = "Clear the default instructions used for future summaries.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn clear_summary_instructions(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Enrich {
                action: Some(EnrichCmd::Instructions {
                    action: InstructionsCmd::Clear,
                }),
                args: EnrichArgs::default(),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Set an HTTP provider’s server URL and model. Credentials remain managed by the user.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn configure_provider(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ProviderConfigurationInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Providers {
                action: ProvidersCmd::Configure {
                    id: p.id,
                    base_url: p.base_url,
                    model: p.model,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Disable a provider and withdraw its existing consent.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn disable_provider(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ProviderInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Providers {
                action: ProvidersCmd::Disable { id: p.id },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Send a tiny invented prompt to test the configured provider.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn test_provider(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ProviderInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Providers {
                action: ProvidersCmd::Test { id: p.id },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Prefer an already enabled provider for future summaries.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn prefer_provider(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ProviderInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Providers {
                action: ProvidersCmd::Prefer { id: p.id },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Cache a directory photo for an existing person using the selected account.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn cache_person_photo(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<PersonPhotoInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::People {
                action: Some(PeopleCmd::Photo {
                    person: p.person,
                    account: p.account,
                }),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Set the public Google calendar OAuth client ID. Set the client secret through the desktop app or CLI.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn configure_google_calendar(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<CalendarClientInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Integrations {
                action: IntegrationsCmd::Setup {
                    integration: IntegrationArg::Google,
                    client_id: Some(p.client_id),
                    client_secret_stdin: false,
                    calendar_ids: vec![],
                    clear_calendars: false,
                    spec_stdin: false,
                    clear_client_secret: false,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Replace the native calendar selection. An empty list clears the selection.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn select_system_calendars(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SystemCalendarsInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Integrations {
                action: IntegrationsCmd::Setup {
                    integration: IntegrationArg::SystemCalendar,
                    client_id: None,
                    client_secret_stdin: false,
                    clear_calendars: p.calendar_ids.is_empty(),
                    calendar_ids: p.calendar_ids,
                    spec_stdin: false,
                    clear_client_secret: false,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Replace the remote calendar selection using both generation fences from the catalog.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn select_calendars(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SelectCalendarsInput>,
    ) -> CallToolResult {
        let valid = match p.integration {
            IntegrationArg::Google => p.calendar_ids.is_some() && p.targets.is_none(),
            IntegrationArg::MicrosoftGraph => p.targets.is_some() && p.calendar_ids.is_none(),
            IntegrationArg::SystemCalendar => false,
        };
        if !valid {
            return reply::<Value>(Err(Error::InvalidInput(
                "supply calendar_ids for Google or targets for Microsoft; use an explicit empty array to clear a selection".into(),
            )));
        }
        self.command_write(&ctx, Command::Integrations { action: IntegrationsCmd::SelectCalendars{ integration: p.integration, account: p.account, generation: p.generation, selection_generation: p.selection_generation, spec_stdin: true } }, Some(json!({ "targets": p.targets.unwrap_or_default(), "calendar_ids": p.calendar_ids.unwrap_or_default() }).to_string())).await
    }

    #[tool(
        description = "Stop syncing one selected Microsoft calendar while keeping other routes.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn remove_selected_calendar(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<RemoveCalendarInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Integrations {
                action: IntegrationsCmd::RemoveCalendar {
                    integration: IntegrationArg::MicrosoftGraph,
                    account: p.account,
                    generation: p.generation,
                    selection_generation: p.selection_generation,
                    spec_stdin: true,
                },
            },
            Some(json!({ "target": p.target }).to_string()),
        )
        .await
    }

    #[tool(
        description = "Refresh selected calendars for one integration or all connected integrations.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn sync_calendars(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<IntegrationFilter>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Integrations {
                action: IntegrationsCmd::Sync {
                    integration: p.integration,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Refresh selected calendars for a single corporate Microsoft account.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn sync_calendar_account(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SyncAccountInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Integrations {
                action: IntegrationsCmd::SyncAccount {
                    integration: IntegrationArg::MicrosoftGraph,
                    account: p.account,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Cancel an exact pending calendar sign-in attempt. A finished attempt keeps its result.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn cancel_calendar_login(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<LoginAttemptInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Integrations {
                action: IntegrationsCmd::CancelLogin {
                    integration: p.integration,
                    attempt: p.attempt,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Install the BroScribe CLI link and skill for locally detected agent tools.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn install_agent_tools(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Agents {
                action: AgentsCmd::Install,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Add a directory e-mail only while the source account is at the reviewed generation.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn apply_directory_email(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<DirectoryEmailInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::People {
                action: Some(PeopleCmd::Edit(PeopleEditArgs {
                    person: p.person,
                    name: None,
                    aliases: vec![],
                    emails: vec![],
                    notes_file: None,
                    spec_stdin: true,
                    expect_generation: Some(p.account_generation),
                })),
            },
            Some(json!({ "add_emails": [p.email], "expected": p.expected }).to_string()),
        )
        .await
    }

    #[tool(
        description = "Request native calendar permission through the background agent.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn connect_system_calendar(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Integrations {
                action: IntegrationsCmd::Login {
                    integration: IntegrationArg::SystemCalendar,
                    attempt: AttemptArg::default(),
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Act on an existing agent notice using one of its displayed actions.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn answer_agent_notice(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<NoticeInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Daemon {
                action: DaemonCmd::Notice {
                    id: p.notice_id,
                    action: p.action,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Stop an idle background agent. Refuses when there is active work.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn stop_daemon(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Daemon {
                action: DaemonCmd::Stop,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Export requested meeting sections to a new visible file outside the library and data folder, with the format’s .md, .txt or .json extension.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn export_meeting_file(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ExportFileInput>,
    ) -> CallToolResult {
        self.command_write(
            &ctx,
            Command::Export(ExportArgs {
                meeting: p.meeting_id,
                format: p.format,
                include: p.include,
                no_timestamps: p.no_timestamps,
                out: Some(p.out),
                spec_stdin: false,
            }),
            None,
        )
        .await
    }
}

fn inline_line(value: &str) -> bool {
    value.len() <= 256
        && !value
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
}
