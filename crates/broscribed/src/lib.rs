//! Capture-independent daemon, local IPC and isolated worker supervision.
mod admission;
pub mod audio_check;
mod autostop;
pub mod bus;
pub mod daemon;
mod detection;
pub mod integrations;
pub mod live;
pub mod live_identity;
mod readers;
mod requests;
#[cfg(test)]
#[path = "../build_support/resource_compiler.rs"]
mod resource_compiler_discovery;
mod sample;
pub mod scheduler;
pub mod server;
mod setup;
pub mod shutdown;
pub mod worker_cmd;
mod writes;
pub use daemon::{ClientConnection, Daemon, DaemonConfig};
pub use worker_cmd::WorkerCommand;
