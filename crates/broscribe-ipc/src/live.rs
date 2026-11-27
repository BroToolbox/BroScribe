//! Daemon ↔ `broscribe-worker live <session>` (plan §12.1, §13.1).
//!
//! The daemon writes binary frames of 16 kHz mono f32 PCM per track to the
//! live worker's stdin; the worker answers with JSON lines ([`LiveEvent`]) on
//! stdout. It acknowledges every frame (at most once per second), keepalive
//! frames included, so a worker that stops answering for 30 s is known to be
//! stuck and is killed and restarted by the daemon.
//!
//! Frame layout (little endian), 20-byte header then the samples:
//!
//! | bytes | field                                         |
//! |-------|-----------------------------------------------|
//! | 0–3   | magic `BSF1`                                  |
//! | 4     | track: 0 mic, 1 system                        |
//! | 5     | kind: 0 audio, 1 keepalive (no samples)       |
//! | 6–7   | reserved, 0                                   |
//! | 8–15  | start sample (recording time × 16 kHz), u64   |
//! | 16–19 | sample count n, u32 (≤ [`MAX_FRAME_SAMPLES`]) |
//! | 20–   | n × f32                                       |
//!
//! A language frame (kind 2) carries the start sample of a session language
//! span and its BCP 47 tag as n UTF-8 bytes (≤ [`MAX_LANGUAGE_BYTES`]) in
//! place of samples; the track byte is 0.

use std::fmt;
use std::io::{self, ErrorKind, Read, Write};

use serde::{Deserialize, Serialize};

use crate::protocol::IpcError;
pub use broscribe_core::live_identity::{JournalPoint, LiveAction};

/// Sample rate of live frames.
pub const LIVE_RATE: u32 = 16_000;
pub const FRAME_MAGIC: [u8; 4] = *b"BSF1";
pub const FRAME_HEADER_BYTES: usize = 20;
/// Longest frame: 10 s.
pub const MAX_FRAME_SAMPLES: usize = LIVE_RATE as usize * 10;
/// Longest language tag in a language frame.
pub const MAX_LANGUAGE_BYTES: usize = 64;

const KIND_AUDIO: u8 = 0;
const KIND_KEEPALIVE: u8 = 1;
const KIND_LANGUAGE: u8 = 2;

/// One of the two never-mixed capture tracks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Track {
    /// The microphone: the user ("Me").
    Mic,
    /// System audio: the remote participants ("Others").
    System,
}

impl Track {
    pub const ALL: [Track; 2] = [Track::Mic, Track::System];

    pub fn index(self) -> usize {
        match self {
            Track::Mic => 0,
            Track::System => 1,
        }
    }

    pub fn from_index(i: u8) -> Option<Self> {
        match i {
            0 => Some(Track::Mic),
            1 => Some(Track::System),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Track::Mic => "mic",
            Track::System => "system",
        }
    }
}

impl fmt::Display for Track {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 16 kHz mono PCM of one track starting at `start_sample` (recording time).
#[derive(Clone, Debug, PartialEq)]
pub struct LiveFrame {
    pub track: Track,
    pub start_sample: u64,
    pub samples: Vec<f32>,
}

impl LiveFrame {
    pub fn end_sample(&self) -> u64 {
        self.start_sample + self.samples.len() as u64
    }

    pub fn start_s(&self) -> f64 {
        self.start_sample as f64 / f64::from(LIVE_RATE)
    }

    pub fn end_s(&self) -> f64 {
        self.end_sample() as f64 / f64::from(LIVE_RATE)
    }
}

/// What travels on the live worker's stdin.
#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    Audio(LiveFrame),
    /// Sent when no audio flowed for a while (paused, silence on both
    /// taps): the worker acknowledges it like audio.
    Keepalive,
    /// The session's language from `start_sample` on (a language span):
    /// recognition of later audio uses it until the next span.
    Language {
        start_sample: u64,
        language: String,
    },
}

