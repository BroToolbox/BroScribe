//! `broscribe bench` (design §12, plan §4/§9.11): runs model and profile
//! candidates on a local labelled set and scores them.
//!
//! - A labelled set lives in `<data_dir>/bench/<set>/set.json` and never
//!   leaves the machine. [`add_excerpt`] copies the current transcript of a
//!   meeting range as a starting reference (`reference_checked = false`
//!   until the user corrects it by hand) and the turns of confirmed speakers.
//! - [`run`] starts one worker process per candidate through the caller's
//!   launcher (`broscribe bench worker …`, see [`worker`]); each worker loads
//!   only its candidate's models and reports its own peak memory. Workers
//!   return hypotheses; scoring happens here, so reference text never
//!   reaches a child's argv.
//! - With `--calibrate`, a separate calibration-only child fits each embedding
//!   model's thresholds before fresh measurement workers start. Thresholds are
//!   stored per model; only the active model re-resolves the library.
//! - Results (figures only, no text) go to `<data_dir>/bench/results/`.

pub mod metrics;
#[cfg(test)]
mod tests;
pub mod worker;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use broscribe_core::config::{Config, Profile, Thresholds};
use broscribe_core::schema::{self, Assignment, Meeting, Person};
use broscribe_core::{Error, MeetingId, Result, fsutil};
use broscribe_models::{Registry, Role};
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

use crate::Env;
use crate::hints::group_hints;
use crate::manifest::{self, Manifest};
use metrics::{
    COLLAR_S, DiarErrors, LabelledTurn, Metric, WordErrors, clusters_per_speaker,
    diarization_errors, naming, normalize_words, word_errors,
};
use worker::WorkerReport;

pub const SET_FILE: &str = "set.json";
pub const SET_SCHEMA_VERSION: u32 = 1;
pub const RESULTS_SCHEMA_VERSION: u32 = 1;
/// Candidate spec for the active profile (`default` is accepted too).
pub const ACTIVE_CANDIDATE: &str = "active";

/// `<data_dir>/bench`.
pub fn bench_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("bench")
}

/// `<data_dir>/bench/results`.
pub fn results_dir(data_dir: &Path) -> PathBuf {
    bench_dir(data_dir).join("results")
}

/// Set names are folder names: 1–64 of `A–Z a–z 0–9 _ -`, not `results`.
pub fn validate_set_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        && name != "results";
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidInput(format!(
            "not a bench set name (1-64 letters, digits, '_' or '-'; not \"results\"): {name:?}"
        )))
    }
}

/// `<data_dir>/bench/<set>/set.json`.
pub fn set_path(data_dir: &Path, set: &str) -> Result<PathBuf> {
    validate_set_name(set)?;
    Ok(bench_dir(data_dir).join(set).join(SET_FILE))
}

/// Where reference speaker turns came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ReferenceSource {
    /// Confirmed speakers of the meeting ([`add_excerpt`]).
    #[serde(rename = "confirmed")]
    Confirmed,
    /// Written by hand in `set.json`.
    #[serde(rename = "manual")]
    Manual,
    /// A manifest's speaker hints (`bench run --manifest`): DER/JER are
    /// labelled "hint-referenced".
    #[serde(rename = "hint-referenced")]
    Hints,
}

/// A labelled set (`set.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BenchSet {
    pub schema_version: u32,
    #[serde(default)]
    pub excerpts: Vec<Excerpt>,
}

impl Default for BenchSet {
    fn default() -> Self {
        Self {
            schema_version: SET_SCHEMA_VERSION,
            excerpts: Vec::new(),
        }
    }
}

/// One labelled range of a meeting or of an audio file, in seconds of that
/// recording's timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Excerpt {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meeting_id: Option<MeetingId>,
    /// An audio file outside the library (instead of `meeting_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_path: Option<PathBuf>,
    pub start: f64,
    pub end: f64,
    /// Reference text; WER is computed only once `reference_checked`.
    #[serde(default)]
    pub reference_text: Option<String>,
    /// Set to `true` after correcting `reference_text` by hand.
    #[serde(default)]
    pub reference_checked: bool,
    /// Reference speaker turns (`label` = person ID, or any stable label
    /// for someone outside the library).
    #[serde(default)]
    pub reference_turns: Option<Vec<LabelledTurn>>,
    /// Origin of `reference_turns` (`manual` when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns_source: Option<ReferenceSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<FixedOffset>>,
}

