//! Client ↔ daemon messages, one JSON object per line.
//!
//! - Request: `{"v":8,"id":"r-…","method":"record.start","params":{…}}`
//!   (`params` absent for methods without parameters).
//! - Response: `{"v":8,"id":"r-…","ok":true,"result":…}` or
//!   `{"v":8,"id":"r-…","ok":false,"error":{"code","message"}}`.
//! - Event (only on a connection that called `subscribe`):
//!   `{"v":8,"event":"state",…}`. Live-preview events name the recording
//!   whose worker produced them (`session_id`); consumers refuse another
//!   session's before applying or relaying them.
//!
//! Every command on a recording (stop, pause, resume, language, bookmark,
//! note, tags, process now) names the session it was meant for: the daemon
//! runs it only while that session is the active one, under its session
//! mutex, and answers `stale_session` otherwise. Each side refuses a line of
//! another version (`unsupported_version`) instead of running an unfenced
//! command or misreading a typed state.
//!
//! Request IDs are chosen by the client. The daemon remembers the outcome of
//! recent mutating requests by ID: repeating a mutating request with the same
//! ID returns the first outcome instead of running it twice, and
//! `request.status` reports it (`unknown` when the daemon never saw it or has
//! forgotten it, e.g. after a restart). A client whose connection broke after
//! sending a mutating request reconciles through `request.status` and never
//! re-runs the operation locally on its own (plan §14.5).

use std::fmt;

