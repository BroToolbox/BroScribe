//! [`run_handoff`]: replacement steps that run after the installation's
//! processes exited, for platforms that cannot move files in use.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use broscribe_core::config::Config;
use broscribe_core::{Error, Result};
use broscribe_platform::update::{PayloadIdentity, Replacement, UpdatePlatform};
use broscribe_platform::{Autostart, ProcessProbe};
use broscribe_updates::{Cancel, InstallLease, RecoveryOutcome, UpdateExpectation, Updater, codes};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Offline, RUNNER_TIMEOUT, handoff_path, load, remove, report_path, save, state_dir};

/// The replacement step handed over.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HandoffAction {
    /// Install exactly the staged update `expected` names (the one the
    /// command checked and the user confirmed).
    Install { expected: UpdateExpectation },
    /// Restore the previous installation in place of exactly the
    /// installed, unconfirmed update `expected` names.
    Rollback { expected: UpdateExpectation },
    /// Finish or undo an interrupted replacement.
    Recover,
}

/// What `broscribe update` hands the runner. Written only after the install
/// admission `fence` was confirmed (durable): it outlives the command's
/// connection and the agent's restart, so no capture starts until
/// `update confirm`, `update rollback` or `update recover` releases it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Handoff {
    pub schema: u32,
    pub data_dir: PathBuf,
    /// The installed `broscribe` executable: the installation is qualified
    /// from it (not from the runner's own location) and the agent is the
    /// one registered beside it.
    pub executable: PathBuf,
    /// The installation root the command qualified: nothing moves while
    /// any process runs from it (the app, the agent, assistant
    /// connections, children started later).
    pub installation_root: PathBuf,
    /// The active agent's library and profile paths, captured before it stops.
    #[serde(default)]
    pub protected_paths: Vec<PathBuf>,
    /// Discovered profile folders, and the CLI working directory used by
    /// any relative library in their configuration.
    #[serde(default)]
    pub profile_dirs: Vec<PathBuf>,
    #[serde(default)]
    pub config_base: PathBuf,
    /// Absolute library reported by the agent before it stopped.
    #[serde(default)]
    pub agent_library: Option<PathBuf>,
    /// Locations (such as the user's home) the installation must not contain.
    #[serde(default)]
    pub contained_paths: Vec<PathBuf>,
    /// Processes that must exit before the agent is stopped (the command
    /// that wrote the handoff).
    pub wait_pids: Vec<u32>,
    pub fence: u64,
    pub action: HandoffAction,
}

impl Handoff {
    pub const SCHEMA: u32 = 2;

    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        load(&handoff_path(data_dir), Self::SCHEMA, |h: &Self| h.schema)
    }

    pub fn save(&self) -> Result<()> {
        save(&handoff_path(&self.data_dir), self)
    }

    pub fn remove(data_dir: &Path) -> Result<()> {
        remove(&handoff_path(data_dir))
    }
}

/// How a handed-over step ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RunnerOutcome {
    /// Replaced and natively verified; awaiting `update confirm` (or
    /// `update rollback`).
    Installed {
        from: PayloadIdentity,
        to: PayloadIdentity,
    },
    /// `restored` is installed again; `update recover` proves the agent
    /// runs it and ends the admission.
    RolledBack { restored: PayloadIdentity },
    /// Nothing was being replaced.
    Unchanged,
    /// The step failed: nothing was replaced, or the previous installation
    /// was restored, or (`update_recovery_failed`) the journal is kept.
    /// `code` is the updater's stable code. The admission stays held:
    /// `update recover` ends it once the agent proves it runs the
    /// installation left in place ([`crate::InstallRecord::unchanged`]
    /// when nothing stayed replaced).
    Failed { code: String, message: String },
}

/// What the runner leaves for `update status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerReport {
    pub schema: u32,
    pub fence: u64,
    pub finished_at: DateTime<Utc>,
    pub outcome: RunnerOutcome,
    /// Why starting the agent again failed, if it did.
    pub agent_start_error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<RunnerWarning>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerWarning {
    pub state_dir: PathBuf,
    pub message: String,
}

impl RunnerReport {
    pub const SCHEMA: u32 = 1;

    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        load(&report_path(data_dir), Self::SCHEMA, |r: &Self| r.schema)
    }

    pub fn remove(data_dir: &Path) -> Result<()> {
        remove(&report_path(data_dir))
    }
}