impl Excerpt {
    fn check(&self) -> Result<()> {
        let invalid = |why: &str| {
            Err(Error::InvalidInput(format!(
                "bench excerpt {}: {why}",
                self.id
            )))
        };
        if self.meeting_id.is_some() == self.audio_path.is_some() {
            return invalid("needs exactly one of meeting_id and audio_path");
        }
        if !(self.start.is_finite() && self.end.is_finite() && 0.0 <= self.start)
            || self.end <= self.start
        {
            return invalid("start must be ≥ 0 and before end");
        }
        Ok(())
    }

    /// Groups excerpts of one recording (fixes are counted per meeting).
    fn source_key(&self) -> String {
        match (&self.meeting_id, &self.audio_path) {
            (Some(m), _) => m.to_string(),
            (None, Some(p)) => p.display().to_string(),
            (None, None) => self.id.clone(),
        }
    }
}

/// Reads a set; `NotFound` when it does not exist.
pub fn load_set(data_dir: &Path, set: &str) -> Result<BenchSet> {
    let path = set_path(data_dir, set)?;
    let s: BenchSet = fsutil::read_json(&path)?;
    if s.schema_version > SET_SCHEMA_VERSION {
        return Err(Error::Schema(format!(
            "{}: schema_version {} is newer than this version of BroScribe reads ({SET_SCHEMA_VERSION})",
            path.display(),
            s.schema_version
        )));
    }
    let mut ids = BTreeSet::new();
    for e in &s.excerpts {
        e.check()?;
        if !ids.insert(e.id.as_str()) {
            return Err(Error::InvalidInput(format!(
                "{}: excerpt ID {} appears twice",
                path.display(),
                e.id
            )));
        }
    }
    Ok(s)
}

fn save_set(data_dir: &Path, set: &str, value: &BenchSet) -> Result<PathBuf> {
    let path = set_path(data_dir, set)?;
    let dir = path.parent().expect("set path has a folder");
    std::fs::create_dir_all(dir).map_err(|e| Error::io_at(dir, e))?;
    fsutil::write_json_atomic(&path, value)?;
    Ok(path)
}

/// What [`add_excerpt`] stored (counts only; the text stays in the file).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ExcerptAdded {
    pub set: String,
    pub path: PathBuf,
    pub excerpt_id: String,
    pub meeting_id: MeetingId,
    pub start: f64,
    pub end: f64,
    pub reference_words: usize,
    /// Reference turns taken from confirmed speakers; `None` when some
    /// speech in the range belongs to unconfirmed speakers.
    pub reference_turns: Option<usize>,
    pub notes: Vec<String>,
}

/// Adds the range `[from, to)` of `meeting_id` to `set` (created when
/// missing): its current transcript text as a starting reference to correct
/// by hand, and the turns of its speakers when every one of them speaking in
/// the range is confirmed. Reads the library only.
pub fn add_excerpt(
    env: &Env<'_>,
    data_dir: &Path,
    set: &str,
    meeting_id: &MeetingId,
    from: f64,
    to: f64,
) -> Result<ExcerptAdded> {
    validate_set_name(set)?;
    if !(from.is_finite() && to.is_finite() && from >= 0.0 && to > from) {
        return Err(Error::InvalidInput(format!(
            "bench excerpt: need 0 ≤ from < to (got {from} and {to})"
        )));
    }
    let meeting = env.lib.load_meeting(meeting_id)?;
    if meeting.duration_s > 0.0 && from >= meeting.duration_s {
        return Err(Error::InvalidInput(format!(
            "bench excerpt: {meeting_id} lasts {:.1} s; {from} is past its end",
            meeting.duration_s
        )));
    }
    let to = if meeting.duration_s > 0.0 {
        to.min(meeting.duration_s)
    } else {
        to
    };
    let text = range_text(&meeting, from, to);
    let (turns, unconfirmed_s) = confirmed_turns(&meeting, from, to);
    let mut notes = vec![
        "Correct reference_text by hand, then set reference_checked to true: WER is only computed on checked text.".to_owned(),
    ];
    let turns = if unconfirmed_s > 0.0 {
        notes.push(format!(
            "{unconfirmed_s:.1} s of speech in the range belong to unconfirmed speakers: no reference turns (confirm them and add the excerpt again, or pass --manifest to bench run)."
        ));
        None
    } else {
        Some(turns)
    };
    let mut bench = match load_set(data_dir, set) {
        Ok(s) => s,
        Err(Error::NotFound(_)) => BenchSet::default(),
        Err(e) => return Err(e),
    };
    let next = bench
        .excerpts
        .iter()
        .filter_map(|e| e.id.strip_prefix('e')?.parse::<u32>().ok())
        .max()
        .unwrap_or(0)
        + 1;
    let excerpt = Excerpt {
        id: format!("e{next}"),
        meeting_id: Some(meeting_id.clone()),
        audio_path: None,
        start: from,
        end: to,
        reference_text: Some(text.clone()),
        reference_checked: false,
        turns_source: turns.as_ref().map(|_| ReferenceSource::Confirmed),
        reference_turns: turns.clone(),
        created_at: Some(schema::now()),
    };
    bench.schema_version = SET_SCHEMA_VERSION;
    bench.excerpts.push(excerpt.clone());
    let path = save_set(data_dir, set, &bench)?;
    Ok(ExcerptAdded {
        set: set.to_owned(),
        path,
        excerpt_id: excerpt.id,
        meeting_id: meeting_id.clone(),
        start: from,
        end: to,
        reference_words: normalize_words(&text).len(),
        reference_turns: turns.map(|t| t.len()),
        notes,
    })
}

