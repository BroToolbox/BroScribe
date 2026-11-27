//! Live-preview speaker identity (plan D, R1): provisional voices and
//! suggestions for the turns of a recording in progress, and how the user's
//! decisions on them reach the final meeting.
//!
//! - [`tail`]: a bounded reader of the session journal's closed, fsynced
//!   chunks (never the capture pipe), resampled to 16 kHz, at most
//!   `retain_s` per track; backlog beyond one window is skipped and reported.
//!   Chunks are placed on the live clock by `broscribe_capture::anchors`,
//!   the rule the daemon also anchors decisions with.
//! - [`tracker`]: per-window diarization and clean-speech embeddings, with
//!   provisional voices linked across windows only through unambiguous
//!   source-range overlap (the reprocess `link_speakers` rule), never a
//!   person threshold; suggestions through `resolve_voice`, from the
//!   library's samples and transient session-local samples of turns the user
//!   named.
//! - [`driver`]: the live-speakers worker's loop body (windows every hop,
//!   catch-up, session samples).
//! - [`finalize`]: the publication maps decisions through the journal
//!   timeline onto final segments; only fully supported lines become line
//!   attributions, the rest stay durable, reviewable records.
//!
//! The daemon's turn table (`broscribed::live_identity`) holds the turns,
//! revisions and decisions; it does not link these inference crates.
//! Nothing here writes voiceprints or names a speaker: the final clean-sample
//! confirmation remains the only way a voice is learned.

mod driver;
mod finalize;
mod replay;
mod tail;
mod tracker;

#[cfg(test)]
mod tests;

pub use broscribe_capture::anchors::{Placement, journal_point, placements};
pub use driver::{Driver, Step};
pub use finalize::{apply_session_decisions, map_point, map_range, recheck_meeting};
pub use replay::{ReplayOptions, ReplaySummary, replay};
pub use tail::{Polled, TrackTail};
pub use tracker::{
    Analysed, Evidence, EvidenceSource, LibraryEvidence, SessionPool, SpeakerModels,
    SuggestionPool, VoiceTracker, session_sample,
};

use broscribe_capture::journal::Track as CaptureTrack;
use broscribe_ipc::Track;

/// Live sample rate.
pub(crate) const RATE: u32 = broscribe_ipc::live::LIVE_RATE;

pub(crate) fn capture_track(track: Track) -> CaptureTrack {
    match track {
        Track::Mic => CaptureTrack::Mic,
        Track::System => CaptureTrack::System,
    }
}

pub(crate) fn seconds(sample: u64) -> f64 {
    sample as f64 / f64::from(RATE)
}

pub(crate) fn sample(seconds: f64) -> u64 {
    (seconds.max(0.0) * f64::from(RATE)).round() as u64
}
