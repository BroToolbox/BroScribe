//! Queue jobs the daemon creates for recordings, and the progress lines
//! `broscribe-worker run-one` prints (the format of `queue work
//! --json-progress`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::capture::ChunkCutoff;

/// Input of [`broscribe_core::queue::RECORD_FINAL_KIND`] and
/// [`broscribe_core::queue::RECORD_PROVISIONAL_KIND`] jobs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordJobInput {
    pub session_id: String,
    pub journal_dir: PathBuf,
    /// Provisional jobs only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutoff: Option<ChunkCutoff>,
}

impl RecordJobInput {
    /// `<session>@<mic chunk>.<system chunk>`: one provisional job per cutoff.
    pub fn provisional_key(session_id: &str, cutoff: &ChunkCutoff) -> String {
        let parts: Vec<String> = cutoff
            .chunks
            .iter()
            .map(|(track, i)| format!("{track}{i}"))
            .collect();
        format!("{session_id}@{}", parts.join("."))
    }
}

/// One progress line of a worker:
/// `{"event","job_id","kind","fraction","message"}`, `event` one of
/// `job_started`, `progress`, `job_done`, `job_failed`, `idle`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerLine {
    pub event: String,
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub fraction: Option<f64>,
    #[serde(default)]
    pub message: Option<String>,
}

impl WorkerLine {
    /// One stdout line of a worker; `None` for anything that is not a
    /// worker line (the worker's other output is never progress).
    pub fn parse(line: &str) -> Option<Self> {
        serde_json::from_str(line.trim_end()).ok()
    }

    /// The [`crate::protocol::JobProgress::phase`] this line reports; `None` for `idle` and
    /// unknown events.
    pub fn phase(&self) -> Option<&'static str> {
        Some(match self.event.as_str() {
            "job_started" => "started",
            "progress" => "progress",
            "job_done" => "done",
            "job_failed" => "failed",
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::live::Track;

    #[test]
    fn provisional_keys_differ_per_cutoff() {
        let cut = |m, s| ChunkCutoff {
            chunks: BTreeMap::from([(Track::Mic, m), (Track::System, s)]),
            recording_s: 0.0,
        };
        assert_eq!(
            RecordJobInput::provisional_key("s-1", &cut(3, 4)),
            "s-1@mic3.system4"
        );
        assert_ne!(
            RecordJobInput::provisional_key("s-1", &cut(3, 4)),
            RecordJobInput::provisional_key("s-1", &cut(3, 5))
        );
    }
}
