//! `broscribe update …`: the signed updater (SH23).
//!
//! Checks use the compiled stable release source by default.
//! `check` and `download` verify
//! the signed envelope and stage a natively verified payload; nothing is
//! replaced before `install --yes` or `install --expectation-stdin`. The
//! latter (what the app sends) names
//! the exact update the user confirmed (`from`, `to` with its target, and
//! the verified signer); the updater compares it under its locks before
//! any admission is asked for and refuses anything else with
//! `update_changed`, so a confirmation never installs or rolls back an
//! update staged or installed after it was shown.
//!
//! Replacement is fenced by BroScribe Agent's install admission over a
//! live protocol connection ([`AgentLease`]): `install.acquire` is refused
//! while anything records, may record or is not saved, and binds the
//! admission to that connection; the updater confirms it right before the
//! first irreversible step, which makes it durable. A durable admission
//! outlives the connection and agent restarts, so no capture can open
//! between the replacement and `confirm` or `rollback`. Later commands
//! reattach to that same held fence (never release and re-acquire, which
//! would leave a gap for a capture to start), and release it only once the
//! agent proved it is a new process running the expected build
//! ([`InstallRecord`]), or, after a step that replaced nothing, the
//! unchanged build the record names. Every failure that may leave an
//! installation half-replaced keeps the admission held; `update recover`
//! finishes.
//!
//! Where the platform cannot move files in use
//! ([`Replacement::AfterExit`]), the replacement step is handed to the
//! external update runner (`broscribe-update-runner`), copied outside the
//! installation, with the confirmed admission.
//!
//! Nothing here stops a recording: the agent is restarted only while it
//! holds the durable admission, which means nothing records.
//!
//! `update admission acquire|release` give a replacement made outside this
//! updater (`scripts/update-macos.py`) the same durable admission, owned
//! through its own record so that `update recover` never ends it.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use broscribe_core::config::{CONFIG_STALE, Config, UpdateSettings};
use broscribe_core::{Error, Result, fsutil};
use broscribe_ipc::{AgentBuild, Call, DaemonState, InstallHold, IpcError, PROTOCOL_VERSION};
use broscribe_platform::paths::ENV_DATA_DIR;
use broscribe_platform::update::{
    PayloadIdentity, Replacement, SignerIdentity, UpdatePlatform, installed_executable,
};
use broscribe_platform::{AppPaths, TrustStore};
use broscribe_platform_native::ipc::{CallError, Client, Endpoint};
use broscribe_update_runner::{
    Handoff, HandoffAction, InstallRecord, Offline, RunnerReport, runner_dir, runner_file_name,
    state_dir,
};
use broscribe_updates::{
    Cancel, CheckOutcome, ExistingKey, HttpsTransport, InstallLease, Limits, RecoveryOutcome,
    ReleaseKeyLabel, Transport, UpdateConfig, UpdateExpectation, UpdateOffer, UpdateStatus,
    Updater, codes, generate_release_key, new_release_key_file, release_key, sign_manifest,
    sign_manifest_file, verify_release_envelope,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cli::{AdmissionCmd, ReleaseKeyCmd, UpdateCmd, UpdateSettingsCmd};
use crate::{Ctx, LibrarySource};

/// Error code: the agent did not prove it is a new process running the
/// expected build, so the install admission stays held.
pub(crate) const AGENT_UNPROVEN: &str = "update_agent_unproven";
const RESTART_LIBRARY_MISMATCH: &str = "update_restart_library_mismatch";
/// Error code: the agent speaks another IPC protocol than this command, so
/// this command can neither hold its admission nor prove which build it
/// runs. Waiting does not change that.
const HELPER_OUTDATED: &str = "helper_outdated";

/// Per-call reply timeout of the agent connection.
const AGENT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a restarted agent has to prove it runs the expected build.
const RESTART_WAIT: Duration = Duration::from_secs(60);
const RESTART_POLL: Duration = Duration::from_millis(250);

/// What `broscribe update` needs from the system: the platform's updater
/// side and where HTTPS trust comes from.
pub struct UpdateDeps {
    pub(super) platform: Arc<dyn UpdatePlatform>,
    transport: TransportSource,
}

enum TransportSource {
    /// [`HttpsTransport`] with the platform-selected TLS strategy, built only
    /// by download commands so other operations survive an unavailable probe.
    /// Native evaluation failure never retries with a weaker verifier.
    System(Arc<dyn TrustStore>),
    Given(Arc<dyn Transport>),
}

impl UpdateDeps {
    /// The native updater using the platform's HTTPS verification strategy.
    pub fn system(platform: Arc<dyn UpdatePlatform>, trust: Arc<dyn TrustStore>) -> Self {
        Self {
            platform,
            transport: TransportSource::System(trust),
        }
    }

    /// An explicit transport (tests serve synthetic feeds this way).
    pub fn with_transport(
        platform: Arc<dyn UpdatePlatform>,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self {
            platform,
            transport: TransportSource::Given(transport),
        }
    }

    fn transport(&self) -> Result<Arc<dyn Transport>> {
        match &self.transport {
            TransportSource::System(trust) => Ok(Arc::new(HttpsTransport::new(trust.as_ref())?)),
            TransportSource::Given(transport) => Ok(Arc::clone(transport)),
        }
    }
}

pub(super) fn run(ctx: &mut Ctx<'_>, action: UpdateCmd) -> Result<()> {
    match action {
        UpdateCmd::Status => status(ctx),
        UpdateCmd::Check => check(ctx),
        UpdateCmd::Download => download(ctx),
        UpdateCmd::Discard => discard(ctx),
        // clap requires `--yes` or `--expectation-stdin` for both.
        UpdateCmd::Install {
            yes: _,
            expectation_stdin,
        } => install(ctx, read_expectation(expectation_stdin, "update install")?),
        UpdateCmd::Confirm => confirm(ctx),
        UpdateCmd::Rollback {
            yes: _,
            expectation_stdin,
        } => rollback(ctx, read_expectation(expectation_stdin, "update rollback")?),
        UpdateCmd::Recover => recover(ctx),
        UpdateCmd::Admission { action } => match action {
            AdmissionCmd::Acquire => admission_acquire(ctx),
            AdmissionCmd::Release { fence } => admission_release(ctx, fence),
        },
        UpdateCmd::Settings { action } => match action {
            UpdateSettingsCmd::Show => settings_show(ctx),
            UpdateSettingsCmd::Set {
                spec_stdin: _,
                enabled,
                expect_revision,
            } => settings_set(ctx, enabled, expect_revision),
        },
        UpdateCmd::ReleaseKey { action } => match action {
            ReleaseKeyCmd::Generate {
                label,
                replace_existing,
            } => release_key_generate(ctx, &label, replace_existing),
            ReleaseKeyCmd::Show { label } => release_key_show(ctx, &label),
            ReleaseKeyCmd::New { out } => {
                let key = new_release_key_file(&out)?;
                emit(
                    ctx,
                    json!({ "public_key": key.public_key_hex(), "key_id": key.key_id.to_string() }),
                )
            }
        },
        UpdateCmd::SignManifest {
            keys,
            key_file,
            manifest_stdin: _,
        } => sign(ctx, &keys, key_file.as_deref()),
        UpdateCmd::VerifyManifest {
            envelope_stdin: _,
            allow_expired,
        } => verify_release(ctx, allow_expired),
    }
}

fn emit(ctx: &mut Ctx<'_>, value: Value) -> Result<()> {
    ctx.emit(value.clone(), |out| super::write_value(out, &value, 0))
}

/// Opens the updater with `settings`, the `[updates]` table the caller read
/// from the config, and holds its state lock for the updater's lifetime.
/// Once the lock is held those settings must still be the saved ones:
/// `update settings set` replaces enabled settings only while it holds
/// this lock and nothing is pending, so a command that read them before
/// such a change refuses (`config_stale`, nothing done) instead of
/// checking, staging or installing under settings no longer saved.
fn open<'a>(
    deps: &'a UpdateDeps,
    data_dir: &Path,
    settings: &UpdateSettings,
    transport: &'a dyn Transport,
) -> Result<Updater<'a>> {
    #[cfg(test)]
    pause(Pause::BeforeLock);
    let updater = Updater::open(
        &state_dir(data_dir),
        settings,
        deps.platform.as_ref(),
        transport,
    )?;
    if Config::load(data_dir)?.updates != *settings {
        return Err(Error::Service {
            code: CONFIG_STALE,
            message: "the update settings changed after this command read them; nothing was \
                      done, run it again"
                .into(),
        });
    }
    #[cfg(test)]
    pause(Pause::Locked);
    Ok(updater)
}

/// `update install|rollback --expectation-stdin`: the exact update the
/// user confirmed (`{"from","to","signer"}`), read whole from stdin.
fn read_expectation(given: bool, command: &str) -> Result<Option<UpdateExpectation>> {
    if given {
        crate::query::read_stdin(command).map(Some)
    } else {
        Ok(None)
    }
}

fn status_value(status: &UpdateStatus) -> Value {
    match status {
        UpdateStatus::Idle => json!({ "state": "idle" }),
        UpdateStatus::Staged { from, to, signer } => {
            json!({ "state": "staged", "from": from, "to": to, "signer": signer })
        }
        UpdateStatus::Interrupted { from, to } => {
            json!({ "state": "interrupted", "from": from, "to": to })
        }
        UpdateStatus::AwaitingConfirmation { from, to, signer } => {
            json!({ "state": "awaiting_confirmation", "from": from, "to": to, "signer": signer })
        }
    }
}

fn signer_text(signer: &SignerIdentity) -> String {
    match &signer.signer {
        Some(signer) => signer.clone(),
        None => "the signed release manifest only (no OS code signature)".into(),
    }
}

fn not_awaiting(status: &UpdateStatus) -> Error {
    match status {
        UpdateStatus::Idle => Error::NotFound("no installed update awaits confirmation".into()),
        _ => Error::Service {
            code: codes::PENDING,
            message:
                "the pending update is not awaiting confirmation; check `broscribe update status`"
                    .into(),
        },
    }
}

// ---- the agent ----

fn agent_required() -> Error {
    Error::DaemonUnavailable(
        "BroScribe Agent isn't running; start it with `broscribe daemon start`: only the \
         agent can keep recording off while BroScribe is replaced"
            .into(),
    )
}