/// Writes one frame with a single `write_all`.
pub fn write_frame(w: &mut dyn Write, frame: &Frame) -> io::Result<()> {
    let (track, kind, start, samples): (u8, u8, u64, &[f32]) = match frame {
        Frame::Audio(f) => {
            if f.samples.len() > MAX_FRAME_SAMPLES {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("live frame of {} samples is too long", f.samples.len()),
                ));
            }
            (
                f.track.index() as u8,
                KIND_AUDIO,
                f.start_sample,
                f.samples.as_slice(),
            )
        }
        Frame::Keepalive => (0, KIND_KEEPALIVE, 0, &[]),
        Frame::Language {
            start_sample,
            language,
        } => {
            if language.len() > MAX_LANGUAGE_BYTES {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    format!("live language tag of {} bytes is too long", language.len()),
                ));
            }
            let mut buf = Vec::with_capacity(FRAME_HEADER_BYTES + language.len());
            buf.extend_from_slice(&FRAME_MAGIC);
            buf.extend_from_slice(&[0, KIND_LANGUAGE, 0, 0]);
            buf.extend_from_slice(&start_sample.to_le_bytes());
            buf.extend_from_slice(&(language.len() as u32).to_le_bytes());
            buf.extend_from_slice(language.as_bytes());
            return w.write_all(&buf);
        }
    };
    let mut buf = Vec::with_capacity(FRAME_HEADER_BYTES + samples.len() * 4);
    buf.extend_from_slice(&FRAME_MAGIC);
    buf.push(track);
    buf.push(kind);
    buf.extend_from_slice(&[0, 0]);
    buf.extend_from_slice(&start.to_le_bytes());
    buf.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    for s in samples {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    w.write_all(&buf)
}

/// Reads one frame; `None` at a clean end of stream (between frames).
pub fn read_frame(r: &mut dyn Read) -> io::Result<Option<Frame>> {
    let mut header = [0u8; FRAME_HEADER_BYTES];
    let mut filled = 0;
    while filled < header.len() {
        match r.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(n) => filled += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let invalid = |m: String| io::Error::new(ErrorKind::InvalidData, m);
    if header[..4] != FRAME_MAGIC {
        return Err(invalid("bad live frame magic".into()));
    }
    let mut start = [0u8; 8];
    start.copy_from_slice(&header[8..16]);
    let mut count = [0u8; 4];
    count.copy_from_slice(&header[16..20]);
    let n = u32::from_le_bytes(count) as usize;
    if n > MAX_FRAME_SAMPLES {
        return Err(invalid(format!("live frame of {n} samples is too long")));
    }
    match header[5] {
        KIND_KEEPALIVE => {
            if n != 0 {
                return Err(invalid("keepalive frame with samples".into()));
            }
            Ok(Some(Frame::Keepalive))
        }
        KIND_AUDIO => {
            let track = Track::from_index(header[4])
                .ok_or_else(|| invalid(format!("unknown track {}", header[4])))?;
            let mut bytes = vec![0u8; n * 4];
            r.read_exact(&mut bytes)?;
            let samples = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            Ok(Some(Frame::Audio(LiveFrame {
                track,
                start_sample: u64::from_le_bytes(start),
                samples,
            })))
        }
        KIND_LANGUAGE => {
            if n > MAX_LANGUAGE_BYTES {
                return Err(invalid(format!(
                    "live language tag of {n} bytes is too long"
                )));
            }
            let mut bytes = vec![0u8; n];
            r.read_exact(&mut bytes)?;
            let language = String::from_utf8(bytes)
                .map_err(|_| invalid("live language tag is not UTF-8".into()))?;
            Ok(Some(Frame::Language {
                start_sample: u64::from_le_bytes(start),
                language,
            }))
        }
        k => Err(invalid(format!("unknown live frame kind {k}"))),
    }
}

/// Who spoke a live segment by track alone (design §6.1): the mic track is
/// Me, the system track Others. Speaker identity in the live preview travels
/// separately, as provisional [`LiveTurn`]s.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LiveSpeaker {
    Me,
    Others,
    Person { person_id: String, name: String },
}

/// One recognised word; times are recording seconds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveWord {
    pub text: String,
    pub start_s: f64,
    pub end_s: f64,
}

