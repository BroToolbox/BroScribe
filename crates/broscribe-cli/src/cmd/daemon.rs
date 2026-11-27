//! Agent lifecycle and shared request-id reconciled transport.
use crate::{Ctx, cli::DaemonCmd};
use broscribe_core::{Error, Result, lock::DaemonStartupLock};
use broscribe_ipc::{AgentPing, AgentReadiness, Call, IpcError, SystemCalendarAction};
use broscribe_platform::bounded::LOGIN_MANAGER_TIMEOUT;
use broscribe_platform::{
    AgentInstanceProbe, Autostart, CalendarProvider, LoginJob, LoginRegistrationGuard, ProcessProbe,
};
use broscribe_platform_native::ipc::{CallError, CancelToken, Client, Endpoint, RECONCILE_WINDOW};
use serde::Deserialize as _;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// Reply timeout per call: most answer at once, but the daemon's network work
/// (each signed-in account's calendar sync, 30 s per HTTP request after a token
/// refresh) and the audio check take far longer. Connecting a native calendar
/// waits for the user's answer to the macOS prompt (bounded at 120 s there).
fn call_timeout(call: &Call) -> Duration {
    match call {
        Call::AudioCheck { .. } => Duration::from_secs(30),
        Call::PermissionsRequest { .. } => Duration::from_secs(150),
        // The helper answers once the sample is written: the capture itself
        // plus opening the microphone and writing the file.
        Call::CaptureSample { seconds, .. } => {
            Duration::from_secs_f64(seconds.clamp(0.0, 120.0)) + Duration::from_secs(15)
        }
        // An active recording is stopped and made durable before the answer.
        Call::Shutdown => Duration::from_secs(30),
        Call::IntegrationsSync { .. } => Duration::from_secs(120),
        Call::SystemCalendar {
            action: SystemCalendarAction::Connect,
        } => Duration::from_secs(150),
        Call::SystemCalendar { .. } => Duration::from_secs(30),
        _ => Duration::from_secs(5),
    }
}

/// How long a mutating call whose reply was lost is reconciled by its request
/// ID; `None` keeps the transport's default.
fn reconcile_window(call: &Call) -> Option<Duration> {
    matches!(
        call,
        Call::SystemCalendar {
            action: SystemCalendarAction::Connect
        }
    )
    .then_some(Duration::from_secs(180))
}

/// Agent codes that keep their identity through the CLI and app (native
/// calendars, recording-session and live-turn fences, install admission,
/// retries, setup captures and the integration service's finite, sanitized
/// failures); every other remote code keeps [`IpcError::into_core`]'s
/// mapping, so an unknown code fails closed as `internal`.
const SERVICE_CODES: &[&str] = &[
    "helper_required",
    "helper_outdated",
    "calendar_denied",
    "calendar_restricted",
    "calendar_write_only",
    "calendar_pending",
    "busy",
    "superseded",
    "signed_out",
    "stale_session",
    "library_changed",
    "confirmation_required",
    broscribe_ipc::recording_words::JOURNAL_LIBRARY_MISMATCH,
    broscribe_ipc::recording_words::JOURNAL_LIBRARY_UNKNOWN,
    "stale_binding",
    "stale_turn",
    "stale_revision",
    "stale_evidence",
    IpcError::STALE_READER,
    "partial_outcome",
    "cancelled",
    // Integration service failures (`IntegrationError::public`).
    "denied",
    "timeout",
    "forbidden",
    "oauth",
    "service",
    "transient",
    "conditional_access_blocked",
    "admin_consent_required",
    "trust_store_unavailable",
    "insufficient_grant",
    "calendar_partial",
    "calendar_provenance_unverified",
    "invalid_provider_response",
    "invalid_pagination",
    "pagination_limit",
    "account_required",
    "account_not_found",
    "invalid_secret_reference",
    "account_registry_version",
    "google_stamp_required",
    // A platform credential store that did not answer (a locked keyring).
    broscribe_core::secret::TIMEOUT_CODE,
    IpcError::SHUTTING_DOWN,
    IpcError::IN_PROGRESS,
    IpcError::RECORDING_ACTIVE,
    IpcError::INSTALLING,
    IpcError::ADMISSION_LOST,
    IpcError::ADMISSION_DAMAGED,
    IpcError::NOT_RETRYABLE,
    IpcError::DISK_LOW,
    IpcError::MICROPHONE_PRIVACY,
];

pub(crate) fn service_code(code: &str) -> Option<&'static str> {
    SERVICE_CODES.iter().copied().find(|known| *known == code)
}

pub(crate) fn helper_required() -> Error {
    Error::Service {
        code: "helper_required",
        message: "system calendars are read only by BroScribe Agent, which is not running; start it with `broscribe daemon start` and try again".into(),
    }
}