fn connect(data_dir: &Path) -> Result<Option<Client>> {
    Client::connect_daemon(&Endpoint::new(data_dir), AGENT_TIMEOUT)
        .map_err(|e| Error::io("connecting to the agent", e))
}

fn remote(error: IpcError) -> Error {
    let code = match error.code.as_str() {
        IpcError::RECORDING_ACTIVE => Some(codes::RECORDING_ACTIVE),
        IpcError::ADMISSION_LOST => Some(codes::ADMISSION_LOST),
        IpcError::INSTALLING => Some(codes::INSTALLING),
        IpcError::UNSUPPORTED_VERSION => Some(HELPER_OUTDATED),
        other => super::daemon::service_code(other),
    };
    match code {
        Some(code) => Error::Service {
            code,
            message: error.message,
        },
        None => error.into_core(),
    }
}

fn call(client: &mut Client, call: Call) -> Result<Value> {
    client.call(call).map_err(|e| match e {
        CallError::Remote(e) => remote(e),
        CallError::NotSent(io) => match other_protocol(&io) {
            Some(refusal) => remote(refusal.clone()),
            None => Error::DaemonUnavailable(format!(
                "BroScribe Agent did not answer: {}",
                CallError::NotSent(io)
            )),
        },
        other => Error::DaemonUnavailable(format!("BroScribe Agent did not answer: {other}")),
    })
}

/// The version refusal an agent of another protocol answered with: its
/// reply carries that protocol's version, so the client cannot read it as
/// a response and reports the refusal as unreadable data instead.
fn other_protocol(error: &std::io::Error) -> Option<&IpcError> {
    error
        .get_ref()?
        .downcast_ref::<IpcError>()
        .filter(|refusal| refusal.code == IpcError::UNSUPPORTED_VERSION)
}

fn daemon_state(client: &mut Client) -> Result<DaemonState> {
    let ping = broscribe_ipc::AgentPing::from_value(call(client, Call::Ping)?)
        .map_err(IpcError::into_core)?;
    if ping.mode != broscribe_core::maintenance::Mode::Normal {
        return Err(super::maintenance::denied());
    }
    // Ping classifies the responder only. Normal Status remains mandatory
    // for new-PID, runs_build and durable-fence proof downstream.
    let value = call(client, Call::Status)?;
    let state: DaemonState = serde_json::from_value(value)
        .map_err(|e| Error::Internal(format!("BroScribe Agent’s status is unreadable: {e}")))?;
    if state.protocol != PROTOCOL_VERSION {
        return Err(Error::Service {
            code: HELPER_OUTDATED,
            message: format!(
                "BroScribe Agent speaks protocol {}, not {PROTOCOL_VERSION}; it cannot hold an install admission",
                state.protocol
            ),
        });
    }
    if state.pid != ping.pid {
        return Err(Error::Internal(
            "BroScribe Agent changed during updater observation".into(),
        ));
    }
    Ok(state)
}

fn agent_state(data_dir: &Path) -> Result<Option<DaemonState>> {
    match connect(data_dir)? {
        None => Ok(None),
        Some(mut client) => daemon_state(&mut client).map(Some),
    }
}

pub(super) fn admitted_agent_pid(data_dir: &Path, fence: u64) -> Result<Option<u32>> {
    let Some(state) = agent_state(data_dir)? else {
        return Ok(None);
    };
    verify_durable_fence(&state, fence)?;
    Ok(Some(state.pid))
}

fn verify_durable_fence(state: &DaemonState, fence: u64) -> Result<()> {
    if state.install
        != Some(InstallHold {
            fence,
            durable: true,
        })
    {
        return Err(Error::Service {
            code: codes::ADMISSION_LOST,
            message: format!("BroScribe Agent does not hold durable install admission {fence}"),
        });
    }
    Ok(())
}

/// One install admission of the agent, over the live connection that
/// acquired it or reattached to it. An admission not yet confirmed ends
/// when this connection closes (this process exits); a confirmed one is
/// kept by the agent until released by fence.
struct AgentLease {
    client: Mutex<Client>,
    fence: u64,
    /// Confirmed, or possibly confirmed (the answer was lost): treated as
    /// held, never as released.
    durable: AtomicBool,
}

impl AgentLease {
    fn acquire(mut client: Client) -> Result<Self> {
        let value = call(&mut client, Call::InstallAcquire)?;
        let fence = value
            .get("fence")
            .and_then(Value::as_u64)
            .ok_or_else(|| Error::Internal("BroScribe Agent granted no install fence".into()))?;
        Ok(Self {
            client: Mutex::new(client),
            fence,
            durable: AtomicBool::new(false),
        })
    }

    /// The durable admission the agent already holds, on the same
    /// connection that read its state, or a new one when it holds none:
    /// a held admission is reattached, never released and acquired again.
    fn held_or_acquire(data_dir: &Path) -> Result<(Self, DaemonState)> {
        let mut client = connect(data_dir)?.ok_or_else(agent_required)?;
        let state = daemon_state(&mut client)?;
        let lease = match state.install {
            Some(InstallHold {
                fence,
                durable: true,
            }) => Self {
                client: Mutex::new(client),
                fence,
                durable: AtomicBool::new(true),
            },
            Some(InstallHold { durable: false, .. }) => {
                return Err(Error::Service {
                    code: "busy",
                    message: "another process is acquiring the install admission".into(),
                });
            }
            None => Self::acquire(client)?,
        };
        Ok((lease, state))
    }

    fn is_durable(&self) -> bool {
        self.durable.load(Ordering::SeqCst)
    }
}

impl InstallLease for AgentLease {
    fn fence(&self) -> u64 {
        self.fence
    }

    fn confirm(&self) -> Result<()> {
        let mut client = self.client.lock().unwrap_or_else(PoisonError::into_inner);
        match client.call(Call::InstallConfirm { fence: self.fence }) {
            Ok(_) => {
                self.durable.store(true, Ordering::SeqCst);
                Ok(())
            }
            Err(CallError::Remote(e)) => Err(remote(e)),
            Err(other) => {
                // The agent may have made it durable before the answer was
                // lost: keep treating it as held.
                self.durable.store(true, Ordering::SeqCst);
                Err(Error::Service {
                    code: codes::ADMISSION_LOST,
                    message: format!(
                        "the connection to the agent broke while confirming the install \
                         admission ({other}); nothing more is replaced"
                    ),
                })
            }
        }
    }
}

/// The admission of recovery steps that replace nothing: confirming it
/// fails, so an unexpected replacement step stops instead of running
/// unfenced.
struct NoAdmission;

impl InstallLease for NoAdmission {
    fn fence(&self) -> u64 {
        0
    }

    fn confirm(&self) -> Result<()> {
        Err(Error::Service {
            code: codes::ADMISSION_LOST,
            message: "no install admission is held for this step".into(),
        })
    }
}

/// Ends admission `fence` (the agent accepts it from any connection).
fn release(data_dir: &Path, fence: u64) -> Result<()> {
    let mut client = connect(data_dir)?.ok_or_else(agent_required)?;
    call(&mut client, Call::InstallRelease { fence }).map(drop)
}

/// The agent reports that it runs exactly `expected`: its version and the
/// build number and target it read from its installation at start-up. An
/// agent that cannot say which build it runs proves nothing. Returns that
/// build.
fn runs_build<'s>(state: &'s DaemonState, expected: &PayloadIdentity) -> Result<&'s AgentBuild> {
    let Some(build) = &state.build else {
        return Err(Error::Service {
            code: AGENT_UNPROVEN,
            message:
                "BroScribe Agent cannot tell which build it runs, so the installed build is not \
                      proven to run"
                    .into(),
        });
    };
    if state.version != expected.version
        || build.build != expected.build
        || build.target != expected.target
    {
        return Err(Error::Service {
            code: AGENT_UNPROVEN,
            message: format!(
                "BroScribe Agent runs {} build {} ({}), not the installed {} build {} ({})",
                state.version,
                build.build,
                build.target,
                expected.version,
                expected.build,
                expected.target
            ),
        });
    }
    Ok(build)
}

/// The agent is another process than `before`, [`runs_build`] `expected`
/// over this protocol and still holds the durable admission `fence`.
fn prove<'s>(
    state: &'s DaemonState,
    expected: &PayloadIdentity,
    before: u32,
    fence: u64,
) -> Result<&'s AgentBuild> {
    if state.pid == before {
        return Err(Error::Service {
            code: AGENT_UNPROVEN,
            message: format!(
                "BroScribe Agent has not restarted since the replacement (process {before}); restart it \
                 with `broscribe daemon stop` and `broscribe daemon start`"
            ),
        });
    }
    let build = runs_build(state, expected)?;
    verify_durable_fence(state, fence)?;
    Ok(build)
}

fn prove_restart<'s>(state: &'s DaemonState, record: &InstallRecord) -> Result<&'s AgentBuild> {
    let build = prove(state, &record.expected, record.agent_pid, record.fence)?;
    if record.agent_library.as_deref().is_some_and(|library| {
        let actual = Path::new(&state.library);
        !actual.is_absolute()
            || broscribe_platform::stable_path(actual) != broscribe_platform::stable_path(library)
    }) {
        return Err(Error::Service {
            code: AGENT_UNPROVEN,
            message: "BroScribe Agent restarted with a different library than the admitted agent"
                .into(),
        });
    }
    Ok(build)
}

/// Waits for the agent to [`prove`] itself; a missing admission is final,
/// and so is an agent of another protocol, which this command can never
/// prove.
fn wait_for_agent(
    data_dir: &Path,
    record: &InstallRecord,
    stopped_pid: Option<u32>,
) -> Result<DaemonState> {
    let deadline = Instant::now() + RESTART_WAIT;
    loop {
        let last = match agent_state(data_dir) {
            Ok(Some(state)) if Some(state.pid) == stopped_pid => Error::Service {
                code: AGENT_UNPROVEN,
                message: "BroScribe Agent did not restart after shutdown".into(),
            },
            Ok(Some(state)) => match prove_restart(&state, record).err() {
                None => return Ok(state),
                Some(e) if e.code() == codes::ADMISSION_LOST => return Err(e),
                Some(e) => e,
            },
            Ok(None) => agent_required(),
            Err(e) if e.code() == HELPER_OUTDATED => return Err(e),
            Err(e) => e,
        };
        if Instant::now() >= deadline {
            return Err(last);
        }
        std::thread::sleep(RESTART_POLL);
    }
}

