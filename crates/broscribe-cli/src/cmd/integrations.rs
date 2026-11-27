//! Explicit integration actions; background refresh never initiates sign-in.
//! System calendars belong to the running agent: this process never asks macOS
//! for calendar access or reads calendars itself.
//!
//! Provider traffic verifies against the operating system's trust store
//! (`Deps::execution`); one malformed `[integration.<name>]` table disables
//! only that integration. Every browser sign-in runs under a
//! [`LoginAttemptId`] that `cancel-login` (any process) or this process's own
//! Ctrl-C/SIGTERM cancels durably through the attempt journal, so a cancelled
//! sign-in never commits later. A change already saved stays saved when the
//! running agent cannot be told about it (`agent_notified: false`).
use crate::{
    Ctx,
    cli::{
        AttemptArg, CalendarRemovalSpec, CalendarSelectionSpec, IntegrationArg, IntegrationsCmd,
        SystemCalendarSelectionSpec,
    },
    query::read_stdin,
    to_value,
};
use broscribe_core::calendar::{AccountId, AccountRef, GoogleCalendarStamp, RemoteIntegration};
use broscribe_core::config::Config;
use broscribe_core::{Error, Result};
use broscribe_integrations::{
    IntegrationId, IntegrationSettings, IntegrationStatus, Integrations, IntegrationsConfig,
    InvalidTables, LoginAttemptId, PublicError, RevocationOutcome, SyncRow, SystemCalendarStatus,
};
use broscribe_ipc::{Call, SystemCalendarAction};
use broscribe_platform::CalendarProvider;
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use std::sync::Arc;

pub(super) fn qualified_account(value: &str) -> Result<AccountRef> {
    let (provider, account) = value
        .split_once('/')
        .ok_or_else(|| Error::InvalidInput("--account requires PROVIDER/ACCOUNT_ID".into()))?;
    let integration = match provider {
        "microsoft-graph" => IntegrationArg::MicrosoftGraph,
        _ => {
            return Err(Error::InvalidInput(
                "--account requires microsoft-graph".into(),
            ));
        }
    };
    account_ref(integration, account)
}

fn account_ref(integration: IntegrationArg, account: &str) -> Result<AccountRef> {
    let integration = match integration {
        IntegrationArg::MicrosoftGraph => RemoteIntegration::MicrosoftGraph,
        _ => {
            return Err(Error::InvalidInput(
                "account actions require microsoft-graph".into(),
            ));
        }
    };
    Ok(AccountRef {
        integration,
        account_id: AccountId::parse(account)?,
    })
}

fn corporate(integration: IntegrationArg) -> Result<IntegrationId> {
    match integration {
        IntegrationArg::MicrosoftGraph => Ok(IntegrationId::MicrosoftGraph),
        _ => Err(Error::InvalidInput(
            "account actions require microsoft-graph".into(),
        )),
    }
}

/// The integration error as a CLI error with the same stable code and its
/// sanitized message ([`broscribe_integrations::PublicError`]): never raw
/// provider text, tokens or codes.
pub(crate) fn core(e: broscribe_integrations::IntegrationError) -> Error {
    use broscribe_integrations::IntegrationError as E;
    match e {
        E::Core(e) => e,
        E::AccountExists { account_id } => Error::AccountExists { account_id },
        e @ E::NotConfigured(..) => Error::NotConfigured(e.public().message),
        e => Error::Service {
            code: e.code(),
            message: e.public().message,
        },
    }
}

impl From<IntegrationArg> for IntegrationId {
    fn from(value: IntegrationArg) -> Self {
        match value {
            IntegrationArg::SystemCalendar => Self::SystemCalendar,
            IntegrationArg::MicrosoftGraph => Self::MicrosoftGraph,
            IntegrationArg::Google => Self::Google,
        }
    }
}

/// The selection exactly as given, minus repeats; never widened.
fn calendar_selection(ids: Vec<String>, flag: &str) -> Result<Vec<String>> {
    let mut selected: Vec<String> = Vec::with_capacity(ids.len());
    for id in ids {
        if id.trim().is_empty() {
            return Err(Error::InvalidInput(format!("{flag} must not be empty")));
        }
        if !selected.contains(&id) {
            selected.push(id);
        }
    }
    Ok(selected)
}

/// No usable agent, so native-calendar access is unknown, never guessed.
fn agent_unavailable(code: &str) -> bool {
    matches!(code, "helper_required" | "helper_outdated")
}

