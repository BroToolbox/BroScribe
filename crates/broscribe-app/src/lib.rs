//! The BroScribe desktop app (design §4, plan M3). A library so the view
//! models are testable with GPUI's `TestAppContext`; `main.rs` wires the
//! native platform.

pub mod daemon;
pub mod generation;
pub mod jobs;
pub mod launch;
pub mod learning;
pub mod menu;
pub mod notify;
pub mod readers;
pub mod state;
#[cfg(test)]
pub(crate) mod test_support;
pub mod ui;
pub mod ui_state;
pub mod views;
pub mod watch;