/// A recognised live segment; times are recording seconds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveSegment {
    pub track: Track,
    pub start_s: f64,
    pub end_s: f64,
    pub text: String,
    pub speaker: LiveSpeaker,
    /// The recogniser's word times, when it reports token times.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<LiveWord>,
}

/// One JSON line on the live worker's stdout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LiveEvent {
    /// Still loading models, about once a second until [`LiveEvent::Ready`]
    /// and never after it: `elapsed_ms` since the worker began loading. A
    /// sign of life before the first acknowledgement; the daemon's total
    /// load deadline still applies, so slow loading is never unbounded.
    Loading {
        model: String,
        elapsed_ms: u64,
    },
    /// Models loaded; frames are being consumed.
    Ready {
        model: String,
    },
    /// Frames consumed up to this recording time (latest track end).
    Ack {
        received_until_s: f64,
    },
    Segment(LiveSegment),
    /// A recoverable problem (a segment failed to decode).
    Warning {
        message: String,
    },
    /// Terminal: the worker cannot run for this recording (its declared
    /// live model is not installed, or its config or profile is invalid)
    /// and exits after this line. The daemon shows the preview as
    /// unavailable for `reason` and never restarts it in a loop; the
    /// recording itself is unaffected.
    Unavailable {
        reason: crate::protocol::WorkerProblem,
    },
}

impl LiveEvent {
    pub fn to_line(&self) -> Result<String, IpcError> {
        serde_json::to_string(self).map_err(|e| IpcError::protocol(format!("serialising: {e}")))
    }

    pub fn parse(line: &str) -> Result<Self, IpcError> {
        serde_json::from_str(line.trim_end())
            .map_err(|e| IpcError::protocol(format!("invalid live event: {e}")))
    }
}

// ---- live speaker analysis (plan D) ----
//
// `broscribe-worker live-speakers <session>` is a companion of the live ASR
// worker: it reads only closed, fsynced journal chunks (never the capture
// pipe), diarizes bounded windows and embeds clean speech at utility
// priority. Its first stdin line is a [`SpeakersStart`]; later lines are
// [`SpeakersInput`]; end of stdin stops it. It answers with
// [`SpeakersEvent`] JSON lines. No embedding, audio or name crosses this
// boundary: spans, opaque speaker numbers and person IDs only.

/// Bounded live speaker-analysis settings. Data, not model constants: the
/// defaults (10 s window every 5 s, 60 s of audio and 200 turns kept) come
/// from synthetic-replay performance measurements, not from accuracy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LiveIdentitySettings {
    /// Run the companion worker at all.
    pub enabled: bool,
    /// Audio diarized per analysis (seconds).
    pub window_s: f64,
    /// A new window every this much new closed audio (seconds).
    pub hop_s: f64,
    /// Most audio kept per track (seconds); older backlog is skipped and
    /// reported as a preview gap.
    pub retain_s: f64,
    /// Most preview turns the daemon keeps.
    pub max_turns: usize,
    /// Most speaker embeddings computed per window.
    pub max_embeds_per_window: usize,
    /// Most embeddings kept per provisional voice.
    pub max_embeds_per_voice: usize,
    /// Most provisional voices kept per track.
    pub max_voices: usize,
    /// Most session-local samples (from turns the user named) kept.
    pub max_session_samples: usize,
    /// Kill and restart a worker with pending audio and no completed window
    /// for this long (seconds); distinct from its heartbeats.
    pub progress_timeout_s: f64,
    /// Journal poll interval (milliseconds).
    pub poll_ms: u64,
}

impl Default for LiveIdentitySettings {
    fn default() -> Self {
        Self {
            enabled: true,
            window_s: 10.0,
            hop_s: 5.0,
            retain_s: 60.0,
            max_turns: 200,
            max_embeds_per_window: 3,
            max_embeds_per_voice: 8,
            max_voices: 16,
            max_session_samples: 32,
            progress_timeout_s: 30.0,
            poll_ms: 500,
        }
    }
}

/// The shortest accepted [`LiveIdentitySettings::hop_s`] (seconds). A
/// shorter hop rounds to no live samples at all (the same window analysed
/// forever) or to a few (thousands of windows per poll). The one floor for
/// both the daemon and the live-speakers driver, which each enforce it
/// through [`LiveIdentitySettings::validate`]: the daemon reports such
/// settings as invalid once instead of restarting a worker that refuses them.
pub const MIN_HOP_S: f64 = 0.25;