/// The local native-calendar state, with permission and calendars unknown
/// and the reason the agent could not report them.
fn without_agent(mut status: IntegrationStatus, error: &Error) -> Result<Value> {
    let unknown = Some(SystemCalendarStatus {
        authorization: None,
        helper_required: true,
        calendars: Vec::new(),
        calendars_error: None,
    });
    status.system_calendar = unknown;
    let mut value = to_value(&status)?;
    value["helper_error"] = json!({"code":error.code(),"message":error.to_string()});
    Ok(value)
}

/// The agent's report (permission, selection, eligible calendars) is the
/// authority. `strict` fails on agent errors other than its absence; otherwise
/// they are reported beside the local state.
fn native_status(
    data_dir: &std::path::Path,
    provider: CalendarProvider,
    local: IntegrationStatus,
    strict: bool,
) -> Result<Value> {
    match super::daemon::native_calendar(data_dir, provider, SystemCalendarAction::Status) {
        Ok(value) => Ok(value),
        Err(e) if !strict || agent_unavailable(e.code()) => without_agent(local, &e),
        Err(e) => Err(e),
    }
}

/// Setup takes no OAuth client: the explicit calendar selection is its only
/// setting, from `--calendar-id`/`--clear-calendars` or a
/// [`SystemCalendarSelectionSpec`] on stdin (read only after the argv checks).
fn setup_native(
    ctx: &Ctx<'_>,
    provider: CalendarProvider,
    oauth_flags: bool,
    calendar_ids: Vec<String>,
    clear_calendars: bool,
    spec_stdin: bool,
) -> Result<Value> {
    let id = IntegrationId::of_provider(provider);
    if oauth_flags {
        return Err(Error::InvalidInput(format!(
            "{id} reads calendars through macOS permission and needs no OAuth client; --client-id, --client-secret-stdin and --clear-client-secret do not apply"
        )));
    }
    let calendar_ids = if spec_stdin {
        let spec: SystemCalendarSelectionSpec = read_stdin("integrations setup")?;
        calendar_selection(spec.calendar_ids, "calendar_ids")?
    } else if clear_calendars {
        Vec::new()
    } else if calendar_ids.is_empty() {
        let config = IntegrationsConfig::default();
        return to_value(&broscribe_integrations::setup(id, &config.settings(id)));
    } else {
        calendar_selection(calendar_ids, "--calendar-id")?
    };
    super::daemon::native_calendar(
        ctx.data_dir(),
        provider,
        SystemCalendarAction::Configure { calendar_ids },
    )
}

fn notify(
    notes: crate::BackgroundNotes,
) -> Arc<dyn Fn(broscribe_integrations::ReauthNotification) + Send + Sync> {
    Arc::new(move |notice: broscribe_integrations::ReauthNotification| {
        notes.push(format!("{}: {}", notice.integration, notice.message));
    })
}

/// The service over the isolated `[integration.*]` settings, verifying
/// provider TLS against the operating system's trust store, with the
/// integrations whose own settings did not parse.
fn open(ctx: &Ctx<'_>) -> Result<(Integrations, InvalidTables)> {
    let data_dir = ctx.data_dir();
    let deps = ctx.deps;
    let (config, invalid) = IntegrationsConfig::load_isolated(data_dir).map_err(core)?;
    let service = Integrations::new(
        data_dir,
        config,
        deps.secrets.clone(),
        deps.browser.clone(),
        notify(ctx.background_notes()),
    )
    .map_err(core)?
    .with_trust_store(deps.execution.trust_store())
    .with_reauth_notification_delivery(false);
    Ok((service, invalid))
}

/// Why `id`'s own settings are unusable: its table, or the `[integration]`
/// key itself, did not parse.
fn config_error(invalid: &InvalidTables, id: IntegrationId) -> Option<&String> {
    invalid
        .get(id.as_str())
        .or_else(|| invalid.get("integration"))
}

/// Every integration with usable settings; a malformed table disables only
/// its own integration (it reads as not configured).
pub(crate) fn service(ctx: &Ctx<'_>) -> Result<Integrations> {
    open(ctx).map(|(service, _)| service)
}

/// For an action of `id`: its own malformed settings fail closed with their
/// message instead of a misleading missing-client error.
fn service_for(ctx: &Ctx<'_>, id: IntegrationId) -> Result<Integrations> {
    let (service, invalid) = open(ctx)?;
    match config_error(&invalid, id) {
        Some(message) => Err(Error::NotConfigured(message.clone())),
        None => Ok(service),
    }
}

