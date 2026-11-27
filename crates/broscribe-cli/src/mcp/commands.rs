//! Typed MCP adapters for the JSON commands shared with the CLI. There is
//! no arbitrary command runner: each tool constructs one allowed operation.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::OwnedSemaphorePermit;

use super::*;
use crate::cli::*;

mod input;
mod read;
mod session;
mod write;

struct CancelCommand(Arc<AtomicBool>);

impl Drop for CancelCommand {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

pub(super) struct CommandJob {
    data_dir: PathBuf,
    library: PathBuf,
    command: Command,
    input: Option<String>,
    cancel: Cancel,
    cancel_flag: Arc<AtomicBool>,
    progress: tokio::sync::watch::Sender<Option<(f64, f64)>>,
    edit_history: Arc<Mutex<crate::cmd::identity_review::EditHistory>>,
    reply: oneshot::Sender<Result<Value>>,
    _permit: OwnedSemaphorePermit,
}

impl CommandJob {
    pub(super) fn run(self, deps: &Deps) {
        let Self {
            data_dir,
            library,
            command,
            input,
            cancel,
            cancel_flag,
            progress,
            edit_history,
            reply,
            _permit,
        } = self;
        let result = (|| {
            cancel.check()?;
            enabled_config(&data_dir)?;
            let mut paths = resolve_paths(deps.paths.as_ref())?;
            paths.data_dir = data_dir;
            let mut out = Vec::new();
            let mut err = std::io::sink();
            crate::Ctx::scoped(deps, true, None, paths, None, &mut out, &mut err, |ctx| {
                ctx.request_input = input;
                ctx.request_library = Some(library);
                ctx.request_cancel = Some(cancel_flag);
                ctx.request_progress = Some(progress);
                ctx.request_edit_history = Some(edit_history);
                cancel.check()?;
                crate::cmd::dispatch(ctx, command)
            })?;
            serde_json::from_slice(&out)
                .map_err(|e| Error::Internal(format!("command did not return a JSON result: {e}")))
        })();
        let _ = reply.send(result);
    }
}

/// Keep excluded command capabilities outside the tool dispatcher, even
/// if an adapter's name or parameters change.
fn check_command(command: &Command) -> Result<()> {
    let excluded = matches!(
        command,
        Command::Uninstall(_)
            | Command::Delete { .. }
            | Command::Record {
                action: RecordCmd::Discard { .. } | RecordCmd::ResolveJournal { .. }
            }
            | Command::Retention { .. }
            | Command::Update { .. }
            | Command::Bench { .. }
            | Command::Maintenance {
                action: MaintenanceCmd::Enable | MaintenanceCmd::Disable
            }
            | Command::Mcp {
                action: Some(McpCmd::Enable | McpCmd::Disable) | None
            }
            | Command::Agents {
                action: AgentsCmd::Uninstall
            }
            | Command::Daemon {
                action: DaemonCmd::Uninstall { .. }
            }
            | Command::Queue {
                action: QueueCmd::Work { .. } | QueueCmd::DiscardSetup { .. } | QueueCmd::Prune(_)
            }
            | Command::People {
                action: Some(
                    PeopleCmd::Merge { .. }
                        | PeopleCmd::Edit(PeopleEditArgs {
                            notes_file: Some(_),
                            ..
                        })
                )
            }
            | Command::Speakers {
                action: SpeakersCmd::Merge { .. }
                    | SpeakersCmd::Unmerge { .. }
                    | SpeakersCmd::NotSame { .. }
            }
            | Command::Identity {
                action: IdentityCmd::ReResolve { dry_run: false } | IdentityCmd::Calibrate { .. }
            }
            | Command::Detect {
                action: DetectCmd::Policy { .. }
            }
            | Command::Enrich { action: None, .. }
            | Command::Templates {
                action: TemplatesCmd::Delete { .. }
                    | TemplatesCmd::Reset { .. }
                    | TemplatesCmd::Run { .. }
            }
            | Command::Providers {
                action: ProvidersCmd::Enable { .. }
                    | ProvidersCmd::SetKey { .. }
                    | ProvidersCmd::DeleteKey { .. }
            }
            | Command::Integrations {
                action: IntegrationsCmd::Login {
                    integration: IntegrationArg::Google | IntegrationArg::MicrosoftGraph,
                    ..
                } | IntegrationsCmd::Logout { .. }
                    | IntegrationsCmd::AddAccount { .. }
                    | IntegrationsCmd::ReconnectAccount { .. }
                    | IntegrationsCmd::GrantAccount { .. }
                    | IntegrationsCmd::RemoveAccount { .. }
                    | IntegrationsCmd::DiscardLegacy { .. }
                    | IntegrationsCmd::Setup {
                        client_secret_stdin: true,
                        ..
                    }
                    | IntegrationsCmd::Setup {
                        clear_client_secret: true,
                        ..
                    }
            }
            | Command::Library {
                action: LibraryCmd::Switch { .. }
            }
            | Command::Settings {
                action: SettingsCmd::LibraryFolder { path: Some(_) }
                    | SettingsCmd::ImportHandling {
                        value: Some(HandlingArg::Move)
                    }
                    | SettingsCmd::ConsentReminder {
                        value: Some(OnOff::Off)
                    },
            }
            | Command::Models {
                action: ModelsCmd::Pull {
                    allow_unverified_license: true,
                    ..
                } | ModelsCmd::Use {
                    allow_unverified_license: true,
                    ..
                },
            }
    ) || match command {
        Command::Import(args) => {
            matches!(args.handling, Some(HandlingArg::Move))
                || args.manifest.is_some()
                || args.spec_stdin
                || args.enrich
        }
        Command::Library {
            action: LibraryCmd::Repair { only, .. },
        } => only.iter().any(|scope| *scope != RepairScopeArg::Index),
        Command::Setup { action, args } => {
            args.allow_unverified_license
                || args.spec_stdin
                || args.words_file.is_some()
                || args.people_file.is_some()
                || args.enroll_me.is_some()
                || match action.as_deref() {
                    Some(SetupCmd::MeetingApps {
                        policy: Some(_), ..
                    }) => true,
                    Some(SetupCmd::Voice(args)) => {
                        args.delete_enrollment
                            || args.from_file.is_some()
                            || args.confirm_file.is_some()
                    }
                    Some(SetupCmd::Try(args)) => args.from_file.is_some(),
                    _ => false,
                }
        }
        _ => false,
    };
    if excluded {
        return Err(Error::Unsupported(
            "That operation is unavailable through MCP.".into(),
        ));
    }
    Ok(())
}

impl Server {
    async fn command_read(
        &self,
        ctx: &RequestContext<RoleServer>,
        command: Command,
        input: Option<String>,
    ) -> CallToolResult {
        reply(self.run_command(ctx, command, input, false).await)
    }