impl LiveIdentitySettings {
    /// Refuses settings that could not bound memory or progress.
    pub fn validate(&self) -> Result<(), String> {
        let positive = |v: f64| v.is_finite() && v > 0.0;
        if !positive(self.window_s) || !positive(self.hop_s) || self.hop_s > self.window_s {
            return Err("live identity needs 0 < hop_s <= window_s".into());
        }
        if self.hop_s < MIN_HOP_S {
            return Err(format!(
                "live identity hop_s must be at least {MIN_HOP_S} s"
            ));
        }
        if !positive(self.retain_s) || self.retain_s < self.window_s + self.hop_s {
            return Err("live identity retain_s must hold a window and a hop".into());
        }
        if self.retain_s > 600.0 || self.window_s > 120.0 {
            return Err("live identity window or retention is too long".into());
        }
        if self.max_turns == 0
            || self.max_embeds_per_voice == 0
            || self.max_voices == 0
            || !positive(self.progress_timeout_s)
            || self.poll_ms == 0
        {
            return Err("live identity limits must be positive".into());
        }
        Ok(())
    }

    /// `seconds` as live samples.
    pub fn samples(seconds: f64) -> u64 {
        (seconds.max(0.0) * f64::from(LIVE_RATE)).round() as u64
    }
}

/// First stdin line of the live-speakers worker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpeakersStart {
    pub session_id: String,
    /// The session's journal folder.
    pub journal_dir: std::path::PathBuf,
    /// Supervisor restart counter; stamps every window.
    pub epoch: u64,
    pub settings: LiveIdentitySettings,
}

/// Later stdin lines of the live-speakers worker. Both carry the session
/// decision number they come from: the worker applies, per turn, only a
/// message newer than the last one it applied, so a delayed older message
/// (a Name delivered after its Clear) never restores evidence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SpeakersInput {
    /// The user named or confirmed this exact range as `person_id`: while
    /// its audio is still retained, its clean speech becomes a transient,
    /// session-local sample for suggestions on later speech. Never stored,
    /// never a voiceprint. `forget_fence` is the person's opaque forget
    /// token read in the decision's own locked commit: the sample counts
    /// only while the library's current token is still equal (any later
    /// forget changes it). Missing: unknown, never a sample.
    SessionSample {
        turn_id: String,
        decision: u64,
        #[serde(default)]
        forget_fence: Option<String>,
        track: Track,
        from_sample: u64,
        to_sample: u64,
        person_id: String,
    },
    /// Decision `decision` cleared or replaced the turn's session sample.
    RevokeSessionSample { turn_id: String, decision: u64 },
}

/// A diarized span of one track (live samples, recording time) and the
/// epoch-scoped provisional voice it belongs to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpeakerSpan {
    pub from_sample: u64,
    pub to_sample: u64,
    /// Provisional voice number, unique within the track and epoch. Not a
    /// window cluster label and never a person.
    pub voice: u32,
    /// Another voice was diarized over part of it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub overlap: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<JournalPoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<JournalPoint>,
}

/// Where a suggestion's evidence came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuggestionSource {
    /// The library's stored voice samples.
    Library,
    /// Turns the user named earlier in this recording (transient).
    Session,
}

/// A provisional voice of a window, with its person suggestion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VoiceSuggestion {
    pub voice: u32,
    /// Clean (non-overlapped) speech of the voice embedded so far, seconds.
    pub clean_s: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SuggestionSource>,
    /// The speaker model has no calibrated naming thresholds: this is only
    /// the closest stored voice, not a thresholded match. The user's own
    /// person is suggested this way only under
    /// `[live] allow_uncalibrated_me_suggestions`; never an automatic name
    /// or enrollment.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub uncalibrated: bool,
}

