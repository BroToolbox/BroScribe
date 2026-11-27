mod dispatcher;
mod display;
mod platform;
mod window;

pub use dispatcher::*;
pub(crate) use display::*;
pub(crate) use platform::*;
pub(crate) use window::*;

#[cfg(any(test, feature = "test-support"))]
pub use platform::{TestScreenCaptureSource, TestScreenCaptureStream};
// BroScribe local modification: see window.rs and ../../../UPSTREAM.toml.
#[cfg(any(test, feature = "test-support"))]
pub use window::TestWindowRequests;
