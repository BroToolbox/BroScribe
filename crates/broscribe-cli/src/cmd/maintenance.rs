//! Offline, nonmutating preflight before changing the persisted request.
use std::path::Path;
use std::time::Duration;

use broscribe_core::maintenance::{Mode, ModeChange, ModeLease};
use broscribe_core::{Error, Result};
use broscribe_ipc::{AgentPing, Call};
use broscribe_platform_native::ipc::{CallError, Client, Endpoint};
use broscribe_update_runner::{Handoff, InstallRecord};
use broscribe_updates::{UpdateStatus, pending_replacement};
use serde_json::{Value, json};

use crate::Ctx;
use crate::cli::{
    AppCmd, Command, DaemonCmd, IdentityCmd, MaintenanceCmd, QueueCmd, SwitchArg, TemplatesCmd,
};

pub(crate) fn denied() -> Error {
    Error::Service {
        code: broscribe_core::maintenance::ERROR_CODE,
        message: "maintenance is enabled; close clients, disable maintenance offline, then explicitly relaunch".into(),
    }
}

/// Deliberately independent of read-only hints: index reads migrate schemas.
pub(crate) fn allowed(command: &Command) -> bool {
    match command {
        Command::Version
        | Command::Show { .. }
        | Command::Transcript { action: None, .. }
        | Command::Identity {
            action: IdentityCmd::Status,
        }
        | Command::Queue {
            action: QueueCmd::Status,
        }
        | Command::Templates {
            action:
                TemplatesCmd::List { .. } | TemplatesCmd::Show { .. } | TemplatesCmd::Result { .. },
        }
        | Command::Mcp { action: None }
        | Command::Daemon {
            action:
                DaemonCmd::Status
                | DaemonCmd::Start
                | DaemonCmd::Stop
                | DaemonCmd::Install
                | DaemonCmd::Uninstall { .. },
        } => true,
        Command::App { action } => matches!(
            action,
            AppCmd::About
                | AppCmd::ReadingSize { size: None }
                | AppCmd::OpenAtLogin {
                    state: SwitchArg::Status
                }
                | AppCmd::LiveIdentity {
                    state: SwitchArg::Status
                }
                | AppCmd::UncalibratedMeSuggestions {
                    state: SwitchArg::Status
                }
        ),
        Command::Export(args) => {
            !args.spec_stdin && args.out.as_ref().is_none_or(|path| path == Path::new("-"))
        }
        Command::Maintenance { .. } => true,
        Command::Daemon {
            action: DaemonCmd::Watch | DaemonCmd::Notice { .. },
        }
        | Command::Uninstall(_)
        | Command::Diagnostics
        | Command::Import(_)
        | Command::Search(_)
        | Command::Meetings(_)
        | Command::Meeting { .. }
        | Command::Transcript {
            action: Some(_), ..
        }
        | Command::People { .. }
        | Command::Speakers { .. }
        | Command::Tag { .. }
        | Command::Title { .. }
        | Command::Delete { .. }
        | Command::Identity { .. }
        | Command::Models { .. }
        | Command::Index { .. }
        | Command::Setup { .. }
        | Command::Settings { .. }
        | Command::Doctor
        | Command::Library { .. }
        | Command::Record { .. }
        | Command::Detect { .. }
        | Command::Readers { .. }
        | Command::Integrations { .. }
        | Command::AudioCheck { .. }
        | Command::Mcp { action: Some(_) }
        | Command::Agents { .. }
        | Command::Update { .. }
        | Command::Queue { .. }
        | Command::Audio { .. }
        | Command::Retention { .. }
        | Command::Enrich { .. }
        | Command::Templates { .. }
        | Command::Actions { .. }
        | Command::Decisions(_)
        | Command::Providers { .. }
        | Command::Reprocess(_)
        | Command::Bench { .. } => false,
    }
}

/// A bounded authenticated observation; never starts, repairs or removes an endpoint.
pub(crate) fn observation(data_dir: &Path) -> Value {
    let mut client = match Client::connect_daemon(&Endpoint::new(data_dir), Duration::from_secs(1))
    {
        Ok(Some(client)) => client,
        Ok(None) => return json!({"observation":"absent"}),
        Err(error) => return json!({"observation":"unreachable", "message":error.to_string()}),
    };
    match client.call(Call::Ping) {
        Ok(value) => {
            let legacy = value["protocol"]
                .as_u64()
                .is_some_and(|protocol| protocol != u64::from(broscribe_ipc::PROTOCOL_VERSION))
                || (value["pid"].is_u64()
                    && value["version"].is_string()
                    && (value.get("mode").is_none() || value.get("readiness").is_none()));
            match AgentPing::from_value(value) {
                Ok(ping) => json!({"observation":"verified", "ping":ping}),
                Err(error) => {
                    json!({"observation":if legacy { "legacy_incompatible" } else { "unreadable" }, "message":error.to_string()})
                }
            }
        }
        Err(CallError::Remote(error))
            if error.code == broscribe_ipc::IpcError::UNSUPPORTED_VERSION =>
        {
            json!({"observation":"legacy_incompatible", "message":error.to_string()})
        }
        Err(CallError::NotSent(error))
            if error
                .get_ref()
                .and_then(|e| e.downcast_ref::<broscribe_ipc::IpcError>())
                .is_some_and(|e| e.code == broscribe_ipc::IpcError::UNSUPPORTED_VERSION) =>
        {
            json!({"observation":"legacy_incompatible", "message":error.to_string()})
        }
        Err(error) => json!({"observation":"unreachable", "message":error.to_string()}),
    }
}

pub(super) fn run(ctx: &mut Ctx<'_>, action: MaintenanceCmd) -> Result<()> {
    if matches!(action, MaintenanceCmd::Status) {
        let lease = ModeLease::acquire(ctx.data_dir(), ModeLease::DEFAULT_WAIT)?;
        let value = json!({"configured_mode":lease.mode(), "agent":observation(ctx.data_dir()), "inventory":"unassessed"});
        return super::daemon::emit(ctx, value);
    }
    let change = ModeChange::acquire(ctx.data_dir())?;
    // Connecting alone proves presence, including an incompatible legacy agent.
    // Any connection ambiguity refuses instead of treating it as absence.
    match Client::connect_daemon(&Endpoint::new(change.data_dir()), Duration::from_secs(1)) {
        Ok(None) => {}
        Ok(Some(_)) => {
            return Err(busy(
                "an agent is listening; stop it before changing maintenance",
            ));
        }
        Err(error) => {
            return Err(busy(&format!(
                "agent absence could not be established: {error}"
            )));
        }
    }
    let data_dir = change.data_dir();
    if matches!(
        pending_replacement(&broscribe_update_runner::state_dir(data_dir))?,
        UpdateStatus::Interrupted { .. } | UpdateStatus::AwaitingConfirmation { .. }
    ) || InstallRecord::load(data_dir)?.is_some()
        || Handoff::load(data_dir)?.is_some()
        || broscribe_ipc::install_admission::Persisted::load(data_dir)?
            .is_some_and(|record| record.has_durable_hold())
    {
        return Err(busy(
            "an update replacement or durable admission remains pending",
        ));
    }
    let mode = if matches!(action, MaintenanceCmd::Enable) {
        Mode::Maintenance
    } else {
        Mode::Normal
    };
    change.set(mode)?;
    super::daemon::emit(
        ctx,
        json!({"configured_mode":mode, "inventory":"unassessed"}),
    )
}

fn busy(message: &str) -> Error {
    Error::Service {
        code: broscribe_core::maintenance::BUSY_CODE,
        message: message.into(),
    }
}