/// Determines whether restarting means stopping a verified service-manager job
/// or shutting down the admitted endpoint. Windows Run entries never own a live job.
fn update_restart_job(
    ctx: &Ctx<'_>,
    live_pid: impl FnOnce() -> Result<Option<u32>>,
) -> Result<bool> {
    use broscribe_platform::LoginJob;
    let autostart = ctx.deps.autostart.as_ref();
    autostart.check_update_restart()?;
    let job = autostart.login_job(ctx.data_dir());
    if ctx.deps.updates.platform.replacement() == Replacement::AfterExit
        && matches!(
            job,
            LoginJob::Registered {
                serves_profile: Some(false)
            }
        )
    {
        return Ok(false);
    }
    super::daemon::verify_login_job(job, live_pid)
}

fn prepare_restart(
    ctx: &Ctx<'_>,
    before: &DaemonState,
) -> Result<broscribe_platform::LoginRegistrationGuard> {
    use broscribe_platform::LoginRegistrationGuard;
    let registration = ctx
        .deps
        .autostart
        .lock_login_registration(LoginRegistrationGuard::DEFAULT_WAIT)?;
    update_restart_job(ctx, || Ok(Some(before.pid)))?;
    let configured = ctx.config()?.library;
    let library = ctx
        .deps
        .autostart
        .update_restart_library(configured.as_deref(), &ctx.paths.default_library_dir)?;
    if broscribe_platform::stable_path(&library)
        != broscribe_platform::stable_path(Path::new(&before.library))
    {
        return Err(Error::Service {
            code: RESTART_LIBRARY_MISMATCH,
            message: format!(
                "Restart BroScribe Agent first so it uses {}, then update.",
                library.display()
            ),
        });
    }
    Ok(registration)
}

/// Restarts the agent under durable admission and proves the new process.
fn restart_agent(ctx: &Ctx<'_>, record: &InstallRecord) -> Result<DaemonState> {
    use broscribe_platform::LoginRegistrationGuard;
    let registration = ctx
        .deps
        .autostart
        .lock_login_registration(LoginRegistrationGuard::DEFAULT_WAIT)?;
    restart_with_registration(ctx, record, registration)
}

fn restart_with_registration(
    ctx: &Ctx<'_>,
    record: &InstallRecord,
    registration: broscribe_platform::LoginRegistrationGuard,
) -> Result<DaemonState> {
    let autostart = ctx.deps.autostart.as_ref();
    let stopped_pid =
        if update_restart_job(ctx, || admitted_agent_pid(ctx.data_dir(), record.fence))? {
            autostart.stop_login_job(&registration)?;
            None
        } else {
            stop_admitted_endpoint(ctx, record.fence)?
        };
    if let Some(library) = &record.agent_library {
        autostart.start_for_update(&registration, ctx.data_dir(), library)?;
    } else {
        autostart.start_with_registration(&registration)?;
    }
    drop(registration);
    wait_for_agent(ctx.data_dir(), record, stopped_pid)
}

/// A one-shot login entry has no service manager job to stop. Shut down
/// the verified agent over the same profile connection that proved the
/// durable fence, while updater state is still locked.
fn stop_admitted_endpoint(ctx: &Ctx<'_>, fence: u64) -> Result<Option<u32>> {
    let Some(mut client) = connect(ctx.data_dir())? else {
        if super::daemon::agent_instance_held(ctx.data_dir(), ctx.deps.agent_instance.as_ref())? {
            return Err(Error::Service {
                code: AGENT_UNPROVEN,
                message: "BroScribe Agent holds this profile's instance lock without a reachable endpoint".into(),
            });
        }
        return Ok(None);
    };
    let state = daemon_state(&mut client)?;
    verify_durable_fence(&state, fence)?;
    if state.recording.is_some() || !state.stopped.is_empty() {
        return Err(Error::Service {
            code: codes::RECORDING_ACTIVE,
            message: "BroScribe Agent reports an active or unsaved recording".into(),
        });
    }
    call(&mut client, Call::Shutdown)?;
    drop(client);
    if !super::daemon::wait_exit(ctx.deps.process.as_ref(), state.pid, RESTART_WAIT) {
        return Err(Error::Service {
            code: AGENT_UNPROVEN,
            message: "BroScribe Agent did not exit after shutdown".into(),
        });
    }
    Ok(Some(state.pid))
}

fn agent_value(outcome: &Result<DaemonState>) -> Value {
    match outcome {
        Ok(state) => json!({ "proven": true, "pid": state.pid, "version": state.version }),
        Err(e) => json!({ "proven": false, "code": e.code(), "message": e.to_string() }),
    }
}

/// Refuses while a handed-over replacement step may still run; removes a
/// handoff whose runner is gone (its admission stays held for recovery).
fn settle_handoff(ctx: &Ctx<'_>) -> Result<()> {
    let data_dir = ctx.data_dir();
    if Handoff::load(data_dir)?.is_none() {
        return Ok(());
    }
    let runner = InstallRecord::load(data_dir)?.and_then(|record| record.runner_pid);
    if runner.is_some_and(|pid| ctx.deps.process.is_alive(pid)) {
        return Err(Error::Service {
            code: codes::BUSY,
            message:
                "the update runner is still replacing BroScribe; see `broscribe update status`"
                    .into(),
        });
    }
    Handoff::remove(data_dir)
}

// ---- status, check, download, discard ----

fn status(ctx: &mut Ctx<'_>) -> Result<()> {
    let config = ctx.config()?;
    let data_dir = ctx.data_dir().to_path_buf();
    let deps = ctx.deps;
    let settings = &config.updates;
    let update = if settings.enabled {
        match broscribe_updates::UpdateConfig::from_settings(settings)
            .and_then(|_| broscribe_updates::inspect_update_directory(&state_dir(&data_dir)))
            .and_then(|()| broscribe_updates::pending_replacement(&state_dir(&data_dir)))
        {
            Ok(update) => status_value(&update),
            Err(e) => json!({ "state": "unavailable", "code": e.code(), "message": e.to_string() }),
        }
    } else {
        json!({ "state": "off" })
    };
    let agent = match agent_state(&data_dir) {
        Ok(Some(state)) => json!({
            "running": true, "pid": state.pid, "version": state.version, "install": state.install,
        }),
        Ok(None) => json!({ "running": false }),
        Err(e) => json!({ "running": null, "code": e.code(), "message": e.to_string() }),
    };
    let runner = RunnerReport::load(&data_dir)?;
    if !ctx.json
        && let Some(report) = &runner
    {
        for warning in &report.warnings {
            ctx.note(format!(
                "warning: skipped update state at {}: {}",
                warning.state_dir.display(),
                warning.message
            ));
        }
    }
    let value = json!({
        "settings": {
            "enabled": settings.enabled,
            "feed_configured": true,
            "source": update_source(settings),
            "channel": settings.channel,
            "trusted_keys": settings.effective_keys().count(),
            "signer": signer_pin(settings, deps.updates.platform.as_ref()),
        },
        "update": update,
        "agent": agent,
        "handed_off": Handoff::load(&data_dir)?.map(|handoff| handoff.action),
        "installed": deps.updates.platform.installation().ok().map(|installation| installation.identity),
        "runner": runner,
    });
    emit(ctx, value)
}

fn signer_pin<'a>(settings: &'a UpdateSettings, platform: &dyn UpdatePlatform) -> Option<&'a str> {
    settings
        .signer
        .as_deref()
        .or_else(|| platform.default_signer_pin())
}

fn update_source(settings: &UpdateSettings) -> &'static str {
    if settings.uses_built_in_source() {
        "built_in"
    } else {
        "overridden"
    }
}

fn offer_value(offer: &UpdateOffer, pin: Option<&str>) -> Value {
    json!({
        "state": "available",
        "version": offer.manifest.version,
        "build": offer.manifest.build,
        "channel": offer.manifest.channel,
        "target": offer.artifact.target,
        "app_id": offer.artifact.app_id,
        "size": offer.artifact.size,
        "installed_size": offer.artifact.installed_size,
        "release_notes": offer.manifest.release_notes,
        "published_at": offer.manifest.published_at,
        "expires_at": offer.manifest.expires_at,
        "installed": offer.installation.identity,
        "installed_signer": offer.installation.signer,
        "signer_pin": pin,
    })
}

fn check(ctx: &mut Ctx<'_>) -> Result<()> {
    let config = ctx.config()?;
    let data_dir = ctx.data_dir().to_path_buf();
    let deps = ctx.deps;
    let transport = deps.updates.transport()?;
    let mut updater = open(
        &deps.updates,
        &data_dir,
        &config.updates,
        transport.as_ref(),
    )?;
    updater.track_seen_profiles(discover_update_profiles(ctx)?.0);
    let value = match with_inventory_warnings(ctx, &mut updater, |updater| {
        updater.check(Utc::now(), &Cancel::new())
    })? {
        CheckOutcome::UpToDate {
            installed,
            latest_version,
            latest_build,
        } => json!({
            "state": "up_to_date",
            "installed": installed,
            "latest": { "version": latest_version, "build": latest_build },
        }),
        CheckOutcome::NoVerifiedUpdate { installed } => {
            json!({ "state": "no_verified_update", "installed": installed })
        }
        CheckOutcome::Available(offer) => offer_value(
            &offer,
            signer_pin(&config.updates, deps.updates.platform.as_ref()),
        ),
    };
    emit(ctx, value)
}

/// Download progress on stderr: `{"progress":{"step":"download","bytes","total"}}`
/// lines under `--json` (the app reads them), a percentage otherwise. At
/// most one line per whole percent.
struct Progress<'w> {
    err: &'w mut dyn Write,
    json: bool,
    total: u64,
    last: Option<u64>,
}

impl Progress<'_> {
    fn report(&mut self, bytes: u64) {
        let percent = bytes.saturating_mul(100) / self.total.max(1);
        if self.last == Some(percent) {
            return;
        }
        self.last = Some(percent);
        let _ = if self.json {
            writeln!(
                self.err,
                "{}",
                json!({ "progress": { "step": "download", "bytes": bytes, "total": self.total } })
            )
        } else {
            writeln!(self.err, "downloading: {percent}%")
        };
    }
}