use broscribe_core::Error;
use chrono::{DateTime, FixedOffset, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::PROTOCOL_VERSION;
use crate::live::{LiveSegment, Track};

// ---- request IDs ----

const REQUEST_ID_MAX: usize = 64;

/// A client-chosen request ID: 1–64 characters of `A-Z a-z 0-9 . _ : -`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RequestId(String);

impl RequestId {
    /// `r-` followed by 16 random hex digits.
    pub fn random() -> Self {
        Self(format!("r-{:016x}", rand::random::<u64>()))
    }

    pub fn parse(s: &str) -> Result<Self, IpcError> {
        let ok = !s.is_empty()
            && s.len() <= REQUEST_ID_MAX
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
        if ok {
            Ok(Self(s.to_owned()))
        } else {
            Err(IpcError::new(
                IpcError::INVALID_REQUEST,
                format!("invalid request id {s:?}"),
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for RequestId {
    type Error = IpcError;
    fn try_from(s: String) -> Result<Self, IpcError> {
        Self::parse(&s)
    }
}

impl From<RequestId> for String {
    fn from(id: RequestId) -> String {
        id.0
    }
}

// ---- errors ----

/// `{"code","message"}`: the stable codes of [`broscribe_core::Error`] plus
/// the protocol's own ([`IpcError::PROTOCOL`] …).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcError {
    pub code: String,
    pub message: String,
}

impl IpcError {
    /// Malformed line (not JSON, not an object, line too long).
    pub const PROTOCOL: &'static str = "protocol";
    /// Well-formed JSON that is not a valid request (unknown method, bad params).
    pub const INVALID_REQUEST: &'static str = "invalid_request";
    pub const UNSUPPORTED_VERSION: &'static str = "unsupported_version";
    /// The daemon is stopping and takes no new work.
    pub const SHUTTING_DOWN: &'static str = "shutting_down";
    /// A mutating request with this ID is still running.
    pub const IN_PROGRESS: &'static str = "in_progress";
    /// Capture of any kind is in progress or being admitted.
    pub const BUSY: &'static str = "busy";
    pub const DISK_LOW: &'static str = "disk_low";
    pub const MICROPHONE_PRIVACY: &'static str = "microphone_privacy";
    /// An install admission was refused: a recording is capturing, paused,
    /// in its stop grace or stopped and not saved yet.
    pub const RECORDING_ACTIVE: &'static str = "recording_active";
    /// A capture start was refused: an install admission is held.
    pub const INSTALLING: &'static str = "update_installing";
    /// The named install admission is not the one held (released, lost
    /// with its connection, or superseded).
    pub const ADMISSION_LOST: &'static str = "update_admission_lost";
    /// The agent's install admission record cannot be read: capture stays
    /// refused and no admission is granted, confirmed or released until the
    /// record is repaired (its fences are unknown).
    pub const ADMISSION_DAMAGED: &'static str = "update_admission_damaged";
    /// `record.retry` of a recording whose processing has not failed.
    pub const NOT_RETRYABLE: &'static str = "not_retryable";
    /// A reader call (`reader_events`, `reader.attach`, `reader.refused`)
    /// names a reader incarnation that is no longer the session's current
    /// one, or the session has no reader now.
    pub const STALE_READER: &'static str = "stale_reader";

    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn protocol(message: impl Into<String>) -> Self {
        Self::new(Self::PROTOCOL, message)
    }

    pub fn from_core(e: &Error) -> Self {
        Self::new(e.code(), e.to_string())
    }

    /// The core error with the same code, for callers that report errors
    /// through [`broscribe_core::Error`]; protocol codes become `internal`.
    pub fn into_core(self) -> Error {
        let m = self.message;
        match self.code.as_str() {
            "daemon_unavailable" => Error::DaemonUnavailable(m),
            "not_configured" => Error::NotConfigured(m),
            "consent_required" => Error::ConsentRequired(m),
            "reauth_required" => Error::ReauthRequired(m),
            "mcp_disabled" => Error::McpDisabled(m),
            "library_busy" => Error::LibraryBusy(m),
            "not_found" => Error::NotFound(m),
            "already_exists" => Error::AlreadyExists(m),
            "invalid_input" | Self::INVALID_REQUEST => Error::InvalidInput(m),
            "invalid_manifest" => Error::InvalidManifest(m),
            "unsupported" | Self::UNSUPPORTED_VERSION => Error::Unsupported(m),
            "model_missing" => Error::ModelMissing(m),
            "schema" => Error::Schema(m),
            "decode" => Error::Decode(m),
            "inference" => Error::Inference(m),
            "lease_lost" => Error::LeaseLost(m),
            broscribe_core::maintenance::ERROR_CODE => Error::Service {
                code: broscribe_core::maintenance::ERROR_CODE,
                message: m,
            },
            broscribe_core::maintenance::BUSY_CODE => Error::Service {
                code: broscribe_core::maintenance::BUSY_CODE,
                message: m,
            },
            broscribe_core::secret::TIMEOUT_CODE => Error::Service {
                code: broscribe_core::secret::TIMEOUT_CODE,
                message: m,
            },
            "io" => Error::io("daemon", std::io::Error::other(m)),
            _ => Error::Internal(m),
        }
    }
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for IpcError {}

impl From<Error> for IpcError {
    fn from(e: Error) -> Self {
        Self::from_core(&e)
    }
}

// ---- requests ----

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantAction {
    Confirm,
    Remove,
}

/// Parameters of `record.start`; every field is optional.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordStartParams {
    /// Meeting title, when known (detected window title or user input).
    pub title: Option<String>,
    /// The meeting app (bundle ID) that triggered the recording, if any.
    pub app: Option<String>,
    /// BCP 47 tag of the first language span; the configured default when absent.
    pub language: Option<String>,
    /// Audio setup override (`headphones`, `speakers_mic` …); detected when absent.
    pub setup: Option<String>,
    pub tags: Vec<String>,
    /// `title` is an observed window title, not one the user typed; a matching
    /// calendar event may replace it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub title_detected: bool,
}

/// Explicit system-calendar actions; only Connect may request OS permission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SystemCalendarAction {
    Status,
    Connect,
    Configure { calendar_ids: Vec<String> },
    Disconnect,
}

/// A request's method and parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Call {
    /// Liveness check; result `{"pid","version","protocol","build"}`
    /// (`build`: [`AgentBuild`] or null when unknown).
    #[serde(rename = "ping")]
    Ping,
    /// Result: [`DaemonState`].
    #[serde(rename = "status")]
    Status,
    /// Result: [`RequestState`] of an earlier request.
    #[serde(rename = "request.status")]
    RequestStatus { request_id: RequestId },
    /// Turns the connection into an event stream after the response.
    #[serde(rename = "subscribe")]
    Subscribe,
    /// Stops the daemon (an active recording is stopped and queued first).
    #[serde(rename = "daemon.stop")]
    Shutdown,
    /// Stops only while capture and worker admission are idle.
    #[serde(rename = "daemon.stop_if_idle")]
    ShutdownIfIdle,
    /// Connection-bound hold for unloading a restarted login agent. Fences
    /// capture and job starts while leaving durable queued work for next launch.
    #[serde(rename = "daemon.stop_acquire")]
    ShutdownAcquire,
    /// Asks the running app to show its main window (a second app launch
    /// hands off here); result `{"subscribers": n}`, the number of
    /// subscribers the request reached.
    #[serde(rename = "app.show")]
    AppShow,

    #[serde(rename = "record.start")]
    RecordStart(RecordStartParams),
    /// Stops `session_id` and queues its final job; another session (or
    /// none) is `stale_session` and nothing stops.
    #[serde(rename = "record.stop")]
    RecordStop { session_id: String },
    /// Fenced like [`Call::RecordStop`].
    #[serde(rename = "record.pause")]
    RecordPause { session_id: String },
    /// Fenced like [`Call::RecordStop`].
    #[serde(rename = "record.resume")]
    RecordResume { session_id: String },
    /// Result: `{"recording": RecordingState | null}`.
    #[serde(rename = "record.status")]
    RecordStatus,
    /// Starts a new language span now; fenced like [`Call::RecordStop`].
    #[serde(rename = "record.lang")]
    RecordLanguage {
        session_id: String,
        language: String,
    },
    /// Fenced like [`Call::RecordStop`].
    #[serde(rename = "record.bookmark")]
    RecordBookmark {
        session_id: String,
        #[serde(default)]
        label: Option<String>,
    },
    /// Fenced like [`Call::RecordStop`].
    #[serde(rename = "record.note")]
    RecordNote { session_id: String, text: String },
    /// `+a`/`a` adds, `-b` removes (a literal leading `-` is sent as `+-x`).
    /// Refused (`stale_session`) unless `session_id` is the active
    /// recording. Tag changes are applied one at a time, journal and
    /// provisional meeting together, in the order the daemon accepted them.
    /// Result `{"session_id", "tags": current, "added", "removed"}`.
    #[serde(rename = "record.tag")]
    RecordTag {
        tags: Vec<String>,
        session_id: String,
    },
    /// Sets the user title of the active recording or of a stopped one not
    /// saved yet (its provisional meeting too). Result
    /// `{"status":"saved","session_id","title","meeting_id"?}` or
    /// `{"status":"already_published","meeting_id"}` (rename the meeting
    /// instead); another session is `stale_session`.
    #[serde(rename = "record.title")]
    RecordTitle {
        session_id: String,
        request_id: RequestId,
        title: String,
    },
    /// The session's title changed in its files (a rename of its
    /// provisional meeting through the shared core service): the agent
    /// re-reads the title sidecar, the authoritative latest edit, and
    /// publishes it. Edits nothing, so repeating it is harmless. Result
    /// `{"session_id","title"}`; another session is `stale_session`.
    #[serde(rename = "record.title_refresh")]
    RecordTitleRefresh { session_id: String },
    /// Discards the active recording or a stopped one not saved yet: a
    /// durable intent first, then capture stops without queueing a final
    /// job, and its journal, jobs and provisional meeting are removed in the
    /// background. Result `{"status":"discarded","session_id","cleanup":
    /// "complete"|"pending"}` or `{"status":"already_saved","meeting_id"}`
    /// (delete the meeting instead); another session is `stale_session`.
    #[serde(rename = "record.discard")]
    RecordDiscard {
        session_id: String,
        request_id: RequestId,
    },
    /// Queues a provisional job over the durable chunks so far (plan §14.2);
    /// fenced like [`Call::RecordStop`].
    #[serde(rename = "record.process_now")]
    RecordProcessNow { session_id: String },
    /// Runs the final job of a stopped recording again after its
    /// processing failed (`StoppedRecordingState::Failed`), with fresh
    /// attempts; never automatic. Another state is `not_retryable`, an
    /// unknown or saved session `stale_session`. The outer request ID must
    /// equal `request_id`. Result `{"session_id","job_id"}`.
    #[serde(rename = "record.retry")]
    RecordRetry {
        session_id: String,
        request_id: RequestId,
    },
    /// Explicitly resolve one journal with no recorded library identity.
    #[serde(rename = "record.resolve_journal")]
    RecordResolveJournal {
        session_id: String,
        request_id: RequestId,
        resolution: JournalResolution,
        library_root: std::path::PathBuf,
        confirmed: bool,
    },
    /// Session-scoped AX observations; ignored outside the active recording.
    /// A batch must carry the session's current reader incarnation
    /// (`reader_generation`), else it is `stale_reader`.
    #[serde(rename = "reader_events")]
    ReaderEvents {
        session_id: String,
        batch: broscribe_readers::ReaderBatch,
    },
    /// A reader host (re)starting a reader for `session_id` asks for a
    /// fresh incarnation, naming the `reader_generation` it was told
    /// (`start_reader` event). Only while that generation is still current
    /// does the daemon issue a new one: earlier batches (old sequences,
    /// another host, a crashed run) become `stale_reader` and the new
    /// reader's sequences start afresh. Another session, a paused one or no
    /// reader is `stale_session`/`stale_reader`. Result
    /// `{"session_id","app_id","pid","reader_generation"}`. Reconciled by
    /// request ID as well as generation, so a lost answer cannot issue a
    /// second incarnation.
    #[serde(rename = "reader.attach")]
    ReaderAttach {
        session_id: String,
        reader_generation: u64,
    },
    /// A reader host could not start the incarnation `reader_generation`
    /// of `session_id` for `app_id`/`pid` (NA35): a typed, content-free
    /// stage, diagnostics only and never call evidence. Retained only while
    /// that is the session's current, unpaused incarnation of that reader
    /// and none of its batches arrived; a new incarnation clears it.
    /// Another session is `stale_session`, another app or process
    /// `invalid_input`, anything else `stale_reader`. Idempotent and fenced
    /// by generation like reader batches, so it bypasses the request log.
    /// Result `{"retained":true}`.
    #[serde(rename = "reader.refused")]
    ReaderRefused {
        session_id: String,
        app_id: String,
        pid: u32,
        reader_generation: u64,
        refusal: broscribe_readers::ReaderStartRefusal,
    },
    #[serde(rename = "record_participant")]
    RecordParticipant {
        session_id: String,
        request_id: RequestId,
        person_ref: String,
        action: ParticipantAction,
    },
    /// The live speaker preview of the active recording (plan D): result
    /// [`crate::live::LiveSnapshot`]; another session is `stale_session`.
    #[serde(rename = "record.live")]
    RecordLive { session_id: String },
    /// Names, confirms or clears exactly one shown live turn (plan D);
    /// result [`crate::live::LiveAttributeOutcome`]. Fenced by session,
    /// turn revision, epoch and evidence (`stale_session`,
    /// `stale_turn`, `stale_revision`, `stale_evidence`).
    #[serde(rename = "record.live_attribute")]
    RecordLiveAttribute(crate::live::LiveAttributeParams),
    /// Enables, disables or links automatic stop of a recording (plan H);
    /// result the updated [`AutoStopStatus`]. Fenced by session and binding
    /// generation (`stale_session`, `stale_binding`); the outer request ID
    /// must equal `request_id`.
    #[serde(rename = "record.auto_stop")]
    RecordAutoStop(RecordAutoStopParams),

    /// Re-scans the queue now; result `{job_ids:[...]}`.
    #[serde(rename = "queue.kick")]
    KickQueue,
    #[serde(rename = "detect.status")]
    DetectStatus,
    #[serde(rename = "detect.reload")]
    DetectReload,
    #[serde(rename = "audio_check")]
    AudioCheck { seconds: f64 },
    /// A bounded helper-owned setup capture; result is a private WAV path.
    #[serde(rename = "capture_sample")]
    CaptureSample {
        session: String,
        seconds: f64,
        purpose: String,
    },
    #[serde(rename = "capture_sample_stop")]
    CaptureSampleStop { session: String, discard: bool },
    #[serde(rename = "permissions_status")]
    PermissionsStatus,
    #[serde(rename = "permissions_request")]
    PermissionsRequest { permission: String },
    #[serde(rename = "integrations.reload")]
    IntegrationsReload,
    #[serde(rename = "integrations.sync")]
    IntegrationsSync { integration: Option<String> },
    #[serde(rename = "integrations.system_calendar")]
    SystemCalendar {
        #[serde(flatten)]
        action: SystemCalendarAction,
    },
    /// Minimal live authority for consumers of durable native calendar snapshots.
    #[serde(rename = "integrations.system_calendar_authority")]
    SystemCalendarAuthority,
    /// Read-only authority for exactly one remote Microsoft account.
    #[serde(rename = "integrations.microsoft_calendar_authority")]
    MicrosoftCalendarAuthority {
        account: broscribe_core::calendar::AccountRef,
    },
    /// Read-only authority of the Google account's calendar sync:
    /// `{"authority": {"generation": u64, "selection_generation": u64} |
    /// null}` ([`broscribe_core::calendar::GoogleCalendarStamp`]): the
    /// account generation and the calendar-selection generation together,
    /// never one scalar; `null` while not signed in with the configured
    /// client.
    #[serde(rename = "integrations.google_calendar_authority")]
    GoogleCalendarAuthority,
    /// Enqueues a job; result `{"job_id"}`.
    #[serde(rename = "queue.enqueue")]
    QueueEnqueue {
        kind: String,
        key: String,
        #[serde(default)]
        input: Value,
    },

    /// Install admission (signed updater): refused with
    /// `recording_active` while a recording is capturing, paused, in its
    /// stop grace or stopped and not saved yet, with `busy` during another
    /// capture or admission. Granted under the session mutex with a fence
    /// strictly greater than every earlier one (persisted across agent
    /// restarts) and bound to this connection: closing it releases an
    /// admission not yet confirmed. While held every capture start is
    /// refused with `update_installing`. Result `{"fence"}`. Fenced by
    /// fence and connection instead of the request log.
    #[serde(rename = "install.acquire")]
    InstallAcquire,
    /// Connection-owned idle admission for switching libraries. Blocks new capture and jobs.
    #[serde(rename = "library.switch_acquire")]
    LibrarySwitchAcquire,
    /// Re-validates the held admission right before an irreversible step;
    /// `update_admission_lost` unless `fence` is still held and nothing
    /// records. A confirmed admission becomes durable: it outlives its
    /// connection and agent restarts (the replacement restarts the agent)
    /// until `install.release`. Result `{"fence","durable":true}`.
    #[serde(rename = "install.confirm")]
    InstallConfirm { fence: u64 },
    /// Ends the admission `fence` (installed and verified, or rolled
    /// back), from any connection; another fence is
    /// `update_admission_lost`. Result `{"released": fence}`.
    #[serde(rename = "install.release")]
    InstallRelease { fence: u64 },

    /// `tag <meeting> +a -b`.
    #[serde(rename = "meeting.tag")]
    MeetingTag {
        meeting_id: String,
        #[serde(default)]
        add: Vec<String>,
        #[serde(default)]
        remove: Vec<String>,
    },
    /// `speakers name`: centroid naming now, sample-based confirmation
    /// queued; core also requests the library's speaker recheck
    /// (`reidentify`), which the daemon's scheduler runs.
    #[serde(rename = "speaker.name")]
    SpeakerName {
        meeting_id: String,
        speaker_id: String,
        person: String,
    },
    /// `speakers reject`.
    #[serde(rename = "speaker.reject")]
    SpeakerReject {
        meeting_id: String,
        speaker_id: String,
        #[serde(default)]
        person: Option<String>,
    },
    /// `people forget` / `people forget-voice`.
    #[serde(rename = "person.forget")]
    PersonForget {
        person: String,
        #[serde(default)]
        voice_only: bool,
    },
    /// Runs retention now for these meetings (all when absent).
    #[serde(rename = "retention.apply")]
    RetentionApply {
        #[serde(default)]
        meetings: Option<Vec<String>>,
    },
    /// The app forwards a notification action (Record / Ignore / Open).
    #[serde(rename = "notification.action")]
    NotificationAction {
        notification_id: String,
        action: String,
    },
}

impl Call {
    /// The wire method name.
    pub fn method(&self) -> &'static str {
        match self {
            Call::Ping => "ping",
            Call::Status => "status",
            Call::RequestStatus { .. } => "request.status",
            Call::Subscribe => "subscribe",
            Call::Shutdown => "daemon.stop",
            Call::ShutdownIfIdle => "daemon.stop_if_idle",
            Call::ShutdownAcquire => "daemon.stop_acquire",
            Call::AppShow => "app.show",
            Call::RecordStart(_) => "record.start",
            Call::RecordStop { .. } => "record.stop",
            Call::RecordPause { .. } => "record.pause",
            Call::RecordResume { .. } => "record.resume",
            Call::RecordStatus => "record.status",
            Call::RecordLanguage { .. } => "record.lang",
            Call::RecordBookmark { .. } => "record.bookmark",
            Call::RecordNote { .. } => "record.note",
            Call::RecordTag { .. } => "record.tag",
            Call::RecordTitle { .. } => "record.title",
            Call::RecordTitleRefresh { .. } => "record.title_refresh",
            Call::RecordDiscard { .. } => "record.discard",
            Call::RecordProcessNow { .. } => "record.process_now",
            Call::RecordRetry { .. } => "record.retry",
            Call::RecordResolveJournal { .. } => "record.resolve_journal",
            Call::ReaderEvents { .. } => "reader_events",
            Call::ReaderAttach { .. } => "reader.attach",
            Call::ReaderRefused { .. } => "reader.refused",
            Call::RecordParticipant { .. } => "record_participant",
            Call::RecordLive { .. } => "record.live",
            Call::RecordLiveAttribute(_) => "record.live_attribute",
            Call::RecordAutoStop(_) => "record.auto_stop",
            Call::KickQueue => "queue.kick",
            Call::DetectStatus => "detect.status",
            Call::DetectReload => "detect.reload",
            Call::AudioCheck { .. } => "audio_check",
            Call::CaptureSample { .. } => "capture_sample",
            Call::CaptureSampleStop { .. } => "capture_sample_stop",
            Call::PermissionsStatus => "permissions_status",
            Call::PermissionsRequest { .. } => "permissions_request",
            Call::IntegrationsReload => "integrations.reload",
            Call::IntegrationsSync { .. } => "integrations.sync",
            Call::SystemCalendar { .. } => "integrations.system_calendar",
            Call::SystemCalendarAuthority => "integrations.system_calendar_authority",
            Call::MicrosoftCalendarAuthority { .. } => "integrations.microsoft_calendar_authority",
            Call::GoogleCalendarAuthority => "integrations.google_calendar_authority",
            Call::QueueEnqueue { .. } => "queue.enqueue",
            Call::InstallAcquire => "install.acquire",
            Call::LibrarySwitchAcquire => "library.switch_acquire",
            Call::InstallConfirm { .. } => "install.confirm",
            Call::InstallRelease { .. } => "install.release",
            Call::MeetingTag { .. } => "meeting.tag",
            Call::SpeakerName { .. } => "speaker.name",
            Call::SpeakerReject { .. } => "speaker.reject",
            Call::PersonForget { .. } => "person.forget",
            Call::RetentionApply { .. } => "retention.apply",
            Call::NotificationAction { .. } => "notification.action",
        }
    }

    /// Whether the call changes state: its outcome is remembered by request
    /// ID and a repeat with the same ID does not run it again.
    pub fn is_mutating(&self) -> bool {
        !matches!(
            self,
            Call::Ping
                | Call::Status
                | Call::RequestStatus { .. }
                | Call::Subscribe
                | Call::AppShow
                | Call::RecordStatus
                | Call::RecordLive { .. }
                | Call::DetectStatus
                | Call::PermissionsStatus
                | Call::SystemCalendar {
                    action: SystemCalendarAction::Status
                }
                | Call::SystemCalendarAuthority
                | Call::MicrosoftCalendarAuthority { .. }
                | Call::GoogleCalendarAuthority
                | Call::RecordTitleRefresh { .. }
                // An idempotent, generation-fenced diagnostic: a repeat
                // retains the same refusal or is stale.
                | Call::ReaderRefused { .. }
        )
    }

    /// The session a recording command is fenced to: the daemon runs it
    /// only while this session is the active recording.
    pub fn recording_session(&self) -> Option<&str> {
        match self {
            Call::RecordStop { session_id }
            | Call::RecordPause { session_id }
            | Call::RecordResume { session_id }
            | Call::RecordLanguage { session_id, .. }
            | Call::RecordBookmark { session_id, .. }
            | Call::RecordNote { session_id, .. }
            | Call::RecordTag { session_id, .. }
            | Call::RecordProcessNow { session_id } => Some(session_id),
            _ => None,
        }
    }

    /// The explicit reconciliation ID a mutation carries in its parameters
    /// (participant, title, discard, retry, live decision, automatic stop):
    /// the outer request ID must equal it, so a retry is reconciled through
    /// `request.status` and never runs twice.
    pub fn reconciliation_id(&self) -> Option<&RequestId> {
        match self {
            Call::RecordParticipant { request_id, .. }
            | Call::RecordTitle { request_id, .. }
            | Call::RecordDiscard { request_id, .. }
            | Call::RecordRetry { request_id, .. }
            | Call::RecordResolveJournal { request_id, .. } => Some(request_id),
            Call::RecordLiveAttribute(params) => Some(&params.request_id),
            Call::RecordAutoStop(params) => Some(&params.request_id),
            _ => None,
        }
    }

    /// Install-admission calls: fenced by their fence and holder connection
    /// instead of the request log (a replayed `install.acquire` must never
    /// hand back a fence its connection already lost).
    pub fn is_install_admission(&self) -> bool {
        matches!(
            self,
            Call::InstallAcquire
                | Call::ShutdownAcquire
                | Call::LibrarySwitchAcquire
                | Call::InstallConfirm { .. }
                | Call::InstallRelease { .. }
        )
    }
}

/// One request line.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    pub id: RequestId,
    pub call: Call,
}

/// A request line that could not be parsed; `id` is set when the line had a
/// valid one, so the error can be answered on it.
#[derive(Clone, Debug, PartialEq)]
pub struct ParseError {
    pub id: Option<RequestId>,
    pub error: IpcError,
}

impl Request {
    /// A fresh ID, or the explicit reconciliation ID of the call
    /// ([`Call::reconciliation_id`]).
    pub fn new(call: Call) -> Self {
        let id = call
            .reconciliation_id()
            .cloned()
            .unwrap_or_else(RequestId::random);
        Self { id, call }
    }

    /// The JSON line, without the trailing newline.
    pub fn to_line(&self) -> Result<String, IpcError> {
        let mut value = to_value(&self.call)?;
        let Value::Object(map) = &mut value else {
            return Err(IpcError::protocol("request did not serialise to an object"));
        };
        map.insert("v".into(), Value::from(PROTOCOL_VERSION));
        map.insert("id".into(), Value::from(self.id.as_str()));
        to_line(&value)
    }

    pub fn parse(line: &str) -> Result<Self, ParseError> {
        let no_id = |error| ParseError { id: None, error };
        let value: Value = serde_json::from_str(line.trim_end())
            .map_err(|e| no_id(IpcError::protocol(format!("not JSON: {e}"))))?;
        let Value::Object(mut map) = value else {
            return Err(no_id(IpcError::protocol("a request is a JSON object")));
        };
        let id = match map.remove("id") {
            Some(Value::String(s)) => RequestId::parse(&s).map_err(no_id)?,
            _ => {
                return Err(no_id(IpcError::new(
                    IpcError::INVALID_REQUEST,
                    "missing request id",
                )));
            }
        };
        let with_id = |error| ParseError {
            id: Some(id.clone()),
            error,
        };
        check_version(map.remove("v")).map_err(with_id)?;
        let call: Call = serde_json::from_value(Value::Object(map))
            .map_err(|e| with_id(IpcError::new(IpcError::INVALID_REQUEST, e.to_string())))?;
        Ok(Self { id, call })
    }
}

// ---- responses ----

/// The answer to one request.
#[derive(Clone, Debug, PartialEq)]
pub struct Response {
    pub id: RequestId,
    pub outcome: Result<Value, IpcError>,
}

#[derive(Serialize, Deserialize)]
struct WireResponse {
    v: u32,
    id: RequestId,
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<IpcError>,
}

impl Response {
    pub fn ok(id: RequestId, result: Value) -> Self {
        Self {
            id,
            outcome: Ok(result),
        }
    }

    pub fn err(id: RequestId, error: IpcError) -> Self {
        Self {
            id,
            outcome: Err(error),
        }
    }

    pub fn to_line(&self) -> Result<String, IpcError> {
        let wire = match &self.outcome {
            Ok(result) => WireResponse {
                v: PROTOCOL_VERSION,
                id: self.id.clone(),
                ok: true,
                result: Some(result.clone()),
                error: None,
            },
            Err(error) => WireResponse {
                v: PROTOCOL_VERSION,
                id: self.id.clone(),
                ok: false,
                result: None,
                error: Some(error.clone()),
            },
        };
        to_line(&wire)
    }

    fn from_value(value: Value) -> Result<Self, IpcError> {
        let wire: WireResponse = serde_json::from_value(value)
            .map_err(|e| IpcError::protocol(format!("invalid response: {e}")))?;
        if wire.v != PROTOCOL_VERSION {
            return Err(unsupported_version(u64::from(wire.v)));
        }
        let outcome = if wire.ok {
            Ok(wire.result.unwrap_or(Value::Null))
        } else {
            Err(wire
                .error
                .unwrap_or_else(|| IpcError::protocol("error response without an error")))
        };
        Ok(Self {
            id: wire.id,
            outcome,
        })
    }
}

/// What the daemon knows about a request ID (`request.status`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RequestState {
    /// Never seen, or forgotten (the daemon keeps the most recent ones and
    /// loses them on restart).
    Unknown,
    Running,
    Done {
        result: Value,
    },
    Failed {
        error: IpcError,
    },
}