/// Transcript words whose midpoint lies in `[from, to)`; segments without
/// word times count by their own midpoint.
fn range_text(meeting: &Meeting, from: f64, to: f64) -> String {
    let inside = |a: f64, b: f64| {
        let mid = (a + b) / 2.0;
        from <= mid && mid < to
    };
    let mut parts: Vec<&str> = Vec::new();
    let mut segments: Vec<_> = meeting.segments.iter().collect();
    segments.sort_by(|a, b| a.start.total_cmp(&b.start));
    for seg in segments {
        if seg.words.is_empty() {
            if inside(seg.start, seg.end) {
                parts.push(seg.text.trim());
            }
        } else {
            parts.extend(
                seg.words
                    .iter()
                    .filter(|w| inside(w.start, w.end))
                    .map(|w| w.text.trim()),
            );
        }
    }
    parts.retain(|p| !p.is_empty());
    parts.join(" ")
}

/// Segment spans of confirmed speakers clipped to `[from, to)`, adjacent
/// spans of one person joined, plus the seconds spoken in the range by
/// anyone not confirmed.
fn confirmed_turns(meeting: &Meeting, from: f64, to: f64) -> (Vec<LabelledTurn>, f64) {
    let mut turns: Vec<LabelledTurn> = Vec::new();
    let mut unconfirmed = 0.0;
    let mut segments: Vec<_> = meeting.segments.iter().collect();
    segments.sort_by(|a, b| a.start.total_cmp(&b.start));
    for seg in segments {
        let (s, e) = (seg.start.max(from), seg.end.min(to));
        if e <= s {
            continue;
        }
        let person = seg
            .speaker
            .and_then(|id| meeting.speaker(id))
            .filter(|sp| sp.assignment == Assignment::Confirmed)
            .and_then(|sp| sp.person_id.as_ref());
        let Some(person) = person else {
            unconfirmed += e - s;
            continue;
        };
        let label = person.to_string();
        match turns.last_mut() {
            Some(last) if last.label == label && s - last.end < 0.5 => last.end = last.end.max(e),
            _ => turns.push(LabelledTurn {
                start: s,
                end: e,
                label,
            }),
        }
    }
    (turns, unconfirmed)
}

// ---- candidates ----

/// The profile a candidate spec stands for: `active` (or `default`) is the
/// configured profile; otherwise `+`-joined registry model IDs, each
/// replacing the active profile's model for its role (ASR, diarization,
/// speaker embedding or VAD).
pub fn candidate_profile(spec: &str, active: &Profile, registry: &Registry) -> Result<Profile> {
    let spec = spec.trim();
    if spec == ACTIVE_CANDIDATE || spec == "default" {
        return Ok(active.clone());
    }
    let mut profile = active.clone();
    let mut roles_seen: BTreeSet<&'static str> = BTreeSet::new();
    for id in spec.split('+').map(str::trim) {
        let entry = registry.require(id)?;
        let (role, slot) = if entry.has_role(Role::Asr) {
            ("asr", &mut profile.asr)
        } else if entry.has_role(Role::Diarization) {
            ("diarization", &mut profile.diarization)
        } else if entry.has_role(Role::SpeakerEmbedding) {
            ("speaker_embedding", &mut profile.speaker_embedding)
        } else if entry.has_role(Role::Vad) {
            ("vad", &mut profile.vad)
        } else {
            return Err(Error::InvalidInput(format!(
                "bench candidate {spec:?}: model {id} has no ASR, diarization, speaker-embedding or VAD role"
            )));
        };
        if !roles_seen.insert(role) {
            return Err(Error::InvalidInput(format!(
                "bench candidate {spec:?}: two models for the {role} role"
            )));
        }
        *slot = id.to_owned();
    }
    Ok(profile)
}