fn remote_error(call_is_native: bool, e: IpcError) -> Error {
    // An agent speaking an older protocol refuses every request rather than
    // running a recording command without its session fence.
    if e.code == IpcError::UNSUPPORTED_VERSION {
        return Error::Service {
            code: "helper_outdated",
            message: format!(
                "BroScribe Agent speaks an older protocol; update and restart it with \
                 `broscribe daemon stop` and `broscribe daemon start` ({})",
                e.message
            ),
        };
    }
    // An agent from before a native calendar provider rejects the method itself.
    if call_is_native && e.code == IpcError::INVALID_REQUEST {
        return Error::Service {
            code: "helper_outdated",
            message: format!(
                "BroScribe Agent is older than this command and cannot read this calendar provider; update and restart it ({})",
                e.message
            ),
        };
    }
    match service_code(&e.code) {
        Some(code) => Error::Service {
            code,
            message: e.message,
        },
        None => e.into_core(),
    }
}

/// A transport or remote failure of a non-native call as a core error.
pub(crate) fn call_error(e: CallError) -> Error {
    match e {
        CallError::Remote(e) => remote_error(false, e),
        other => Error::Internal(other.to_string()),
    }
}

/// Connects to the running agent of `data_dir`, if any.
pub(crate) fn connect(data_dir: &std::path::Path, timeout: Duration) -> Result<Option<Client>> {
    Client::connect_daemon(&Endpoint::new(data_dir), timeout)
        .map_err(|e| Error::io("connecting to daemon", e))
}

pub(super) fn ping(client: &mut Client) -> Result<AgentPing<'static>> {
    AgentPing::from_value(client.call(Call::Ping).map_err(call_error)?).map_err(IpcError::into_core)
}

// Status is read on the same connection that will stop or report the agent.
fn check_mcp_library(library: Option<&std::path::Path>, state: &Value) -> Result<()> {
    if let Some(library) = library {
        let state = broscribe_ipc::DaemonState::deserialize(state)
            .map_err(|_| Error::Internal("BroScribe Agent returned an invalid state".into()))?;
        crate::mcp::check_agent_library(library, &state)?;
    }
    Ok(())
}

fn check_mcp_start(ctx: &Ctx<'_>) -> Result<()> {
    if let Some(library) = &ctx.request_library {
        let configured = ctx.configured_library_path(&ctx.config()?).0;
        crate::mcp::check_library_path(library, &configured)?;
        if let Some(mut client) = connect(ctx.data_dir(), Duration::from_secs(5))? {
            let state = client.call(Call::Status).map_err(call_error)?;
            check_mcp_library(Some(library), &state)?;
        }
    }
    Ok(())
}

fn lifecycle_status(ctx: &mut Ctx<'_>) -> Result<()> {
    let installed = installed(ctx)?;
    let Some(mut client) = connect(ctx.data_dir(), Duration::from_secs(1))? else {
        let held = agent_instance_held(ctx.data_dir(), ctx.deps.agent_instance.as_ref())?;
        let service = ctx.deps.autostart.agent_status()?;
        return emit(
            ctx,
            json!({"installed":installed, "running":held, "service_status":service, "agent":null, "observation":if held { "unreachable" } else { "absent" }, "state":null, "inventory":"unassessed"}),
        );
    };
    let agent = ping(&mut client)?;
    let state = if agent.mode == broscribe_core::maintenance::Mode::Normal {
        let value = client.call(Call::Status).map_err(call_error)?;
        check_mcp_library(ctx.request_library.as_deref(), &value)?;
        Some(value)
    } else {
        None
    };
    emit(
        ctx,
        json!({"installed":installed, "running":true, "service_status":broscribe_platform::AgentServiceStatus::Running, "agent":agent, "observation":"verified", "state":state, "inventory":if state.is_some() { "assessed" } else { "unassessed" }}),
    )
}

pub(crate) fn optional_call(data_dir: &std::path::Path, call: Call) -> Result<Option<Value>> {
    call_with(data_dir, call, None)
}

/// [`optional_call`] that `cancel` interrupts: before a mutating call is
/// sent it is `cancelled` and nothing ran; after, waiting stops and the
/// outcome is reported uncertain (never run again locally).
pub(crate) fn cancellable_call(
    data_dir: &std::path::Path,
    call: Call,
    cancel: &CancelToken,
) -> Result<Option<Value>> {
    call_with(data_dir, call, Some(cancel))
}

fn call_with(
    data_dir: &std::path::Path,
    call: Call,
    cancel: Option<&CancelToken>,
) -> Result<Option<Value>> {
    let endpoint = Endpoint::new(data_dir);
    let timeout = call_timeout(&call);
    let native = matches!(
        call,
        Call::SystemCalendar { .. } | Call::GoogleCalendarAuthority
    );
    let Some(mut client) = Client::connect_daemon(&endpoint, timeout)
        .map_err(|e| Error::io("connecting to daemon", e))?
    else {
        return Ok(None);
    };
    let result = if call.is_mutating() {
        // Once a daemon has answered, never fall back to local execution,
        // including on a later connect failure. The client reconciles uncertain
        // writes under the same request ID and never resends them.
        drop(client);
        let window = reconcile_window(&call).unwrap_or(RECONCILE_WINDOW);
        match cancel {
            Some(cancel) => {
                Client::call_mutating_cancellable(&endpoint, call, timeout, window, cancel)
            }
            None => Client::call_mutating_within(&endpoint, call, timeout, window),
        }
    } else {
        client.call(call)
    };
    result.map(Some).map_err(|e| match e {
        CallError::Remote(e) => remote_error(native, e),
        CallError::NotSent(e) if e.kind() == std::io::ErrorKind::Interrupted => Error::Service {
            code: "cancelled",
            message: "cancelled before the agent received the request".into(),
        },
        other => Error::Internal(other.to_string()),
    })
}

