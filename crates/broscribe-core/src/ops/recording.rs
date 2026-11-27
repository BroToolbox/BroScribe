//! Shared playback file lookup and cache jobs.

use std::path::{Path, PathBuf};

use crate::queue::{JobId, PLAYBACK_KIND, Queue};
use crate::retention::PLAYBACK_CACHE_DIR;
use crate::schema::{RecordingState, VoiceprintKind, VoiceprintSource};
use crate::{Error, Library, MeetingId, PersonId, Result};

/// Folder holding the retained source of enrollment voice samples.
pub const ENROLLMENT_DIR: &str = "enrollment";

/// Queue key of the `playback-cache` job for one recording.
pub fn playback_cache_key(meeting: &MeetingId, sha256: &str) -> String {
    format!("{meeting}/{sha256}")
}

/// Queues the `playback-cache` job for the recording.
pub fn enqueue_playback_cache(
    lib: &Library,
    meeting_id: &MeetingId,
    sha256: &str,
) -> Result<JobId> {
    lib.write(|tx| {
        Queue::open(tx.root())?.enqueue(
            tx,
            PLAYBACK_KIND,
            &playback_cache_key(meeting_id, sha256),
            serde_json::json!({ "meeting_id": meeting_id }),
        )
    })
}

/// `<cache_dir>/playback/<sha256>.wav` (the folder retention cleans,
/// [`PLAYBACK_CACHE_DIR`]).
pub fn playback_cache_path(cache_dir: &Path, sha256: &str) -> PathBuf {
    cache_dir
        .join(PLAYBACK_CACHE_DIR)
        .join(format!("{sha256}.wav"))
}

// ---- progress events ----

/// Where a voice sample's audio lives.
#[derive(Clone, Debug, PartialEq)]
pub enum ClipSource {
    /// A meeting's recording; `sha256` pins the recording the sample came
    /// from, when known.
    Meeting {
        meeting_id: MeetingId,
        sha256: Option<String>,
    },
    /// The kept enrollment passage `people/<id>/enrollment/<sha256>.wav`.
    Enrollment { person_id: PersonId, sha256: String },
}

/// A bounded range of a recording, in seconds from its start.
#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    pub source: ClipSource,
    pub start_s: f64,
    pub end_s: f64,
}

/// The playable cache of a recording another format than the player's.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackCache {
    pub path: PathBuf,
    pub exists: bool,
    pub meeting_id: MeetingId,
    pub sha256: String,
}

/// A clip's files as found on disk.
#[derive(Clone, Debug, PartialEq)]
pub enum ClipFiles {
    Found {
        audio: PathBuf,
        /// `None` for files that are always playable (enrollment WAVs).
        cache: Option<PlaybackCache>,
        mix_required: bool,
    },
    Unavailable(String),
}

/// Finds the files of `source` (reads the disk: background executor only).
pub fn locate_clip(lib: &Library, cache_dir: &Path, source: &ClipSource) -> Result<ClipFiles> {
    match source {
        ClipSource::Enrollment { person_id, sha256 } => {
            let audio = lib
                .person_dir(person_id)
                .join(ENROLLMENT_DIR)
                .join(format!("{sha256}.wav"));
            Ok(if audio.is_file() {
                ClipFiles::Found {
                    audio,
                    cache: None,
                    mix_required: false,
                }
            } else {
                ClipFiles::Unavailable("The voice reading was deleted.".into())
            })
        }
        ClipSource::Meeting { meeting_id, sha256 } => {
            let meeting = match lib.load_meeting(meeting_id) {
                Ok(m) => m,
                Err(Error::NotFound(_)) => {
                    return Ok(ClipFiles::Unavailable("The meeting was deleted.".into()));
                }
                Err(e) => return Err(e),
            };
            let Some(rec) = meeting.recording.as_ref() else {
                return Ok(ClipFiles::Unavailable(
                    "This meeting has no recording.".into(),
                ));
            };
            if sha256.as_ref().is_some_and(|s| *s != rec.sha256) {
                return Ok(ClipFiles::Unavailable(
                    "The recording this sample came from is no longer in the meeting.".into(),
                ));
            }
            match rec.state {
                RecordingState::Available => {}
                RecordingState::Deleted => {
                    return Ok(ClipFiles::Unavailable("The recording was deleted.".into()));
                }
                RecordingState::Missing => {
                    return Ok(ClipFiles::Unavailable(
                        "The recording file is missing.".into(),
                    ));
                }
            }
            let addressed = match rec.sources.first() {
                Some(source) => Some(source.path.clone()),
                None => lib.meeting_audio_path(&meeting)?,
            };
            let Some(audio) = addressed else {
                return Ok(ClipFiles::Unavailable(
                    "This meeting has no recording.".into(),
                ));
            };
            if (rec.sources.is_empty() && !audio.is_file())
                || rec.sources.iter().any(|source| !source.path.is_file())
            {
                return Ok(ClipFiles::Unavailable(
                    "The recording file is missing.".into(),
                ));
            }
            let path = playback_cache_path(cache_dir, &rec.sha256);
            let exists = path.is_file();
            Ok(ClipFiles::Found {
                audio,
                mix_required: !rec.sources.is_empty(),
                cache: Some(PlaybackCache {
                    path,
                    exists,
                    meeting_id: meeting.id.clone(),
                    sha256: rec.sha256.clone(),
                }),
            })
        }
    }
}

/// Where a voiceprint's audio can be replayed from.
pub fn clip_of(person: &PersonId, src: &VoiceprintSource) -> Option<Clip> {
    let valid = src.start >= 0.0 && src.end > src.start;
    if !valid {
        return None;
    }
    let source = match (src.kind, &src.meeting_id, &src.sha256) {
        (_, Some(meeting_id), sha256) => ClipSource::Meeting {
            meeting_id: meeting_id.clone(),
            sha256: sha256.clone(),
        },
        (VoiceprintKind::Enrollment, None, Some(sha256)) => ClipSource::Enrollment {
            person_id: person.clone(),
            sha256: sha256.clone(),
        },
        _ => return None,
    };
    Some(Clip {
        source,
        start_s: src.start,
        end_s: src.end,
    })
}
