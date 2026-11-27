//! `broscribe bench worker`: one candidate in its own process.
//!
//! Measurement children load the candidate's models, run ASR, diarization and
//! identity resolution on every independent track of each excerpt, and report
//! hypotheses plus peak memory ([`ResourceUsage`]). Calibration runs only in
//! a separate `--calibrate-only` child using a temporary library. Both modes
//! only read the user's library; scoring is the parent's job ([`super::run`]).
//!
//! The report carries hypothesis text, so it travels only through the
//! worker's stdout pipe; it is never logged or stored.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use broscribe_core::config::Thresholds;
use broscribe_core::schema::{self, LanguageSpan, Meeting, Source, TrackRole};
use broscribe_core::{Error, Library, MeetingId, Result, SpeakerId, ops};
use broscribe_models::{ModelProvider, ModelSet, Registry};
use broscribe_platform::ResourceUsage;
use serde::{Deserialize, Serialize};

use super::metrics::{HypIdentity, LabelledTurn};
use super::{CandidateModels, Excerpt, candidate_profile, load_set};
use crate::Env;
use crate::audio::{
    TARGET_RATE, TrackSource, decode_16k, decode_meeting_track, file_sha256, meeting_tracks,
    slice_s,
};
use crate::calibrate::{CalibrateOptions, CalibrationReport, calibrate};
use crate::finish::{apply_merge, resolve_identities};
use crate::manifest::{self, ManifestSource};
use crate::multitrack::analyze_decoded;

/// Version of the worker's JSON report; the parent refuses others.
pub const WORKER_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorkerOptions {
    pub set: String,
    pub candidate: String,
    pub manifest: Option<PathBuf>,
    pub calibrate_only: bool,
}

/// The worker's stdout document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerReport {
    pub schema_version: u32,
    pub candidate: String,
    pub models: CandidateModels,
    /// Thresholds the excerpts were resolved with.
    #[serde(default)]
    pub thresholds: Option<Thresholds>,
    /// `calibrated`, `config` or `none`.
    pub thresholds_source: String,
    #[serde(default)]
    pub calibration: Option<WorkerCalibration>,
    /// People with voiceprints of the candidate's embedding model; naming
    /// cannot be judged without any.
    pub people_with_voiceprints: usize,
    pub load_s: f64,
    /// Decoding the recordings (shared by all candidates; not part of
    /// `processing_s`).
    #[serde(default)]
    pub decode_s: f64,
    pub excerpts: Vec<WorkerExcerpt>,
    /// `getrusage(RUSAGE_SELF)` peak resident memory, in bytes.
    #[serde(default)]
    pub peak_rss_bytes: Option<u64>,
    /// User + system CPU seconds of the worker.
    #[serde(default)]
    pub cpu_s: Option<f64>,
    /// Why the usage could not be measured.
    #[serde(default)]
    pub usage_error: Option<String>,
}

impl WorkerReport {
    /// Checks the contract: schema version and candidate.
    pub fn check(&self, candidate: &str) -> Result<()> {
        if self.schema_version != WORKER_SCHEMA_VERSION {
            return Err(Error::Schema(format!(
                "bench worker report version {} (expected {WORKER_SCHEMA_VERSION})",
                self.schema_version
            )));
        }
        if self.candidate != candidate {
            return Err(Error::Internal(format!(
                "bench worker answered for {:?}, not {candidate:?}",
                self.candidate
            )));
        }
        Ok(())
    }
}

/// Calibration done by the worker (thresholds not stored yet).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkerCalibration {
    /// Fitted thresholds; `None` when calibration found too little data.
    #[serde(default)]
    pub thresholds: Option<Thresholds>,
    #[serde(default)]
    pub reason: Option<String>,
    /// Calibration failed with this error.
    #[serde(default)]
    pub error: Option<String>,
    /// [`CalibrationReport`] (counts only, no names).
    #[serde(default)]
    pub report: serde_json::Value,
}

