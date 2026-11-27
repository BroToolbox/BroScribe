//! The `broscribed` protocol (design §3.1, plan M4 §2, §12–§14).
//!
//! - [`protocol`]: newline-delimited JSON between clients (CLI, app, MCP)
//!   and the daemon: requests `{"v":8,"id","method","params"}`, responses
//!   `{"v":8,"id","ok","result"|"error"}` and events `{"v":8,"event",…}`.
//!   Every request carries a request ID; a client that lost the response of
//!   a mutating request asks `request.status` for that ID instead of running
//!   the operation again (plan §14.5).
//! - [`live`]: the binary PCM frames the daemon writes to
//!   `broscribe-worker live` on stdin and the JSON lines it answers with.
//! - [`capture`]: the capture-engine boundary the daemon drives.
//! - [`jobs`]: queue job kinds and inputs the daemon creates for recordings.
//!
//! Transport (private Unix socket or current-user named pipe, framing, peer
//! check) lives in `broscribe_platform_native::ipc`; this crate holds data
//! types and the bounded persisted-admission reader, never platform services.

pub mod agent;
pub mod capture;
pub mod install_admission;
pub mod jobs;
pub mod live;
pub mod protocol;
pub mod recording_words;

pub use agent::{AgentPing, AgentReadiness};
pub use capture::{
    CaptureEngine, CaptureEvents, CaptureHealth, CaptureNotice, CaptureOutputs, CaptureWarning,
    ChunkCutoff, DiskHealth, DiskLevel, LiveSink, OrphanFailure, OrphanRecovery, SessionConfig,
    SessionHandle, SessionStatus, StoppedSession, TrackHealth, TrackState,
};
pub use jobs::{RecordJobInput, WorkerLine};
pub use live::{
    Frame, LiveEvent, LiveFrame, LiveSegment, LiveSpeaker, Track, read_frame, write_frame,
};
pub use protocol::{
    AgentBuild, AutoStopApp, AutoStopPending, AutoStopReason, AutoStopStatus, Call, CaptureAccess,
    CapturePermission, DaemonState, Event, IndexState, InstallHold, IpcError, JobProgress,
    JobsState, JournalIssue, JournalResolution, NotificationButton, NotificationRequest,
    ParseError, ParticipantAction, PreviewState, ReaderDegraded, ReaderStatus,
    RecordAutoStopParams, RecordStartParams, RecordingState, RecordingStatus, Request, RequestId,
    RequestState, Response, SAVED_RECORDINGS_MAX, SavedRecording, ServerMessage, SetupLevel,
    StoppedRecording, StoppedRecordingState, SystemCalendarAction, WorkerProblem,
};

/// Protocol version carried as `"v"` in every message.
/// Version 2: every recording command names its session (fail closed
/// against version-1 clients and agents, which sent them unfenced) and
/// reader hosts take a daemon-issued incarnation (`reader.attach`).
/// Version 3: typed preview/identity/reader/capture-health/stopped-job
/// states, `record.retry` and install admission; an older agent or client
/// would misread these mandatory shapes, so both sides refuse the other.
/// Version 4: `WorkerProblem::ModelMissing` (a live worker whose model is
/// not installed, terminal) and `RecordingState.reader_health`; an older
/// peer cannot parse the new problem, so both sides refuse the other.
/// Version 5: typed reader start refusals (`reader.refused`,
/// `ReaderStatus::Refused`), the reader's identity-registry cap
/// (`IncompleteReason::IdentityCap`) and Google calendar authority and
/// events stamped with the account and calendar-selection generation
/// pair; an older peer would misread or drop these shapes, so both sides
/// refuse the other.
/// Version 6: the live-preview events (`live_transcript`, `live_identity`,
/// `preview`, `preview_gap`) name the recording whose worker produced
/// them, so a stopped recording's late output is never shown as the next
/// one's; a version-5 peer would send or accept them unscoped, so both
/// sides refuse the other.
/// Version 7: ping requires effective mode/readiness. A maintenance responder
/// serves only control metadata/shutdown and never fabricates normal state.
/// Older peers cannot prove this boundary and are refused in both directions.
/// Version 8 adds the restarted-agent stop hold and confirmed legacy journal resolution.
pub const PROTOCOL_VERSION: u32 = 8;

/// Longest accepted protocol line, newline included. Longer lines are a
/// protocol error and close the connection.
pub const MAX_LINE_BYTES: usize = 1 << 20;