// ---- state and events ----

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingStatus {
    Recording,
    Paused,
}

/// The active capture session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordingState {
    pub session_id: String,
    pub status: RecordingStatus,
    pub started_at: DateTime<FixedOffset>,
    /// Recording time so far (pauses excluded).
    pub elapsed_s: f64,
    pub language: Option<String>,
    pub app: Option<String>,
    pub title: Option<String>,
    /// Detected or chosen audio setup (design §5).
    pub setup: Option<String>,
    /// User-confirmed library person IDs, restored on subscription/reconnect.
    #[serde(default)]
    pub participants: Vec<String>,
    /// Reader observations, never user confirmations.
    #[serde(default)]
    pub observed_roster: Vec<broscribe_readers::RosterMember>,
    /// The session's current tags (journaled).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Automatic stop of this recording (plan H).
    #[serde(default)]
    pub auto_stop: Option<AutoStopStatus>,
    /// Per-track capture state, dropped frames and disk headroom, as the
    /// capture engine last measured them. Diagnostics only: never call
    /// evidence and never an end.
    pub health: crate::capture::CaptureHealth,
    /// The meeting reader of this recording; `None`: it has no meeting app
    /// to read (a manual recording not linked to a call).
    #[serde(default)]
    pub reader: Option<ReaderStatus>,
    /// Diagnostics the current reader incarnation sent with its latest
    /// accepted batch; `None` before its first batch, after a pause and
    /// whenever a new incarnation is issued. Never call evidence.
    #[serde(default)]
    pub reader_health: Option<broscribe_readers::ReaderHealth>,
}

