//! `broscribe-update-runner --data-dir <DIR>`: runs the handoff that
//! `broscribe update install` left in DIR's update state, then exits.
//! Started only by that command, from a copy outside the installation it
//! replaces (see the library documentation).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use broscribe_core::{Error, Result};
use broscribe_platform_native::NativePlatform;
use broscribe_platform_native::ipc::{Client, Endpoint};
use broscribe_update_runner::{Handoff, RunnerDeps, RunnerOutcome, run_handoff};

/// Longest wait for the command to exit and for the agent to stop.
const TIMEOUT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(250);
/// One connection attempt when checking whether the agent still answers.
const PROBE: Duration = Duration::from_secs(2);

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<bool> {
    let mut args = std::env::args_os().skip(1);
    let data_dir = match (args.next(), args.next(), args.next()) {
        (Some(flag), Some(dir), None) if flag == "--data-dir" => PathBuf::from(dir),
        _ => {
            return Err(Error::InvalidInput(
                "usage: broscribe-update-runner --data-dir <DIR>".into(),
            ));
        }
    };
    let data_dir = std::path::absolute(&data_dir)
        .map_err(|error| Error::io("resolving update data directory", error))?;
    let handoff = Handoff::load(&data_dir)?
        .ok_or_else(|| Error::NotFound("no update was handed over".into()))?;
    if handoff.data_dir != data_dir {
        return Err(Error::InvalidInput(
            "the handoff belongs to another data folder".into(),
        ));
    }
    let platform = NativePlatform::updater_for(handoff.executable.clone());
    let autostart = NativePlatform::autostart(handoff.executable.clone());
    let probe = NativePlatform::process_probe();
    let agent_listening = |dir: &std::path::Path| -> Result<bool> {
        Client::connect_daemon(&Endpoint::new(dir), PROBE)
            .map(|client| client.is_some())
            .map_err(|e| Error::io("checking for the agent", e))
    };
    let report = run_handoff(
        &handoff,
        &RunnerDeps {
            platform: platform.as_ref(),
            autostart: autostart.as_ref(),
            probe: probe.as_ref(),
            agent_listening: &agent_listening,
            timeout: TIMEOUT,
            poll: POLL,
        },
    )?;
    if let Some(error) = &report.agent_start_error {
        eprintln!("error: the agent did not start again: {error}");
    }
    // A finished rollback or recovery succeeded as much as an install did.
    Ok(!matches!(report.outcome, RunnerOutcome::Failed { .. })
        && report.agent_start_error.is_none())
}