/// The runner's view of the system.
pub struct RunnerDeps<'a> {
    /// Qualifies the installation the handoff names
    /// (`NativePlatform::updater_for`).
    pub platform: &'a dyn UpdatePlatform,
    /// The agent registered beside the installed executable.
    pub autostart: &'a dyn Autostart,
    pub probe: &'a dyn ProcessProbe,
    /// Whether an agent answers on the data directory's endpoint.
    pub agent_listening: &'a (dyn Fn(&Path) -> Result<bool> + Sync),
    /// Longest wait for the processes to exit and for the agent to stop.
    pub timeout: Duration,
    pub poll: Duration,
}

/// The confirmed install admission while the agent is stopped: it holds
/// nothing in memory then, and the persisted durable admission makes any
/// agent that starts refuse every capture. Confirming refuses once an agent
/// answers again, so no replacement step runs next to an agent the runner
/// cannot reach.
pub struct StoppedAgentLease<'a> {
    fence: u64,
    data_dir: &'a Path,
    listening: &'a (dyn Fn(&Path) -> Result<bool> + Sync),
}

impl<'a> StoppedAgentLease<'a> {
    pub fn new(
        fence: u64,
        data_dir: &'a Path,
        listening: &'a (dyn Fn(&Path) -> Result<bool> + Sync),
    ) -> Self {
        Self {
            fence,
            data_dir,
            listening,
        }
    }
}

impl InstallLease for StoppedAgentLease<'_> {
    fn fence(&self) -> u64 {
        self.fence
    }

    fn confirm(&self) -> Result<()> {
        if (self.listening)(self.data_dir)? {
            return Err(Error::Service {
                code: codes::ADMISSION_LOST,
                message: "the agent started during the replacement; nothing more is replaced"
                    .into(),
            });
        }
        Ok(())
    }
}

/// Runs `handoff`: waits for its processes to exit, stops the agent in an
/// orderly way, runs the step, starts the agent again (on whatever
/// installation is in place afterwards) and saves the report. The durable
/// admission stays held in every case; the handoff is removed.
pub fn run_handoff(handoff: &Handoff, deps: &RunnerDeps<'_>) -> Result<RunnerReport> {
    let admission = broscribe_core::maintenance::ModeLease::acquire(
        &handoff.data_dir,
        broscribe_core::maintenance::ModeLease::DEFAULT_WAIT,
    )?;
    admission.require_normal()?;
    let mut warnings = Vec::new();
    let outcome =
        step(handoff, deps, &mut warnings).unwrap_or_else(|error| RunnerOutcome::Failed {
            code: error.code().to_owned(),
            message: error.to_string(),
        });
    let agent_start_error = handoff
        .agent_library
        .as_deref()
        .map_or_else(
            || deps.autostart.start(),
            |library| {
                let guard = deps.autostart.lock_login_registration(
                    broscribe_platform::LoginRegistrationGuard::DEFAULT_WAIT,
                )?;
                deps.autostart
                    .start_for_update(&guard, &handoff.data_dir, library)
            },
        )
        .err()
        .map(|e| e.to_string());
    let report = RunnerReport {
        schema: RunnerReport::SCHEMA,
        fence: handoff.fence,
        finished_at: Utc::now(),
        outcome,
        agent_start_error,
        warnings,
    };
    save(&report_path(&handoff.data_dir), &report)?;
    Handoff::remove(&handoff.data_dir)?;
    Ok(report)
}

fn step(
    handoff: &Handoff,
    deps: &RunnerDeps<'_>,
    warnings: &mut Vec<RunnerWarning>,
) -> Result<RunnerOutcome> {
    wait_until(deps, "the BroScribe command to exit", || {
        Ok(handoff
            .wait_pids
            .iter()
            .all(|pid| !deps.probe.is_alive(*pid)))
    })?;
    // Held from here on, so no other update step runs meanwhile; a handoff
    // for another update than the pending one stops before the agent does.
    let config = Config::load(&handoff.data_dir)?;
    let mut updater = Updater::open(
        &state_dir(&handoff.data_dir),
        &config.updates,
        deps.platform,
        &Offline,
    )?;
    let result = step_with_updater(handoff, deps, &mut updater)
        .map_err(|error| explain_root_sharing_violation(handoff, deps.platform, error));
    warnings.extend(
        updater
            .take_inventory_warnings()
            .into_iter()
            .map(|warning| RunnerWarning {
                state_dir: warning.state_dir,
                message: warning.error.to_string(),
            }),
    );
    result
}

