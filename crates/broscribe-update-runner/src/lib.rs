//! The external update runner (SH23) and the update records that
//! `broscribe update` and the runner share.
//!
//! Where the platform can replace the running installation in place
//! ([`broscribe_platform::update::Replacement::InPlace`]: macOS bundles,
//! Linux folders) `broscribe update install` replaces it itself. Where files
//! in use cannot move ([`broscribe_platform::update::Replacement::AfterExit`]:
//! Windows), the command copies this runner out of the installation, hands
//! it the confirmed (durable) install admission in a [`Handoff`] and exits;
//! [`run_handoff`] waits for that command to exit, stops the agent in an
//! orderly way (the durable admission means nothing records, and a
//! restarted agent keeps refusing capture until the admission is released),
//! replaces the installation, starts the agent again and leaves a
//! [`RunnerReport`]. `update confirm` (with the new agent's health) or
//! `update rollback` ends the admission; after a failed step, `update
//! recover` ends it once the agent proves it runs the installation the
//! step left in place.
//!
//! Records live in the updater's state folder, `<data_dir>/updates`, as
//! user-only (0600) JSON files written atomically. They carry identities,
//! fences and process IDs, never feeds, keys or paths of user content.

use std::path::{Path, PathBuf};

use broscribe_core::{Error, Result, fsutil};
use broscribe_platform::update::PayloadIdentity;
use broscribe_updates::{Cancel, Transport};
use serde::{Deserialize, Serialize};

mod runner;

pub use runner::{
    Handoff, HandoffAction, RunnerDeps, RunnerOutcome, RunnerReport, RunnerWarning,
    StoppedAgentLease, run_handoff,
};

/// The runner's executable name, without the platform suffix.
pub const RUNNER_NAME: &str = "broscribe-update-runner";
/// Error code: the runner gave up waiting for a process or the agent.
pub const RUNNER_TIMEOUT: &str = "update_runner_timeout";

const INSTALL_RECORD: &str = "install-record.json";
const HANDOFF: &str = "runner-handoff.json";
const REPORT: &str = "runner-report.json";

/// The updater's state folder of a data directory.
pub fn state_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("updates")
}

/// Where `update install` copies the runner before starting it: outside
/// the installation, which the runner replaces.
pub fn runner_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("update-runner")
}

/// The runner's file name on this platform.
pub fn runner_file_name() -> String {
    format!("{RUNNER_NAME}{}", std::env::consts::EXE_SUFFIX)
}

/// The [`Transport`] of updater commands that never download (status,
/// install, confirm, rollback, recover): [`broscribe_updates::Updater`]
/// takes one, and these commands must keep working when the system trust
/// store is unavailable. A fetch through it is a programming error and
/// fails before any network or trust-store access.
#[derive(Clone, Copy, Debug, Default)]
pub struct Offline;

impl Transport for Offline {
    fn fetch(
        &self,
        _url: &str,
        _limit: u64,
        _sink: &mut dyn std::io::Write,
        _cancel: &Cancel,
    ) -> Result<u64> {
        Err(Error::Internal(
            "this update step never uses the network".into(),
        ))
    }
}

/// The replacement step in progress, written before its first irreversible
/// change: the admission fence it holds, the agent process that ran before
/// the step, the installation the agent must run once the step completed
/// and the one it runs when the step ended without replacing anything. The
/// admission is released only once the agent proves it is another process
/// running one of those two (whichever the updater, with nothing pending,
/// left in place) and still holds `fence`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRecord {
    pub schema: u32,
    pub fence: u64,
    pub agent_pid: u32,
    /// The admitted agent's absolute library. Older records lacked this field.
    #[serde(default)]
    pub agent_library: Option<PathBuf>,
    /// The installed update for an install, the restored installation for a
    /// rollback or recovery.
    pub expected: PayloadIdentity,
    /// The installation in place when the step ends without replacing
    /// anything: the one an install started from, the installed update a
    /// rollback would have replaced. A recovery only ever ends with
    /// `expected` restored, so it records `expected` here too. Kept from
    /// before the handoff, so a failed step whose updater state is gone
    /// (discarded stage, restored installation) can still be proven.
    pub unchanged: PayloadIdentity,
    /// The runner started for this step, while it may still run.
    pub runner_pid: Option<u32>,
}