fn protected_paths(
    ctx: &Ctx<'_>,
    config: &Config,
    agent: Option<&DaemonState>,
) -> Result<Vec<PathBuf>> {
    let mut paths = vec![
        ctx.library_path(config).0,
        ctx.paths.default_library_dir.clone(),
        ctx.data_dir().to_path_buf(),
        ctx.deps.paths.data_dir(),
        ctx.paths.cache_dir.clone(),
        ctx.data_dir().join("cache"),
        ctx.deps.paths.cache_dir(),
    ];
    paths.extend(config.library.iter().cloned());
    paths.extend(ctx.paths.library_from_env.iter().cloned());
    if let Some(agent) = agent {
        if !Path::new(&agent.library).is_absolute() {
            return Err(Error::Service {
                code: codes::LOCATION_UNSUPPORTED,
                message: "BroScribe Agent did not report an absolute library path".into(),
            });
        }
        paths.push(PathBuf::from(&agent.library));
    }
    if ctx.deps.updates.platform.replacement() == Replacement::AfterExit
        && let Some(base) = super::maintenance_config_base()
    {
        for path in &mut paths {
            if !path.is_absolute() {
                *path = base.join(&*path);
            }
        }
    }
    // Retain both the spelling (rechecked at replacement) and the location
    // it names now, in the event a symlink is retargeted in between.
    paths.extend(
        paths
            .iter()
            .map(|path| broscribe_platform::stable_path(path))
            .collect::<Vec<_>>(),
    );
    Ok(paths)
}

struct ProfileProtection {
    dirs: Vec<PathBuf>,
    config_base: PathBuf,
    paths: Vec<PathBuf>,
}

fn discover_update_profiles(ctx: &Ctx<'_>) -> Result<(Vec<PathBuf>, PathBuf, Vec<PathBuf>)> {
    let default = ctx.deps.paths.data_dir();
    let mut dirs = vec![default.clone(), ctx.data_dir().to_path_buf()];
    let registered = ctx.deps.autostart.registered_profiles()?;
    dirs.extend(registered.iter().map(|profile| profile.data_dir.clone()));
    dirs.extend(ctx.deps.agents.uninstall.shortcut_profiles()?);
    let pending = super::uninstall::pending_protection(&default)?;
    dirs.extend(pending.dirs);
    let config_base = if ctx.deps.updates.platform.replacement() == Replacement::AfterExit
        && let Some(base) = super::maintenance_config_base()
    {
        base
    } else {
        std::env::current_dir().map_err(|e| Error::io("resolving profile paths", e))?
    };
    let mut dirs = dirs
        .into_iter()
        .map(|path| config_base.join(path))
        .collect::<Vec<_>>();
    dirs.extend(
        dirs.iter()
            .map(|path| broscribe_platform::stable_path(path))
            .collect::<Vec<_>>(),
    );
    dirs.sort_unstable();
    dirs.dedup();
    let mut libraries: Vec<_> = registered
        .into_iter()
        .filter_map(|profile| profile.library)
        .collect();
    libraries.extend(pending.libraries);
    libraries.extend(
        libraries
            .iter()
            .map(|path| broscribe_platform::stable_path(path))
            .collect::<Vec<_>>(),
    );
    Ok((dirs, config_base, libraries))
}

fn protect_profiles(ctx: &Ctx<'_>, updater: &mut Updater<'_>) -> Result<ProfileProtection> {
    let (dirs, config_base, libraries) = discover_update_profiles(ctx)?;
    let mut paths = updater.protect_profiles(dirs.clone(), config_base.clone())?;
    updater.protect_paths(libraries.iter().cloned())?;
    paths.extend(libraries);
    Ok(ProfileProtection {
        dirs,
        config_base,
        paths,
    })
}

/// The user's home in both spellings. An installation may live inside it
/// (`~/.local/lib/broscribe`, `%LOCALAPPDATA%\\Programs\\BroScribe`) but must
/// never be it or contain it.
fn home_paths(ctx: &Ctx<'_>) -> Result<Vec<PathBuf>> {
    let home = ctx.deps.agents.home.clone().ok_or_else(|| Error::Service {
        code: codes::LOCATION_UNSUPPORTED,
        message: "the user's home folder could not be determined".into(),
    })?;
    let stable = broscribe_platform::stable_path(&home);
    Ok(vec![home, stable])
}

fn download(ctx: &mut Ctx<'_>) -> Result<()> {
    let config = ctx.config()?;
    let data_dir = ctx.data_dir().to_path_buf();
    let deps = ctx.deps;
    let transport = deps.updates.transport()?;
    let mut updater = open(
        &deps.updates,
        &data_dir,
        &config.updates,
        transport.as_ref(),
    )?;
    let cancel = Cancel::new();
    updater.track_seen_profiles(discover_update_profiles(ctx)?.0);
    let offer = match with_inventory_warnings(ctx, &mut updater, |updater| {
        updater.check(Utc::now(), &cancel)
    })? {
        CheckOutcome::Available(offer) => *offer,
        CheckOutcome::NoVerifiedUpdate { installed } => {
            return emit(
                ctx,
                json!({ "state": "no_verified_update", "installed": installed }),
            );
        }
        CheckOutcome::UpToDate {
            installed,
            latest_version,
            latest_build,
        } => {
            let value = json!({
                "state": "up_to_date",
                "installed": installed,
                "latest": { "version": latest_version, "build": latest_build },
            });
            return emit(ctx, value);
        }
    };
    let agent = agent_state(&data_dir)?;
    if agent.is_none()
        && super::daemon::agent_instance_held(&data_dir, deps.agent_instance.as_ref())?
    {
        return Err(agent_required());
    }
    updater.protect_paths(protected_paths(ctx, &config, agent.as_ref())?)?;
    protect_profiles(ctx, &mut updater)?;
    updater.protect_from_containing(home_paths(ctx)?)?;
    let staged = {
        let mut progress = Progress {
            err: &mut *ctx.err,
            json: ctx.json,
            total: offer.artifact.size,
            last: None,
        };
        updater.download(&offer, &cancel, &mut |bytes| progress.report(bytes))?
    };
    let value = json!({
        "state": "staged",
        "from": staged.from,
        "to": staged.to,
        "size": offer.artifact.size,
        "signer": staged.signer,
    });
    emit(ctx, value)
}

fn with_inventory_warnings<'a, T>(
    ctx: &mut Ctx<'_>,
    updater: &mut Updater<'a>,
    work: impl FnOnce(&mut Updater<'a>) -> Result<T>,
) -> Result<T> {
    let result = work(updater);
    for warning in updater.take_inventory_warnings() {
        ctx.note(format!(
            "warning: skipped update state at {}: {}",
            warning.state_dir.display(),
            warning.error
        ));
    }
    result
}

fn discard(ctx: &mut Ctx<'_>) -> Result<()> {
    settle_handoff(ctx)?;
    let config = ctx.config()?;
    let data_dir = ctx.data_dir().to_path_buf();
    let deps = ctx.deps;
    let mut updater = open(&deps.updates, &data_dir, &config.updates, &Offline)?;
    let agent = agent_state(&data_dir)?;
    if agent.is_none()
        && super::daemon::agent_instance_held(&data_dir, deps.agent_instance.as_ref())?
    {
        return Err(agent_required());
    }
    updater.protect_paths(protected_paths(ctx, &config, agent.as_ref())?)?;
    protect_profiles(ctx, &mut updater)?;
    updater.protect_from_containing(home_paths(ctx)?)?;
    if let Some(kept) = with_inventory_warnings(ctx, &mut updater, Updater::discard)? {
        ctx.note(format!("Kept changed update staging at {}", kept.display()));
        emit(
            ctx,
            json!({ "state": "idle", "discarded": true, "kept_staging": kept }),
        )
    } else {
        emit(ctx, json!({ "state": "idle", "discarded": true }))
    }
}

// ---- install, confirm, rollback, recover ----

/// Check the raw selectors before cwd resolution or profile admission.
pub(crate) fn check_path_selectors(
    paths: &dyn AppPaths,
    profile_override: Option<&Path>,
    library_flag: Option<&Path>,
    library_env: Option<&Path>,
) -> Result<()> {
    if let Some(path) = profile_override.filter(|path| !path.is_absolute()) {
        return Err(Error::InvalidInput(format!(
            "The path from {ENV_DATA_DIR} ({}) is relative. Use an absolute path in {ENV_DATA_DIR} before running the update command.",
            path.display(),
        )));
    }
    if let Some(detail) = crate::relative_default_data_dir(paths) {
        return Err(Error::InvalidInput(format!(
            "{detail} before running the update command."
        )));
    }
    // Every library source counts, not only the one that wins selection: the
    // updater also protects a shadowed environment or config library, and a
    // relative one would be read against whatever folder a retry runs from.
    let data_dir = profile_override.map_or_else(|| paths.data_dir(), Path::to_path_buf);
    if let Some(path) = library_flag {
        require_absolute_library(path, LibrarySource::Flag, &data_dir)?;
    }
    if let Some(path) = library_env {
        require_absolute_library(path, LibrarySource::Env, &data_dir)?;
    }
    let config = Config::load(&data_dir)?;
    if let Some(path) = &config.library {
        require_absolute_library(path, LibrarySource::Config, &data_dir)?;
    }
    Ok(())
}

fn require_absolute_library(path: &Path, source: LibrarySource, data_dir: &Path) -> Result<()> {
    if path.is_absolute() || source == LibrarySource::Default {
        return Ok(());
    }
    let source = match source {
        LibrarySource::Config => format!("`library` in {}", Config::path(data_dir).display()),
        _ => source.as_str().to_owned(),
    };
    Err(Error::InvalidInput(format!(
        "The library path ({}) from {source} is relative. Set an absolute path in {source} before running the update command.",
        path.display()
    )))
}

fn replacement_config(ctx: &Ctx<'_>) -> Result<Config> {
    // The profile config could have changed while waiting for the mode lease.
    let config = ctx.config()?;
    let (path, source) = ctx.configured_library_path(&config);
    require_absolute_library(&path, source, ctx.data_dir())?;
    if let Some(path) = &config.library {
        require_absolute_library(path, LibrarySource::Config, ctx.data_dir())?;
    }
    Ok(config)
}