/// The models a candidate runs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateModels {
    pub vad: String,
    pub asr: String,
    pub diarization: String,
    pub speaker_embedding: String,
}

impl From<&Profile> for CandidateModels {
    fn from(p: &Profile) -> Self {
        Self {
            vad: p.vad.clone(),
            asr: p.asr.clone(),
            diarization: p.diarization.clone(),
            speaker_embedding: p.speaker_embedding.clone(),
        }
    }
}

// ---- run ----

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunOptions {
    pub set: String,
    /// Candidate specs (see [`candidate_profile`]).
    pub candidates: Vec<String>,
    /// Manifest whose hints give reference turns to excerpts without them
    /// (and the data `calibrate` fits on).
    pub manifest: Option<PathBuf>,
    pub calibrate: bool,
}

/// What a worker is asked to do (its command-line arguments; no text).
#[derive(Clone, Debug, PartialEq)]
pub struct WorkerRequest {
    pub set: String,
    pub candidate: String,
    pub manifest: Option<PathBuf>,
    /// Calibration-only child; never used for performance figures.
    pub calibrate_only: bool,
}

/// Stored thresholds after a calibrating worker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationOutcome {
    pub model: String,
    /// Thresholds written to the config for `model`.
    pub written: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thresholds: Option<Thresholds>,
    /// `model` is the active embedding model and the library was
    /// re-resolved: number of speakers whose naming changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub re_resolved_changes: Option<usize>,
    /// Calibration report of the worker (counts, no names).
    #[serde(default)]
    pub report: serde_json::Value,
}

/// Per-excerpt figures (no text).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExcerptScore {
    pub id: String,
    pub audio_s: f64,
    pub processing_s: f64,
    pub wer: Metric,
    pub der: Metric,
    pub jer: Metric,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_reference: Option<ReferenceSource>,
    pub clusters_per_true_speaker: Metric,
    pub naming_accuracy: Metric,
    pub estimated_manual_fixes: Metric,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One candidate's figures over the set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateMetrics {
    /// Corpus WER over hand-checked excerpts.
    pub wer: Metric,
    /// DER with a 0.25 s collar, overlap scored.
    pub der: Metric,
    pub jer: Metric,
    /// `confirmed`, `manual`, `hint-referenced`, or a mix such as
    /// `partly hint-referenced`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diarization_reference: Option<String>,
    pub clusters_per_true_speaker: Metric,
    pub naming_accuracy: Metric,
    /// Estimated corrections per meeting: speakers named wrongly, left
    /// unnamed or suggested wrongly after resolve.
    pub estimated_manual_fixes_per_meeting: Metric,
    /// Mean seconds from audio to final transcript per excerpt.
    pub time_to_transcript_s: Metric,
    /// Processing seconds per audio second.
    pub real_time_factor: Metric,
    pub model_load_s: f64,
    /// Peak resident memory of the worker process.
    pub peak_rss_bytes: Metric,
    pub excerpts_failed: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CandidateResult {
    pub candidate: String,
    pub models: CandidateModels,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thresholds: Option<Thresholds>,
    /// `config`, `calibrated` or `none`.
    #[serde(default)]
    pub thresholds_source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calibration: Option<CalibrationOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<CandidateMetrics>,
    #[serde(default)]
    pub excerpts: Vec<ExcerptScore>,
    /// The worker failed; no figures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A results file (`<data_dir>/bench/results/*.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BenchResults {
    pub schema_version: u32,
    pub set: String,
    pub created_at: DateTime<FixedOffset>,
    pub collar_s: f64,
    pub excerpts: usize,
    /// Hint references from a manifest were used.
    pub manifest_hints: bool,
    pub candidates: Vec<CandidateResult>,
}

/// [`run`]'s result: the figures and the file they were saved to.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BenchRun {
    pub path: PathBuf,
    #[serde(flatten)]
    pub results: BenchResults,
}