/// Native opt-out and local status never depend on unrelated OAuth settings.
fn local_native_service(ctx: &Ctx<'_>) -> Result<Integrations> {
    Integrations::new(
        ctx.data_dir(),
        IntegrationsConfig::default(),
        ctx.deps.secrets.clone(),
        ctx.deps.browser.clone(),
        notify(ctx.background_notes()),
    )
    .map(|service| service.with_reauth_notification_delivery(false))
    .map_err(core)
}

/// The caller's `--attempt`, or a new random one.
fn attempt_id(arg: AttemptArg) -> Result<LoginAttemptId> {
    match arg.attempt {
        Some(id) => LoginAttemptId::parse(&id).map_err(core),
        None => Ok(LoginAttemptId::generate()),
    }
}

/// Stops cancelling once the sign-in returned.
struct CancelGuard(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for CancelGuard {
    fn drop(&mut self) {
        if let Some(done) = self.0.take() {
            let _ = done.send(());
        }
    }
}

/// Runs `sign_in` as `attempt` of `id`. Ctrl-C or SIGTERM cancels that
/// attempt in the attempt journal (never just this process), so nothing of it
/// is saved even if the browser completes later; a second request quits at
/// once. The attempt ID is announced first so another process can cancel it.
fn signing_in<T>(
    ctx: &mut Ctx<'_>,
    id: IntegrationId,
    attempt: &LoginAttemptId,
    sign_in: impl FnOnce() -> Result<T>,
) -> Result<T> {
    ctx.note(format!(
        "Sign-in attempt {attempt}; cancel it with `broscribe integrations cancel-login {id} --attempt {attempt}` or Ctrl-C."
    ));
    let (done, finished) = tokio::sync::oneshot::channel::<()>();
    let _guard = CancelGuard(Some(done));
    let data_dir = ctx.data_dir().to_path_buf();
    let secrets = ctx.deps.secrets.clone();
    let browser = ctx.deps.browser.clone();
    let own = attempt.clone();
    let notes = ctx.background_notes();
    std::thread::spawn(move || {
        use broscribe_platform_native::ipc::termination;
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        rt.block_on(async move {
            tokio::select! {
                _ = finished => return,
                signal = termination() => if signal.is_err() { return },
            }
            let cancelled = Integrations::new(
                &data_dir,
                IntegrationsConfig::default(),
                secrets,
                browser,
                notify(notes.clone()),
            )
            .and_then(|service| service.cancel_login(id, &own));
            match cancelled {
                Ok(status) => notes.push(format!(
                    "Cancelling sign-in attempt {own}: {}; press Ctrl-C again to quit now.",
                    attempt_state(&status.state)
                )),
                Err(error) => notes.push(format!(
                    "Sign-in attempt {own} could not be cancelled ({}); press Ctrl-C again to quit.",
                    error.code()
                )),
            }
            if termination().await.is_ok() {
                std::process::exit(130);
            }
        });
    });
    sign_in()
}

fn attempt_state(state: &broscribe_integrations::LoginAttemptState) -> &'static str {
    use broscribe_integrations::LoginAttemptState as S;
    match state {
        S::Running => "running",
        S::Cancelled => "cancelled, nothing of it is saved",
        S::Succeeded => "it had already succeeded and stays signed in",
        S::Failed { .. } => "it had already failed",
        S::Interrupted => "it was interrupted while saving",
        S::NotFound => "not found",
    }
}

/// `value` with the attempt it ran as.
fn with_attempt(mut value: Value, attempt: &LoginAttemptId) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.insert("attempt".into(), json!(attempt));
    }
    value
}

/// Where a user removes a Google grant themselves.
const GOOGLE_PERMISSIONS: &str = "https://myaccount.google.com/permissions";

/// What the revocation at Google did, for people.
fn revocation_note(outcome: &RevocationOutcome) -> String {
    match outcome {
        RevocationOutcome::RevocationRequested => {
            "Google accepted the revocation request; it may take a while to take full effect."
                .into()
        }
        RevocationOutcome::NoStoredGrant => format!(
            "No stored Google sign-in was found, so nothing was sent to Google. To remove an older grant, remove your OAuth app (the name on your consent screen) at {GOOGLE_PERMISSIONS}."
        ),
        RevocationOutcome::Failed { code } => format!(
            "Revoking at Google failed ({code}), so access may still be granted. Remove your OAuth app (the name on your consent screen) at {GOOGLE_PERMISSIONS}."
        ),
    }
}