fn check_replacement_working_directory(
    ctx: &Ctx<'_>,
    config: &Config,
    config_base: &Path,
) -> Result<()> {
    let platform = ctx.deps.updates.platform.as_ref();
    if platform.replacement() == Replacement::AfterExit {
        super::require_working_directory_outside_installation(
            &platform.installation()?.root,
            || {
                let display_path = |path: &Path| {
                    broscribe_platform::plain_verbatim(&broscribe_platform::stable_path(path))
                };
                let data = display_path(ctx.data_dir());
                let library = display_path(&config_base.join(ctx.library_path(config).0));
                format!(
                    "\nData folder: {}\nLibrary: {}\nChange to a folder outside the installation and run the same update command again, without changing BROSCRIBE_DATA_DIR.",
                    data.display(),
                    library.display(),
                )
            },
        )?;
    }
    Ok(())
}

/// `expected`: the exact update the user confirmed (`--expectation-stdin`),
/// compared under the updater's locks before any admission is asked for;
/// any other staged update is refused (`update_changed`), never installed.
/// Without it (`--yes`) the update staged now is installed, named first.
fn install(ctx: &mut Ctx<'_>, expected: Option<UpdateExpectation>) -> Result<()> {
    let config = replacement_config(ctx)?;
    settle_handoff(ctx)?;
    let data_dir = ctx.data_dir().to_path_buf();
    let deps = ctx.deps;
    // Held until the replacement (or the handoff): nothing restages meanwhile.
    let mut updater = open(&deps.updates, &data_dir, &config.updates, &Offline)?;
    updater.protect_paths(protected_paths(ctx, &config, None)?)?;
    let profiles = protect_profiles(ctx, &mut updater)?;
    let home = home_paths(ctx)?;
    updater.protect_from_containing(home.iter().cloned())?;
    let staged = with_inventory_warnings(ctx, &mut updater, |updater| {
        updater.prepare_install(expected.as_ref())
    })?;
    check_replacement_working_directory(ctx, &config, &profiles.config_base)?;
    ctx.note(format!(
        "Installing BroScribe {} (build {}, {}) over {} (build {}), signed by {}.",
        staged.to.version,
        staged.to.build,
        staged.to.target,
        staged.from.version,
        staged.from.build,
        signer_text(&staged.signer),
    ));
    let expected = expected.unwrap_or_else(|| UpdateExpectation::from(&staged));
    let mut client = connect(&data_dir)?.ok_or_else(agent_required)?;
    let before = daemon_state(&mut client)?;
    let paths = protected_paths(ctx, &config, Some(&before))?;
    // The live agent may use a library different from the caller's.
    updater.protect_paths(paths.iter().cloned())?;
    let registration = (deps.updates.platform.replacement() == Replacement::InPlace)
        .then(|| prepare_restart(ctx, &before))
        .transpose()?;
    let lease = AgentLease::acquire(client)?;
    let record = InstallRecord {
        schema: InstallRecord::SCHEMA,
        fence: lease.fence,
        agent_pid: before.pid,
        agent_library: Some(PathBuf::from(&before.library)),
        expected: staged.to.clone(),
        unchanged: staged.from.clone(),
        runner_pid: None,
    };
    record.save(&data_dir)?;
    if deps.updates.platform.replacement() == Replacement::AfterExit {
        drop(updater);
        return hand_off(
            ctx,
            &lease,
            record,
            HandoffAction::Install { expected },
            paths,
            home,
            profiles,
        );
    }
    match with_inventory_warnings(ctx, &mut updater, |updater| {
        updater.install(&expected, &lease, &Cancel::new())
    }) {
        Ok(installed) => {
            // The confirmed admission stays with the agent after this
            // connection closes, across its restart, until `confirm`.
            drop(lease);
            let agent = restart_with_registration(
                ctx,
                &record,
                registration.expect("in-place update holds the registration"),
            );
            if agent.is_err() {
                ctx.note(
                    "note: the agent has not proven it runs the new version yet; restart it \
                     before `broscribe update confirm`",
                );
            }
            ctx.note(
                "Open the new version, then keep it with `broscribe update confirm` or return to \
                 the previous one with `broscribe update rollback --yes`. Recording stays off \
                 until then.",
            );
            let value = json!({
                "state": "awaiting_confirmation",
                "from": installed.from,
                "to": installed.to,
                "fence": record.fence,
                "agent": agent_value(&agent),
            });
            emit(ctx, value)
        }
        Err(error) => {
            let after = updater.status();
            settle_failed_step(ctx, &lease, after)?;
            Err(error)
        }
    }
}

/// After a failed install, rollback or recovery step: when nothing is left
/// half-replaced (idle or staged) the admission ends, otherwise it stays
/// held so nothing records until `update recover` finishes.
fn settle_failed_step(
    ctx: &mut Ctx<'_>,
    lease: &AgentLease,
    after: Result<UpdateStatus>,
) -> Result<()> {
    let data_dir = ctx.data_dir().to_path_buf();
    match after {
        Ok(UpdateStatus::Idle | UpdateStatus::Staged { .. }) => {
            if lease.is_durable()
                && let Err(e) = release(&data_dir, lease.fence)
            {
                ctx.note(format!(
                    "note: the install admission is still held ({e}); `broscribe update recover` \
                     releases it"
                ));
                return Ok(());
            }
            InstallRecord::remove(&data_dir)
        }
        _ => {
            ctx.note(
                "The install admission stays held, so nothing records; run `broscribe update \
                 recover`.",
            );
            Ok(())
        }
    }
}

/// Hands `action` to the external runner with the confirmed admission and
/// returns while the runner waits for this process to exit.
fn hand_off(
    ctx: &mut Ctx<'_>,
    lease: &AgentLease,
    mut record: InstallRecord,
    action: HandoffAction,
    mut protected_paths: Vec<PathBuf>,
    contained_paths: Vec<PathBuf>,
    profiles: ProfileProtection,
) -> Result<()> {
    let data_dir = ctx.data_dir().to_path_buf();
    protected_paths.extend(profiles.paths.iter().cloned());
    // Durable before this process (and its connection) goes away.
    lease.confirm()?;
    let started = start_runner(
        ctx.deps.updates.platform.as_ref(),
        &data_dir,
        &record,
        &action,
        protected_paths,
        contained_paths,
        profiles,
    );
    let pid = match started {
        Ok(pid) => pid,
        Err(error) => {
            Handoff::remove(&data_dir)?;
            // Nothing was replaced. Only an install's admission was asked
            // for this step; a rollback or recovery keeps the admission
            // its pending update holds, so nothing records meanwhile.
            if matches!(action, HandoffAction::Install { .. }) {
                release(&data_dir, lease.fence)?;
                InstallRecord::remove(&data_dir)?;
            }
            return Err(error);
        }
    };
    record.runner_pid = Some(pid);
    record.save(&data_dir)?;
    ctx.note(
        "BroScribe is replaced once every BroScribe program running from its installation (this \
         command, the app, the agent and any assistant connection) has exited: quit the app now. \
         `broscribe update status` shows the result.",
    );
    emit(
        ctx,
        json!({ "state": "handed_off", "action": action, "fence": lease.fence, "runner_pid": pid }),
    )
}

/// Copies the runner out of the installation, writes the handoff and starts
/// it. The runner waits for this process, then for every process running
/// from the installation root `platform` qualifies.
fn start_runner(
    platform: &dyn UpdatePlatform,
    data_dir: &Path,
    record: &InstallRecord,
    action: &HandoffAction,
    protected_paths: Vec<PathBuf>,
    contained_paths: Vec<PathBuf>,
    profiles: ProfileProtection,
) -> Result<u32> {
    let installation_root = platform.installation()?.root;
    let executable = installed_executable(None)?;
    let handoff = Handoff {
        schema: Handoff::SCHEMA,
        data_dir: data_dir.to_path_buf(),
        executable,
        installation_root,
        protected_paths,
        profile_dirs: profiles.dirs,
        config_base: profiles.config_base,
        agent_library: record.agent_library.clone(),
        contained_paths,
        wait_pids: vec![std::process::id()],
        fence: record.fence,
        action: action.clone(),
    };
    let source = handoff.executable.with_file_name(runner_file_name());
    let dir = runner_dir(data_dir);
    let runner: PathBuf = dir.join(runner_file_name());
    let mut command = Command::new(&runner);
    command
        .current_dir(
            broscribe_platform_native::NativePlatform::update_runner_working_directory(&dir)?,
        )
        .env("BROSCRIBE_DATA_DIR", data_dir)
        .env_remove(broscribe_platform::paths::ENV_MAINTENANCE_CONFIG_BASE)
        .arg("--data-dir")
        .arg(data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(library) = &handoff.agent_library {
        command.env(broscribe_platform::paths::ENV_LIBRARY, library);
    }
    #[cfg(test)]
    if let Some(start) = TEST_RUNNER.get() {
        handoff.save()?;
        return start(data_dir, &command);
    }
    copy_runner(&source, &runner)?;
    handoff.save()?;
    broscribe_platform_native::NativePlatform::spawn_update_runner(&mut command)
}

/// Publish a complete runner copy without writing through a pre-existing entry.
fn copy_runner(source: &Path, runner: &Path) -> Result<()> {
    use std::io::ErrorKind;
    let dir = runner.parent().expect("runner has a parent");
    broscribe_updates::ensure_update_directory(dir)?;
    let mut input =
        broscribe_platform::open_regular_nofollow(std::fs::OpenOptions::new().read(true), source)
            .map_err(|e| Error::io_at(source, e))?;
    let permissions = input
        .metadata()
        .map_err(|e| Error::io_at(source, e))?
        .permissions();
    // The installation lock covers runner staging and publication.
    for entry in std::fs::read_dir(dir).map_err(|e| Error::io_at(dir, e))? {
        let entry = entry.map_err(|e| Error::io_at(dir, e))?;
        if entry.file_name().to_str().is_some_and(|name| {
            broscribe_platform::profile_cleanup::is_update_runner_staging(name, cfg!(windows))
        }) {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).map_err(|e| Error::io_at(&path, e))?;
            if meta.is_file() && !broscribe_platform::release_layout::is_reparse(&meta) {
                std::fs::remove_file(&path).map_err(|e| Error::io_at(&path, e))?;
            }
        }
    }
    let staging = format!(".broscribe-update-runner.{}.tmp", std::process::id());
    let mut temp = tempfile::Builder::new()
        .prefix(&staging)
        .rand_bytes(0)
        .tempfile_in(dir)
        .map_err(|e| Error::io_at(&dir.join(&staging), e))?;
    std::io::copy(&mut input, &mut temp).map_err(|e| Error::io_at(temp.path(), e))?;
    temp.as_file()
        .set_permissions(permissions)
        .map_err(|e| Error::io_at(temp.path(), e))?;
    let temp = temp.into_temp_path();
    match std::fs::symlink_metadata(runner) {
        Ok(meta)
            if meta.is_symlink()
                || meta.is_file() && !broscribe_platform::release_layout::is_reparse(&meta) =>
        {
            #[cfg(windows)]
            let result = {
                use std::os::windows::fs::FileTypeExt as _;
                if meta.file_type().is_symlink_dir() {
                    std::fs::remove_dir(runner)
                } else {
                    std::fs::remove_file(runner)
                }
            };
            #[cfg(not(windows))]
            let result = std::fs::remove_file(runner);
            if let Err(e) = result
                && e.kind() != ErrorKind::NotFound
            {
                return Err(Error::io_at(runner, e));
            }
        }
        Ok(_) => {
            return Err(Error::InvalidInput(format!(
                "The update runner path {} is not a regular file or symbolic link.",
                runner.display()
            )));
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(Error::io_at(runner, e)),
    }
    // Another entry appearing after the unlink is a failed handoff, even on Unix.
    temp.persist_noclobber(runner)
        .map_err(|e| Error::io_at(runner, e.error))
}

#[cfg(test)]
type TestRunner = fn(&Path, &Command) -> Result<u32>;

/// Where a test pauses this thread's command to run a competing command
/// at an exact point of a race.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pause {
    /// [`open`]: the caller read its settings; the state lock is not held.
    BeforeLock,
    /// [`open`]: the state lock is held and the settings are still saved.
    Locked,
    /// [`settings_set`]: checked against the update state, not yet saved.
    SettingsChecked,
}