/// A native-calendar action of one provider, which only the running agent can perform.
pub(crate) fn native_calendar(
    data_dir: &std::path::Path,
    provider: CalendarProvider,
    action: SystemCalendarAction,
) -> Result<Value> {
    let call = match provider {
        CalendarProvider::EventKit => Call::SystemCalendar { action },
    };
    optional_call(data_dir, call)?.ok_or_else(helper_required)
}
pub(crate) fn required_call(ctx: &Ctx<'_>, call: Call) -> Result<Value> {
    let response = match &ctx.request_cancel {
        Some(flag) => cancellable_call(ctx.data_dir(), call, &CancelToken::from_flag(flag.clone())),
        None => optional_call(ctx.data_dir(), call),
    };
    response?.ok_or_else(|| {
        Error::DaemonUnavailable(
            "BroScribe Agent isn't running; run `broscribe daemon start`".into(),
        )
    })
}
pub(crate) fn emit(ctx: &mut Ctx<'_>, value: Value) -> Result<()> {
    ctx.emit(value.clone(), |out| super::write_value(out, &value, 0))
}
pub(crate) fn installed(ctx: &Ctx<'_>) -> Result<bool> {
    ctx.deps.autostart.is_enabled()
}

/// `daemon stop` bounds each wait for startup, process exit and endpoint removal.
const STOP_WAIT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(50);

fn wait_for_stop_recovery(wait: Duration, mut ready: impl FnMut() -> Result<bool>) -> Result<()> {
    let deadline = Instant::now() + wait;
    loop {
        if ready()? {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::Service {
                code: IpcError::BUSY,
                message:
                    "BroScribe Agent was left running. Wait for recovery or saving to finish, \
                          then run `broscribe daemon stop` again"
                        .into(),
            });
        }
        std::thread::sleep(POLL.min(remaining));
    }
}

/// Why `daemon stop` refuses to touch the running agent `state`: an
/// active recording (stopping the agent must never end one the user did
/// not stop) or an update install holding the agent. The same check guards
/// an agent the login item restarted, so each message names only the agent
/// that was left running.
fn stop_refusal(state: &Value) -> Option<Error> {
    if let Some(session) = state["recording"]["session_id"].as_str() {
        return Some(Error::Service {
            code: IpcError::RECORDING_ACTIVE,
            message: format!(
                "recording {session} is active, so the agent was left running. Stop it first \
                 with `broscribe record stop` (or discard it), then stop the agent"
            ),
        });
    }
    if !state["install"].is_null() {
        return Some(Error::Service {
            code: IpcError::INSTALLING,
            message: "an update is installing and restarts the agent itself, so the agent was \
                      left running"
                .into(),
        });
    }
    None
}

/// Waits until process `pid` is gone; `false` when it still runs at the
/// deadline.
pub(super) fn wait_exit(probe: &dyn ProcessProbe, pid: u32, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    while probe.is_alive(pid) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL);
    }
    true
}

fn stop_peer(
    data_dir: &std::path::Path,
    timeout: Duration,
) -> Result<Option<(Client, AgentPing<'static>)>> {
    connect(data_dir, timeout)?
        .map(|mut client| ping(&mut client).map(|agent| (client, agent)))
        .transpose()
}

/// The agent the login job restarted after `stopped` exited (launchd's
/// KeepAlive restarts it at once), held for the unload: a connection
/// holding a stop admission, which fences capture and job starts
/// while the login item unloads it. One whose status shows a
/// recording or an update's install admission (a durable one survives the
/// restart) is refused by [`stop_refusal`]; one whose admission is refused
/// (recording work not saved yet, another capture, or an admission taken
/// since its status) is reported with the agent's own code: nothing is
/// unloaded.
fn hold_restarted(
    mut client: Client,
    agent: AgentPing<'_>,
    library: Option<&std::path::Path>,
    wait: Duration,
) -> Result<Client> {
    if agent.mode == broscribe_core::maintenance::Mode::Maintenance {
        // The retained shared mode lease fences normal work. The control
        // agent still acknowledges the stop hold on this connection.
        client.call(Call::ShutdownAcquire).map_err(call_error)?;
        return Ok(client);
    }
    let state = client.call(Call::Status).map_err(call_error)?;
    check_mcp_library(library, &state)?;
    if let Some(refusal) = stop_refusal(&state) {
        return Err(refusal);
    }
    let mut readiness = agent.readiness;
    wait_for_stop_recovery(wait, || {
        let mut result = client.call(Call::ShutdownAcquire);
        if matches!(&result, Err(CallError::Remote(e)) if e.code == IpcError::BUSY)
            && let Ok(peer) = ping(&mut client)
        {
            let was_starting = readiness == AgentReadiness::NormalStarting;
            readiness = peer.readiness;
            if readiness == AgentReadiness::NormalStarting {
                return Ok(false);
            }
            if was_starting && readiness == AgentReadiness::NormalOperational {
                // Startup may have finished between the refusal and the ping.
                result = client.call(Call::ShutdownAcquire);
            }
        }
        match result {
            Ok(_) => Ok(true),
            Err(CallError::Remote(e)) if e.code == IpcError::RECORDING_ACTIVE => {
                Err(Error::Service {
                    code: IpcError::RECORDING_ACTIVE,
                    message: format!(
                        "BroScribe Agent’s login item restarted it and it is protecting recording work, \
                         so it was left running ({}); wait for recovery or saving to finish, or stop \
                         its active recording, then run `broscribe daemon stop` again",
                        e.message
                    ),
                })
            }
            Err(e) => Err(call_error(e)),
        }
    })?;
    Ok(client)
}

