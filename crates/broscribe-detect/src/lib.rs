//! Meeting detection (design §9, plan M4 §5): which meeting app holds the
//! microphone, whether its window rule matches, and when a meeting starts
//! and ends.
//!
//! - [`rules`]: the `detect.toml` rules table (embedded default + user
//!   override in the data directory), helper → app mapping.
//! - [`observe`]: turns one poll of [`MicUsageMonitor`] and
//!   [`WindowInspector`] output into per-app [`Sighting`]s.
//! - [`machine`]: the pure start/end/cooldown state machine.
//! - [`Detector`]: the daemon-facing poller tying them together with a
//!   [`Clock`].
//! - [`Settings`]: the user's rules and policy overrides, loaded so that a
//!   part that does not load never weakens a policy.
//!
//! [`MicUsageMonitor`]: broscribe_platform::MicUsageMonitor
//! [`WindowInspector`]: broscribe_platform::WindowInspector

mod detector;
mod glob;
pub mod machine;
pub mod observe;
pub mod rules;
mod settings;

pub use detector::{Clock, ClockReading, Detector, FakeClock, Observation, SystemClock};
pub use glob::glob_match;
pub use machine::{Answer, DetectEvent, DetectState, DetectionMachine, EndReason, OpenDetection};
pub use observe::{Sighting, TitleState, observe};
pub use rules::{AppRule, DetectRules, HelperRule, Policy, PolicyOverrides, Timing};
pub use settings::Settings;