#[cfg(test)]
type TestPause = (Pause, Box<dyn FnOnce()>);

#[cfg(test)]
thread_local! {
    /// Tests start no runner process: [`start_runner`] writes the handoff
    /// and calls this instead, for the runner's process ID, and the test
    /// runs that handoff with `broscribe_update_runner::run_handoff`.
    static TEST_RUNNER: std::cell::Cell<Option<TestRunner>> =
        const { std::cell::Cell::new(None) };
    /// What the next `update settings set` on this thread reads in place
    /// of stdin.
    static TEST_SETTINGS: std::cell::Cell<Option<UpdateSettings>> =
        const { std::cell::Cell::new(None) };
    /// The competing command to run, once, when this thread's command
    /// reaches that [`Pause`].
    static TEST_PAUSE: std::cell::RefCell<Option<TestPause>> =
        const { std::cell::RefCell::new(None) };
}

/// Runs the competing command a test set for `at`, if any, consuming it.
#[cfg(test)]
fn pause(at: Pause) {
    let competitor = TEST_PAUSE.with_borrow_mut(|slot| {
        if slot.as_ref().is_some_and(|(point, _)| *point == at) {
            slot.take().map(|(_, competitor)| competitor)
        } else {
            None
        }
    });
    if let Some(competitor) = competitor {
        competitor();
    }
}

fn confirm(ctx: &mut Ctx<'_>) -> Result<()> {
    settle_handoff(ctx)?;
    let config = ctx.config()?;
    let data_dir = ctx.data_dir().to_path_buf();
    let deps = ctx.deps;
    let mut updater = open(&deps.updates, &data_dir, &config.updates, &Offline)?;
    updater.protect_paths(protected_paths(ctx, &config, None)?)?;
    protect_profiles(ctx, &mut updater)?;
    updater.protect_from_containing(home_paths(ctx)?)?;
    let status = updater.status()?;
    let UpdateStatus::AwaitingConfirmation { to, .. } = &status else {
        return Err(not_awaiting(&status));
    };
    let record = InstallRecord::load(&data_dir)?
        .filter(|record| record.expected == *to)
        .ok_or_else(|| Error::Service {
            code: AGENT_UNPROVEN,
            message: "there is no record of the agent that ran before this install, so the new \
                      agent cannot be proven; run `broscribe update rollback --yes`"
                .into(),
        })?;
    let state = agent_state(&data_dir)?.ok_or_else(agent_required)?;
    prove_restart(&state, &record)?;
    updater.protect_paths(record.agent_library.iter().cloned())?;
    if env!("CARGO_PKG_VERSION") != to.version {
        return Err(Error::Service {
            code: AGENT_UNPROVEN,
            message: format!(
                "this command is version {}, not the installed {}; run the installed `broscribe`",
                env!("CARGO_PKG_VERSION"),
                to.version
            ),
        });
    }
    updater.protect_paths(protected_paths(ctx, &config, Some(&state))?)?;
    let confirmed = with_inventory_warnings(ctx, &mut updater, Updater::confirm)?;
    release(&data_dir, record.fence)?;
    InstallRecord::remove(&data_dir)?;
    RunnerReport::remove(&data_dir)?;
    emit(ctx, json!({ "state": "confirmed", "installed": confirmed }))
}

/// `expected`: as for [`install`], the exact installed update the user
/// chose to roll back, compared under the updater's locks before any
/// admission is used.
fn rollback(ctx: &mut Ctx<'_>, expected: Option<UpdateExpectation>) -> Result<()> {
    let config = replacement_config(ctx)?;
    settle_handoff(ctx)?;
    let data_dir = ctx.data_dir().to_path_buf();
    let deps = ctx.deps;
    let mut updater = open(&deps.updates, &data_dir, &config.updates, &Offline)?;
    let home = home_paths(ctx)?;
    updater.protect_from_containing(home.iter().cloned())?;
    updater.protect_paths(protected_paths(ctx, &config, None)?)?;
    let profiles = protect_profiles(ctx, &mut updater)?;
    let expected = match expected {
        Some(expected) => {
            updater.awaiting_expected(&expected)?;
            expected
        }
        None => match updater.status()? {
            UpdateStatus::AwaitingConfirmation { from, to, signer } => {
                UpdateExpectation { from, to, signer }
            }
            status => return Err(not_awaiting(&status)),
        },
    };
    check_replacement_working_directory(ctx, &config, &profiles.config_base)?;
    let (from, to) = (&expected.from, &expected.to);
    ctx.note(format!(
        "Restoring BroScribe {} (build {}) in place of {} (build {}).",
        from.version, from.build, to.version, to.build
    ));
    let (lease, before) = AgentLease::held_or_acquire(&data_dir)?;
    let paths = protected_paths(ctx, &config, Some(&before))?;
    updater.protect_paths(paths.iter().cloned())?;
    updater.awaiting_expected(&expected)?;
    let record = InstallRecord {
        schema: InstallRecord::SCHEMA,
        fence: lease.fence,
        agent_pid: before.pid,
        agent_library: Some(PathBuf::from(&before.library)),
        expected: from.clone(),
        unchanged: to.clone(),
        runner_pid: None,
    };
    let registration = (deps.updates.platform.replacement() == Replacement::InPlace)
        .then(|| prepare_restart(ctx, &before))
        .transpose()?;
    record.save(&data_dir)?;
    if deps.updates.platform.replacement() == Replacement::AfterExit {
        drop(updater);
        return hand_off(
            ctx,
            &lease,
            record,
            HandoffAction::Rollback { expected },
            paths,
            home,
            profiles,
        );
    }
    match with_inventory_warnings(ctx, &mut updater, |updater| {
        updater.rollback(&expected, &lease)
    }) {
        Ok(restored) => {
            drop(lease);
            finish_restore(
                ctx,
                &record,
                &restored,
                registration.expect("in-place restore holds the registration"),
            )
        }
        Err(error) => {
            let after = updater.status();
            settle_failed_step(ctx, &lease, after)?;
            Err(error)
        }
    }
}

/// After the previous installation is back: restart the agent onto it,
/// and end the admission once it proved itself.
fn finish_restore(
    ctx: &mut Ctx<'_>,
    record: &InstallRecord,
    restored: &PayloadIdentity,
    registration: broscribe_platform::LoginRegistrationGuard,
) -> Result<()> {
    let data_dir = ctx.data_dir().to_path_buf();
    if let Err(e) = restart_with_registration(ctx, record, registration) {
        // A restored build of another protocol is proven only by its own
        // CLI: this command cannot read its agent, now or later.
        let next = if e.code() == HELPER_OUTDATED {
            "this command speaks another protocol than the restored agent, so the install \
             admission stays held and recording stays off until the restored installation's \
             own `broscribe update recover` proves its agent"
        } else {
            "recording stays off until `broscribe update recover` succeeds"
        };
        return Err(Error::Service {
            code: AGENT_UNPROVEN,
            message: format!(
                "BroScribe {} (build {}) is installed again, but the agent has not proven it runs \
                 it ({e}); {next}",
                restored.version, restored.build
            ),
        });
    }
    release(&data_dir, record.fence)?;
    InstallRecord::remove(&data_dir)?;
    RunnerReport::remove(&data_dir)?;
    emit(ctx, json!({ "state": "rolled_back", "restored": restored }))
}