impl InstallRecord {
    /// Schema 2 records `unchanged` (schema 1 did not).
    pub const SCHEMA: u32 = 2;

    pub fn load(data_dir: &Path) -> Result<Option<Self>> {
        load(
            &state_dir(data_dir).join(INSTALL_RECORD),
            Self::SCHEMA,
            |r: &Self| r.schema,
        )
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        save(&state_dir(data_dir).join(INSTALL_RECORD), self)
    }

    pub fn remove(data_dir: &Path) -> Result<()> {
        remove(&state_dir(data_dir).join(INSTALL_RECORD))
    }
}

pub(crate) fn handoff_path(data_dir: &Path) -> PathBuf {
    state_dir(data_dir).join(HANDOFF)
}

pub(crate) fn report_path(data_dir: &Path) -> PathBuf {
    state_dir(data_dir).join(REPORT)
}

/// Reads a record; `None` when absent. A record of another schema or an
/// unreadable one is an error: callers fail closed.
fn load<T: serde::de::DeserializeOwned>(
    path: &Path,
    schema: u32,
    schema_of: impl Fn(&T) -> u32,
) -> Result<Option<T>> {
    let name = path
        .file_name()
        .and_then(|part| part.to_str())
        .ok_or_else(|| Error::Internal("update records have a file name".into()))?;
    let dir = path
        .parent()
        .ok_or_else(|| Error::Internal("update records live in a folder".into()))?;
    let record = broscribe_updates::read_update_record(dir, name)?;
    if record
        .as_ref()
        .is_some_and(|record| schema_of(record) != schema)
    {
        return Err(Error::Schema("update record has an unknown schema".into()));
    }
    Ok(record)
}

fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::Internal("update records live in a folder".into()))?;
    broscribe_updates::ensure_update_directory(dir)?;
    let mut bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| Error::Internal(format!("serialising {}: {e}", path.display())))?;
    bytes.push(b'\n');
    fsutil::write_atomic_with_mode(path, &bytes, Some(0o600))
}

fn remove(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::io_at(path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(build: u64) -> PayloadIdentity {
        PayloadIdentity {
            product: "BroScribe".into(),
            app_id: "BroScribe".into(),
            version: "1.0.0".into(),
            build,
            target: "x86_64-pc-windows-msvc".into(),
        }
    }

    #[test]
    fn install_records_round_trip_and_refuse_other_schemas() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(InstallRecord::load(dir.path()).unwrap(), None);
        let record = InstallRecord {
            schema: InstallRecord::SCHEMA,
            fence: 4,
            agent_pid: 77,
            agent_library: Some(dir.path().join("library")),
            expected: id(2),
            unchanged: id(1),
            runner_pid: Some(5),
        };
        record.save(dir.path()).unwrap();
        assert_eq!(
            InstallRecord::load(dir.path()).unwrap(),
            Some(record.clone())
        );
        save(
            &state_dir(dir.path()).join(INSTALL_RECORD),
            &InstallRecord {
                schema: 9,
                ..record
            },
        )
        .unwrap();
        assert!(InstallRecord::load(dir.path()).is_err());
        InstallRecord::remove(dir.path()).unwrap();
        InstallRecord::remove(dir.path()).unwrap();
        assert_eq!(InstallRecord::load(dir.path()).unwrap(), None);
    }

    #[test]
    fn the_offline_transport_never_fetches() {
        let error = Offline
            .fetch(
                "https://example.org/feed",
                10,
                &mut Vec::new(),
                &Cancel::new(),
            )
            .unwrap_err();
        assert_eq!(error.code(), "internal");
    }
}