/// One excerpt's hypothesis, times in the recording's timeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerExcerpt {
    pub id: String,
    pub audio_s: f64,
    /// Audio to final transcript: ASR, diarization, merge and resolve.
    pub processing_s: f64,
    pub text: String,
    pub turns: Vec<LabelledTurn>,
    pub speakers: Vec<HypIdentity>,
    #[serde(default)]
    pub error: Option<WorkerError>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkerError {
    pub code: String,
    pub message: String,
}

impl WorkerExcerpt {
    fn failed(e: &Excerpt, err: &Error) -> Self {
        Self {
            id: e.id.clone(),
            audio_s: 0.0,
            processing_s: 0.0,
            text: String::new(),
            turns: Vec::new(),
            speakers: Vec::new(),
            error: Some(WorkerError {
                code: err.code().to_owned(),
                message: err.to_string(),
            }),
        }
    }
}

/// Runs one candidate (see the module docs). `data_dir` holds the config,
/// the model registry and the set.
pub fn run_worker(
    env: &Env<'_>,
    provider: &dyn ModelProvider,
    data_dir: &Path,
    opts: &WorkerOptions,
    usage: &dyn ResourceUsage,
) -> Result<WorkerReport> {
    let registry = Registry::load(data_dir)?;
    let profile = candidate_profile(&opts.candidate, &env.config.profile, &registry)?;
    let set = load_set(data_dir, &opts.set)?;

    let started = Instant::now();
    let mut models = provider.load(&profile)?;
    let load_s = started.elapsed().as_secs_f64();
    let model = models.embedder.model_id().to_owned();
    let mut models_used = CandidateModels::from(&profile);
    models_used.speaker_embedding.clone_from(&model);

    let mut thresholds = env.config.thresholds(&model).copied();
    let mut thresholds_source = if thresholds.is_some() {
        "config"
    } else {
        "none"
    };
    let mut calibration = None;
    if opts.calibrate_only {
        let Some(manifest_path) = &opts.manifest else {
            return Err(Error::InvalidInput(
                "bench worker --calibrate-only needs --manifest".into(),
            ));
        };
        env.say(format_args!("calibrating {model}"));
        let cal = match calibrate_in_temp(env, &mut models, manifest_path) {
            Ok(report) => {
                let fitted = report.thresholds.filter(|_| report.written);
                if let Some(t) = fitted {
                    thresholds = Some(t);
                    thresholds_source = "calibrated";
                }
                WorkerCalibration {
                    thresholds: fitted,
                    reason: report.reason.clone(),
                    error: None,
                    report: serde_json::to_value(&report)
                        .map_err(|e| Error::Internal(format!("serialising calibration: {e}")))?,
                }
            }
            Err(e) => WorkerCalibration {
                error: Some(format!("{}: {e}", e.code())),
                ..WorkerCalibration::default()
            },
        };
        calibration = Some(cal);
    }

    let people = ops::load_person_samples(env.lib, &model)?;
    let people_with_voiceprints = people.for_meeting(&outside_meeting_id()).len();
    // Excerpts grouped by recording so each one is decoded once.
    let mut order: Vec<usize> = if opts.calibrate_only {
        Vec::new()
    } else {
        (0..set.excerpts.len()).collect()
    };
    order.sort_by_key(|&i| set.excerpts[i].source_key());
    let mut audio = AudioCache::default();
    let mut out: Vec<Option<WorkerExcerpt>> = vec![None; order.len()];
    for (n, &i) in order.iter().enumerate() {
        let e = &set.excerpts[i];
        env.say(format_args!("[{}/{}] excerpt {}", n + 1, order.len(), e.id));
        let done = process_excerpt(
            env,
            e,
            &mut models,
            &model,
            &people,
            thresholds.as_ref(),
            &mut audio,
        );
        out[i] = Some(done.unwrap_or_else(|err| WorkerExcerpt::failed(e, &err)));
    }

    let (peak_rss_bytes, cpu_s, usage_error) = match usage.self_usage() {
        Ok(u) => (
            Some(u.peak_rss_bytes),
            Some(u.user_cpu_s + u.system_cpu_s),
            None,
        ),
        Err(e) => (None, None, Some(e.to_string())),
    };
    Ok(WorkerReport {
        schema_version: WORKER_SCHEMA_VERSION,
        candidate: opts.candidate.clone(),
        models: models_used,
        thresholds,
        thresholds_source: thresholds_source.to_owned(),
        calibration,
        people_with_voiceprints,
        load_s,
        decode_s: audio.decode_s,
        excerpts: out.into_iter().flatten().collect(),
        peak_rss_bytes,
        cpu_s,
        usage_error,
    })
}