fn recover(ctx: &mut Ctx<'_>) -> Result<()> {
    let config = replacement_config(ctx)?;
    settle_handoff(ctx)?;
    let data_dir = ctx.data_dir().to_path_buf();
    let deps = ctx.deps;
    let mut updater = open(&deps.updates, &data_dir, &config.updates, &Offline)?;
    let home = home_paths(ctx)?;
    updater.protect_from_containing(home.iter().cloned())?;
    updater.protect_paths(protected_paths(ctx, &config, None)?)?;
    let profiles = protect_profiles(ctx, &mut updater)?;
    if let UpdateStatus::Interrupted { from, .. } = updater.status()? {
        check_replacement_working_directory(ctx, &config, &profiles.config_base)?;
        let (lease, before) = AgentLease::held_or_acquire(&data_dir)?;
        let paths = protected_paths(ctx, &config, Some(&before))?;
        updater.protect_paths(paths.iter().cloned())?;
        with_inventory_warnings(ctx, &mut updater, Updater::prepare_recovery)?;
        let record = InstallRecord {
            schema: InstallRecord::SCHEMA,
            fence: lease.fence,
            agent_pid: before.pid,
            agent_library: Some(PathBuf::from(&before.library)),
            unchanged: from.clone(),
            expected: from,
            runner_pid: None,
        };
        let registration = (deps.updates.platform.replacement() == Replacement::InPlace)
            .then(|| prepare_restart(ctx, &before))
            .transpose()?;
        record.save(&data_dir)?;
        if deps.updates.platform.replacement() == Replacement::AfterExit {
            drop(updater);
            return hand_off(
                ctx,
                &lease,
                record,
                HandoffAction::Recover,
                paths,
                home,
                profiles,
            );
        }
        return match with_inventory_warnings(ctx, &mut updater, |updater| updater.recover(&lease)) {
            Ok(RecoveryOutcome::RolledBack { restored }) => {
                drop(lease);
                finish_restore(
                    ctx,
                    &record,
                    &restored,
                    registration.expect("in-place restore holds the registration"),
                )
            }
            Ok(other) => Err(Error::Internal(format!(
                "an interrupted replacement recovered as {other:?}"
            ))),
            Err(error) => {
                let after = updater.status();
                settle_failed_step(ctx, &lease, after)?;
                Err(error)
            }
        };
    }
    let agent = agent_state(&data_dir)?;
    if agent.is_none()
        && super::daemon::agent_instance_held(&data_dir, deps.agent_instance.as_ref())?
    {
        return Err(agent_required());
    }
    updater.protect_paths(protected_paths(ctx, &config, agent.as_ref())?)?;
    let value = match with_inventory_warnings(ctx, &mut updater, |updater| {
        updater.recover(&NoAdmission)
    })? {
        RecoveryOutcome::AwaitingConfirmation { from, to } => {
            // Recording stays off until confirm or rollback: make sure the
            // agent holds a durable admission (it normally already does).
            let (lease, _) = AgentLease::held_or_acquire(&data_dir)?;
            if !lease.is_durable() {
                lease.confirm()?;
            }
            json!({ "state": "awaiting_confirmation", "from": from, "to": to, "fence": lease.fence })
        }
        RecoveryOutcome::Clean => {
            json!({ "state": "clean", "admission": end_stale_admission(ctx)? })
        }
        RecoveryOutcome::DiscardedStaged => {
            json!({ "state": "discarded_staged", "admission": end_stale_admission(ctx)? })
        }
        RecoveryOutcome::RolledBack { restored } => {
            return Err(Error::Internal(format!(
                "recovery restored {} without an admission",
                restored.version
            )));
        }
    };
    emit(ctx, value)
}

/// With nothing pending (this process holds the updater's state lock and
/// its journal is gone), a durable admission the agent still holds that an
/// update step recorded ([`InstallRecord`]) is left over from a step that
/// ended without releasing it: it completed, or it failed and replaced
/// nothing (or the updater restored what it replaced), as a handed-off
/// install does when the runner gives up waiting. The installation in place
/// must be the one the record names for either ending; a step that replaced
/// nothing is reconciled to its unchanged installation, durably, before any
/// proof. The admission ends once the agent proves it runs that
/// installation as a new process; an agent that never stopped is restarted
/// first, which is safe under the held admission (nothing records). An
/// admission held for a manual upgrade ([`AdmissionRecord`]) or by an owner
/// nothing recorded is never ended here, whatever build the agent runs.
fn end_stale_admission(ctx: &mut Ctx<'_>) -> Result<Value> {
    let data_dir = ctx.data_dir().to_path_buf();
    let Some(state) = agent_state(&data_dir)? else {
        return Ok(json!({ "agent": "not_running" }));
    };
    let Some(InstallHold {
        fence,
        durable: true,
    }) = state.install
    else {
        InstallRecord::remove(&data_dir)?;
        return Ok(json!({ "held": state.install.is_some() }));
    };
    let Some(mut record) = InstallRecord::load(&data_dir)?.filter(|record| record.fence == fence)
    else {
        let manual = AdmissionRecord::load(&data_dir)?.is_some_and(|r| r.fence == fence);
        return Err(Error::Service {
            code: "busy",
            message: if manual {
                format!(
                    "install admission {fence} is held for a manual upgrade; it ends with \
                     `broscribe update admission release --fence {fence}` once the upgraded \
                     agent runs"
                )
            } else {
                format!(
                    "install admission {fence} was not acquired by an update step recorded here, \
                     so it stays held until its owner releases it"
                )
            },
        });
    };
    let installed = ctx.deps.updates.platform.installation()?.identity;
    if record.expected != installed {
        if record.unchanged != installed {
            return Err(Error::Service {
                code: AGENT_UNPROVEN,
                message: format!(
                    "BroScribe {} build {} ({}) is installed, which the last update step left \
                     neither by completing ({} build {}) nor by replacing nothing ({} build {}); \
                     the install admission stays held and recording stays off until one of \
                     those builds is installed again and `broscribe update recover` proves its \
                     agent",
                    installed.version,
                    installed.build,
                    installed.target,
                    record.expected.version,
                    record.expected.build,
                    record.unchanged.version,
                    record.unchanged.build,
                ),
            });
        }
        // The step replaced nothing: from now on its agent must prove the
        // unchanged installation, whatever happens next.
        record.expected = installed.clone();
        record.save(&data_dir)?;
    }
    let state = if state.pid == record.agent_pid {
        restart_agent(ctx, &record)?
    } else {
        state
    };
    prove_restart(&state, &record)?;
    release(&data_dir, fence)?;
    InstallRecord::remove(&data_dir)?;
    RunnerReport::remove(&data_dir)?;
    Ok(json!({ "released": fence }))
}

// ---- admission for a replacement made outside the updater ----

/// Refuses while the signed updater has a replacement under way or
/// awaiting confirmation: that step holds (and ends) its own admission.
fn refuse_pending_replacement(ctx: &Ctx<'_>) -> Result<()> {
    let config = ctx.config()?;
    if !config.updates.enabled {
        // Settings cannot be turned off while an update is pending.
        return Ok(());
    }
    let updater = open(&ctx.deps.updates, ctx.data_dir(), &config.updates, &Offline)?;
    match updater.status()? {
        UpdateStatus::Idle | UpdateStatus::Staged { .. } => Ok(()),
        _ => Err(Error::Service {
            code: codes::PENDING,
            message: "an update is installing or awaits confirmation; finish it with `broscribe \
                      update confirm`, `rollback` or `recover` first"
                .into(),
        }),
    }
}

/// The record of an admission `update admission acquire` holds, kept in
/// the updater's state folder (user-only) until `update admission release`
/// proved the agent: who owns the fence, the agent process that ran when
/// it was acquired and the installation it ran from. Separate from
/// [`InstallRecord`], so the updater's own recovery never ends it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AdmissionRecord {
    schema: u32,
    fence: u64,
    agent_pid: u32,
    installed: PayloadIdentity,
}

impl AdmissionRecord {
    const SCHEMA: u32 = 1;

    fn path(data_dir: &Path) -> PathBuf {
        state_dir(data_dir).join("manual-admission.json")
    }

    /// `None` when absent; another schema or an unreadable record fails
    /// closed.
    fn load(data_dir: &Path) -> Result<Option<Self>> {
        let path = Self::path(data_dir);
        match broscribe_updates::read_update_record::<Self>(
            &state_dir(data_dir),
            "manual-admission.json",
        )? {
            Some(record) if record.schema != Self::SCHEMA => Err(Error::Schema(format!(
                "{} has an unknown schema",
                path.display()
            ))),
            record => Ok(record),
        }
    }

    fn save(&self, data_dir: &Path) -> Result<()> {
        let dir = state_dir(data_dir);
        broscribe_updates::ensure_update_directory(&dir)?;
        let mut bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| Error::Internal(format!("serialising the admission record: {e}")))?;
        bytes.push(b'\n');
        fsutil::write_atomic_with_mode(&Self::path(data_dir), &bytes, Some(0o600))
    }

    fn remove(data_dir: &Path) -> Result<()> {
        let path = Self::path(data_dir);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::io_at(&path, e)),
        }
    }
}

/// `{"pid","version","build","target"}` of an agent whose build is known.
fn agent_identity(state: &DaemonState, build: &AgentBuild) -> Value {
    json!({
        "pid": state.pid,
        "version": state.version,
        "build": build.build,
        "target": build.target,
    })
}

/// `update admission acquire`: the agent's install admission for a
/// replacement made outside this updater (the manual upgrade). Acquired
/// and confirmed on one connection, so it is durable before this process
/// exits: it outlives the agent's unregistration and restarts, and nothing
/// records until `update admission release`. The agent's refusal (it
/// records, may record, or a recording is not saved) is the only idle
/// check. An admission already held is never reattached.
fn admission_acquire(ctx: &mut Ctx<'_>) -> Result<()> {
    settle_handoff(ctx)?;
    let data_dir = ctx.data_dir().to_path_buf();
    if InstallRecord::load(&data_dir)?.is_some() {
        return Err(Error::Service {
            code: codes::PENDING,
            message: "an update step has not finished; run `broscribe update recover` first".into(),
        });
    }
    refuse_pending_replacement(ctx)?;
    let installed = ctx.deps.updates.platform.installation()?.identity;
    let mut client = connect(&data_dir)?.ok_or_else(agent_required)?;
    let before = daemon_state(&mut client)?;
    if let Some(hold) = &before.install {
        let owned = AdmissionRecord::load(&data_dir)?.is_some_and(|r| r.fence == hold.fence);
        return Err(Error::Service {
            code: "busy",
            message: if owned {
                format!(
                    "install admission {} is already held for a manual upgrade; end it with \
                     `broscribe update admission release --fence {}`",
                    hold.fence, hold.fence
                )
            } else {
                "BroScribe Agent already holds an install admission".into()
            },
        });
    }
    // The agent holds nothing: a record left by an earlier acquire is stale.
    AdmissionRecord::remove(&data_dir)?;
    // An agent that cannot say which build it runs could never prove the
    // replacement, so the admission would never end.
    let Some(build) = &before.build else {
        return Err(Error::Service {
            code: AGENT_UNPROVEN,
            message: "BroScribe Agent cannot tell which build it runs; update it before upgrading"
                .into(),
        });
    };
    let lease = AgentLease::acquire(client)?;
    // Before the admission becomes durable, so a durable one always has an
    // owner record.
    AdmissionRecord {
        schema: AdmissionRecord::SCHEMA,
        fence: lease.fence,
        agent_pid: before.pid,
        installed: installed.clone(),
    }
    .save(&data_dir)?;
    if let Err(error) = lease.confirm() {
        // Refused: nothing is held. Answer lost: it may be held; end this
        // fence if the agent answers, otherwise the record keeps it owned.
        if !lease.is_durable() || release(&data_dir, lease.fence).is_ok() {
            AdmissionRecord::remove(&data_dir)?;
        }
        return Err(error);
    }
    let value = json!({
        "state": "held",
        "fence": lease.fence,
        "durable": true,
        "agent": agent_identity(&before, build),
        "installed": installed,
    });
    emit(ctx, value)
}

