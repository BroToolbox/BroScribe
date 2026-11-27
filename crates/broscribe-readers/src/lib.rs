//! Best-effort meeting-app readers. Rules are unverified structural hypotheses;
//! no rule's presence implies live-call verification. AX access stays in platform
//! crates; the app owns observations on GPUI's loop. Event times are recording
//! timeline seconds supplied by the capture owner; the only wall-clock value is
//! a call observation's snapshot start, which the daemon checks for age only.

pub mod alignment;
pub mod captions;
pub mod dump;
pub mod host;
pub mod rules;
pub mod selector;

use serde::{Deserialize, Serialize};

pub use captions::{CaptionTracker, folded_name, normalize_name};
pub use host::{
    HEARTBEAT_S, MAX_BACKOFF_S, MIN_INTERVAL_S, Polled, ReaderHost, ReaderSession, SessionPolicy,
    SnapshotJob, WallClock,
};
pub use rules::{
    CallContext, CallReading, CallRule, Extracted, ReaderRule, RuleEngine, builtin_rules,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RosterMember {
    pub name: String,
    pub email: Option<String>,
    pub is_self: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RosterEvent {
    pub observed_s: f64,
    pub members: Vec<RosterMember>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptionEvent {
    pub caption_id: String,
    pub revision: u64,
    pub speaker_name: String,
    pub is_self: bool,
    pub text: String,
    /// When AX was observed, mapped through the session's pause-aware timeline.
    pub observed_s: f64,
    /// Estimated speech interval, distinct from observation time.
    pub start_s: f64,
    pub end_s: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActiveSpeakerEvent {
    pub observed_s: f64,
    pub speaker_name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ReaderEvent {
    Roster(RosterEvent),
    Caption(CaptionEvent),
    ActiveSpeaker(ActiveSpeakerEvent),
}

/// What one snapshot showed about the call (design §9, plan H).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    /// A recognised in-call control surface (e.g. the leave/hang-up control).
    Active,
    /// After this reader incarnation saw that very call `Active`, in a
    /// complete snapshot with nothing in call: a recognised post-call
    /// surface of the same document and call code, or (rules opting in)
    /// every top-level window that showed the call verified destroyed.
    Ended,
    /// Nothing recognised, or the snapshot could not be trusted: never an end.
    Unknown,
}

/// The call evidence of one snapshot. `sequence` and
/// `snapshot_started_at_ms` (host wall clock, milliseconds since the Unix
/// epoch) are taken before the snapshot starts; `complete` is true only for
/// a successful, untruncated snapshot read with call selectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallObservation {
    pub sequence: u64,
    pub snapshot_started_at_ms: u64,
    pub state: CallState,
    pub complete: bool,
}

/// IPC-ready payload; the daemon must reject a stale session id or reader
/// generation, persist only during recording, and remove the journal when the
/// recording is discarded. Every completed snapshot sends one, even when its
/// events were all deduplicated away (`events` empty): the call observation
/// is the heartbeat.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReaderBatch {
    pub session_id: String,
    pub app: String,
    pub pid: u32,
    /// The daemon-issued generation of this reader incarnation (StartReader).
    pub reader_generation: u64,
    pub call_observation: CallObservation,
    pub events: Vec<ReaderEvent>,
    /// Diagnostics of the reader that took this snapshot. Never call
    /// evidence: only `call_observation` is.
    #[serde(default)]
    pub health: ReaderHealth,
}

/// Why a reader's latest observation is incomplete.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncompleteReason {
    /// The snapshot reached its node cap.
    NodeCap,
    /// The snapshot reached its depth cap.
    DepthCap,
    /// The snapshot ran out of time.
    TimeBudget,
    /// An element, its children or an attribute could not be read.
    ReadFailed,
    /// A text attribute was longer than the platform reader keeps.
    Clipped,
    /// The app did not answer this time; the reader retries with backoff.
    Transient,
    /// The snapshot failed and the reader stopped.
    Failed,
    /// The incarnation's bounded window/document identity registry is
    /// full: windows and documents it has not seen yet stay unidentified,
    /// so they can never bind or end a call.
    IdentityCap,
    /// The rule has no call selectors: it never reports call state.
    NoCallRule,
}

/// What a reader did about `AXManualAccessibility` for its app.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManualAccessibility {
    /// The rule does not use it.
    #[default]
    NotApplicable,
    /// The app already had it on; BroScribe leaves it as it is.
    AlreadyOn,
    /// BroScribe turned it on and turns it off when the last reader stops.
    EnabledByBroscribe,
    /// The app does not offer it.
    Unsupported,
    /// Reading or setting it failed; the reader runs without it.
    Failed,
}

/// Why a reader host could not start the reader incarnation the daemon
/// announced (NA35). Content-free: the stage that failed, never an error
/// message, and never inferred from one. Diagnostics only, never call
/// evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReaderStartRefusal {
    /// The platform reported this host's accessibility access as not
    /// trusted (`AppUiReader::is_trusted`).
    Untrusted,
    /// This host has no reader rule for the app.
    NoRule,
    /// The start could not be prepared before asking for an incarnation:
    /// the built-in rule set could not be loaded.
    Prepare,
    /// `reader.attach` issued no incarnation as far as the host knows: it
    /// was not sent, the daemon answered another error than a stale one,
    /// or its outcome stayed unknown. Reported for the generation the host
    /// was told, never a guessed issued one.
    Attach,
    /// The issued incarnation could not begin observing the app: the reader
    /// host refused to start it for another reason than permission or rule.
    Observe,
}

/// Reader diagnostics for status displays (RT23), never call evidence.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReaderHealth {
    /// Why the latest observation is incomplete; `None` when it is whole.
    pub incomplete: Option<IncompleteReason>,
    /// Failed snapshots in a row, the latest included.
    pub consecutive_failures: u32,
    /// Recording timeline second before which no snapshot is retried.
    pub retry_after_s: Option<f64>,
    pub manual_accessibility: ManualAccessibility,
    /// Web documents read in the latest snapshot.
    pub scoped_documents: u32,
    /// Web documents of other origins whose contents were not read.
    pub skipped_documents: u32,
}