/// References of one excerpt as scored.
#[derive(Clone, Debug, Default)]
struct References {
    words: Option<Vec<String>>,
    words_missing: String,
    turns: Option<Vec<LabelledTurn>>,
    source: Option<ReferenceSource>,
    turns_missing: String,
}

/// Runs every candidate through `launch` (one worker process each) and
/// scores them. Stores thresholds per model with `calibrate`; re-resolves
/// the library only for the active embedding model. Writes the results
/// file and returns it.
pub fn run(
    env: &Env<'_>,
    data_dir: &Path,
    opts: &RunOptions,
    launch: &mut dyn FnMut(&WorkerRequest) -> Result<WorkerReport>,
) -> Result<BenchRun> {
    if opts.candidates.is_empty() {
        return Err(Error::InvalidInput(
            "bench run: name at least one candidate (active, or model IDs)".into(),
        ));
    }
    if opts.calibrate && opts.manifest.is_none() {
        return Err(Error::InvalidInput(
            "bench run --calibrate needs --manifest (thresholds are fitted on its hints)".into(),
        ));
    }
    let registry = Registry::load(data_dir)?;
    let mut candidates: Vec<(String, Profile)> = Vec::new();
    for spec in &opts.candidates {
        let spec = spec.trim().to_owned();
        if candidates.iter().any(|(s, _)| *s == spec) {
            continue;
        }
        let profile = candidate_profile(&spec, &env.config.profile, &registry)?;
        candidates.push((spec, profile));
    }
    let set = load_set(data_dir, &opts.set)?;
    if set.excerpts.is_empty() {
        return Err(Error::InvalidInput(format!(
            "bench set {} has no excerpts (add some with `bench excerpt add`)",
            opts.set
        )));
    }
    let manifest = match &opts.manifest {
        Some(p) => {
            let mut m = manifest::read(p)?;
            m.mark_self(&env.config.setup);
            Some(m)
        }
        None => None,
    };
    let known = env.lib.list_people()?;
    let refs: BTreeMap<String, References> = set
        .excerpts
        .iter()
        .map(|e| Ok((e.id.clone(), references(env, e, manifest.as_ref(), &known)?)))
        .collect::<Result<_>>()?;
    let known_ids: BTreeSet<String> = known.iter().map(|p| p.id.to_string()).collect();

    let mut calibrated: BTreeSet<String> = BTreeSet::new();
    // Thresholds stored by this run's calibration, by embedding model.
    let mut fresh: BTreeMap<String, Thresholds> = BTreeMap::new();
    let mut results = Vec::new();
    for (n, (spec, profile)) in candidates.iter().enumerate() {
        env.say(format_args!(
            "[{}/{}] bench candidate {spec}",
            n + 1,
            candidates.len()
        ));
        let calibrate = opts.calibrate && calibrated.insert(profile.speaker_embedding.clone());
        let request = WorkerRequest {
            set: opts.set.clone(),
            candidate: spec.clone(),
            manifest: opts.manifest.clone(),
            calibrate_only: false,
        };
        let mut result = CandidateResult {
            candidate: spec.clone(),
            models: CandidateModels::from(profile),
            thresholds: None,
            thresholds_source: String::new(),
            calibration: None,
            metrics: None,
            excerpts: Vec::new(),
            error: None,
        };
        if calibrate {
            let calibration_request = WorkerRequest {
                calibrate_only: true,
                ..request.clone()
            };
            match launch(&calibration_request).and_then(|r| r.check(spec).map(|()| r)) {
                Ok(report) => {
                    if let Some(cal) = &report.calibration {
                        let stored = store_calibration(
                            env,
                            data_dir,
                            &report.models.speaker_embedding,
                            cal,
                        )?;
                        if let Some(t) = stored.thresholds.filter(|_| stored.written) {
                            fresh.insert(stored.model.clone(), t);
                        }
                        result.calibration = Some(stored);
                    }
                }
                Err(e) => {
                    result.error = Some(format!("{}: {e}", e.code()));
                    results.push(result);
                    continue;
                }
            }
        }
        let report = launch(&request).and_then(|r| r.check(spec).map(|()| r));
        let report = match report {
            Ok(r) => r,
            Err(e) => {
                result.error = Some(format!("{}: {e}", e.code()));
                results.push(result);
                continue;
            }
        };
        result.models = report.models.clone();
        result.thresholds = report.thresholds;
        // The measuring child reads the thresholds this run just stored:
        // they are calibrated, not hand-set configuration.
        let calibrated_now = report
            .thresholds
            .is_some_and(|t| fresh.get(&report.models.speaker_embedding) == Some(&t));
        result.thresholds_source = if calibrated_now {
            "calibrated".to_owned()
        } else {
            report.thresholds_source.clone()
        };
        let (metrics, scores) = score(&set, &refs, &report, &known_ids);
        result.metrics = Some(metrics);
        result.excerpts = scores;
        results.push(result);
    }
    let results = BenchResults {
        schema_version: RESULTS_SCHEMA_VERSION,
        set: opts.set.clone(),
        created_at: schema::now(),
        collar_s: COLLAR_S,
        excerpts: set.excerpts.len(),
        manifest_hints: refs
            .values()
            .any(|r| r.source == Some(ReferenceSource::Hints)),
        candidates: results,
    };
    let path = save_results(data_dir, &results)?;
    Ok(BenchRun { path, results })
}