/// One analysed window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpeakerWindow {
    pub epoch: u64,
    pub track: Track,
    pub from_sample: u64,
    pub to_sample: u64,
    pub spans: Vec<SpeakerSpan>,
    pub voices: Vec<VoiceSuggestion>,
    /// Voice-evidence fingerprint the suggestions were computed with.
    pub evidence: String,
    pub elapsed_ms: u64,
    /// Older audio was skipped to catch up.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub behind: bool,
}

/// One JSON line on the live-speakers worker's stdout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SpeakersEvent {
    /// Still loading models, about once a second until
    /// [`SpeakersEvent::Ready`] and never after it. Not a heartbeat: the
    /// daemon's total load deadline still applies.
    Loading {
        diarizer: String,
        embedder: String,
        elapsed_ms: u64,
    },
    /// Models loaded.
    Ready {
        epoch: u64,
        diarizer: String,
        embedder: String,
    },
    /// Sign of life, about once a second, from a thread independent of
    /// inference: closed audio waiting for analysis and how long the
    /// current window has been running (0 when idle).
    Heartbeat {
        pending_s: f64,
        busy_ms: u64,
    },
    Window(SpeakerWindow),
    /// Audio of `track` in this range was never analysed (backlog skipped,
    /// journal gap); the recording itself is unaffected.
    Gap {
        track: Track,
        from_sample: u64,
        to_sample: u64,
    },
    /// Whether a session sample could be taken (its audio may be gone).
    SessionSample {
        turn_id: String,
        used: bool,
    },
    Warning {
        message: String,
    },
}

impl SpeakersEvent {
    pub fn to_line(&self) -> Result<String, IpcError> {
        serde_json::to_string(self).map_err(|e| IpcError::protocol(format!("serialising: {e}")))
    }

    pub fn parse(line: &str) -> Result<Self, IpcError> {
        serde_json::from_str(line.trim_end())
            .map_err(|e| IpcError::protocol(format!("invalid speakers event: {e}")))
    }
}

// ---- live turns: daemon ↔ clients ----

/// What a live turn shows about its speaker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LiveTurnStatus {
    /// Track only (speaker analysis has not covered it, or is unavailable).
    Preview,
    /// Analysed, but not attributable to one voice (see `unresolved`).
    Unresolved,
    /// One provisional voice, no person suggested.
    Voice,
    /// One provisional voice with a person suggestion.
    Suggested,
    /// The user named this turn (exactly its range).
    Named,
}

/// Why an analysed turn stays unresolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedReason {
    /// Its words fall on more than one voice.
    Mixed,
    /// Voices overlap in it.
    Overlap,
    /// Too little of it lies on any diarized voice.
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LivePerson {
    pub person_id: String,
    pub name: String,
}

/// A suggestion, with where its evidence came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveSuggestion {
    pub person: LivePerson,
    pub source: SuggestionSource,
    /// See [`VoiceSuggestion::uncalibrated`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub uncalibrated: bool,
}

/// One provisional live-preview turn: an ASR segment of one track,
/// identified by its source anchor. `revision` grows with every change of
/// what it shows; actions name the revision they were taken on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveTurn {
    pub session_id: String,
    /// `<track>-<from_sample>`: stable across windows and worker restarts.
    pub turn_id: String,
    pub revision: u64,
    /// Speaker-worker epoch of the shown voice (0: none).
    pub epoch: u64,
    pub track: Track,
    pub from_sample: u64,
    pub to_sample: u64,
    pub start_s: f64,
    pub end_s: f64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub words: Vec<LiveWord>,
    pub status: LiveTurnStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unresolved: Option<UnresolvedReason>,
    /// Provisional voice number within (epoch, track); display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<LiveSuggestion>,
    /// The user's decision for exactly this turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub named: Option<LivePerson>,
    /// Voice-evidence fingerprint of the suggestion; pickers send it back.
    pub evidence: String,
}

/// Live speaker-analysis availability: distinct from the ASR preview
/// ([`crate::PreviewState`]) and the meeting reader.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LiveIdentityState {
    pub status: LiveIdentityStatus,
    /// Speaker-worker epoch the status belongs to (0: none started).
    pub epoch: u64,
}

impl LiveIdentityState {
    /// Suggestions and voices are being produced now.
    pub fn is_available(&self) -> bool {
        matches!(
            self.status,
            LiveIdentityStatus::Ready | LiveIdentityStatus::Behind
        )
    }
}