/// What a recording's meeting reader is doing, apart from the live
/// preview's model state and capture health.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReaderStatus {
    /// The recording's app has no meeting reader on this system: only the
    /// microphone-release fallback can end it automatically.
    Unavailable,
    /// A reader incarnation was issued; none of its batches arrived yet.
    Starting,
    /// Its batches arrive and their call state is usable.
    Reading,
    /// The reader is silent or its call state is not usable; automatic
    /// stop falls back to the microphone release.
    Degraded { reason: ReaderDegraded },
    /// The reader host reported that it could not start the current
    /// incarnation, and nothing of it was heard since; automatic stop
    /// falls back to the microphone release. Diagnostics only: the daemon
    /// still announces the incarnation again, and a new one clears it.
    Refused {
        refusal: broscribe_readers::ReaderStartRefusal,
    },
    /// The recording is paused; no reader runs.
    Paused,
}

/// Why a reader is [`ReaderStatus::Degraded`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReaderDegraded {
    /// No batch for longer than the reader's heartbeat allows.
    Silent,
    /// Batches arrive, but the call state they carry is unknown,
    /// incomplete or out of order.
    UnusableCallState,
}

/// A detected meeting app, by its data-defined detection rule (never a
/// window title).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoStopApp {
    pub app_id: String,
    pub app_name: String,
}

/// Why a recording is about to stop automatically.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoStopReason {
    /// Microphone-release fallback: the app released the mic and its
    /// detection rule no longer matches (debounce passed).
    MicReleasedRuleChanged,
    /// Microphone-release fallback: the app released the mic for the whole
    /// end-confirmation time.
    MicReleasedTimeout,
    /// The meeting reader saw the app's explicit post-call screen for the
    /// debounce, after seeing the call active.
    CallEnded,
}

/// The cancellation grace of an end decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoStopPending {
    pub reason: AutoStopReason,
    /// Seconds of grace left (rounded up); display only, never a deadline
    /// that authorizes the stop.
    pub remaining_s: u64,
}

/// Automatic stop of one recording (`RecordingState.auto_stop`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoStopStatus {
    pub enabled: bool,
    /// The detection app the recording is bound to; `None`: manual.
    pub app: Option<AutoStopApp>,
    /// Binding/policy generation `record.auto_stop` must name.
    pub generation: u64,
    /// Fresh call-state evidence from the meeting reader is in use; without
    /// it only the microphone-release fallback applies.
    pub reader_available: bool,
    pub pending: Option<AutoStopPending>,
    /// Meeting apps detected on the microphone now, linkable by `app_id`.
    #[serde(default)]
    pub candidates: Vec<AutoStopApp>,
}

