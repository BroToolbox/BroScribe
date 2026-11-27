//! Durable two-track recording, common host-time alignment, and final encoding.
//!
//! Capture callbacks only enqueue audio; the journal writer is the sole durable
//! path. Preview consumers only see closed, fsynced chunks.

pub mod anchors;
pub mod disk;
pub mod finalize;
pub mod journal;
pub mod live;
pub mod resample;
pub mod session;
pub mod timeline;

pub use broscribe_core::{Error, Result};
pub use session::Recorder;