/// What live speaker analysis is doing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LiveIdentityStatus {
    /// No recording, or its analysis ended.
    #[default]
    Idle,
    /// Turned off in the settings (`[live] identity = false`): the preview
    /// shows tracks only; capture and ASR go on, human decisions stay.
    Disabled,
    /// The worker loads its models (`elapsed_ms` since it started).
    Loading {
        elapsed_ms: u64,
    },
    Ready,
    /// Running, but older audio was skipped to catch up.
    Behind,
    /// The worker failed and restarts; turns are track-only meanwhile.
    Restarting {
        reason: crate::protocol::WorkerProblem,
    },
    /// Gave up for this recording.
    Unavailable {
        reason: crate::protocol::WorkerProblem,
    },
}

/// Result of `record.live`: the authoritative rows after a reconnect or lag.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveSnapshot {
    pub session_id: String,
    pub identity: LiveIdentityState,
    pub evidence: String,
    /// In the order the agent published them, oldest first: the order it
    /// drops them in at its turn bound (`LiveIdentitySettings::max_turns`).
    /// The two tracks publish independently, so this is not source-time
    /// order; a client keeping the same bound seeds it from this order and
    /// sorts by start time only what it displays.
    pub turns: Vec<LiveTurn>,
}

/// Params of `record.live_attribute`: one decision on one shown turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveAttributeParams {
    pub session_id: String,
    pub request_id: crate::protocol::RequestId,
    pub turn_id: String,
    /// The revision and epoch the user saw.
    pub revision: u64,
    pub epoch: u64,
    /// The picker's evidence stamp.
    pub evidence: String,
    pub action: LiveAction,
    /// Name: a person ID or e-mail, or a plain typed name (a new name-only
    /// person when nobody has it). Confirm: the suggested person's ID.
    /// Clear: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_ref: Option<String>,
}