fn unverified_login_job(stopped: Option<u32>) -> Error {
    let outcome = if stopped.is_some() {
        "the selected agent stopped and the login job was left loaded"
    } else {
        "nothing was stopped"
    };
    Error::Service {
        code: "login_job_unverified",
        message: format!(
            "the BroScribe Agent login job could not be verified for this profile; {outcome}"
        ),
    }
}

/// Whether a shared login job may be stopped. Check it and act on the
/// result while the same login-registration reservation is held.
pub(super) fn verify_login_job(
    job: LoginJob,
    live_pid: impl FnOnce() -> Result<Option<u32>>,
) -> Result<bool> {
    match job {
        LoginJob::Absent
        | LoginJob::Unsupported
        | LoginJob::Registered {
            serves_profile: Some(true),
        } => Ok(false),
        LoginJob::Idle {
            serves_profile: Some(true),
        } => Ok(true),
        LoginJob::Running(pid) if live_pid()? == Some(pid.get()) => Ok(true),
        _ => Err(unverified_login_job(None)),
    }
}

enum LoginJobStop {
    LeaveLoaded,
    Unsupported,
    Unload { _hold: Option<Client> },
}

/// Keep the startup reservation held while the login manager settles. In
/// particular, a KeepAlive restart may briefly have a PID but no endpoint:
/// it exits on its own when it cannot take the reservation.
fn settle_login_job(
    autostart: &dyn Autostart,
    data_dir: &std::path::Path,
    stopped: Option<u32>,
    library: Option<&std::path::Path>,
    wait: Duration,
    manager_timeout: Duration,
) -> Result<LoginJobStop> {
    let deadline = Instant::now() + wait;
    // A manager query can take its full timeout. Do not start one that
    // could run past the settle deadline while holding the registration lock.
    let query_window = || {
        deadline
            .saturating_duration_since(Instant::now())
            .checked_sub(manager_timeout)
            .ok_or_else(|| unverified_login_job(stopped))
    };
    let check_deadline = || {
        if Instant::now() >= deadline {
            return Err(unverified_login_job(stopped));
        }
        Ok(())
    };
    let query_job = || -> Result<LoginJob> {
        query_window()?;
        let job = autostart.login_job_until(data_dir, deadline);
        check_deadline()?;
        Ok(job)
    };
    loop {
        let job = query_job()?;
        match job {
            LoginJob::Unsupported => return Ok(LoginJobStop::Unsupported),
            LoginJob::Absent
            | LoginJob::Registered {
                serves_profile: Some(_),
            }
            | LoginJob::Idle {
                serves_profile: Some(false),
            } => {
                return Ok(LoginJobStop::LeaveLoaded);
            }
            LoginJob::Idle {
                serves_profile: Some(true),
            } => {
                return Ok(LoginJobStop::Unload { _hold: None });
            }
            LoginJob::Idle {
                serves_profile: None,
            }
            | LoginJob::Registered {
                serves_profile: None,
            } => return Err(unverified_login_job(stopped)),
            LoginJob::Unknown => {}
            LoginJob::Running(pid) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let peer = stop_peer(data_dir, remaining.min(Duration::from_secs(1)))
                    .ok()
                    .flatten()
                    .filter(|(_, agent)| agent.pid == pid.get());
                if let Some((client, agent)) = peer {
                    if Some(pid.get()) == stopped {
                        return Err(Error::Internal(
                            "BroScribe Agent acknowledged the stop but still answers".into(),
                        ));
                    }
                    let hold = hold_restarted(
                        client,
                        agent,
                        library,
                        deadline.saturating_duration_since(Instant::now()),
                    )?;
                    // Recovery may take time. Re-check with admission held
                    // just before an unload. A changed job needs its own check.
                    let current = query_job()?;
                    if current == job {
                        return Ok(LoginJobStop::Unload { _hold: Some(hold) });
                    }
                } else {
                    query_window()?;
                    let matches = autostart.login_job_matches_until(data_dir, deadline);
                    check_deadline()?;
                    if matches == Some(false) {
                        return Ok(LoginJobStop::LeaveLoaded);
                    }
                }
            }
        }
        let remaining = query_window()?;
        if remaining.is_zero() {
            return Err(unverified_login_job(stopped));
        }
        std::thread::sleep(POLL.min(remaining));
    }
}