/// `calibrate` on a temporary library and config: the thresholds are
/// returned, never written to the user's config or library. The temporary
/// library holds a copy of the user's people metadata so manifest hints link
/// to existing identities as normal calibration would.
fn calibrate_in_temp(
    env: &Env<'_>,
    models: &mut ModelSet,
    manifest_path: &Path,
) -> Result<CalibrationReport> {
    let manifest = manifest::read(manifest_path)?;
    let tmp = tempfile::tempdir().map_err(|e| Error::io("creating a temporary folder", e))?;
    let lib = isolated_library(env.lib, &tmp.path().join("library"))?;
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).map_err(|e| Error::io_at(&data, e))?;
    let temp_env = Env { lib: &lib, ..*env };
    calibrate(
        &temp_env,
        models,
        &manifest,
        &data,
        &CalibrateOptions::default(),
    )
}

/// A new library at `root` holding a copy of `source`'s person records
/// (names, aliases, e-mails) and nothing else: no voiceprints, notes or
/// meetings. `source` is only read.
pub(super) fn isolated_library(source: &Library, root: &Path) -> Result<Library> {
    let people = source.list_people()?;
    let lib = Library::open(root.to_path_buf())?;
    if !people.is_empty() {
        lib.write(|tx| {
            for p in &people {
                tx.save_person_with_notes(p, "")?;
            }
            Ok(())
        })?;
    }
    Ok(lib)
}

/// One recording track with its decoded samples.
type DecodedTrack = (TrackSource, Vec<f32>);

/// Last decoded recording, with independent track audio on its own clock.
#[derive(Default)]
struct AudioCache {
    key: Option<String>,
    tracks: Vec<DecodedTrack>,
    /// Library meetings recorded from the same audio as an `audio_path`
    /// excerpt: their samples are held out like the excerpt's own meeting.
    same_audio: Vec<MeetingId>,
    decode_s: f64,
}

impl AudioCache {
    fn get(
        &mut self,
        env: &Env<'_>,
        e: &Excerpt,
        record: Option<&Meeting>,
    ) -> Result<(&[DecodedTrack], &[MeetingId])> {
        let key = e.source_key();
        if self.key.as_ref() != Some(&key) {
            self.key = None;
            self.tracks.clear();
            self.same_audio.clear();
            let t = Instant::now();
            if let Some(record) = record {
                for source in meeting_tracks(record) {
                    let samples = decode_meeting_track(env.lib, env.decoders, record, &source)?;
                    self.tracks.push((source, samples));
                }
            } else if let Some(path) = &e.audio_path {
                let (sha256, _) = file_sha256(path)?;
                self.same_audio = meetings_with_audio(env.lib, &sha256)?;
                self.tracks.push((
                    TrackSource {
                        track: "t0".into(),
                        path: path.clone(),
                        track_index: 0,
                        offset_s: 0.0,
                        sha256,
                    },
                    decode_16k(env.decoders, path)?.samples,
                ));
            }
            self.decode_s += t.elapsed().as_secs_f64();
            if self.tracks.is_empty() {
                return Err(Error::NotFound(
                    "benchmark recording is deleted or missing".into(),
                ));
            }
            self.key = Some(key);
        }
        Ok((&self.tracks, &self.same_audio))
    }
}

/// Meetings whose recording (or one of its sources) has this content hash.
fn meetings_with_audio(lib: &Library, sha256: &str) -> Result<Vec<MeetingId>> {
    let mut out = Vec::new();
    for dir in lib.list_meeting_dirs()? {
        let m = lib.load_meeting_at(&dir)?;
        if m.recording.as_ref().is_some_and(|r| {
            r.sha256 == sha256
                || r.source_sha256.as_deref() == Some(sha256)
                || r.sources.iter().any(|s| s.sha256 == sha256)
        }) {
            out.push(m.id);
        }
    }
    Ok(out)
}