/// Result of `record.live_attribute`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveAttributeOutcome {
    pub decision: u64,
    pub turn: LiveTurn,
    /// A name-only person this decision created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_person_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let frames = [
            Frame::Audio(LiveFrame {
                track: Track::System,
                start_sample: 48_000,
                samples: vec![0.0, 0.5, -0.25, 1.0],
            }),
            Frame::Keepalive,
            Frame::Language {
                start_sample: 32_000,
                language: "pt-BR".into(),
            },
            Frame::Audio(LiveFrame {
                track: Track::Mic,
                start_sample: 0,
                samples: Vec::new(),
            }),
        ];
        let mut buf = Vec::new();
        for f in &frames {
            write_frame(&mut buf, f).unwrap();
        }
        let mut r = buf.as_slice();
        for f in &frames {
            assert_eq!(read_frame(&mut r).unwrap().as_ref(), Some(f));
        }
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn truncated_and_corrupt_frames_fail() {
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &Frame::Audio(LiveFrame {
                track: Track::Mic,
                start_sample: 1,
                samples: vec![0.1; 8],
            }),
        )
        .unwrap();
        let mut cut = &buf[..buf.len() - 3];
        assert!(read_frame(&mut cut).is_err());
        let mut bad = buf.clone();
        bad[0] = b'X';
        assert!(read_frame(&mut bad.as_slice()).is_err());
        let too_long = Frame::Audio(LiveFrame {
            track: Track::Mic,
            start_sample: 0,
            samples: vec![0.0; MAX_FRAME_SAMPLES + 1],
        });
        assert!(write_frame(&mut Vec::<u8>::new(), &too_long).is_err());
    }

    #[test]
    fn live_events_round_trip() {
        for ev in [
            LiveEvent::Loading {
                model: "parakeet-tdt-0.6b-v3".into(),
                elapsed_ms: 1_000,
            },
            LiveEvent::Ready {
                model: "parakeet-tdt-0.6b-v3".into(),
            },
            LiveEvent::Ack {
                received_until_s: 12.5,
            },
            LiveEvent::Segment(LiveSegment {
                track: Track::System,
                start_s: 1.0,
                end_s: 3.0,
                text: "good morning".into(),
                speaker: LiveSpeaker::Others,
                words: vec![LiveWord {
                    text: "good".into(),
                    start_s: 1.0,
                    end_s: 1.4,
                }],
            }),
            LiveEvent::Unavailable {
                reason: crate::protocol::WorkerProblem::ModelMissing,
            },
        ] {
            assert_eq!(LiveEvent::parse(&ev.to_line().unwrap()).unwrap(), ev);
        }
    }

    /// The agreed pre-Ready loading lines of both live workers.
    #[test]
    fn loading_lines_have_their_agreed_wire_shape() {
        let asr = LiveEvent::parse(r#"{"type":"loading","model":"m","elapsed_ms":2000}"#).unwrap();
        assert_eq!(
            asr,
            LiveEvent::Loading {
                model: "m".into(),
                elapsed_ms: 2_000
            }
        );
        let speakers = SpeakersEvent::Loading {
            diarizer: "d".into(),
            embedder: "e".into(),
            elapsed_ms: 3_000,
        };
        let line = speakers.to_line().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap(),
            serde_json::json!({"type":"loading","diarizer":"d","embedder":"e","elapsed_ms":3000})
        );
        assert_eq!(SpeakersEvent::parse(&line).unwrap(), speakers);
        let off = LiveIdentityState {
            status: LiveIdentityStatus::Disabled,
            epoch: 0,
        };
        assert!(!off.is_available());
        assert_eq!(
            serde_json::to_value(&off).unwrap(),
            serde_json::json!({"status": {"state": "disabled"}, "epoch": 0})
        );
    }

    #[test]
    fn record_live_speakers_protocol_round_trips_without_audio_or_names() {
        let window = SpeakersEvent::Window(SpeakerWindow {
            epoch: 2,
            track: Track::System,
            from_sample: 0,
            to_sample: 160_000,
            spans: vec![SpeakerSpan {
                from_sample: 16_000,
                to_sample: 48_000,
                voice: 1,
                overlap: false,
                from: Some(JournalPoint {
                    seq: 0,
                    frame: 48_000,
                }),
                to: Some(JournalPoint {
                    seq: 0,
                    frame: 144_000,
                }),
            }],
            voices: vec![VoiceSuggestion {
                voice: 1,
                clean_s: 2.0,
                person_id: None,
                source: None,
                uncalibrated: false,
            }],
            evidence: "abc".into(),
            elapsed_ms: 70,
            behind: false,
        });
        let line = window.to_line().unwrap();
        assert!(!line.contains("embedding") && !line.contains("name"));
        assert_eq!(SpeakersEvent::parse(&line).unwrap(), window);
        let input = SpeakersInput::SessionSample {
            turn_id: "system-16000".into(),
            decision: 3,
            forget_fence: Some("never".into()),
            track: Track::System,
            from_sample: 16_000,
            to_sample: 48_000,
            person_id: "p".into(),
        };
        let line = serde_json::to_string(&input).unwrap();
        assert_eq!(serde_json::from_str::<SpeakersInput>(&line).unwrap(), input);
        assert!(LiveIdentitySettings::default().validate().is_ok());
        let slow = LiveIdentitySettings {
            hop_s: 20.0,
            ..Default::default()
        };
        assert!(slow.validate().is_err());
    }

    /// The daemon refuses exactly the hops the live-speakers driver
    /// refuses: below [`MIN_HOP_S`] (and non-finite) is invalid, the
    /// minimum itself is accepted.
    #[test]
    fn record_live_identity_settings_refuse_hops_below_the_driver_minimum() {
        let with_hop = |hop_s| LiveIdentitySettings {
            hop_s,
            ..Default::default()
        };
        assert_eq!(MIN_HOP_S, 0.25);
        for hop_s in [1e-6, 0.01, 0.2, 0.249, f64::NAN, f64::INFINITY, 0.0, -1.0] {
            assert!(with_hop(hop_s).validate().is_err(), "{hop_s}");
        }
        for hop_s in [MIN_HOP_S, 1.0, 5.0] {
            assert!(with_hop(hop_s).validate().is_ok(), "{hop_s}");
        }
    }
}