/// Writes `results` to a new file named after its time and set.
pub fn save_results(data_dir: &Path, results: &BenchResults) -> Result<PathBuf> {
    let dir = results_dir(data_dir);
    std::fs::create_dir_all(&dir).map_err(|e| Error::io_at(&dir, e))?;
    let stem = format!(
        "{}-{}",
        results.created_at.format("%Y%m%dT%H%M%S"),
        results.set
    );
    let mut path = dir.join(format!("{stem}.json"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{stem}-{n}.json"));
        n += 1;
    }
    fsutil::write_json_atomic(&path, results)?;
    Ok(path)
}

/// Stores a calibrating worker's thresholds for `model` (other config left
/// as the user set it) and re-resolves the library only when `model` is
/// the active embedding model.
fn store_calibration(
    env: &Env<'_>,
    data_dir: &Path,
    model: &str,
    cal: &worker::WorkerCalibration,
) -> Result<CalibrationOutcome> {
    let mut out = CalibrationOutcome {
        model: model.to_owned(),
        written: false,
        reason: cal.reason.clone().or_else(|| cal.error.clone()),
        thresholds: cal.thresholds,
        re_resolved_changes: None,
        report: cal.report.clone(),
    };
    let Some(thresholds) = cal.thresholds else {
        return Ok(out);
    };
    let (config, thresholds) = Config::update(data_dir, |config| {
        Ok(config.set_calibrated_thresholds(model, thresholds))
    })?;
    out.written = true;
    // Model selection may have changed while the worker was fitting.
    if model == config.profile.speaker_embedding {
        env.say(format_args!(
            "thresholds for {model} stored; re-resolving unconfirmed speakers"
        ));
        let applied = crate::reresolve::re_resolve_with_live(
            env.lib,
            model,
            Some(&thresholds),
            &env.config.live,
        )?;
        out.re_resolved_changes = Some(applied.iter().map(|p| p.changes.len()).sum());
    } else {
        env.say(format_args!(
            "thresholds for {model} stored (used when it becomes the active model)"
        ));
    }
    Ok(out)
}

/// An excerpt's references: checked text, and turns from the set or else
/// from the manifest's hints.
fn references(
    env: &Env<'_>,
    e: &Excerpt,
    manifest: Option<&Manifest>,
    known: &[Person],
) -> Result<References> {
    let mut r = References::default();
    match (&e.reference_text, e.reference_checked) {
        (Some(text), true) => r.words = Some(normalize_words(text)),
        (Some(_), false) => {
            r.words_missing =
                "reference text not hand-checked (set reference_checked after correcting it)"
                    .into();
        }
        (None, _) => r.words_missing = "no reference text".into(),
    }
    if let Some(turns) = &e.reference_turns {
        r.turns = Some(turns.clone());
        r.source = Some(e.turns_source.unwrap_or(ReferenceSource::Manual));
        return Ok(r);
    }
    r.turns_missing = "no reference turns".into();
    let Some(manifest) = manifest else {
        return Ok(r);
    };
    let meeting = match &e.meeting_id {
        Some(id) => match env.lib.load_meeting(id) {
            Ok(m) => Some(m),
            Err(Error::NotFound(_)) => None,
            Err(err) => return Err(err),
        },
        None => None,
    };
    let entry = manifest.meetings.iter().find(|m| match &meeting {
        Some(meeting) => {
            (meeting.source_id.is_some() && meeting.source_id == m.source_id)
                || meeting.recording.as_ref().is_some_and(|rec| {
                    rec.path == m.audio_path
                        || rec.original_path.as_deref() == Some(m.audio_path.as_path())
                })
        }
        None => e.audio_path.as_deref() == Some(m.audio_path.as_path()),
    });
    let Some(entry) = entry else {
        r.turns_missing = "no reference turns, and no manifest meeting matches".into();
        return Ok(r);
    };
    let me = known.iter().find(|p| p.is_self).map(|p| p.id.to_string());
    let mut turns = Vec::new();
    for g in group_hints(&entry.hints, known) {
        let label = match (&g.person_id, g.is_self) {
            (Some(id), _) => id.to_string(),
            (None, true) => me.clone().unwrap_or_else(|| "hint:self".into()),
            (None, false) => format!("hint:{}", g.key),
        };
        for &(a, b) in &g.ranges {
            let (s, t) = (a.max(e.start), b.min(e.end));
            if t > s {
                turns.push(LabelledTurn {
                    start: s,
                    end: t,
                    label: label.clone(),
                });
            }
        }
    }
    if turns.is_empty() {
        r.turns_missing = "no reference turns, and the manifest's hints miss the range".into();
        return Ok(r);
    }
    r.turns = Some(turns);
    r.source = Some(ReferenceSource::Hints);
    Ok(r)
}

/// Scores one worker report against the references.
fn score(
    set: &BenchSet,
    refs: &BTreeMap<String, References>,
    report: &WorkerReport,
    known_ids: &BTreeSet<String>,
) -> (CandidateMetrics, Vec<ExcerptScore>) {
    let known = |label: &str| known_ids.contains(label);
    let naming_missing = (report.people_with_voiceprints == 0).then(|| {
        format!(
            "no voiceprints for {} in the library",
            report.models.speaker_embedding
        )
    });
    let mut words = WordErrors::default();
    let mut words_any = false;
    let mut diar = DiarErrors::default();
    let mut diar_any = false;
    let mut sources: BTreeMap<ReferenceSource, usize> = BTreeMap::new();
    let (mut clusters_sum, mut clusters_n) = (0usize, 0usize);
    let (mut naming_ref_s, mut naming_correct_s) = (0.0, 0.0);
    let mut fixes = 0usize;
    let mut fix_meetings: BTreeSet<String> = BTreeSet::new();
    let (mut processing_s, mut audio_s, mut ok) = (0.0, 0.0, 0usize);
    let mut failed = 0usize;
    let mut scores = Vec::new();
    let missing_refs = References::default();
    for e in &set.excerpts {
        let r = refs.get(&e.id).unwrap_or(&missing_refs);
        let unavailable_all = |why: &str| ExcerptScore {
            id: e.id.clone(),
            audio_s: 0.0,
            processing_s: 0.0,
            wer: Metric::unavailable(why),
            der: Metric::unavailable(why),
            jer: Metric::unavailable(why),
            diarization_reference: None,
            clusters_per_true_speaker: Metric::unavailable(why),
            naming_accuracy: Metric::unavailable(why),
            estimated_manual_fixes: Metric::unavailable(why),
            error: Some(why.to_owned()),
        };
        let Some(hyp) = report.excerpts.iter().find(|x| x.id == e.id) else {
            failed += 1;
            scores.push(unavailable_all("the worker did not report this excerpt"));
            continue;
        };
        if let Some(err) = &hyp.error {
            failed += 1;
            scores.push(unavailable_all(&format!("{}: {}", err.code, err.message)));
            continue;
        }
        ok += 1;
        processing_s += hyp.processing_s;
        audio_s += hyp.audio_s;
        let region = (e.start, e.end);
        let wer = match &r.words {
            Some(ref_words) => {
                let we = word_errors(ref_words, &normalize_words(&hyp.text));
                words.add(&we);
                words_any = true;
                we.wer()
            }
            None => Metric::unavailable(r.words_missing.clone()),
        };
        let mut s = ExcerptScore {
            id: e.id.clone(),
            audio_s: hyp.audio_s,
            processing_s: hyp.processing_s,
            wer,
            der: Metric::unavailable(r.turns_missing.clone()),
            jer: Metric::unavailable(r.turns_missing.clone()),
            diarization_reference: r.source,
            clusters_per_true_speaker: Metric::unavailable(r.turns_missing.clone()),
            naming_accuracy: Metric::unavailable(r.turns_missing.clone()),
            estimated_manual_fixes: Metric::unavailable(r.turns_missing.clone()),
            error: None,
        };
        if let Some(turns) = &r.turns {
            let de = diarization_errors(turns, &hyp.turns, region, COLLAR_S);
            s.der = de.der();
            s.jer = de.jer();
            diar.add(&de);
            diar_any = true;
            if let Some(src) = r.source {
                *sources.entry(src).or_default() += 1;
            }
            let clusters = clusters_per_speaker(turns, &hyp.turns, region);
            let total: usize = clusters.values().sum();
            s.clusters_per_true_speaker = Metric::ratio(
                total as f64,
                clusters.len() as f64,
                "reference speakers",
                "no reference speaker with 1 s of speech",
            );
            clusters_sum += total;
            clusters_n += clusters.len();
            match &naming_missing {
                Some(why) => {
                    s.naming_accuracy = Metric::unavailable(why.clone());
                    s.estimated_manual_fixes = Metric::unavailable(why.clone());
                }
                None => {
                    let out = naming(turns, &hyp.turns, &hyp.speakers, &known, region);
                    s.naming_accuracy = Metric::ratio(
                        out.correct_s,
                        out.reference_s,
                        "reference seconds",
                        "no reference speech",
                    );
                    s.estimated_manual_fixes = Metric::value(out.fixes as f64, 1.0, "excerpt");
                    naming_ref_s += out.reference_s;
                    naming_correct_s += out.correct_s;
                    fixes += out.fixes;
                    fix_meetings.insert(e.source_key());
                }
            }
        }
        scores.push(s);
    }
    let no_turns = "no excerpt has reference turns";
    let metrics = CandidateMetrics {
        wer: if words_any {
            words.wer()
        } else {
            Metric::unavailable("no hand-checked reference text")
        },
        der: if diar_any {
            diar.der()
        } else {
            Metric::unavailable(no_turns)
        },
        jer: if diar_any {
            diar.jer()
        } else {
            Metric::unavailable(no_turns)
        },
        diarization_reference: reference_label(&sources),
        clusters_per_true_speaker: if diar_any {
            Metric::ratio(
                clusters_sum as f64,
                clusters_n as f64,
                "reference speakers",
                "no reference speaker with 1 s of speech",
            )
        } else {
            Metric::unavailable(no_turns)
        },
        naming_accuracy: match (&naming_missing, diar_any) {
            (_, false) => Metric::unavailable(no_turns),
            (Some(why), true) => Metric::unavailable(why.clone()),
            (None, true) => Metric::ratio(
                naming_correct_s,
                naming_ref_s,
                "reference seconds",
                "no reference speech",
            ),
        },
        estimated_manual_fixes_per_meeting: match (&naming_missing, diar_any) {
            (_, false) => Metric::unavailable(no_turns),
            (Some(why), true) => Metric::unavailable(why.clone()),
            (None, true) => Metric::ratio(
                fixes as f64,
                fix_meetings.len() as f64,
                "meetings",
                "no meeting with reference turns",
            ),
        },
        time_to_transcript_s: Metric::ratio(
            processing_s,
            ok as f64,
            "excerpts",
            "no excerpt was processed",
        ),
        real_time_factor: Metric::ratio(
            processing_s,
            audio_s,
            "audio seconds",
            "no excerpt was processed",
        ),
        model_load_s: report.load_s,
        peak_rss_bytes: match report.peak_rss_bytes {
            Some(b) => Metric::value(b as f64, 1.0, "worker process"),
            None => Metric::unavailable(
                report
                    .usage_error
                    .clone()
                    .unwrap_or_else(|| "not measured".into()),
            ),
        },
        excerpts_failed: failed,
    };
    (metrics, scores)
}

/// `confirmed` / `manual` / `hint-referenced` when every scored excerpt
/// shares one source; otherwise a mix (`partly hint-referenced` when any
/// came from hints).
fn reference_label(sources: &BTreeMap<ReferenceSource, usize>) -> Option<String> {
    let label = |s: ReferenceSource| match s {
        ReferenceSource::Confirmed => "confirmed",
        ReferenceSource::Manual => "manual",
        ReferenceSource::Hints => "hint-referenced",
    };
    match sources.len() {
        0 => None,
        1 => sources.keys().next().map(|s| label(*s).to_owned()),
        _ if sources.contains_key(&ReferenceSource::Hints) => {
            Some("partly hint-referenced".to_owned())
        }
        _ => Some("confirmed and manual".to_owned()),
    }
}