/// Parameters of `record.auto_stop`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordAutoStopParams {
    pub session_id: String,
    pub request_id: RequestId,
    /// The `AutoStopStatus.generation` the caller saw.
    pub expected_generation: u64,
    pub enabled: bool,
    /// Links the recording to this fresh candidate (with `enabled`);
    /// omitted keeps the current binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
}

/// What a stopped, not yet saved recording is doing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum StoppedRecordingState {
    /// Its final job waits for a worker (`ahead` runnable jobs first).
    Queued { ahead: usize },
    /// A worker runs its final job now.
    Running {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fraction: Option<f64>,
    },
    /// An attempt failed or crashed; the next runs automatically after its
    /// backoff (`attempts` spent so far).
    Retrying {
        attempts: u32,
        retry_in_s: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Processing failed for good: nothing runs again until the user asks
    /// (`record.retry`). The journal is kept.
    Failed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Discarded; its files are being removed.
    Discarding,
}

impl StoppedRecordingState {
    /// Still renamable, discardable and retryable: not being discarded.
    pub fn is_pending_save(&self) -> bool {
        !matches!(self, Self::Discarding)
    }
}

/// A recording that stopped and is not saved yet (`DaemonState.stopped`):
/// the authoritative list for the stopped-recording panel after reconnects.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoppedRecording {
    pub session_id: String,
    pub title: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub stopped_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub state: StoppedRecordingState,
}

/// A recording saved during this daemon run (`DaemonState.saved`, newest
/// first, at most [`SAVED_RECORDINGS_MAX`]): lets a stopped-recording panel
/// switch to the saved meeting without a mutating call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedRecording {
    pub session_id: String,
    pub meeting_id: String,
    pub title: Option<String>,
    pub saved_at: DateTime<Utc>,
}

/// How many [`SavedRecording`]s `DaemonState.saved` keeps.
pub const SAVED_RECORDINGS_MAX: usize = 5;

/// Progress of a queue job run by `broscribe-worker`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobProgress {
    pub job_id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meeting_id: Option<broscribe_core::MeetingId>,
    /// `started`, `progress`, `done`, `failed`, `crashed`, `gave_up` or
    /// `waiting` (left unfinished on purpose: offered again later, no
    /// attempt spent). `message` is the worker's text: counts and phases,
    /// never transcript text or names.
    pub phase: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fraction: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct JobsState {
    /// The job a worker runs now.
    pub running: Option<JobProgress>,
    /// Runnable jobs waiting for a worker.
    pub waiting: usize,
    /// Jobs held back: final recording jobs while capturing, retry backoff.
    pub deferred: usize,
}

/// The live preview's ASR worker, separately from live speaker identity
/// ([`crate::live::LiveIdentityState`]) and the meeting reader
/// ([`ReaderStatus`]).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PreviewState {
    /// No recording, or its preview ended.
    #[default]
    Off,
    /// The worker started and loads its model (`elapsed_ms` since it
    /// started; `model` once the worker named it).
    Loading {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        elapsed_ms: u64,
    },
    /// Frames are being transcribed.
    Ready { model: String },
    /// The worker failed and restarts after `retry_in_ms`.
    Restarting {
        reason: WorkerProblem,
        retry_in_ms: u64,
    },
    /// The preview gave up for this recording; the recording itself is
    /// unaffected.
    Unavailable { reason: WorkerProblem },
}

impl PreviewState {
    /// Transcribing now.
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Ready { .. })
    }
}

/// Why a live worker (ASR preview or speaker analysis) is not running.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerProblem {
    /// The worker process could not be started.
    StartFailed,
    /// It exited on its own.
    Exited,
    /// It wrote nothing for longer than its watchdog allows.
    Silent,
    /// It runs but completes no work while work is pending.
    Stalled,
    /// Its models were not ready within the total load deadline.
    LoadTimeout,
    /// Its settings are invalid; it is not started.
    InvalidSettings,
    /// Its model is not installed; it is not started again for this
    /// recording until the model is installed (never restarted in a loop).
    ModelMissing,
}

/// What the user chose for a journal that predates library identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalResolution {
    Adopt,
    Discard,
}

/// A journal recovery could not bring to the stopped state; never queued,
/// never deleted. I/O failures are retried with backoff; a corrupt journal
/// stays in place untouched for diagnosis.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalIssue {
    /// The journal folder's name (the session ID it was created for).
    pub session_id: String,
    /// The stable error code.
    pub code: String,
    /// The manifest is unreadable: retrying cannot help.
    pub corrupt: bool,
}

/// Whether the library index can be trusted as current.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum IndexState {
    /// The startup repair has not finished yet.
    #[default]
    Checking,
    /// Every write since the last rebuild reached the index.
    Current,
    /// Files that could not be read stay listed for repair; the rest is
    /// current.
    Pending { skipped: usize },
    /// The repair failed (`code`); it is tried again at maintenance.
    Failed { code: String },
}

/// A held install admission (`install.acquire`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallHold {
    pub fence: u64,
    /// Confirmed: kept across its connection closing and agent restarts
    /// until `install.release`.
    pub durable: bool,
}

/// The agent's current permission for one recording track.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturePermission {
    Granted,
    Denied,
    NotDetermined,
    Restricted,
    Unsupported,
    #[default]
    #[serde(other)]
    Unknown,
}

impl From<&str> for CapturePermission {
    fn from(status: &str) -> Self {
        match status {
            "granted" => Self::Granted,
            "denied" => Self::Denied,
            "not_determined" => Self::NotDetermined,
            "restricted" => Self::Restricted,
            "unsupported" => Self::Unsupported,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureAccess {
    #[serde(default)]
    pub microphone: CapturePermission,
    #[serde(default)]
    pub system_audio: CapturePermission,
}

impl CaptureAccess {
    pub fn denied(self, track: crate::Track) -> bool {
        (match track {
            crate::Track::Mic => self.microphone,
            crate::Track::System => self.system_audio,
        }) == CapturePermission::Denied
    }
}

/// Result of `status` and payload of the `state` event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DaemonState {
    pub pid: u32,
    pub version: String,
    pub protocol: u32,
    pub library: String,
    pub recording: Option<RecordingState>,
    /// Unknown for platforms and older agents without a permission check.
    #[serde(default)]
    pub access: CaptureAccess,
    /// This agent has a meeting detector. Older peers omit the capability.
    #[serde(default)]
    pub detection_enabled: bool,
    pub jobs: JobsState,
    pub preview: PreviewState,
    /// Authoritative prompt snapshot, including an empty list after dismissal.
    #[serde(default)]
    pub pending_notifications: Vec<NotificationRequest>,
    /// Stopped recordings not saved yet, oldest first.
    #[serde(default)]
    pub stopped: Vec<StoppedRecording>,
    /// Recordings saved during this daemon run, newest first.
    #[serde(default)]
    pub saved: Vec<SavedRecording>,
    /// Journals the last recovery sweep could not recover (IDs and codes
    /// only), oldest session first.
    #[serde(default)]
    pub journal_issues: Vec<JournalIssue>,
    /// Whether the library index is known to be current.
    #[serde(default)]
    pub index: IndexState,
    /// The install admission held now, if any: no capture starts.
    #[serde(default)]
    pub install: Option<InstallHold>,
    /// The running agent's build identity, read once when it started from
    /// the build information its installation carries; `None` when unknown
    /// (an unbundled build, missing or invalid build information, or one
    /// naming another version). `version` alone never proves which build
    /// runs.
    #[serde(default)]
    pub build: Option<AgentBuild>,
}

/// The build an agent runs (`DaemonState.build`, `ping`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBuild {
    /// Monotonic build number.
    pub build: u64,
    /// Rust target triple the build was made for.
    pub target: String,
}

/// A notification the app should show (the daemon has no UI loop, plan
/// §12.3); actions come back as `notification.action`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NotificationRequest {
    pub id: String,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub actions: Vec<NotificationButton>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meeting_id: Option<String>,
}