/// `logout google --revoke-at-google`: the local sign-out, then the explicit revocation
/// at Google. Both results are always reported; a failed local sign-out also fails the
/// command (with an `error` beside the revocation outcome).
fn logout_and_revoke(ctx: &mut Ctx<'_>, id: IntegrationId) -> Result<Value> {
    ctx.note(format!(
        "Signing out, then asking Google to revoke this sign-in. Google revokes every permission granted to your OAuth project, for every data folder, Mac or tool signed in with it. If anything fails, remove your OAuth app at {GOOGLE_PERMISSIONS}."
    ));
    let done = service(ctx)?.logout_and_revoke(id).map_err(core)?;
    let note = revocation_note(&done.remote_revocation);
    ctx.note(&note);
    let mut value = json!({
        "integration": id,
        "signed_out": done.local.is_ok(),
        "remote_revocation": to_value(&done.remote_revocation)?,
    });
    if let Err(error) = done.local {
        value["error"] = json!({
            "code": error.code,
            "message": format!("signing out locally failed: {} {note}", error.message),
        });
        ctx.exit_code = Some(1);
    }
    Ok(value)
}

/// Tells a running agent to reload integrations after a saved change. The
/// change stays saved whatever happens here: a refusal or lost reply only
/// reports `agent_notified: false`.
fn notify_agent(ctx: &mut Ctx<'_>, value: &mut Value) {
    let notified = super::daemon::optional_call(ctx.data_dir(), Call::IntegrationsReload);
    if let Err(error) = &notified {
        ctx.note(format!(
            "The change is saved, but BroScribe Agent was not told ({}); it applies the change after it restarts.",
            error.code()
        ));
    }
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "agent_notified".into(),
            json!(matches!(notified, Ok(Some(_)))),
        );
    }
}

fn require_spec(spec_stdin: bool) -> Result<()> {
    if spec_stdin {
        Ok(())
    } else {
        Err(Error::InvalidInput("--spec-stdin is required".into()))
    }
}

/// Saves the client ID and returns the settings it replaced, read under the
/// same config lock (a malformed previous table counts as unconfigured, so a
/// sign-in it issued is retired rather than kept).
fn save_client_id(
    data_dir: &std::path::Path,
    id: IntegrationId,
    client_id: String,
) -> Result<IntegrationSettings> {
    Config::update(data_dir, |config| {
        let table = config
            .integration
            .entry(id.as_str().to_owned())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or_else(|| Error::InvalidInput(format!("[integration.{id}] must be a table")))?;
        let previous = toml::Value::Table(table.clone())
            .try_into::<IntegrationSettings>()
            .unwrap_or_default();
        table.insert("client_id".into(), toml::Value::String(client_id));
        Ok(previous)
    })
    .map(|(_, previous)| previous)
}