pub(super) fn agent_instance_held(
    data_dir: &std::path::Path,
    instance: &dyn AgentInstanceProbe,
) -> Result<bool> {
    instance
        .is_held(data_dir)
        .map_err(|e| Error::io("checking the BroScribe Agent instance lock", e))
}

fn wait_for_agent_absence(
    data_dir: &std::path::Path,
    instance: &dyn AgentInstanceProbe,
    wait: Duration,
) -> Result<()> {
    let deadline = Instant::now() + wait;
    loop {
        // Even the final observation needs time for one connection attempt.
        let listening = connect(data_dir, Duration::from_millis(250))?.is_some();
        let held = agent_instance_held(data_dir, instance)?;
        if !listening && !held {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(if listening {
                Error::Internal(
                    "an agent still answers for this profile; run `broscribe daemon status` and stop it again"
                        .into(),
                )
            } else {
                Error::Service {
                    code: "agent_unreachable",
                    message: "an agent still holds this profile's instance lock without a reachable endpoint; its stop could not be verified".into(),
                }
            });
        }
        std::thread::sleep(POLL.min(remaining));
    }
}

/// `daemon stop`: asks the agent to stop if idle, waits for that process to
/// exit, then unloads a matching login job and verifies the endpoint and
/// instance claim are both gone. The startup reservation fences new agents.
pub(super) fn stop(ctx: &mut Ctx<'_>) -> Result<()> {
    let data_dir = ctx.data_dir().to_path_buf();
    let deadline = Instant::now() + STOP_WAIT;
    let _startup = loop {
        if let Some(startup) = DaemonStartupLock::try_acquire(&data_dir)? {
            break startup;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::Service {
                code: IpcError::BUSY,
                message: "BroScribe Agent is starting. Wait for startup to finish, then run \
                          `broscribe daemon stop` again"
                    .into(),
            });
        }
        std::thread::sleep(POLL.min(remaining));
    };
    let autostart = ctx.deps.autostart.as_ref();
    let job = autostart.login_job(&data_dir);
    let peer = stop_peer(&data_dir, Duration::from_secs(5)).map_err(|error| match job {
        LoginJob::Running(_) => unverified_login_job(None),
        _ => error,
    })?;
    let mut stopped = None;
    let mut answer = Value::Null;
    if let Some((mut client, agent)) = peer {
        match job {
            LoginJob::Running(pid) if pid.get() != agent.pid => {
                return Err(unverified_login_job(None));
            }
            LoginJob::Unknown
            | LoginJob::Idle {
                serves_profile: None,
            } => {
                return Err(unverified_login_job(None));
            }
            _ => {}
        }
        let pid = agent.pid;
        answer = match agent.mode {
            broscribe_core::maintenance::Mode::Normal => {
                if agent.readiness == AgentReadiness::NormalStarting {
                    wait_for_stop_recovery(STOP_WAIT, || {
                        Ok(ping(&mut client)?.readiness == AgentReadiness::NormalOperational)
                    })?;
                }
                if ctx.request_library.is_some() {
                    let state = client.call(Call::Status).map_err(call_error)?;
                    check_mcp_library(ctx.request_library.as_deref(), &state)?;
                }
                let cancel = ctx
                    .request_cancel
                    .clone()
                    .map(CancelToken::from_flag)
                    .unwrap_or_default();
                // The peer that answered the ping decides atomically. If
                // its reply is lost, reconnect only for reconciliation.
                client.call_connected_mutating(
                    &Endpoint::new(&data_dir),
                    Call::ShutdownIfIdle,
                    &cancel,
                )
            }
            broscribe_core::maintenance::Mode::Maintenance => client.call(Call::Shutdown),
        }
        .map_err(call_error)?;
        if !wait_exit(ctx.deps.process.as_ref(), pid, STOP_WAIT) {
            // Unloading now would terminate it before its capture is durable.
            return Err(Error::Internal(format!(
                "BroScribe Agent (process {pid}) acknowledged the stop but has not exited; its login \
                 job was left loaded"
            )));
        }
        stopped = Some(pid);
    }
    let (unloaded, unsupported) = {
        // Reserve the per-user registration after the profile reservation,
        // and keep it through the final observation and unload.
        let registration = autostart.lock_login_registration(STOP_WAIT)?;
        let job = settle_login_job(
            autostart,
            &data_dir,
            stopped,
            ctx.request_library.as_deref(),
            STOP_WAIT,
            LOGIN_MANAGER_TIMEOUT,
        )?;
        let unloaded = matches!(job, LoginJobStop::Unload { .. });
        if unloaded {
            match autostart.stop_login_job(&registration) {
                Ok(()) => {}
                Err(Error::Unsupported(_))
                    if autostart.login_job(&data_dir) == LoginJob::Absent => {}
                Err(e) => return Err(e),
            }
        }
        (unloaded, matches!(job, LoginJobStop::Unsupported))
    };
    // An older agent may hold the profile even if its endpoint moved.
    wait_for_agent_absence(&data_dir, ctx.deps.agent_instance.as_ref(), STOP_WAIT)?;
    let value = json!({
        "stopped": stopped.is_some(),
        "pid": stopped,
        "login_job_unloaded": unloaded,
        "running": false,
        "agent": answer,
    });
    ctx.emit(value, |out| {
        match stopped {
            Some(pid) => writeln!(out, "Stopped BroScribe Agent (process {pid}).")?,
            None => writeln!(out, "BroScribe Agent wasn't running.")?,
        }
        if unloaded {
            writeln!(
                out,
                "Its login job is unloaded until `broscribe daemon start` or the next login."
            )?;
        } else if unsupported {
            writeln!(out, "The login registration was left in place.")?;
        }
        Ok(())
    })
}