impl NotificationRequest {
    /// A detected meeting waits for a Record or Ignore answer. Other
    /// notifications may have actions but do not ask to start capture.
    pub fn is_recording_prompt(&self) -> bool {
        let offers = |action: &str| self.actions.iter().any(|button| button.id == action);
        offers("record") && offers("ignore")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationButton {
    pub id: String,
    pub label: String,
}

/// Events pushed to subscribers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    State(Box<DaemonState>),
    /// Peak levels of the last meter period in dBFS (−120 for silence);
    /// throttled by the daemon to 10 Hz.
    Meters {
        me_dbfs: f32,
        others_dbfs: f32,
    },
    /// A recognised live-preview segment of recording `session_id`: the
    /// session whose worker produced it, never the one active when it
    /// arrives (protocol 6).
    LiveTranscript {
        session_id: String,
        segment: LiveSegment,
    },
    /// A live-preview turn, new or revised (identity preview, plan D).
    LiveTurn(Box<crate::live::LiveTurn>),
    /// Availability of recording `session_id`'s live identity preview.
    LiveIdentity {
        session_id: String,
        identity: crate::live::LiveIdentityState,
    },
    /// The live preview of recording `session_id` dropped audio of `track`
    /// between these recording times; the recording itself is unaffected.
    PreviewGap {
        session_id: String,
        track: Track,
        from_s: f64,
        to_s: f64,
    },
    /// The ASR preview availability of recording `session_id`.
    Preview {
        session_id: String,
        preview: PreviewState,
    },
    /// A problem the active recording's capture survived (a track down or
    /// back, low disk before the protective stop). The recording goes on;
    /// `RecordingState.health` carries the current state after reconnects.
    CaptureWarning {
        session_id: String,
        warning: crate::capture::CaptureWarning,
    },
    JobProgress(JobProgress),
    Notification(NotificationRequest),
    NotificationDismissed {
        id: String,
    },
    StartReader {
        session_id: String,
        app_id: String,
        pid: u32,
        elapsed_s: f64,
        /// Daemon-issued per reader incarnation (start, pause, resume,
        /// relink); every `ReaderBatch` must echo the current one.
        reader_generation: u64,
    },
    StopReader {
        session_id: String,
    },
    /// This subscriber read too slowly: `dropped` events were discarded.
    Lagged {
        dropped: u64,
    },
    /// Live levels of a setup capture in the helper (design §4.1): of the
    /// `capture_sample` whose `session` is given, or of the running
    /// `audio_check` (`session` absent). Levels only, never audio; throttled
    /// by the daemon to 10 Hz, sent only when they change and never after the
    /// capture ended. `others` is the audio check's test sound as played to
    /// the output; absent for a sample capture.
    SetupLevels {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session: Option<String>,
        me: SetupLevel,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        others: Option<SetupLevel>,
    },
    /// Show the main window (`app.show`).
    ShowMainWindow,
}

/// Peak and RMS of one track over a meter period, in dBFS (−120 for silence).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SetupLevel {
    pub peak_dbfs: f32,
    pub rms_dbfs: f32,
}

impl Event {
    pub fn to_line(&self) -> Result<String, IpcError> {
        let mut value = to_value(self)?;
        let Value::Object(map) = &mut value else {
            return Err(IpcError::protocol("event did not serialise to an object"));
        };
        map.insert("v".into(), Value::from(PROTOCOL_VERSION));
        to_line(&value)
    }
}

/// A line from the daemon: a response or an event.
#[derive(Clone, Debug, PartialEq)]
pub enum ServerMessage {
    Response(Response),
    Event(Event),
}

impl ServerMessage {
    pub fn parse(line: &str) -> Result<Self, IpcError> {
        let value: Value = serde_json::from_str(line.trim_end())
            .map_err(|e| IpcError::protocol(format!("not JSON: {e}")))?;
        let Value::Object(mut map) = value else {
            return Err(IpcError::protocol("a message is a JSON object"));
        };
        if map.contains_key("event") {
            check_version(map.remove("v"))?;
            let event: Event = serde_json::from_value(Value::Object(map))
                .map_err(|e| IpcError::protocol(format!("invalid event: {e}")))?;
            return Ok(Self::Event(event));
        }
        Response::from_value(Value::Object(map)).map(Self::Response)
    }
}

// ---- helpers ----

fn check_version(v: Option<Value>) -> Result<(), IpcError> {
    match v.as_ref().and_then(Value::as_u64) {
        Some(n) if n == u64::from(PROTOCOL_VERSION) => Ok(()),
        Some(n) => Err(unsupported_version(n)),
        None => Err(IpcError::new(
            IpcError::INVALID_REQUEST,
            "missing protocol version \"v\"",
        )),
    }
}

fn unsupported_version(v: u64) -> IpcError {
    IpcError::new(
        IpcError::UNSUPPORTED_VERSION,
        format!("protocol version {v} is not supported (this side speaks {PROTOCOL_VERSION})"),
    )
}

fn to_value<T: Serialize>(v: &T) -> Result<Value, IpcError> {
    serde_json::to_value(v).map_err(|e| IpcError::protocol(format!("serialising: {e}")))
}

