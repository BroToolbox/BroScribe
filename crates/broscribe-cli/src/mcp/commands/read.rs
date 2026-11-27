//! Read commands and bounded agent event observation.

use super::input::*;
use super::*;

#[tool_router(router = command_reads, vis = "pub(in crate::mcp)")]
impl Server {
    #[tool(
        description = "Preview how the active voice model would re-resolve saved speaker assignments.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn preview_identity_reresolution(
        &self,
        ctx: RequestContext<RoleServer>,
    ) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Identity {
                action: IdentityCmd::ReResolve { dry_run: true },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Show the active recording and its session, capture health, preview and reader state.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_recording_status(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Record {
                action: RecordCmd::Status,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Show agent lifecycle, recording status, stopped and saved recordings, permissions and prompts.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_daemon_status(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Daemon {
                action: DaemonCmd::Status,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "List meeting detection prompts that can still be answered.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn list_recording_prompts(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Record {
                action: RecordCmd::Prompts,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "List jobs, meeting IDs, generation run tokens, positions, live progress and failures.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_queue_status(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Queue {
                action: QueueCmd::Status,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Show background speaker recheck progress and its latest per-meeting report.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_identity_status(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Identity {
                action: IdentityCmd::Status,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "List names given live that still need review, with decisions and review keys.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn list_live_speaker_reviews(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<OptionalMeetingInput>,
    ) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Speakers {
                action: SpeakersCmd::LiveReview {
                    meeting: p.meeting_id,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "List the best voice sample time ranges for a meeting speaker.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_speaker_samples(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<SpeakerInput>,
    ) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Speakers {
                action: SpeakersCmd::Samples {
                    meeting: p.meeting_id,
                    speaker: p.speaker_id,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "List a person’s voice sample provenance and time ranges without embeddings.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn list_person_samples(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<PersonInput>,
    ) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::People {
                action: Some(PeopleCmd::Samples { person: p.person }),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "List meeting tags with counts and recorded app IDs.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_library_filters(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Library {
                action: LibraryCmd::Filters,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Measure space used by the library and free on its disk.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_library_disk_usage(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Library {
                action: LibraryCmd::DiskUsage,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Report index and library integrity, pending repairs and unreadable files.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn check_library(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Library {
                action: LibraryCmd::Check,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "List a meeting’s bookmarks and notes in recording-time order.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_meeting_timeline(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<MeetingInput>,
    ) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Meeting {
                action: MeetingCmd::Timeline {
                    meeting: p.meeting_id,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read the meeting’s review notes and revision.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn list_meeting_review_notes(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<MeetingInput>,
    ) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Meeting {
                action: MeetingCmd::ReviewNotes {
                    meeting: p.meeting_id,
                    dismiss: None,
                    expect: None,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Show the CLI meeting record, including its local folder, recording reference and published metadata.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn show_meeting_record(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<MeetingInput>,
    ) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Show {
                meeting: p.meeting_id,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Find text or speaker labels in a saved transcript.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn find_transcript(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<FindTranscriptInput>,
    ) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Transcript {
                action: None,
                args: TranscriptArgs {
                    meeting: Some(p.meeting_id),
                    speaker: p.speaker,
                    from: p.from,
                    to: p.to,
                    find_stdin: true,
                },
            },
            Some(p.query),
        )
        .await
    }

    #[tool(
        description = "Read the installed build, signing facts, architecture and licence information.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_about(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::App {
                action: AppCmd::About,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read the CLI and MCP version.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_version(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(&ctx, Command::Version, None).await
    }

    #[tool(
        description = "Read the configured maintenance mode and authenticated agent observation.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_maintenance_status(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Maintenance {
                action: MaintenanceCmd::Status,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read the running desktop app’s notification and Accessibility permissions.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_app_permissions(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::App {
                action: AppCmd::Permissions { request: None },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read the agent’s meeting detection and configured app policies.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_detection_status(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Detect {
                action: DetectCmd::Status,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Inspect a running app’s redacted accessibility structure. Requires the existing native accessibility grant.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn dump_reader_structure(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ReaderDumpInput>,
    ) -> CallToolResult {
        if p.app.is_some() == p.pid.is_some() {
            return reply::<Value>(Err(Error::InvalidInput(
                "supply exactly one of app or pid".into(),
            )));
        }
        self.command_read(
            &ctx,
            Command::Readers {
                action: crate::cmd::readers::ReadersCmd::Dump(crate::cmd::readers::DumpArgs {
                    app: p.app,
                    pid: p.pid,
                    output: None,
                }),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "List microphones and the system default.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn list_audio_devices(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Audio {
                action: AudioCmd::Devices,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read the microphone pin and whether it is connected.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_microphone_pin(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Audio {
                action: AudioCmd::PinStatus,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read saved audio checks and the echo choice.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_audio_check_status(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Audio {
                action: AudioCmd::CheckStatus,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "List registered models, local installation state and active roles.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn list_models(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Models {
                action: ModelsCmd::List,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Check the installed model files and optionally run a synthetic live check.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn check_models(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ModelHealthInput>,
    ) -> CallToolResult {
        if p.profile.is_some() && !p.live {
            return reply::<Value>(Err(Error::InvalidInput("profile requires live".into())));
        }
        self.command_read(
            &ctx,
            Command::Models {
                action: ModelsCmd::Doctor {
                    live: p.live,
                    profile: p.profile,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Estimate the profile’s model download, available disk space and memory.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_setup_model_check(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Setup {
                action: Some(Box::new(SetupCmd::ModelCheck)),
                args: SetupArgs::default(),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Report the count and size of kept enrollment readings.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_voice_enrollment_status(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Setup {
                action: Some(Box::new(SetupCmd::VoiceStatus)),
                args: SetupArgs::default(),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read the completed setup steps, deferral, upcoming steps and Accessibility status.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_setup_status(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Setup {
                action: None,
                args: SetupArgs {
                    non_interactive: true,
                    ..Default::default()
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read the enrollment passage and confirmation passage for the requested or configured language.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_voice_passage(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<PassageInput>,
    ) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Setup {
                action: Some(Box::new(SetupCmd::Voice(VoiceArgs {
                    show_passage: true,
                    lang: p.language,
                    ..VoiceArgs::default()
                }))),
                args: SetupArgs::default(),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Check configuration, library, models, enrollment and decoders.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_setup_diagnostics(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(&ctx, Command::Doctor, None).await
    }

    #[tool(
        description = "Read registered providers, availability and consent terms.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn list_providers(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Providers {
                action: ProvidersCmd::List,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read model names using an already consented provider. No meeting content is sent.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn list_provider_models(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ProviderInput>,
    ) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Providers {
                action: ProvidersCmd::Models { id: p.id },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read the saved instructions used for future summaries.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_summary_instructions(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Enrich {
                args: EnrichArgs::default(),
                action: Some(EnrichCmd::Instructions {
                    action: InstructionsCmd::Show,
                }),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read a template’s instructions and baseline.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_template_definition(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<TemplateInputId>,
    ) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Templates {
                action: TemplatesCmd::Show { id: p.template_id },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read calendar integration status without returning credentials.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_calendar_status(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<IntegrationFilter>,
    ) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Integrations {
                action: IntegrationsCmd::Status {
                    integration: p.integration,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "List connected corporate accounts, generations and grants.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn list_calendar_accounts(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<IntegrationFilter>,
    ) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Integrations {
                action: IntegrationsCmd::Accounts {
                    integration: p.integration,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Discover readable calendars and return the catalog generation fences.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn list_calendars(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<CalendarCatalogInput>,
    ) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Integrations {
                action: IntegrationsCmd::Calendars {
                    integration: p.integration,
                    account: p.account,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read the state of a calendar sign-in attempt.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_calendar_login_attempt(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<LoginAttemptInput>,
    ) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Integrations {
                action: IntegrationsCmd::LoginAttempt {
                    integration: p.integration,
                    attempt: p.attempt,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Look up a name or e-mail in connected directories, with account and generation evidence.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn search_directory(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<DirectoryInput>,
    ) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::People {
                action: Some(PeopleCmd::Lookup {
                    query: Some(p.query),
                    query_stdin: false,
                    integration: p.integration,
                    account: p.account,
                }),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read the installation and availability of BroScribe AI tool integrations.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_agent_tools_status(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Agents {
                action: AgentsCmd::Status,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read MCP enablement and registration state.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_mcp_status(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Mcp {
                action: Some(McpCmd::Status),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read one desktop or recording presentation setting.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_app_setting(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<AppSettingInput>,
    ) -> CallToolResult {
        self.command_read(&ctx, p.setting.command(SwitchArg::Status), None)
            .await
    }

    #[tool(
        description = "Read the configured text size.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_text_size(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Settings {
                action: SettingsCmd::TextSize { value: None },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read whether BroScribe's windows are light, dark or follow the system.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_theme(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Settings {
                action: SettingsCmd::Theme { value: None },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read grouping thresholds for the active speaker model.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_speaker_grouping(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Settings {
                action: SettingsCmd::SpeakerGrouping { preset: None },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read the effective library folder of this connection.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_library_folder(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Settings {
                action: SettingsCmd::LibraryFolder { path: None },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read the configured meeting language or auto-detection choice.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_default_language(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Settings {
                action: SettingsCmd::DefaultLanguage { value: None },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read the saved recognition hint words.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_word_list(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Settings {
                action: SettingsCmd::WordList,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read the saved Settings pane and window position.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_window_settings(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Settings {
                action: SettingsCmd::Window,
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read how new imports store their recordings.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_import_handling(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Settings {
                action: SettingsCmd::ImportHandling { value: None },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read the recording consent reminder setting.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_consent_reminder(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Settings {
                action: SettingsCmd::ConsentReminder { value: None },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Read the automatic summary setting.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn get_automatic_summaries(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Settings {
                action: SettingsCmd::AutomaticSummaries { value: None },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Preview media in a file or folder, or an inline manifest. Returns a count of skipped entries.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn preview_import(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<ImportInput>,
    ) -> CallToolResult {
        match p.into_args(true) {
            Ok(args) => self.command_read(&ctx, Command::Import(args), None).await,
            Err(error) => reply::<Value>(Err(error)),
        }
    }

    #[tool(
        description = "Preview repair of the derived search index.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn preview_index_repair(&self, ctx: RequestContext<RoleServer>) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::Library {
                action: LibraryCmd::Repair {
                    only: vec![RepairScopeArg::Index],
                    dry_run: true,
                    yes: false,
                },
            },
            None,
        )
        .await
    }

    #[tool(
        description = "Find local profiles by name, alias, e-mail or ID.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn search_people(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<PeopleQueryInput>,
    ) -> CallToolResult {
        self.command_read(
            &ctx,
            Command::People {
                action: Some(PeopleCmd::List {
                    query: Some(p.query),
                }),
            },
            None,
        )
        .await
    }

    #[tool(
        description = "List library meetings with metadata and optional transcript matches, in the same order as CLI meetings.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn list_library_meetings(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<LibraryMeetingsInput>,
    ) -> CallToolResult {
        if p.limit.is_some_and(|n| !(1..=500).contains(&n))
            || p.page_size.is_some_and(|n| !(1..=500).contains(&n))
            || (p.page_size.is_some() && (p.include_transcripts || p.limit.is_some()))
            || (p.cursor.is_some() && p.page_size.is_none())
        {
            return reply::<Value>(Err(Error::InvalidInput(
                "invalid limit, page size, cursor or include_transcripts combination".into(),
            )));
        }
        self.command_read(
            &ctx,
            Command::Meetings(MeetingsArgs {
                include_transcripts: p.include_transcripts,
                order: p.order,
                query: p.query,
                person: p.person,
                query_stdin: false,
                tag: p.tag,
                topic: p.topic,
                app: p.app,
                source: p.source,
                since: p.since,
                until: p.until,
                limit: p.limit.unwrap_or(20),
                page: PageArgs {
                    page_size: p.page_size,
                    cursor: p.cursor,
                },
            }),
            None,
        )
        .await
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DaemonEventsInput {
    /// Maximum observation time in milliseconds, 1–5000 (default 1000).
    wait_ms: Option<u64>,
    /// Maximum returned events, 1–100 (default 20).
    limit: Option<usize>,
}

#[tool_router(router = event_reads, vis = "pub(in crate::mcp)")]
impl Server {
    #[tool(
        description = "Read a bounded batch of agent events, beginning with the agent's current state. Events have the same fields as daemon watch.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    async fn poll_daemon_events(
        &self,
        ctx: RequestContext<RoleServer>,
        Parameters(p): Parameters<DaemonEventsInput>,
    ) -> CallToolResult {
        let data_dir = self.data_dir.clone();
        let cancel = Cancel::of(&ctx);
        reply(
            until_cancelled(
                &ctx,
                self.gated(&ctx, move |lib, _| {
                    use std::time::{Duration, Instant};

                    let limit = p.limit.unwrap_or(20);
                    let wait = p.wait_ms.unwrap_or(1000);
                    if !(1..=100).contains(&limit) || !(1..=5000).contains(&wait) {
                        return Err(Error::InvalidInput(
                            "limit must be 1–100 and wait_ms 1–5000".into(),
                        ));
                    }
                    let (client, _) = agent_on_library(&data_dir, lib.root()).map_err(|error| {
                        if matches!(error, Error::DaemonUnavailable(_)) {
                            Error::DaemonUnavailable("BroScribe Agent is not running".into())
                        } else {
                            error
                        }
                    })?;
                    let mut subscription =
                        client.subscribe().map_err(crate::cmd::daemon::call_error)?;
                    let deadline = Instant::now() + Duration::from_millis(wait);
                    let mut events = Vec::new();
                    while events.len() < limit {
                        cancel.check()?;
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        match subscription
                            .next_event_before(deadline.min(now + Duration::from_millis(100)))
                        {
                            Ok(Some(event)) => {
                                if let broscribe_ipc::Event::State(state) = &event {
                                    check_agent_library(lib.root(), state)?;
                                }
                                events.extend(crate::cmd::daemon::watch_event(&event)?)
                            }
                            Ok(None) => break,
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                                ) => {}
                            Err(error) => return Err(Error::io("reading agent events", error)),
                        }
                    }
                    Ok(json!({ "events": events }))
                }),
            )
            .await,
        )
    }
}