/// Stand-in ID for excerpts of audio files outside the library.
fn outside_meeting_id() -> MeetingId {
    MeetingId::parse("m-000000").expect("valid meeting ID")
}

/// Analyzes one excerpt the way an import would: every track through the
/// role-aware production analyzer (a mic + system pair with the meeting's
/// setup and echo handling, participant tracks as one person), merge once,
/// then resolve against the library's people, excluding samples taken from
/// this meeting or from the same audio.
fn process_excerpt(
    env: &Env<'_>,
    e: &Excerpt,
    models: &mut ModelSet,
    model: &str,
    people: &ops::PeopleSamples,
    thresholds: Option<&Thresholds>,
    audio: &mut AudioCache,
) -> Result<WorkerExcerpt> {
    let record = e
        .meeting_id
        .as_ref()
        .map(|id| env.lib.load_meeting(id))
        .transpose()?;
    let (tracks, same_audio) = audio.get(env, e, record.as_ref())?;
    // Always replaced, so one excerpt's names never carry into the next.
    if models.asr.accepts_hotwords() {
        let terms = match &record {
            Some(r) => crate::reprocess::hotwords(env.lib, &env.config.hotwords, r)?,
            None => crate::reprocess::library_hotwords(Some(env.lib), &env.config.hotwords)?,
        };
        models.asr.set_hotwords(&terms)?;
    }
    let mut m = Meeting::new(
        e.meeting_id.clone().unwrap_or_else(outside_meeting_id),
        Source::Import,
        schema::now(),
    );
    let (setup, handling) = match &record {
        Some(record) => {
            m.participants.clone_from(&record.participants);
            m.tracks.clone_from(&record.tracks);
            m.setup.clone_from(&record.setup);
            m.processing
                .echo_report
                .clone_from(&record.processing.echo_report);
            crate::reprocess::capture_setup(record)?
        }
        None => (None, None),
    };
    // Each track's part of the excerpt, offset on the excerpt's timeline.
    let roles: Vec<TrackRole> = tracks
        .iter()
        .map(|(source, _)| {
            record.as_ref().map_or(TrackRole::Mixed, |r| {
                crate::reprocess::track_role(r, &source.track)
            })
        })
        .collect();
    // A mic + system pair stays a pair even when the excerpt clips one side
    // to nothing: the empty side is analyzed as silence (the pair analyzer
    // pads it to the common timeline), as production analyzes the whole
    // recording, and finalization's mic protection assumes paired analysis.
    let pair = crate::multitrack::mic_system_pair(roles.iter().copied());
    let mut inputs = Vec::new();
    let mut slices: Vec<&[f32]> = Vec::new();
    let mut labels: Vec<&str> = Vec::new();
    let mut audio_s: f64 = 0.0;
    for (i, ((source, samples), role)) in tracks.iter().zip(roles).enumerate() {
        let from = (e.start - source.offset_s).max(0.0);
        let slice = slice_s(samples, from, e.end - source.offset_s);
        let paired = pair.is_some_and(|(mic, system)| i == mic || i == system);
        if slice.is_empty() && !paired {
            continue;
        }
        let offset_s = if slice.is_empty() {
            0.0
        } else {
            (source.offset_s - e.start).max(0.0)
        };
        audio_s = audio_s.max(offset_s + slice.len() as f64 / f64::from(TARGET_RATE));
        inputs.push(ManifestSource {
            path: source.path.clone(),
            audio_track_index: Some(source.track_index),
            role,
            person_hint: None,
            offset_s,
        });
        slices.push(slice);
        labels.push(&source.track);
    }
    if audio_s == 0.0 {
        return Err(Error::InvalidInput(format!(
            "bench excerpt {}: the range is outside the recording",
            e.id
        )));
    }
    let languages: Vec<LanguageSpan> = record
        .iter()
        .flat_map(|r| &r.languages)
        .map(|l| LanguageSpan {
            lang: l.lang.clone(),
            from_s: l.from_s - e.start,
            to_s: l.to_s - e.start,
        })
        .filter(|l| l.to_s > 0.0)
        .collect();
    // Held out from every identity decision: this meeting and any meeting
    // recorded from the same audio, so no evaluation-derived sample (Me's
    // included) verifies or names a speaker.
    let held_out: Vec<MeetingId> = std::iter::once(m.id.clone())
        .chain(same_audio.iter().cloned())
        .collect();
    let t = Instant::now();
    let analysis = analyze_decoded(
        env, &inputs, &slices, models, &languages, setup, handling, &held_out,
    )?;
    // Analysis labels tracks `t<i>` by input; keep the meeting's track IDs
    // (roles and setup refer to them) and move times to the recording's clock.
    let label = |track: &str| -> Result<String> {
        track
            .strip_prefix('t')
            .and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| labels.get(i))
            .map(|l| (*l).to_owned())
            .ok_or_else(|| Error::Internal("analysis returned an unknown track".into()))
    };
    for mut speaker in analysis.speakers {
        speaker.track = label(&speaker.track)?;
        if let Some(centroid) = &mut speaker.centroid {
            for range in &mut centroid.ranges {
                range[0] += e.start as f32;
                range[1] += e.start as f32;
            }
        }
        m.speakers.push(speaker);
    }
    for mut segment in analysis.segments {
        segment.track = label(&segment.track)?;
        segment.start += e.start;
        segment.end += e.start;
        for [start, end] in &mut segment.speech_overlap {
            *start += e.start;
            *end += e.start;
        }
        for word in &mut segment.words {
            word.start += e.start;
            word.end += e.start;
        }
        m.segments.push(segment);
    }
    let mut diar = analysis.diar_segments;
    for turn in &mut diar {
        turn.start += e.start;
        turn.end += e.start;
    }
    m.duration_s = e.start + audio_s;
    let floor = m.roster_floor();
    let turns = merge_turns(
        &mut m,
        model,
        thresholds.and_then(|t| t.merge),
        floor,
        &diar,
    )?;
    let mut benchmark_people = people.for_benchmark(&held_out[0]);
    for id in &held_out[1..] {
        benchmark_people = benchmark_people.for_benchmark(id);
    }
    // The production finalization policy: the paired microphone keeps the
    // two-track pass's decisions.
    resolve_identities(&mut m, model, &benchmark_people, thresholds)?;
    let processing_s = t.elapsed().as_secs_f64();
    let speakers = m
        .speakers
        .iter()
        .map(|s| HypIdentity {
            label: s.id.to_string(),
            assignment: s.assignment,
            person: s.person_id.as_ref().map(ToString::to_string),
            suggested: s.suggestion.as_ref().map(|x| x.person_id.to_string()),
        })
        .collect();
    let mut segments: Vec<_> = m.segments.iter().collect();
    segments.sort_by(|a, b| a.start.total_cmp(&b.start));
    let text = segments
        .iter()
        .map(|s| s.text.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    Ok(WorkerExcerpt {
        id: e.id.clone(),
        audio_s,
        processing_s,
        text,
        turns,
        speakers,
        error: None,
    })
}

/// Merges `m` once and labels the diarized turns through the merge's
/// `(merged, kept)` folds, so clusters merged away (wordless ones included)
/// score as the speaker that kept them.
pub(super) fn merge_turns(
    m: &mut Meeting,
    model: &str,
    merge: Option<f32>,
    floor: Option<usize>,
    diar: &[crate::Turn],
) -> Result<Vec<LabelledTurn>> {
    let remap: BTreeMap<SpeakerId, SpeakerId> =
        apply_merge(m, model, merge, floor)?.into_iter().collect();
    Ok(diar
        .iter()
        .map(|turn| LabelledTurn {
            start: turn.start,
            end: turn.end,
            label: remap
                .get(&turn.speaker)
                .copied()
                .unwrap_or(turn.speaker)
                .to_string(),
        })
        .collect())
}