pub(super) fn run(ctx: &mut Ctx<'_>, action: IntegrationsCmd) -> Result<()> {
    let (mut value, reload) = match action {
        IntegrationsCmd::Accounts { integration } => {
            let id = integration.map(corporate).transpose()?;
            (
                to_value(&service(ctx)?.list_accounts(id).map_err(core)?)?,
                false,
            )
        }
        IntegrationsCmd::AddAccount {
            integration,
            attempt,
        } => {
            let id = corporate(integration)?;
            let attempt = attempt_id(attempt)?;
            ctx.note("Sign-in reads your Microsoft profile (User.Read) to verify account and tenant identity. No calendars are selected automatically.");
            let service = service_for(ctx, id)?;
            let account = signing_in(ctx, id, &attempt, || {
                service.add_account(id, &attempt).map_err(core)
            })?;
            (with_attempt(to_value(&account)?, &attempt), true)
        }
        IntegrationsCmd::ReconnectAccount {
            integration,
            account,
            generation,
            attempt,
        } => {
            let account = account_ref(integration, &account)?;
            let attempt = attempt_id(attempt)?;
            ctx.note("Reconnect verifies the same Microsoft profile and tenant using User.Read; a different identity cannot replace this account.");
            let id = IntegrationId::MicrosoftGraph;
            let service = service_for(ctx, id)?;
            let result = signing_in(ctx, id, &attempt, || {
                service
                    .reconnect_account(&account, generation, &attempt)
                    .map_err(core)
            })?;
            (with_attempt(to_value(&result)?, &attempt), true)
        }
        IntegrationsCmd::RemoveAccount {
            integration,
            account,
            generation,
        } => {
            let account = account_ref(integration, &account)?;
            service(ctx)?
                .remove_account(&account, generation)
                .map_err(core)?;
            (json!({"account":account,"removed":true}), true)
        }
        IntegrationsCmd::DiscardLegacy {
            integration,
            generation,
            yes,
        } => {
            let id: IntegrationId = integration.into();
            if id != IntegrationId::MicrosoftGraph {
                return Err(Error::Service {
                    code: "account_required",
                    message: "Only Microsoft providers have a quarantined old sign-in to discard."
                        .into(),
                });
            }
            if !yes {
                return Err(Error::InvalidInput("discard-legacy requires explicit --yes to discard only this provider's quarantined old sign-in, token, calendar cache and photos; registered accounts and library data remain unchanged".into()));
            }
            let legacy = service(ctx)?
                .discard_legacy_microsoft(id, generation)
                .map_err(core)?;
            // None means no quarantined slot remains, including an idempotent
            // no-slot call. It does not claim this invocation deleted anything.
            (json!({"integration":id,"legacy_microsoft":legacy}), true)
        }
        IntegrationsCmd::Calendars {
            integration,
            account,
        } => {
            let id: IntegrationId = integration.into();
            let catalog = match (id, account) {
                (IntegrationId::Google, None) => {
                    to_value(&service_for(ctx, id)?.google_calendars().map_err(core)?)?
                }
                (IntegrationId::Google, Some(_)) => {
                    return Err(Error::InvalidInput(
                        "google has one provider-wide account; --account does not apply".into(),
                    ));
                }
                (IntegrationId::MicrosoftGraph, Some(account)) => {
                    let account = account_ref(integration, &account)?;
                    to_value(
                        &service_for(ctx, id)?
                            .list_calendars(&account)
                            .map_err(core)?,
                    )?
                }
                (IntegrationId::MicrosoftGraph, None) => {
                    return Err(Error::Service {
                        code: "account_required",
                        message: "microsoft-graph calendars need --account".into(),
                    });
                }
                _ => {
                    return Err(Error::InvalidInput(
                        "system calendars are listed by `broscribe integrations status system-calendar`".into(),
                    ));
                }
            };
            (catalog, false)
        }
        IntegrationsCmd::SelectCalendars {
            integration,
            account,
            generation,
            selection_generation,
            spec_stdin,
        } => {
            require_spec(spec_stdin)?;
            let id: IntegrationId = integration.into();
            // Every argv check before stdin is read.
            let target = match (id, account) {
                (IntegrationId::Google, None) => None,
                (IntegrationId::Google, Some(_)) => {
                    return Err(Error::InvalidInput(
                        "google has one provider-wide account; --account does not apply".into(),
                    ));
                }
                (IntegrationId::MicrosoftGraph, Some(account)) => {
                    Some(account_ref(integration, &account)?)
                }
                (IntegrationId::MicrosoftGraph, None) => {
                    return Err(Error::Service {
                        code: "account_required",
                        message: "microsoft-graph selection needs --account".into(),
                    });
                }
                _ => {
                    return Err(Error::InvalidInput(
                        "system calendars are selected with `broscribe integrations setup system-calendar`".into(),
                    ));
                }
            };
            let spec: CalendarSelectionSpec = ctx.input_json("select-calendars")?;
            let result = match target {
                None => {
                    if !spec.targets.is_empty() {
                        return Err(Error::InvalidInput(
                            "google selections name calendar_ids, not targets".into(),
                        ));
                    }
                    let ids = calendar_selection(spec.calendar_ids, "calendar_ids")?;
                    let expected = GoogleCalendarStamp {
                        generation,
                        selection_generation,
                    };
                    to_value(
                        &service_for(ctx, id)?
                            .configure_google_calendars(expected, &ids)
                            .map_err(core)?,
                    )?
                }
                Some(account) => {
                    if !spec.calendar_ids.is_empty() {
                        return Err(Error::InvalidInput(
                            "microsoft-graph selections name routing targets, not calendar_ids"
                                .into(),
                        ));
                    }
                    for target in &spec.targets {
                        target.validate()?;
                    }
                    to_value(
                        &service_for(ctx, id)?
                            .configure_calendars(
                                &account,
                                generation,
                                selection_generation,
                                &spec.targets,
                            )
                            .map_err(core)?,
                    )?
                }
            };
            (result, true)
        }
        IntegrationsCmd::RemoveCalendar {
            integration,
            account,
            generation,
            selection_generation,
            spec_stdin,
        } => {
            require_spec(spec_stdin)?;
            let account = account_ref(integration, &account)?;
            let spec: CalendarRemovalSpec = ctx.input_json("remove-calendar")?;
            spec.target.validate()?;
            let result = service_for(ctx, IntegrationId::MicrosoftGraph)?
                .remove_calendar_target(&account, generation, selection_generation, &spec.target)
                .map_err(core)?;
            // The removal is committed; only the other routes' cached copy was lost.
            if let Some(error) = &result.copy_error {
                ctx.note(format!(
                    "The calendar was removed, but the other selected calendars' cached events could not be kept ({}): {} They are read again at the next sync.",
                    error.code, error.message
                ));
            }
            (to_value(&result)?, true)
        }
        IntegrationsCmd::SyncAccount {
            integration,
            account,
        } => {
            let account = account_ref(integration, &account)?;
            let now = Utc::now();
            (
                to_value(
                    &service_for(ctx, IntegrationId::MicrosoftGraph)?
                        .sync_account(&account, now - Duration::days(1), now + Duration::days(30))
                        .map_err(core)?,
                )?,
                false,
            )
        }
        IntegrationsCmd::GrantAccount {
            integration,
            account,
            generation,
            permission,
            attempt,
        } => {
            let account = account_ref(integration, &account)?;
            let permission = match permission.as_str() {
                "directory" => {
                    ctx.note("Read-only directory consent uses the configured Microsoft directory setting: User.ReadBasic.All searches organisation users and permits directory photos; People.Read accesses relevant people without photos. No permission is requested when directory access is configured off.");
                    broscribe_integrations::AccountPermission::Directory
                }
                "shared_calendars" => broscribe_integrations::AccountPermission::SharedCalendars,
                "microsoft365_groups" => {
                    ctx.note("Group.Read.All permits reading Microsoft 365 group calendars across the tenant and may require administrator approval.");
                    broscribe_integrations::AccountPermission::Microsoft365Groups
                }
                _ => {
                    return Err(Error::InvalidInput(
                        "--permission must be directory, shared_calendars or microsoft365_groups"
                            .into(),
                    ));
                }
            };
            let attempt = attempt_id(attempt)?;
            let id = IntegrationId::MicrosoftGraph;
            let service = service_for(ctx, id)?;
            let result = signing_in(ctx, id, &attempt, || {
                service
                    .grant_account_permission(&account, generation, permission, &attempt)
                    .map_err(core)
            })?;
            (with_attempt(to_value(&result)?, &attempt), true)
        }
        IntegrationsCmd::Setup {
            integration,
            client_id,
            client_secret_stdin,
            calendar_ids,
            clear_calendars,
            spec_stdin,
            clear_client_secret,
        } => {
            let id: IntegrationId = integration.into();
            if let Some(provider) = id.calendar_provider() {
                let oauth_flags = client_id.is_some() || client_secret_stdin || clear_client_secret;
                let value = setup_native(
                    ctx,
                    provider,
                    oauth_flags,
                    calendar_ids,
                    clear_calendars,
                    spec_stdin,
                )?;
                return super::daemon::emit(ctx, value);
            }
            if !calendar_ids.is_empty() || clear_calendars || spec_stdin {
                return Err(Error::InvalidInput(
                    "--calendar-id, --clear-calendars and --spec-stdin apply only to system-calendar"
                        .into(),
                ));
            }
            let client_id = match client_id.as_deref().map(str::trim) {
                Some("") => {
                    return Err(Error::InvalidInput("--client-id must not be empty".into()));
                }
                other => other.map(str::to_owned),
            };
            if clear_client_secret && id != IntegrationId::Google {
                return Err(Error::InvalidInput(
                    "only Google uses --clear-client-secret".into(),
                ));
            }
            // Before the client ID is saved: refused while signed in, so nothing changes then.
            let client_secret_removed = if clear_client_secret {
                Some(service(ctx)?.clear_client_secret(id).map_err(core)?)
            } else {
                None
            };
            if client_secret_stdin {
                if id != IntegrationId::Google {
                    return Err(Error::InvalidInput(
                        "only Google uses --client-secret-stdin".into(),
                    ));
                }
                let mut secret = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut secret)
                    .map_err(|e| Error::io("reading client secret from stdin", e))?;
                let secret = secret.trim();
                if secret.is_empty() {
                    return Err(Error::InvalidInput(
                        "client secret must not be empty".into(),
                    ));
                }
                service(ctx)?.set_client_secret(id, secret).map_err(core)?;
            }
            // The effective client before this edit decides whether the sign-in is retired.
            let previous = match client_id {
                Some(client_id) => Some(save_client_id(ctx.data_dir(), id, client_id)?),
                None => None,
            };
            // Saved from here on: later failures are reported beside the saved
            // settings, never undone.
            let (service, invalid) = open(ctx)?;
            let mut value = to_value(&broscribe_integrations::setup(
                id,
                &service_settings(ctx.data_dir(), id),
            ))?;
            if let Some(removed) = client_secret_removed {
                value["client_secret_removed"] = json!(removed);
            }
            if let Some(previous) = &previous {
                match service.retire_changed_client(id, previous) {
                    Ok(change) => value["client_change"] = to_value(&change)?,
                    Err(error) => {
                        let public = error.public();
                        ctx.note(format!(
                            "The client ID is saved, but retiring the previous sign-in failed ({}); tokens are never sent under another client ID.",
                            public.code
                        ));
                        value["client_change_error"] = to_value(&public)?;
                    }
                }
            }
            match service.status(id) {
                Ok(status) => value["configured"] = json!(status.configured),
                Err(error) => value["status_error"] = to_value(&error.public())?,
            }
            if let Some(message) = config_error(&invalid, id) {
                value["config_error"] = json!(message);
            }
            (
                value,
                previous.is_some() || client_secret_stdin || clear_client_secret,
            )
        }
        IntegrationsCmd::Status { integration } => {
            let value = match integration.map(IntegrationId::from) {
                Some(id) => match id.calendar_provider() {
                    Some(provider) => native_status(
                        ctx.data_dir(),
                        provider,
                        local_native_service(ctx)?.status(id).map_err(core)?,
                        true,
                    )?,
                    None => {
                        let (service, invalid) = open(ctx)?;
                        remote_status(&service.status(id).map_err(core)?, &invalid)?
                    }
                },
                None => {
                    let (service, invalid) = open(ctx)?;
                    let mut statuses = Vec::new();
                    for status in service.statuses().map_err(core)? {
                        statuses.push(match status.integration.calendar_provider() {
                            Some(provider) => {
                                native_status(ctx.data_dir(), provider, status, false)?
                            }
                            None => remote_status(&status, &invalid)?,
                        });
                    }
                    json!({ "integrations": statuses })
                }
            };
            (value, false)
        }
        IntegrationsCmd::Login {
            integration,
            attempt,
        } => {
            let id: IntegrationId = integration.into();
            if let Some(provider) = id.calendar_provider() {
                // A permission request, never a browser sign-in: no attempt.
                if attempt.attempt.is_some() {
                    return Err(Error::InvalidInput(
                        "system calendars connect through macOS permission; --attempt applies only to google and microsoft-graph sign-ins".into(),
                    ));
                }
                ctx.note(match provider {
                    CalendarProvider::EventKit => "Connecting system calendars through BroScribe Agent; answer the macOS calendar prompt if it appears.",
                });
                let value = super::daemon::native_calendar(
                    ctx.data_dir(),
                    provider,
                    SystemCalendarAction::Connect,
                )?;
                return super::daemon::emit(ctx, value);
            }
            if id == IntegrationId::MicrosoftGraph {
                return Err(Error::Service {
                    code: "account_required",
                    message:
                        "Use add-account or reconnect-account with an explicit account handle."
                            .into(),
                });
            }
            let attempt = attempt_id(attempt)?;
            let service = service_for(ctx, id)?;
            let status = signing_in(ctx, id, &attempt, || {
                service.login(id, &attempt).map_err(core)
            })?;
            (with_attempt(to_value(&status)?, &attempt), true)
        }
        IntegrationsCmd::CancelLogin {
            integration,
            attempt,
        } => {
            let id: IntegrationId = integration.into();
            let attempt = LoginAttemptId::parse(&attempt).map_err(core)?;
            // The journal needs no OAuth settings: cancelling always works.
            let status = local_native_service(ctx)?
                .cancel_login(id, &attempt)
                .map_err(core)?;
            (to_value(&status)?, false)
        }
        IntegrationsCmd::LoginAttempt {
            integration,
            attempt,
        } => {
            let id: IntegrationId = integration.into();
            let attempt = LoginAttemptId::parse(&attempt).map_err(core)?;
            let status = local_native_service(ctx)?
                .login_attempt(id, &attempt)
                .map_err(core)?;
            (to_value(&status)?, false)
        }
        IntegrationsCmd::Logout {
            integration,
            revoke_at_google,
        } => {
            let id: IntegrationId = integration.into();
            if revoke_at_google && id != IntegrationId::Google {
                return Err(Error::InvalidInput(
                    "--revoke-at-google applies only to google".into(),
                ));
            }
            if let Some(provider) = id.calendar_provider() {
                let status = match super::daemon::native_calendar(
                    ctx.data_dir(),
                    provider,
                    SystemCalendarAction::Disconnect,
                ) {
                    Ok(status) => status,
                    // No agent holds native-calendar state (none running, or one
                    // from before the feature): disconnect locally. This needs no
                    // permission and never reads calendars. An uncertain agent
                    // reply is reported, never repeated.
                    Err(e) if agent_unavailable(e.code()) => {
                        let service = local_native_service(ctx)?;
                        service.logout(id).map_err(core)?;
                        to_value(&service.status(id).map_err(core)?)?
                    }
                    Err(e) => return Err(e),
                };
                (
                    json!({"integration":id,"signed_out":true,"status":status}),
                    false,
                )
            } else if revoke_at_google {
                (logout_and_revoke(ctx, id)?, true)
            } else {
                let service = service(ctx)?;
                service.logout(id).map_err(core)?;
                (json!({"integration":id,"signed_out":true}), true)
            }
        }
        IntegrationsCmd::Sync { integration } => {
            let named = integration.map(IntegrationId::from);
            if let Some(provider) = named.and_then(IntegrationId::calendar_provider) {
                // Ask first: an agent from before this provider says so here.
                super::daemon::native_calendar(
                    ctx.data_dir(),
                    provider,
                    SystemCalendarAction::Status,
                )?;
            }
            if let Some(value) = super::daemon::optional_call(
                ctx.data_dir(),
                Call::IntegrationsSync {
                    integration: named.map(|id| id.to_string()),
                },
            )? {
                return super::daemon::emit(ctx, value);
            }
            if named.is_some_and(IntegrationId::is_native_calendar) {
                return Err(super::daemon::helper_required());
            }
            let (service, invalid) = open(ctx)?;
            if let Some(message) = named.and_then(|id| config_error(&invalid, id)) {
                return Err(Error::NotConfigured(message.clone()));
            }
            let ids: Vec<IntegrationId> = match named {
                Some(id) => vec![id],
                None => service
                    .statuses()
                    .map_err(core)?
                    .into_iter()
                    .filter(|s| {
                        s.state.status == broscribe_integrations::AccountStatus::Active
                            || !s.microsoft_accounts.is_empty()
                    })
                    .map(|s| s.integration)
                    .collect(),
            };
            let now = Utc::now();
            let mut results = Vec::new();
            for id in ids {
                if id.is_native_calendar() {
                    // Only the agent reads native calendars.
                    let error = super::daemon::helper_required();
                    results.push(SyncRow::ProviderFailed {
                        integration: id,
                        error: PublicError {
                            code: error.code().to_owned(),
                            message: error.to_string(),
                        },
                    });
                    continue;
                }
                if id == IntegrationId::MicrosoftGraph {
                    for account in service.list_accounts(Some(id)).map_err(core)? {
                        results.push(
                            match service.sync_account(
                                &account.account,
                                now - Duration::days(1),
                                now + Duration::days(30),
                            ) {
                                Ok(sync) => SyncRow::Account(sync),
                                Err(error) => SyncRow::account_failed(account.account, &error),
                            },
                        );
                    }
                    continue;
                }
                match service.sync_calendar(id, now - Duration::days(1), now + Duration::days(30)) {
                    Ok(calendar) => results.push(SyncRow::provider(id, calendar.events.len())),
                    Err(error) if integration.is_none() => {
                        results.push(SyncRow::provider_failed(id, &error));
                    }
                    Err(error) => return Err(core(error)),
                }
            }
            (json!({"synced":results}), false)
        }
    };
    if reload {
        notify_agent(ctx, &mut value);
    }
    super::daemon::emit(ctx, value)
}

/// The settings `setup` explains: `id`'s own table when it parses, else none.
fn service_settings(data_dir: &std::path::Path, id: IntegrationId) -> IntegrationSettings {
    IntegrationsConfig::load_settings(data_dir, id).unwrap_or_default()
}

/// A remote integration's status, with why its own settings are unusable.
fn remote_status(status: &IntegrationStatus, invalid: &InvalidTables) -> Result<Value> {
    let mut value = to_value(status)?;
    if let Some(message) = config_error(invalid, status.integration) {
        value["config_error"] = json!(message);
    }
    Ok(value)
}
