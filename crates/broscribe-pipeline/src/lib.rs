//! Processing pipeline: single-track analysis, import, manifest batches,
//! calibration and re-embedding.
//!
//! Inference (decoding, VAD, ASR, diarization, embeddings) always runs outside
//! the library lock; results are committed one meeting at a time inside one
//! [`Library::write`] or a queue lease's commit.

use broscribe_core::Library;
use broscribe_core::config::{Config, Thresholds};
use broscribe_core::process::ProcessProbe;
use broscribe_models::ExecutionContext;
use broscribe_platform::DecoderChain;

pub mod analyze;
pub mod audio;
pub mod batch;
pub mod bench;
pub mod calibrate;
mod caption_replay;
pub mod confirm;
pub mod enrich;
mod finish;
pub mod hints;
pub mod import;
pub mod live_identity;
pub mod manifest;
pub mod multitrack;
pub mod ranges;
pub mod record;
pub mod reembed;
pub mod reidentify;
pub mod reprocess;
pub mod reresolve;
pub mod retention;
mod setup_final;
pub mod template;
#[cfg(test)]
mod testutil;
pub mod vectors;
pub mod worker;

pub use analyze::{Analysis, AnalyzeOptions, Turn, analyze};
pub use batch::{BatchOptions, BatchSummary, import_manifest};
pub use calibrate::{CalibrateOptions, CalibrationReport, calibrate};
pub use confirm::confirmation_samples;
pub use import::{FolderReport, ImportOptions, ImportOutcome, import_file, import_folder};
pub use reembed::{ReembedReport, reembed};

/// Invocation-owned progress transport. The pipeline never chooses a process
/// stream; callers may collect messages or forward them to their own UI.
pub trait ProgressSink: Send + Sync {
    fn say(&self, line: std::fmt::Arguments<'_>);
}
/// What the pipeline works on: the library, the user's configuration, the
/// decoders, the process probe used for queue leases and the model execution
/// context (process-tree containment and TLS trust of provider work).
#[derive(Clone, Copy)]
pub struct Env<'a> {
    pub lib: &'a Library,
    pub config: &'a Config,
    pub decoders: &'a DecoderChain,
    pub probe: &'a dyn ProcessProbe,
    /// Every LLM/embedding provider, model list and agent-CLI child the
    /// pipeline opens runs under this context.
    pub execution: &'a ExecutionContext,
    /// The caller's progress transport; `None` disables notices.
    pub progress: Option<&'a dyn ProgressSink>,
}

impl Env<'_> {
    /// Sends one notice through the invocation's transport when enabled.
    pub fn say(&self, line: impl std::fmt::Display) {
        if let Some(progress) = self.progress {
            progress.say(format_args!("{line}"));
        }
    }

    /// Naming thresholds of the embedding model `model`, if calibrated or set.
    pub fn thresholds(&self, model: &str) -> Option<&Thresholds> {
        self.config.thresholds(model)
    }
}
