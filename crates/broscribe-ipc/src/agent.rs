//! Agent identity/control metadata, independent of normal library inventory.

use std::borrow::Cow;

use broscribe_core::maintenance::Mode;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{AgentBuild, IpcError, PROTOCOL_VERSION};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentReadiness {
    NormalStarting,
    NormalOperational,
    MaintenanceControlReady,
}

/// Required protocol-7 ping identity. Producers borrow static/build metadata;
/// deserializing an observation owns it. Neither mode nor readiness has a default.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentPing<'a> {
    pub pid: u32,
    pub version: Cow<'a, str>,
    pub protocol: u32,
    pub build: Option<Cow<'a, AgentBuild>>,
    pub mode: Mode,
    pub readiness: AgentReadiness,
}

impl<'a> AgentPing<'a> {
    pub fn normal(build: Option<&'a AgentBuild>, starting: bool) -> Self {
        Self::new(
            Mode::Normal,
            if starting {
                AgentReadiness::NormalStarting
            } else {
                AgentReadiness::NormalOperational
            },
            build,
        )
    }

    pub fn maintenance(build: Option<&'a AgentBuild>) -> Self {
        Self::new(
            Mode::Maintenance,
            AgentReadiness::MaintenanceControlReady,
            build,
        )
    }

    fn new(mode: Mode, readiness: AgentReadiness, build: Option<&'a AgentBuild>) -> Self {
        Self {
            pid: std::process::id(),
            version: Cow::Borrowed(env!("CARGO_PKG_VERSION")),
            protocol: PROTOCOL_VERSION,
            build: build.map(Cow::Borrowed),
            mode,
            readiness,
        }
    }
}

impl AgentPing<'static> {
    /// Observe a peer without assuming an old/malformed reply is normal or ready.
    pub fn from_value(value: Value) -> Result<Self, IpcError> {
        let ping: Self = serde_json::from_value(value).map_err(|_| {
            IpcError::protocol("agent ping is missing or has invalid identity fields")
        })?;
        if ping.protocol != PROTOCOL_VERSION {
            return Err(IpcError::new(
                IpcError::UNSUPPORTED_VERSION,
                "agent ping uses an incompatible protocol",
            ));
        }
        if ping.pid == 0
            || !matches!(
                (ping.mode, ping.readiness),
                (
                    Mode::Normal,
                    AgentReadiness::NormalStarting | AgentReadiness::NormalOperational
                ) | (Mode::Maintenance, AgentReadiness::MaintenanceControlReady)
            )
        {
            return Err(IpcError::protocol(
                "agent ping has an inconsistent mode/readiness or missing process identity",
            ));
        }
        Ok(ping)
    }
}