fn watch(ctx: &mut Ctx<'_>) -> Result<()> {
    let mut sub = subscribe(ctx.data_dir())?;
    while let Some(event) = sub
        .next_event()
        .map_err(|e| Error::io("reading agent events", e))?
    {
        if let Some(value) = watch_event(&event)? {
            let result = writeln!(ctx.out, "{value}").and_then(|()| ctx.out.flush());
            if let Err(e) = result {
                if e.kind() == std::io::ErrorKind::BrokenPipe {
                    return Ok(());
                }
                return Err(e.into());
            }
        }
    }
    Ok(())
}

pub(crate) fn subscribe(
    data_dir: &std::path::Path,
) -> Result<broscribe_platform_native::ipc::Subscription> {
    let client = connect(data_dir, Duration::from_secs(5))?
        .ok_or_else(|| Error::DaemonUnavailable("BroScribe Agent is not running".into()))?;
    client.subscribe().map_err(call_error)
}

pub(crate) fn watch_event(event: &broscribe_ipc::Event) -> Result<Option<Value>> {
    use broscribe_ipc::Event;
    if matches!(
        event,
        Event::State(_)
            | Event::Meters { .. }
            | Event::JobProgress(_)
            | Event::CaptureWarning { .. }
            | Event::Notification(_)
            | Event::NotificationDismissed { .. }
            | Event::Lagged { .. }
    ) {
        return crate::to_value(event).map(Some);
    }
    Ok(None)
}

/// The whole-app uninstaller can skip registrations belonging to its other
/// profiles. A direct daemon uninstall always reports a mismatch.
pub(super) fn unregister(
    autostart: &dyn Autostart,
    data_dir: &std::path::Path,
    admission_fence: Option<u64>,
    skip_other_profile: bool,
) -> Result<bool> {
    let guard = autostart.lock_login_registration(LoginRegistrationGuard::DEFAULT_WAIT)?;
    let admitted = admission_fence
        .map(|fence| {
            super::update::admitted_agent_pid(data_dir, fence)?.ok_or_else(|| Error::Service {
                code: broscribe_updates::codes::ADMISSION_LOST,
                message: "BroScribe Agent did not answer to prove its install admission".into(),
            })
        })
        .transpose()?;
    let job = autostart.login_job(data_dir);
    let owned = match job {
        LoginJob::Absent | LoginJob::Unsupported => false,
        LoginJob::Registered {
            serves_profile: Some(true),
        } => true,
        LoginJob::Idle {
            serves_profile: Some(false),
        }
        | LoginJob::Registered {
            serves_profile: Some(false),
        } if skip_other_profile => false,
        _ => verify_login_job(job, || match admitted {
            Some(pid) => Ok(Some(pid)),
            None => Ok(stop_peer(data_dir, Duration::from_secs(5))?.map(|(_, agent)| agent.pid)),
        })?,
    };
    if owned {
        autostart.unregister_profile(data_dir, &guard)?;
    }
    Ok(owned)
}

/// Manual rollback can remove a registration whose agent never came up.
fn unregister_idle(
    autostart: &dyn Autostart,
    instance: &dyn AgentInstanceProbe,
    data_dir: &std::path::Path,
    fence: u64,
) -> Result<()> {
    let _startup = DaemonStartupLock::try_acquire(data_dir)?.ok_or_else(|| Error::Service {
        code: IpcError::BUSY,
        message: "BroScribe Agent is starting".into(),
    })?;
    let registration = autostart.lock_login_registration(LoginRegistrationGuard::DEFAULT_WAIT)?;
    let held = broscribe_ipc::install_admission::Persisted::load(data_dir)?.is_some_and(|record| {
        fence != 0
            && record.last_fence == fence
            && record.held
                == Some(broscribe_ipc::InstallHold {
                    fence,
                    durable: true,
                })
    });
    if !held {
        return Err(Error::Service {
            code: broscribe_updates::codes::ADMISSION_LOST,
            message: "the profile has no matching durable install admission on disk".into(),
        });
    }
    if connect(data_dir, Duration::from_millis(250))?.is_some()
        || agent_instance_held(data_dir, instance)?
    {
        return Err(Error::Service {
            code: IpcError::BUSY,
            message: "BroScribe Agent is still running for this profile".into(),
        });
    }
    match autostart.login_job(data_dir) {
        LoginJob::Idle {
            serves_profile: Some(true),
        }
        | LoginJob::Registered {
            serves_profile: Some(true),
        } => autostart.unregister_profile(data_dir, &registration),
        LoginJob::Absent => Ok(()),
        _ => Err(unverified_login_job(None)),
    }
}