fn to_line<T: Serialize>(v: &T) -> Result<String, IpcError> {
    serde_json::to_string(v).map_err(|e| IpcError::protocol(format!("serialising: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::LiveSpeaker;
    use serde_json::json;

    fn round_trip(call: Call) {
        let req = Request::new(call);
        let line = req.to_line().unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(Request::parse(&line).unwrap(), req);
    }

    #[test]
    fn requests_round_trip() {
        round_trip(Call::Ping);
        round_trip(Call::Status);
        round_trip(Call::ShutdownAcquire);
        round_trip(Call::RequestStatus {
            request_id: RequestId::parse("r-1").unwrap(),
        });
        round_trip(Call::RecordStart(RecordStartParams {
            title: Some("Roadmap review".into()),
            language: Some("pt-BR".into()),
            tags: vec!["weekly".into()],
            ..RecordStartParams::default()
        }));
        round_trip(Call::RecordBookmark {
            session_id: "s-1".into(),
            label: None,
        });
        round_trip(Call::ReaderAttach {
            session_id: "s-1".into(),
            reader_generation: 4,
        });
        round_trip(Call::ReaderRefused {
            session_id: "s-1".into(),
            app_id: "teams".into(),
            pid: 42,
            reader_generation: 4,
            refusal: broscribe_readers::ReaderStartRefusal::Untrusted,
        });
        round_trip(Call::GoogleCalendarAuthority);
        round_trip(Call::QueueEnqueue {
            kind: "playback-cache".into(),
            key: "k".into(),
            input: json!({"meeting_id": "m-000001"}),
        });
        round_trip(Call::SpeakerName {
            meeting_id: "m-000001".into(),
            speaker_id: "S1".into(),
            person: "Ana Example".into(),
        });
        round_trip(Call::CaptureSample {
            session: "sample-1".into(),
            seconds: 2.0,
            purpose: "enrollment".into(),
        });
        round_trip(Call::CaptureSampleStop {
            session: "sample-1".into(),
            discard: true,
        });
        round_trip(Call::PermissionsStatus);
        round_trip(Call::PermissionsRequest {
            permission: "microphone".into(),
        });
        round_trip(Call::IntegrationsSync {
            integration: Some("google".into()),
        });
        round_trip(Call::AppShow);
    }

    #[test]
    fn wire_shape() {
        let req = Request {
            id: RequestId::parse("r-7").unwrap(),
            call: Call::RecordLanguage {
                session_id: "s-1".into(),
                language: "en".into(),
            },
        };
        let v: Value = serde_json::from_str(&req.to_line().unwrap()).unwrap();
        assert_eq!(
            v,
            json!({"v": 8, "id": "r-7", "method": "record.lang",
                "params": {"session_id": "s-1", "language": "en"}})
        );
        let ping = Request::parse(r#"{"v":8,"id":"a","method":"ping"}"#).unwrap();
        assert_eq!(ping.call, Call::Ping);
        assert!(!ping.call.is_mutating());
        assert!(
            Call::RecordStop {
                session_id: "s-1".into()
            }
            .is_mutating()
        );
    }

    #[test]
    fn bad_requests_keep_their_id() {
        let e = Request::parse("nope").unwrap_err();
        assert_eq!((e.id, e.error.code.as_str()), (None, IpcError::PROTOCOL));
        for version in [6, 7] {
            let line = format!(r#"{{"v":{version},"id":"x","method":"ping"}}"#);
            let e = Request::parse(&line).unwrap_err();
            assert_eq!(e.id, Some(RequestId::parse("x").unwrap()));
            assert_eq!(e.error.code, IpcError::UNSUPPORTED_VERSION);
        }
        let e = Request::parse(r#"{"v":8,"id":"x","method":"format.disk"}"#).unwrap_err();
        assert_eq!(e.error.code, IpcError::INVALID_REQUEST);
        let e = Request::parse(r#"{"v":8,"id":"has space","method":"ping"}"#).unwrap_err();
        assert_eq!(e.id, None);
    }

    /// Every recording command names its session on the wire; a version-1
    /// line (which ran on whatever session was active) is refused before
    /// it can run, and a version-2 line without a session is invalid.
    #[test]
    fn recording_commands_are_session_fenced_and_old_lines_fail_closed() {
        let session = || "s-20261117-100000-abcd".to_owned();
        let calls = [
            Call::RecordStop {
                session_id: session(),
            },
            Call::RecordPause {
                session_id: session(),
            },
            Call::RecordResume {
                session_id: session(),
            },
            Call::RecordLanguage {
                session_id: session(),
                language: "en".into(),
            },
            Call::RecordBookmark {
                session_id: session(),
                label: Some("decision".into()),
            },
            Call::RecordNote {
                session_id: session(),
                text: "synthetic note".into(),
            },
            Call::RecordTag {
                session_id: session(),
                tags: vec!["+a".into()],
            },
            Call::RecordProcessNow {
                session_id: session(),
            },
        ];
        for call in calls {
            assert_eq!(call.recording_session(), Some(session().as_str()));
            assert!(call.is_mutating());
            let line: Value =
                serde_json::from_str(&Request::new(call.clone()).to_line().unwrap()).unwrap();
            assert_eq!(line["params"]["session_id"], session());
            let method = call.method();
            let old = format!(r#"{{"v":1,"id":"old-1","method":"{method}"}}"#);
            let e = Request::parse(&old).unwrap_err();
            assert_eq!(e.error.code, IpcError::UNSUPPORTED_VERSION, "{method}");
            let unfenced = format!(r#"{{"v":8,"id":"new-1","method":"{method}","params":{{}}}}"#);
            let e = Request::parse(&unfenced).unwrap_err();
            assert_eq!(e.error.code, IpcError::INVALID_REQUEST, "{method}");
        }
        assert_eq!(Call::RecordStatus.recording_session(), None);
        // Reader incarnation and the Google authority keep their shapes.
        let attach = Request::parse(
            r#"{"v":8,"id":"a-1","method":"reader.attach","params":{"session_id":"s-1","reader_generation":7}}"#,
        )
        .unwrap();
        assert_eq!(
            attach.call,
            Call::ReaderAttach {
                session_id: "s-1".into(),
                reader_generation: 7
            }
        );
        let refused = Request::parse(
            r#"{"v":8,"id":"f-1","method":"reader.refused","params":{"session_id":"s-1","app_id":"teams","pid":42,"reader_generation":7,"refusal":"no_rule"}}"#,
        )
        .unwrap();
        assert_eq!(
            refused.call,
            Call::ReaderRefused {
                session_id: "s-1".into(),
                app_id: "teams".into(),
                pid: 42,
                reader_generation: 7,
                refusal: broscribe_readers::ReaderStartRefusal::NoRule,
            }
        );
        assert!(!refused.call.is_mutating());
        // A refusal names one of the typed stages, never free text.
        assert_eq!(
            Request::parse(
                r#"{"v":8,"id":"f-2","method":"reader.refused","params":{"session_id":"s-1","app_id":"teams","pid":42,"reader_generation":7,"refusal":"permission denied"}}"#,
            )
            .unwrap_err()
            .error
            .code,
            IpcError::INVALID_REQUEST
        );
        let google = Request::parse(
            r#"{"v":8,"id":"g-1","method":"integrations.google_calendar_authority"}"#,
        )
        .unwrap();
        assert_eq!(google.call, Call::GoogleCalendarAuthority);
        assert!(!google.call.is_mutating());
    }

    #[test]
    fn responses_and_events_round_trip() {
        let id = RequestId::parse("r-1").unwrap();
        for resp in [
            Response::ok(id.clone(), json!({"job_id": "j-0123456789abcdef"})),
            Response::ok(id.clone(), Value::Null),
            Response::err(id.clone(), IpcError::new("not_found", "meeting m-1")),
        ] {
            let line = resp.to_line().unwrap();
            assert_eq!(
                ServerMessage::parse(&line).unwrap(),
                ServerMessage::Response(resp)
            );
        }
        let events = [
            Event::Lagged { dropped: 3 },
            Event::Meters {
                me_dbfs: -12.5,
                others_dbfs: -120.0,
            },
            Event::PreviewGap {
                session_id: "s-1".into(),
                track: Track::System,
                from_s: 1.0,
                to_s: 2.5,
            },
            Event::LiveTranscript {
                session_id: "s-1".into(),
                segment: LiveSegment {
                    track: Track::Mic,
                    start_s: 0.5,
                    end_s: 2.0,
                    text: "hello there".into(),
                    speaker: LiveSpeaker::Me,
                    words: Vec::new(),
                },
            },
            Event::LiveIdentity {
                session_id: "s-1".into(),
                identity: crate::live::LiveIdentityState {
                    status: crate::live::LiveIdentityStatus::Idle,
                    epoch: 2,
                },
            },
            Event::JobProgress(JobProgress {
                job_id: "j-0123456789abcdef".into(),
                kind: "import".into(),
                meeting_id: Some(broscribe_core::MeetingId::parse("m-012345").unwrap()),
                phase: "progress".into(),
                fraction: Some(0.5),
                message: None,
            }),
            Event::Preview {
                session_id: "s-1".into(),
                preview: PreviewState::Restarting {
                    reason: WorkerProblem::LoadTimeout,
                    retry_in_ms: 2_000,
                },
            },
            Event::Preview {
                session_id: "s-1".into(),
                preview: PreviewState::Unavailable {
                    reason: WorkerProblem::ModelMissing,
                },
            },
            Event::CaptureWarning {
                session_id: "s-1".into(),
                warning: crate::capture::CaptureWarning::DiskLow {
                    free_bytes: 900 << 20,
                    headroom_s: 1_200.0,
                    stop_below_bytes: 500 << 20,
                    stop_below_s: 600.0,
                },
            },
            Event::ShowMainWindow,
        ];
        for ev in events {
            let line = ev.to_line().unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&line).unwrap()["v"],
                PROTOCOL_VERSION
            );
            assert_eq!(
                ServerMessage::parse(&line).unwrap(),
                ServerMessage::Event(ev)
            );
        }
    }

    /// Live-preview events carry the recording that produced them on the
    /// wire. A line without it (the version-5 shape) is no event of any
    /// session, and a version-5 line is refused outright.
    #[test]
    fn live_preview_events_name_their_recording_and_unscoped_lines_fail_closed() {
        let v: Value = serde_json::from_str(
            &Event::LiveIdentity {
                session_id: "s-a".into(),
                identity: crate::live::LiveIdentityState::default(),
            }
            .to_line()
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            v,
            json!({"v": 8, "event": "live_identity", "session_id": "s-a",
                "identity": {"status": {"state": "idle"}, "epoch": 0}})
        );
        let v: Value = serde_json::from_str(
            &Event::Preview {
                session_id: "s-a".into(),
                preview: PreviewState::Off,
            }
            .to_line()
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            v,
            json!({"v": 8, "event": "preview", "session_id": "s-a",
                "preview": {"state": "off"}})
        );
        let unscoped = [
            json!({"event": "live_identity",
                "identity": {"status": {"state": "idle"}, "epoch": 0}}),
            json!({"event": "live_transcript", "segment": {"track": "mic", "start_s": 0.0,
                "end_s": 1.0, "text": "t", "speaker": {"kind": "me"}}}),
            json!({"event": "preview", "preview": {"state": "off"}}),
            json!({"event": "preview_gap", "track": "mic", "from_s": 0.0, "to_s": 1.0}),
        ];
        for mut line in unscoped {
            line["v"] = json!(PROTOCOL_VERSION);
            let e = ServerMessage::parse(&line.to_string()).unwrap_err();
            assert_eq!(e.code, IpcError::PROTOCOL, "{line}");
            line["v"] = json!(5);
            line["session_id"] = json!("s-a");
            let e = ServerMessage::parse(&line.to_string()).unwrap_err();
            assert_eq!(e.code, IpcError::UNSUPPORTED_VERSION, "{line}");
        }
    }

    #[test]
    fn request_state_shape() {
        let s = RequestState::Done {
            result: json!({"ok": true}),
        };
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v, json!({"state": "done", "result": {"ok": true}}));
        let back: RequestState = serde_json::from_value(json!({"state": "unknown"})).unwrap();
        assert_eq!(back, RequestState::Unknown);
    }

    #[test]
    fn app_show_wire_shape() {
        let req = Request {
            id: RequestId::parse("r-9").unwrap(),
            call: Call::AppShow,
        };
        let v: Value = serde_json::from_str(&req.to_line().unwrap()).unwrap();
        assert_eq!(v, json!({"v": 8, "id": "r-9", "method": "app.show"}));
        let parsed = Request::parse(r#"{"v":8,"id":"a","method":"app.show"}"#).unwrap();
        assert_eq!(parsed.call, Call::AppShow);
        assert_eq!(parsed.call.method(), "app.show");
        assert!(!parsed.call.is_mutating());
        let v: Value = serde_json::from_str(&Event::ShowMainWindow.to_line().unwrap()).unwrap();
        assert_eq!(v, json!({"v": 8, "event": "show_main_window"}));
    }

    #[test]
    fn core_errors_keep_their_code() {
        for error in [
            Error::LibraryBusy("lib".into()),
            Error::DaemonUnavailable("synthetic".into()),
            Error::NotConfigured("synthetic".into()),
            Error::ConsentRequired("synthetic".into()),
            Error::ReauthRequired("synthetic".into()),
            Error::McpDisabled("synthetic".into()),
            Error::Service {
                code: broscribe_core::secret::TIMEOUT_CODE,
                message: "synthetic".into(),
            },
        ] {
            let code = error.code();
            let e = IpcError::from_core(&error);
            assert_eq!(e.code, code);
            assert_eq!(e.into_core().code(), code);
        }
    }
}

#[cfg(test)]
mod extension_tests {
    use super::*;
    #[test]
    fn daemon_extension_calls_round_trip() {
        for call in [
            Call::KickQueue,
            Call::DetectStatus,
            Call::DetectReload,
            Call::AudioCheck { seconds: 3.0 },
            Call::IntegrationsReload,
        ] {
            let request = Request::new(call);
            assert_eq!(
                Request::parse(&request.to_line().unwrap()).unwrap(),
                request
            );
            assert_eq!(
                request.call.is_mutating(),
                !matches!(request.call, Call::DetectStatus)
            );
        }
    }

    #[test]
    fn auto_stop_call_and_status_have_their_wire_shape() {
        let rid = RequestId::parse("auto-1").unwrap();
        let request = Request::new(Call::RecordAutoStop(RecordAutoStopParams {
            session_id: "s-1".into(),
            request_id: rid.clone(),
            expected_generation: 3,
            enabled: true,
            app_id: Some("zoom".into()),
        }));
        assert_eq!(request.id, rid);
        assert!(request.call.is_mutating());
        let line: Value = serde_json::from_str(&request.to_line().unwrap()).unwrap();
        assert_eq!(line["method"], "record.auto_stop");
        assert_eq!(line["params"]["expected_generation"], 3);
        assert_eq!(
            Request::parse(&request.to_line().unwrap()).unwrap(),
            request
        );
        let status = AutoStopStatus {
            enabled: true,
            app: Some(AutoStopApp {
                app_id: "zoom".into(),
                app_name: "Zoom".into(),
            }),
            generation: 3,
            reader_available: false,
            pending: Some(AutoStopPending {
                reason: AutoStopReason::MicReleasedRuleChanged,
                remaining_s: 7,
            }),
            candidates: Vec::new(),
        };
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["pending"]["reason"], "mic_released_rule_changed");
        assert_eq!(
            serde_json::from_value::<AutoStopStatus>(value).unwrap(),
            status
        );
        let start = Event::StartReader {
            session_id: "s-1".into(),
            app_id: "zoom".into(),
            pid: 7,
            elapsed_s: 1.0,
            reader_generation: 9,
        };
        assert!(start.to_line().unwrap().contains("\"reader_generation\":9"));
    }

    /// A refresh edits nothing: it is not logged as a mutation and parses
    /// back to itself.
    #[test]
    fn title_refresh_is_a_read_of_the_session_files() {
        let request = Request::new(Call::RecordTitleRefresh {
            session_id: "s-1".into(),
        });
        assert!(!request.call.is_mutating());
        let line = request.to_line().unwrap();
        assert!(line.contains("\"record.title_refresh\""));
        assert_eq!(Request::parse(&line).unwrap(), request);
    }

    #[test]
    fn lifecycle_calls_use_their_reconciliation_ids_and_stay_compatible() {
        let rid = RequestId::parse("title-1").unwrap();
        for call in [
            Call::RecordTitle {
                session_id: "s-1".into(),
                request_id: rid.clone(),
                title: "Synthetic sync".into(),
            },
            Call::RecordDiscard {
                session_id: "s-1".into(),
                request_id: rid.clone(),
            },
            Call::RecordRetry {
                session_id: "s-1".into(),
                request_id: rid.clone(),
            },
            Call::RecordResolveJournal {
                session_id: "s-1".into(),
                request_id: rid.clone(),
                resolution: JournalResolution::Adopt,
                library_root: "chosen-library".into(),
                confirmed: true,
            },
        ] {
            let request = Request::new(call);
            assert_eq!(request.id, rid);
            assert!(request.call.is_mutating());
            assert_eq!(
                Request::parse(&request.to_line().unwrap()).unwrap(),
                request
            );
        }
        // `record.tag` without a session is never run on whatever recording
        // happens to be active.
        let unfenced =
            Request::parse(r#"{"v":8,"id":"r-1","method":"record.tag","params":{"tags":["a"]}}"#)
                .unwrap_err();
        assert_eq!(unfenced.error.code, IpcError::INVALID_REQUEST);
    }

    /// Stopped jobs, preview, reader and index states are typed and
    /// distinct on the wire: a failed final job is never shown as still
    /// processing, a loading model never as unavailable.
    #[test]
    fn typed_states_have_their_wire_shape() {
        use serde_json::json;
        let failed = StoppedRecording {
            session_id: "s-1".into(),
            title: None,
            tags: Vec::new(),
            stopped_at: "2026-11-17T10:00:00Z".parse().unwrap(),
            stop_reason: None,
            state: StoppedRecordingState::Failed {
                error: Some("worker crashed".into()),
            },
        };
        let v = serde_json::to_value(&failed).unwrap();
        assert_eq!(
            v["state"],
            json!({"phase": "failed", "error": "worker crashed"})
        );
        assert_eq!(
            serde_json::from_value::<StoppedRecording>(v).unwrap(),
            failed
        );
        assert!(failed.state.is_pending_save());
        assert!(!StoppedRecordingState::Discarding.is_pending_save());
        let loading = PreviewState::Loading {
            model: Some("synthetic".into()),
            elapsed_ms: 1_500,
        };
        assert_eq!(
            serde_json::to_value(&loading).unwrap(),
            json!({"state": "loading", "model": "synthetic", "elapsed_ms": 1500})
        );
        assert!(!loading.is_available());
        assert!(PreviewState::Ready { model: "m".into() }.is_available());
        assert_eq!(
            serde_json::to_value(ReaderStatus::Degraded {
                reason: ReaderDegraded::Silent
            })
            .unwrap(),
            json!({"state": "degraded", "reason": "silent"})
        );
        assert_eq!(
            serde_json::to_value(ReaderStatus::Refused {
                refusal: broscribe_readers::ReaderStartRefusal::Attach
            })
            .unwrap(),
            json!({"state": "refused", "refusal": "attach"})
        );
        assert_eq!(
            serde_json::to_value(broscribe_readers::IncompleteReason::IdentityCap).unwrap(),
            json!("identity_cap")
        );
        assert_eq!(
            serde_json::to_value(IndexState::Pending { skipped: 2 }).unwrap(),
            json!({"state": "pending", "skipped": 2})
        );
    }

    /// Admission calls bypass the request log: a replayed acquire must
    /// never return a fence its connection already lost.
    #[test]
    fn install_admission_calls_round_trip_outside_the_request_log() {
        for call in [
            Call::InstallAcquire,
            Call::InstallConfirm { fence: 7 },
            Call::InstallRelease { fence: 7 },
        ] {
            assert!(call.is_install_admission());
            assert!(call.reconciliation_id().is_none());
            let request = Request::new(call);
            assert_eq!(
                Request::parse(&request.to_line().unwrap()).unwrap(),
                request
            );
        }
        assert!(!Call::RecordStatus.is_install_admission());
    }

    #[test]
    fn setup_levels_round_trip() {
        let level = SetupLevel {
            peak_dbfs: -6.0,
            rms_dbfs: -18.5,
        };
        for ev in [
            Event::SetupLevels {
                session: Some("sample-1".into()),
                me: level,
                others: None,
            },
            Event::SetupLevels {
                session: None,
                me: level,
                others: Some(SetupLevel {
                    peak_dbfs: -120.0,
                    rms_dbfs: -120.0,
                }),
            },
        ] {
            let line = ev.to_line().unwrap();
            assert_eq!(
                ServerMessage::parse(&line).unwrap(),
                ServerMessage::Event(ev)
            );
        }
        let v: Value = serde_json::from_str(
            &Event::SetupLevels {
                session: Some("s".into()),
                me: level,
                others: None,
            }
            .to_line()
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"v": 8, "event": "setup_levels", "session": "s",
                "me": {"peak_dbfs": -6.0, "rms_dbfs": -18.5}})
        );
    }
}