fn explain_root_sharing_violation(
    handoff: &Handoff,
    platform: &dyn UpdatePlatform,
    error: Error,
) -> Error {
    let root = &handoff.installation_root;
    // Windows' ERROR_SHARING_VIOLATION. A terminal can hold the directory
    // open even when no executable from the installation remains running.
    if platform.replacement() == Replacement::AfterExit
        && matches!(&error, Error::Io { context, source }
            if context == &root.to_string_lossy() && source.raw_os_error() == Some(32))
    {
        return Error::Service {
            code: codes::INSTALLATION_IN_USE,
            message: format!(
                "Another program or terminal may be using the BroScribe installation folder ({}). \
                 Close the program or change the terminal's current folder before trying again.",
                broscribe_platform::plain_verbatim(root).display(),
            ),
        };
    }
    error
}

fn step_with_updater(
    handoff: &Handoff,
    deps: &RunnerDeps<'_>,
    updater: &mut Updater<'_>,
) -> Result<RunnerOutcome> {
    if handoff.protected_paths.is_empty()
        || handoff.contained_paths.is_empty()
        || !handoff.config_base.is_absolute()
        || !handoff.profile_dirs.contains(&handoff.data_dir)
        || handoff.profile_dirs.iter().any(|p| !p.is_absolute())
        || handoff
            .agent_library
            .as_ref()
            .is_some_and(|p| !p.is_absolute())
    {
        return Err(Error::Service {
            code: codes::LOCATION_UNSUPPORTED,
            message: "the update handoff does not identify the protected profile and home paths"
                .into(),
        });
    }
    updater.protect_from_containing(handoff.contained_paths.iter().cloned())?;
    updater.protect_paths(
        handoff
            .protected_paths
            .iter()
            .map(|path| handoff.config_base.join(path)),
    )?;
    updater.protect_paths([handoff.data_dir.clone(), handoff.data_dir.join("cache")])?;
    let registered = deps.autostart.registered_profiles()?;
    let mut profile_dirs = handoff.profile_dirs.clone();
    profile_dirs.extend(registered.iter().map(|profile| profile.data_dir.clone()));
    updater.protect_profiles(profile_dirs, handoff.config_base.clone())?;
    updater.protect_paths(registered.into_iter().filter_map(|profile| profile.library))?;
    updater.protect_paths(handoff.agent_library.iter().cloned())?;
    match &handoff.action {
        HandoffAction::Install { expected } => {
            updater.prepare_install(Some(expected))?;
        }
        HandoffAction::Rollback { expected } => updater.awaiting_expected(expected)?,
        HandoffAction::Recover => updater.prepare_recovery()?,
    }
    // Orderly: the agent saves and exits; nothing records under the
    // durable admission, so no recording is cut short.
    deps.autostart.stop()?;
    wait_until(deps, "the agent to stop", || {
        Ok(!(deps.agent_listening)(&handoff.data_dir)?)
    })?;
    if deps.platform.replacement() == Replacement::AfterExit {
        // Re-read executable identities, not stale PIDs: every process
        // running from this installation must release its files.
        wait_until(
            deps,
            "every BroScribe program running from the installation to exit (quit the app)",
            || {
                Ok(deps
                    .platform
                    .installation_processes(&handoff.installation_root)?
                    .is_empty())
            },
        )?;
    }
    let lease = StoppedAgentLease::new(handoff.fence, &handoff.data_dir, deps.agent_listening);
    match &handoff.action {
        HandoffAction::Install { expected } => {
            let installed = updater.install(expected, &lease, &Cancel::new())?;
            Ok(RunnerOutcome::Installed {
                from: installed.from,
                to: installed.to,
            })
        }
        HandoffAction::Rollback { expected } => Ok(RunnerOutcome::RolledBack {
            restored: updater.rollback(expected, &lease)?,
        }),
        HandoffAction::Recover => Ok(match updater.recover(&lease)? {
            RecoveryOutcome::RolledBack { restored } => RunnerOutcome::RolledBack { restored },
            RecoveryOutcome::AwaitingConfirmation { from, to } => {
                RunnerOutcome::Installed { from, to }
            }
            RecoveryOutcome::Clean | RecoveryOutcome::DiscardedStaged => RunnerOutcome::Unchanged,
        }),
    }
}

fn wait_until(
    deps: &RunnerDeps<'_>,
    what: &str,
    mut done: impl FnMut() -> Result<bool>,
) -> Result<()> {
    let deadline = Instant::now() + deps.timeout;
    loop {
        if done()? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(Error::Service {
                code: RUNNER_TIMEOUT,
                message: format!("gave up waiting for {what}; nothing was replaced"),
            });
        }
        std::thread::sleep(deps.poll);
    }
}