/// `update admission release --fence N`: ends the admission
/// [`admission_acquire`] recorded, once the agent proves it runs exactly
/// the installation this command belongs to (version, build and target),
/// as a new process when that installation changed since the admission
/// was acquired. Otherwise the admission and its record stay.
fn admission_release(ctx: &mut Ctx<'_>, fence: u64) -> Result<()> {
    let data_dir = ctx.data_dir().to_path_buf();
    let record = AdmissionRecord::load(&data_dir)?
        .filter(|record| record.fence == fence)
        .ok_or_else(|| Error::Service {
            code: codes::ADMISSION_LOST,
            message: format!("no install admission {fence} was acquired for a manual upgrade"),
        })?;
    let installed = ctx.deps.updates.platform.installation()?.identity;
    let state = agent_state(&data_dir)?.ok_or_else(agent_required)?;
    let build = if installed == record.installed {
        let build = runs_build(&state, &installed)?;
        if state.install
            != Some(InstallHold {
                fence,
                durable: true,
            })
        {
            return Err(Error::Service {
                code: codes::ADMISSION_LOST,
                message: format!("BroScribe Agent no longer holds install admission {fence}"),
            });
        }
        build
    } else {
        prove(&state, &installed, record.agent_pid, fence)?
    };
    let agent = agent_identity(&state, build);
    release(&data_dir, fence)?;
    AdmissionRecord::remove(&data_dir)?;
    emit(
        ctx,
        json!({ "state": "released", "fence": fence, "agent": agent }),
    )
}

// ---- settings ----

fn settings_show(ctx: &mut Ctx<'_>) -> Result<()> {
    let config = ctx.config()?;
    let value = settings_value(&config, ctx.deps.updates.platform.as_ref());
    emit(ctx, value)
}

fn settings_value(config: &Config, platform: &dyn UpdatePlatform) -> Value {
    let settings = &config.updates;
    // A private mirror may use a capability URL. A preference-only write
    // preserves the saved URL without returning it.
    let feed = settings.effective_feed();
    json!({
        "revision": config.revision(),
        "updates": {
            "enabled": settings.enabled,
            "feed_url": (feed == broscribe_core::config::RELEASE_FEED).then_some(feed),
            "channel": settings.channel,
            "trusted_keys": settings.trusted_keys,
            "signer": settings.signer,
        },
        "source": update_source(settings),
        "effective_signer": signer_pin(settings, platform),
        "effective_trusted_keys": settings.effective_keys().count(),
    })
}

/// Validates `settings` as the updater would. Settings that are off may be
/// incomplete, but whatever they name must be well formed.
fn validate(settings: &UpdateSettings) -> Result<()> {
    let probe = UpdateSettings {
        enabled: true,
        ..settings.clone()
    };
    match UpdateConfig::from_settings(&probe) {
        Ok(_) => Ok(()),
        Err(Error::NotConfigured(_)) if !settings.enabled => Ok(()),
        Err(error) => Err(error),
    }
}

/// `update settings set` input: the `[updates]` table read whole from stdin.
fn read_settings() -> Result<UpdateSettings> {
    #[cfg(test)]
    if let Some(settings) = TEST_SETTINGS.take() {
        return Ok(settings);
    }
    crate::query::read_stdin("update settings set")
}

/// Replaces `[updates]`. A pending update keeps the settings it was checked
/// with: before enabled settings change, the updater's state lock is taken
/// and held until the change is saved, so no check, download, install or
/// recovery runs between the finding that nothing is pending and the save
/// (lock order: the state lock, then the config lock, a leaf under which
/// nothing else is locked). Pending includes an update step whose
/// [`InstallRecord`] remains after its journal ended (a rollback or failed
/// handoff whose agent was not proven yet): its admission still holds
/// recording off and only `update recover`, which opens the updater under
/// these settings, ends it; an unreadable record refuses as it does there.
/// Settings that are off need no lock: nothing opens the updater under
/// them, and a command that read earlier settings refuses once it holds
/// the lock ([`open`]). The save refuses with `config_stale` unless the
/// saved `[updates]` are still the ones this command checked and, with
/// `expect_revision`, the revision is that one.
fn settings_set(
    ctx: &mut Ctx<'_>,
    enabled: Option<bool>,
    expect_revision: Option<u64>,
) -> Result<()> {
    let spec = enabled.is_none().then(read_settings).transpose()?;
    let data_dir = ctx.data_dir().to_path_buf();
    let deps = ctx.deps;
    let current = ctx.config()?.updates;
    let settings = match enabled {
        Some(enabled) => UpdateSettings {
            enabled,
            ..current.clone()
        },
        None => spec.expect("clap requires an update settings change"),
    };
    validate(&settings)?;
    let state = if current.enabled && current != settings {
        let updater = open(&deps.updates, &data_dir, &current, &Offline)?;
        if updater.status()? != UpdateStatus::Idle {
            return Err(Error::Service {
                code: codes::PENDING,
                message: "an update is pending; confirm, roll back, discard or recover it before \
                          changing the update settings"
                    .into(),
            });
        }
        if InstallRecord::load(&data_dir)?.is_some() {
            return Err(Error::Service {
                code: codes::PENDING,
                message: "an update step has not finished; run `broscribe update recover` \
                          before changing the update settings"
                    .into(),
            });
        }
        Some(updater)
    } else {
        None
    };
    #[cfg(test)]
    pause(Pause::SettingsChecked);
    let (config, ()) = Config::update(&data_dir, |config| {
        if let Some(expected) = expect_revision
            && config.revision() != expected
        {
            return Err(Error::Service {
                code: CONFIG_STALE,
                message: format!(
                    "settings changed since they were read (revision {expected} is now {}); \
                     reload them and try again",
                    config.revision()
                ),
            });
        }
        if config.updates != current {
            return Err(Error::Service {
                code: CONFIG_STALE,
                message: "the update settings changed since they were read; reload them and \
                          try again"
                    .into(),
            });
        }
        config.updates = settings;
        Ok(())
    })?;
    drop(state);
    let value = settings_value(&config, ctx.deps.updates.platform.as_ref());
    emit(ctx, value)
}

// ---- release signing (publisher side) ----

fn release_key_generate(ctx: &mut Ctx<'_>, label: &str, replace_existing: bool) -> Result<()> {
    let label = ReleaseKeyLabel::parse(label)?;
    let existing = if replace_existing {
        ExistingKey::Replace
    } else {
        ExistingKey::Refuse
    };
    let generated = generate_release_key(ctx.deps.secrets.as_ref(), &label, existing)?;
    let value = crate::to_value(&generated)?;
    emit(ctx, value)
}

fn release_key_show(ctx: &mut Ctx<'_>, label: &str) -> Result<()> {
    let label = ReleaseKeyLabel::parse(label)?;
    let key = release_key(ctx.deps.secrets.as_ref(), &label)?;
    let value = crate::to_value(&key)?;
    emit(ctx, value)
}

fn read_release_stdin(what: &str) -> Result<Vec<u8>> {
    let limit = Limits::default().max_envelope_bytes;
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::io(format!("reading the {what} from stdin"), e))?;
    if bytes.len() as u64 > limit {
        return Err(Error::InvalidInput(format!(
            "the {what} is larger than {limit} bytes"
        )));
    }
    Ok(bytes)
}

fn verify_release(ctx: &mut Ctx<'_>, allow_expired: bool) -> Result<()> {
    let config = UpdateConfig::from_settings(&UpdateSettings::default())?;
    let manifest = verify_release_envelope(
        &read_release_stdin("envelope")?,
        &config.anchors,
        &config.limits,
        Utc::now(),
        allow_expired,
    )?;
    emit(
        ctx,
        json!({ "version": manifest.version, "build": manifest.build }),
    )
}

/// Signs the exact manifest bytes from stdin, bounded by the envelope limit.
fn sign(ctx: &mut Ctx<'_>, keys: &[String], key_file: Option<&Path>) -> Result<()> {
    let manifest = String::from_utf8(read_release_stdin("manifest")?)
        .map_err(|_| Error::InvalidInput("the manifest is not UTF-8 text".into()))?;
    let now = Utc::now();
    let cancel = Cancel::new();
    let signed = if let Some(path) = key_file {
        sign_manifest_file(path, &manifest, now, &cancel)?
    } else {
        let labels = keys
            .iter()
            .map(|key| ReleaseKeyLabel::parse(key))
            .collect::<Result<Vec<_>>>()?;
        sign_manifest(ctx.deps.secrets.as_ref(), &labels, &manifest, now, &cancel)?
    };
    if ctx.json {
        let value = crate::to_value(&signed)?;
        return crate::print_json(ctx.out, &value);
    }
    for signer in &signed.signers {
        ctx.note(format!(
            "signed with {} (key ID {})",
            signer.label.as_str(),
            signer.key_id
        ));
    }
    writeln!(ctx.out, "{}", signed.envelope).map_err(|e| Error::io("writing output", e))
}

#[cfg(test)]
mod tests;
