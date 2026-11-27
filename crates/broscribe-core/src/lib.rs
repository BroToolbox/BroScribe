//! BroScribe core: the library file schema, layout and files, the
//! single-writer lock, configuration, retention policy, the `meeting.md`
//! renderer and the one transcript projection every front end reads.

pub mod calendar;
pub mod config;
pub mod detected_apps;
pub mod display_units;
pub mod error;
pub mod export;
pub mod fsutil;
pub mod identity;
pub mod ids;
pub mod import_files;
pub mod index;
pub mod learning;
pub mod library;
pub mod lifecycle;
pub mod live_identity;
pub mod lock;
pub mod maintenance;
pub mod names;
pub mod ops;
pub mod paths;
pub mod process;
pub mod provider_address;
pub mod queue;
pub mod render;
pub mod retention;
pub mod schema;
pub mod secret;
pub mod templates;
pub mod transcript;

pub use error::{Error, Result};
pub use ids::{MeetingId, PersonId, SpeakerId};
pub use library::{Library, WriteTx};