pub(super) fn run(ctx: &mut Ctx<'_>, action: DaemonCmd) -> Result<()> {
    match action {
        DaemonCmd::Status => lifecycle_status(ctx),
        DaemonCmd::Watch => watch(ctx),
        DaemonCmd::Notice { id, action } => {
            let value = crate::mcp::recording_call(
                ctx,
                Call::NotificationAction {
                    notification_id: id,
                    action,
                },
            )?;
            emit(ctx, value)
        }
        DaemonCmd::Stop => stop(ctx),
        DaemonCmd::Start => {
            check_mcp_start(ctx)?;
            ctx.deps.autostart.start()?;
            lifecycle_status(ctx)
        }
        DaemonCmd::Install => {
            check_mcp_start(ctx)?;
            ctx.deps.autostart.set_enabled(true)?;
            lifecycle_status(ctx)
        }
        DaemonCmd::Uninstall {
            admission_fence,
            admission_fence_if_idle,
        } => {
            check_mcp_start(ctx)?;
            if let Some(fence) = admission_fence_if_idle {
                unregister_idle(
                    ctx.deps.autostart.as_ref(),
                    ctx.deps.agent_instance.as_ref(),
                    ctx.data_dir(),
                    fence,
                )?;
            } else {
                unregister(
                    ctx.deps.autostart.as_ref(),
                    ctx.data_dir(),
                    admission_fence,
                    false,
                )?;
            }
            lifecycle_status(ctx)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrations_sync_waits_longer_than_one_http_request() {
        let sync = call_timeout(&Call::IntegrationsSync { integration: None });
        assert!(sync >= Duration::from_secs(120));
        assert_eq!(call_timeout(&Call::Status), Duration::from_secs(5));
    }

    #[test]
    fn a_setup_capture_outlasts_its_recording() {
        let capture = |seconds| Call::CaptureSample {
            session: "r-0".into(),
            seconds,
            purpose: "enrollment".into(),
        };
        assert!(call_timeout(&capture(45.0)) > Duration::from_secs(45));
        assert!(call_timeout(&capture(120.0)) > Duration::from_secs(120));
        assert!(call_timeout(&Call::Shutdown) > Duration::from_secs(5));
    }

    #[test]
    fn calendar_connect_outlasts_the_permission_prompt() {
        let connect = Call::SystemCalendar {
            action: SystemCalendarAction::Connect,
        };
        // The agent bounds the macOS prompt at 120 s; the reply must not be cut short.
        assert_eq!(call_timeout(&connect), Duration::from_secs(150));
        assert_eq!(reconcile_window(&connect), Some(Duration::from_secs(180)));
        assert_eq!(
            reconcile_window(&Call::SystemCalendar {
                action: SystemCalendarAction::Disconnect
            }),
            None
        );
    }

    #[test]
    fn known_native_codes_survive_and_others_keep_their_mapping() {
        for &code in SERVICE_CODES {
            let error = remote_error(false, IpcError::new(code, "synthetic"));
            assert_eq!(error.code(), code);
            assert_eq!(error.to_string(), "synthetic");
        }
        assert_eq!(
            remote_error(false, IpcError::new("not_found", "synthetic")).code(),
            "not_found"
        );
        assert_eq!(
            remote_error(false, IpcError::new("unheard_of", "synthetic")).code(),
            "internal"
        );
        // Only a native call's rejected method means the agent is outdated.
        let invalid = || IpcError::new(IpcError::INVALID_REQUEST, "unknown variant");
        assert_eq!(remote_error(true, invalid()).code(), "helper_outdated");
        assert_eq!(remote_error(false, invalid()).code(), "invalid_input");
        // An agent of the older protocol never runs anything: outdated.
        let old = IpcError::new(IpcError::UNSUPPORTED_VERSION, "protocol version 2");
        assert_eq!(remote_error(false, old).code(), "helper_outdated");
    }

    #[test]
    fn stopping_refuses_an_active_recording_or_an_install() {
        let idle = json!({"pid": 7, "recording": null, "install": null});
        assert!(stop_refusal(&idle).is_none());
        let recording = json!({"pid": 7, "recording": {"session_id": "s-1"}, "install": null});
        assert_eq!(
            stop_refusal(&recording).unwrap().code(),
            IpcError::RECORDING_ACTIVE
        );
        let installing = json!({"pid": 7, "recording": null, "install": {"fence": 3}});
        assert_eq!(
            stop_refusal(&installing).unwrap().code(),
            IpcError::INSTALLING
        );
    }

    #[test]
    fn stop_recovery_timeout_explains_when_to_retry() {
        let error = wait_for_stop_recovery(Duration::ZERO, || Ok(false)).unwrap_err();
        assert_eq!(error.code(), IpcError::BUSY);
        assert!(error.to_string().contains("Wait for recovery"));
        assert!(error.to_string().contains("`broscribe daemon stop` again"));
    }

    #[test]
    fn stop_verifies_agent_absence_with_the_instance_probe() {
        use broscribe_platform::fakes::FakeAgentInstanceProbe;
        use std::sync::atomic::Ordering;

        let home = tempfile::tempdir().unwrap();
        let instance = FakeAgentInstanceProbe::default();
        instance.held.store(true, Ordering::SeqCst);
        let error = wait_for_agent_absence(home.path(), &instance, Duration::ZERO).unwrap_err();
        assert_eq!(error.code(), "agent_unreachable");
        instance.held.store(false, Ordering::SeqCst);
        wait_for_agent_absence(home.path(), &instance, Duration::ZERO).unwrap();
    }

    #[test]
    fn settling_a_running_job_without_a_matching_peer_uses_the_configuration_or_deadline() {
        use broscribe_platform::fakes::FakeAutostart;
        use std::num::NonZeroU32;

        let home = tempfile::tempdir().unwrap();
        let pid = NonZeroU32::new(4242).unwrap();
        let job = FakeAutostart {
            login_job: LoginJob::Running(pid).into(),
            login_job_profile: Some(false).into(),
            ..Default::default()
        };
        assert!(matches!(
            settle_login_job(&job, home.path(), Some(1), None, STOP_WAIT, Duration::ZERO),
            Ok(LoginJobStop::LeaveLoaded)
        ));
        *job.login_job_profile.lock().unwrap() = None;
        let budget = Duration::from_millis(60);
        let started = Instant::now();
        let Err(error) = settle_login_job(&job, home.path(), Some(1), None, budget, Duration::ZERO)
        else {
            panic!("a running login job of unknown profile never settled");
        };
        assert!(started.elapsed() >= budget);
        assert_eq!(error.code(), "login_job_unverified");
        assert!(error.to_string().contains("the selected agent stopped"));
    }

    #[test]
    fn settling_does_not_start_a_query_that_cannot_fit_its_deadline() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct TimedOutManager {
            calls: AtomicUsize,
            timeout: Duration,
        }
        impl Autostart for TimedOutManager {
            fn is_enabled(&self) -> Result<bool> {
                unreachable!()
            }
            fn set_enabled(&self, _: bool) -> Result<()> {
                unreachable!()
            }
            fn login_job(&self, _: &std::path::Path) -> LoginJob {
                self.calls.fetch_add(1, Ordering::Relaxed);
                std::thread::sleep(self.timeout);
                LoginJob::Unknown
            }
        }

        let home = tempfile::tempdir().unwrap();
        let timeout = Duration::from_millis(150);
        let job = TimedOutManager {
            calls: AtomicUsize::new(0),
            timeout,
        };
        let budget = Duration::from_millis(275);
        let Err(error) = settle_login_job(&job, home.path(), Some(1), None, budget, timeout) else {
            panic!("the manager never answered");
        };
        assert_eq!(error.code(), "login_job_unverified");
        assert!(error.to_string().contains("the selected agent stopped"));
        assert_eq!(job.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn settling_rejects_an_idle_profile_match_returned_after_the_deadline() {
        struct SlowIdleProfile;
        impl Autostart for SlowIdleProfile {
            fn is_enabled(&self) -> Result<bool> {
                unreachable!()
            }
            fn set_enabled(&self, _: bool) -> Result<()> {
                unreachable!()
            }
            fn login_job(&self, _: &std::path::Path) -> LoginJob {
                std::thread::sleep(Duration::from_millis(80));
                LoginJob::Idle {
                    serves_profile: Some(true),
                }
            }
        }

        let home = tempfile::tempdir().unwrap();
        let result = settle_login_job(
            &SlowIdleProfile,
            home.path(),
            Some(1),
            None,
            Duration::from_millis(40),
            Duration::ZERO,
        );
        let Err(error) = result else {
            panic!("a late idle match authorized an unload");
        };
        assert_eq!(error.code(), "login_job_unverified");
    }

    #[test]
    fn waiting_for_exit_follows_the_process_probe() {
        struct Gone;
        impl ProcessProbe for Gone {
            fn is_alive(&self, _: u32) -> bool {
                false
            }
            fn current_pid(&self) -> u32 {
                1
            }
        }
        struct Alive;
        impl ProcessProbe for Alive {
            fn is_alive(&self, _: u32) -> bool {
                true
            }
            fn current_pid(&self) -> u32 {
                1
            }
        }
        assert!(wait_exit(&Gone, 42, Duration::ZERO));
        assert!(!wait_exit(&Alive, 42, Duration::from_millis(60)));
    }
}