    async fn command_write(
        &self,
        ctx: &RequestContext<RoleServer>,
        command: Command,
        input: Option<String>,
    ) -> CallToolResult {
        reply(self.run_command(ctx, command, input, true).await)
    }

    async fn run_command(
        &self,
        ctx: &RequestContext<RoleServer>,
        command: Command,
        input: Option<String>,
        write: bool,
    ) -> Result<Value> {
        check_command(&command)?;
        if write || !crate::cmd::maintenance::allowed(&command) {
            self.require_normal()?;
        }
        let slots = if write {
            &self.write_slots
        } else {
            &self.command_slots
        };
        until_cancelled(ctx, async {
            let permit = slots.clone().acquire_owned().await
                .map_err(|_| Error::Internal("command slots closed".into()))?;
            let (tx, mut rx) = oneshot::channel();
            let (updates, mut latest) = tokio::sync::watch::channel(None);
            let cancel_flag = CancelCommand(Arc::new(AtomicBool::new(false)));
            let mut progress = Progress::of(ctx, 1);
            progress.phase("starting command").await;
            self.jobs.send(WorkerJob::Command(Box::new(CommandJob {
                data_dir: self.data_dir.clone(),
                library: self.lib.root().to_owned(),
                command,
                input,
                cancel: Cancel::of(ctx),
                cancel_flag: cancel_flag.0.clone(),
                progress: updates,
                edit_history: self.edit_history.clone(),
                reply: tx,
                _permit: permit,
            }))).await.map_err(|_| Error::Internal("command worker closed".into()))?;
            let mut last_progress = 0.0;
            loop {
                tokio::select! {
                    biased;
                    result = &mut rx => return result.map_err(|_| Error::Internal("command worker closed".into()))?,
                    changed = latest.changed() => {
                        if changed.is_err() {
                            return rx.await.map_err(|_| Error::Internal("command worker closed".into()))?;
                        }
                        let fraction = *latest.borrow_and_update();
                        if let Some((done, total)) = fraction && done > last_progress {
                            last_progress = done;
                            progress.notify(done, total, "downloading models").await;
                        }
                    }
                }
            }
        }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renamed_adapters_cannot_dispatch_excluded_commands_or_arguments() {
        for args in [
            &["delete", "m-000001", "--yes"][..],
            &["record", "discard", "--session", "s-1", "--yes"],
            &[
                "record",
                "resolve-journal",
                "discard",
                "--session",
                "s-1",
                "--yes",
            ],
            &[
                "record",
                "resolve-journal",
                "adopt",
                "--session",
                "s-1",
                "--yes",
            ],
            &["retention", "apply", "--dry-run"],
            &["retention", "policy", "processed"],
            &["people", "merge", "--spec-stdin"],
            &["people", "merge", "--resume"],
            &["people", "edit", "synthetic", "--notes-file", "./notes.txt"],
            &["speakers", "merge", "m-000001", "S1", "S2"],
            &["speakers", "unmerge", "m-000001", "S1"],
            &["speakers", "not-same", "m-000001", "S1", "S2"],
            &["identity", "re-resolve"],
            &["providers", "enable", "synthetic", "--accept"],
            &["providers", "set-key", "synthetic"],
            &["providers", "delete-key", "synthetic"],
            &["agents", "uninstall"],
            &["daemon", "uninstall"],
            &["mcp", "enable"],
            &["mcp", "disable"],
            &["maintenance", "enable"],
            &["maintenance", "disable"],
            &["update", "install", "--yes"],
            &["update", "rollback", "--yes"],
            &["update", "release-key", "generate", "--label", "synthetic"],
            &["integrations", "login", "google"],
            &["uninstall", "--dry-run"],
            &["uninstall", "--yes"],
            &["integrations", "login", "microsoft-graph"],
            &["integrations", "logout", "google"],
            &["integrations", "logout", "google", "--revoke-at-google"],
            &["integrations", "add-account", "google"],
            &["integrations", "setup", "google", "--client-secret-stdin"],
            &["integrations", "setup", "google", "--clear-client-secret"],
            &["detect", "policy", "zoom", "auto"],
            &["setup", "voice", "--delete-enrollment"],
            &[
                "setup",
                "voice",
                "--from-file",
                "./synthetic.wav",
                "--check-recording",
            ],
            &["setup", "--pull-models", "--allow-unverified-license"],
            &["setup", "--words-file", "./words.txt"],
            &["setup", "--people-file", "./people.csv"],
            &["setup", "--spec-stdin", "--non-interactive"],
            &["templates", "delete", "synthetic"],
            &["templates", "reset", "synthetic"],
            &["templates", "run", "m-000001", "synthetic"],
            &["enrich", "m-000001"],
            &["queue", "work"],
            &["library", "repair", "--only", "deletions", "--yes"],
            &["library", "switch", "./elsewhere"],
            &["settings", "library-folder", "./elsewhere"],
            &["import", "./synthetic.wav", "--handling", "move"],
            &["import", "--manifest", "./synthetic.json"],
            &["import", "--spec-stdin"],
            &["import", "./synthetic.wav", "--enrich"],
            &["settings", "import-handling", "move"],
            &["settings", "consent-reminder", "off"],
            &["models", "pull", "synthetic", "--allow-unverified-license"],
        ] {
            let crate::cli::Parsed::Run(cli) = crate::cli::parse(
                ["broscribe"]
                    .into_iter()
                    .chain(args.iter().copied())
                    .map(Into::into)
                    .collect(),
            ) else {
                panic!("test command did not parse: {args:?}");
            };
            assert!(check_command(&cli.command).is_err(), "{args:?}");
        }
    }

    #[test]
    fn typed_guard_rejects_a_confirmation_file_without_a_recording_file() {
        let command = Command::Setup {
            action: Some(Box::new(SetupCmd::Voice(VoiceArgs {
                confirm_file: Some(PathBuf::from("synthetic.wav")),
                ..Default::default()
            }))),
            args: SetupArgs::default(),
        };
        assert!(check_command(&command).is_err());
    }

    #[test]
    fn typed_guard_preserves_supported_counterparts() {
        for args in [
            &["providers", "disable", "synthetic"][..],
            &["integrations", "login", "system-calendar"],
            &[
                "integrations",
                "setup",
                "google",
                "--client-id",
                "synthetic",
            ],
            &["people", "forget", "synthetic", "--yes"],
            &["people", "edit", "synthetic", "--name", "Synthetic Person"],
            &["people", "edit", "synthetic", "--spec-stdin"],
            &["import", "./synthetic.wav", "--dry-run"],
            &["identity", "re-resolve", "--dry-run"],
            &["library", "repair", "--only", "index", "--yes"],
            &["mcp", "status"],
        ] {
            let crate::cli::Parsed::Run(cli) = crate::cli::parse(
                ["broscribe"]
                    .into_iter()
                    .chain(args.iter().copied())
                    .map(Into::into)
                    .collect(),
            ) else {
                panic!("test command did not parse: {args:?}");
            };
            assert!(check_command(&cli.command).is_ok(), "{args:?}");
        }
    }
}
