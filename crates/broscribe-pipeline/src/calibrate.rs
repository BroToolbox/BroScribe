//! `identity calibrate --manifest` (design §8.3, plan §13.7): fits the
//! naming thresholds of the active embedding model on hinted meetings.
//!
//! 1. Hinted entries are deduplicated by recording SHA-256 (a repeat is
//!    reported, with the metadata it disagrees on), so no recording sits on
//!    both sides of a split. Each remaining meeting is analyzed without ASR
//!    into a temporary library (reference handling); nothing touches the
//!    user's library until the end. Hinted people get canonical identities
//!    from a metadata-only registry (the user's people, `people[]`, then the
//!    normal e-mail / name matching rules).
//! 2. Meetings are split by date ([`identity::split_by_date`]).
//! 3. People are enrolled from the enrollment set only, as in a normal
//!    import (hint agreement, then at most 50 diverse samples).
//! 4. On the calibration set: `merge` is fitted on within-meeting pairs of
//!    labelled clusters, then `auto_name`/`margin`/`suggest` on the merged
//!    clusters. A cluster's fitting label is its majority hinted person with
//!    purity ≥ 0.8 and ≥ 1 s of overlap; impure clusters are counted and left
//!    out.
//! 5. Guard: without an enrolled person in the calibration set nothing is
//!    written and the reason is reported.
//! 6. The evaluation set is scored once, per hinted second, with the frozen
//!    thresholds.
//! 7. The thresholds are written to the config for the model and every
//!    unconfirmed speaker of the user's library is re-resolved.
//!
//! The report holds counts, not names, unless the caller asks for names.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use broscribe_core::config::{Config, SetupProgress, Thresholds};
use broscribe_core::identity::{
    self, CalMeeting, EvalInterval, EvalReport, MAX_CONFIRMED, NameKind, PersonOrSelf,
    PersonSamples, Trial,
};
use broscribe_core::schema::{
    self, Assignment, Handling, Meeting, Participant, Person, Processing, ProcessingStatus,
    Segment, Source, Speaker, Track, TrackRole,
};
use broscribe_core::{Error, Library, MeetingId, PersonId, Result, SpeakerId, ops};
use broscribe_models::ModelSet;
use serde::Serialize;

use crate::Env;
use crate::analyze::{AnalyzeOptions, TRACK_ID, Turn, analyze, speaker_ids, speaker_ranges};
use crate::audio::{decode_16k, file_sha256};
use crate::batch::{HintPick, MeetingFailure, pick_hint_person};
use crate::finish::{apply_merge, title_from_path};
use crate::hints::{
    HintGroup, HintPerson, HintSpeaker, MIN_LABEL_OVERLAP_S, embed_hints, group_hints,
};
use crate::import::{models_used, recording};
use crate::manifest::{Manifest, ManifestIssue, ManifestMeeting, match_person};
use crate::ranges;

/// Purity a cluster needs to be a fitting label.
pub const LABEL_PURITY: f64 = 0.8;

#[derive(Clone, Debug, Default)]
pub struct CalibrateOptions {
    /// Add the hinted people's names to the report (off by default).
    pub include_names: bool,
}

/// Everything calibration found, with denominators.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CalibrationReport {
    /// Embedding model the thresholds belong to.
    pub model: String,
    /// Thresholds were written to the config.
    pub written: bool,
    /// Why nothing was written.
    pub reason: Option<String>,
    /// Valid manifest meetings.
    pub meetings: usize,
    /// Meetings with hints that were analyzed.
    pub analyzed: usize,
    pub issues: Vec<ManifestIssue>,
    pub failed: Vec<MeetingFailure>,
    /// Hinted entries left out because an earlier entry has the same
    /// recording (SHA-256), so no recording crosses a split.
    pub duplicates: Vec<DuplicateRecording>,
    pub splits: SplitCounts,
    pub enrollment: EnrollmentCounts,
    pub fitting: FittingCounts,
    pub thresholds: Option<Thresholds>,
    pub evaluation: Option<EvalReport>,
    /// Speakers of the user's library whose naming changed.
    pub changes: Vec<ops::SpeakerChange>,
    /// Only with [`CalibrateOptions::include_names`].
    pub people: Option<Vec<CalibratedPerson>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SplitCounts {
    pub enroll: usize,
    pub calibrate: usize,
    pub evaluate: usize,
}

/// A manifest entry whose recording an earlier entry already has.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DuplicateRecording {
    /// Index in the manifest's `meetings[]`.
    pub index: usize,
    /// Index of the entry that was kept.
    pub duplicate_of: usize,
    /// Metadata fields in which the two entries disagree (empty: a plain
    /// repeat).
    pub conflicting: Vec<&'static str>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct EnrollmentCounts {
    /// Hinted people in the enrollment set.
    pub hinted: usize,
    pub enrolled: usize,
    /// Not enrolled, per agreement reason.
    pub not_enrolled: BTreeMap<String, usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct FittingCounts {
    /// Calibration-set clusters with a centroid (before merging).
    pub clusters: usize,
    pub labelled: usize,
    /// Overlapping hints but below the purity bar: left out.
    pub impure: usize,
    /// No hinted overlap: left out.
    pub unlabelled: usize,
    pub merge_pairs: usize,
    pub same_person_pairs: usize,
    /// Clusters after merging with the fitted `merge`.
    pub merged_clusters: usize,
    pub trials: usize,
    pub impostor_trials: usize,
    /// No natural impostors: each enrolled trial was repeated without its
    /// own person.
    pub leave_one_out: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CalibratedPerson {
    pub name: Option<String>,
    pub email: Option<String>,
    pub is_self: bool,
    pub enrolled: bool,
    pub reason: identity::AgreementReason,
}

/// One analyzed meeting of the manifest.
struct CalData {
    meeting: Meeting,
    turns: Vec<Turn>,
    groups: Vec<HintGroup>,
    /// Canonical person of each group ([`Registry`]).
    ids: Vec<PersonId>,
    people: Vec<HintPerson>,
    labelled: Vec<HintSpeaker>,
}

impl CalData {
    /// The canonical person of the group with this person key.
    fn id_of_key(&self, key: &str) -> Option<&PersonId> {
        self.groups
            .iter()
            .position(|g| g.key == key)
            .map(|i| &self.ids[i])
    }
}

/// Canonical identities for calibration: the user's people and the
/// manifest's `people[]`, then every hinted person in manifest order,
/// matched with the normal e-mail / name rules ([`pick_hint_person`]).
/// Metadata only: nothing is written and no voice is attached.
struct Registry {
    people: Vec<Person>,
}

impl Registry {
    fn new(lib: &Library, manifest: &Manifest) -> Result<Self> {
        let mut reg = Self {
            people: lib.list_people()?,
        };
        for mp in &manifest.people {
            if match_person(&reg.people, mp.name.as_deref(), mp.email.as_deref()).is_some() {
                continue;
            }
            let name = mp.name.clone().or_else(|| mp.email.clone());
            let mut p = Person::new(reg.new_id(lib), name.unwrap_or_default());
            p.emails.extend(mp.email.clone());
            reg.people.push(p);
        }
        Ok(reg)
    }

    /// An ID free in the user's library and among the transient people.
    fn new_id(&self, lib: &Library) -> PersonId {
        loop {
            let id = lib.new_person_id();
            if !self.people.iter().any(|p| p.id == id) {
                return id;
            }
        }
    }

    fn resolve(&mut self, lib: &Library, setup: &SetupProgress, g: &HintGroup) -> PersonId {
        let pick = pick_hint_person(
            &self.people,
            setup,
            g.is_self,
            g.person_id.as_ref(),
            g.name.as_deref(),
            g.email.as_deref(),
        );
        match pick {
            HintPick::Existing(id) => id,
            HintPick::Promote(id) => {
                if let Some(p) = self.people.iter_mut().find(|p| p.id == id) {
                    p.is_self = true;
                }
                id
            }
            HintPick::New { name, emails } => {
                let mut p = Person::new(self.new_id(lib), name);
                p.is_self = g.is_self;
                p.emails = emails;
                let id = p.id.clone();
                self.people.push(p);
                id
            }
        }
    }
}

/// Metadata fields in which two entries for the same recording disagree.
fn conflicting_fields(a: &ManifestMeeting, b: &ManifestMeeting) -> Vec<&'static str> {
    let mut out = Vec::new();
    if a.source_id != b.source_id {
        out.push("source_id");
    }
    if a.title != b.title {
        out.push("title");
    }
    if a.started_at != b.started_at {
        out.push("started_at");
    }
    if a.language != b.language {
        out.push("language");
    }
    if a.participants != b.participants {
        out.push("participants");
    }
    if a.hints != b.hints {
        out.push("speaker_hints");
    }
    out
}

fn failure(entry: &ManifestMeeting, e: &Error) -> MeetingFailure {
    MeetingFailure {
        index: entry.index,
        source_id: entry.source_id.clone(),
        audio_path: entry.audio_path.clone(),
        code: e.code(),
        message: e.to_string(),
    }
}

/// Runs calibration (see the module docs). `data_dir` holds the config the
/// thresholds are written to.
pub fn calibrate(
    env: &Env<'_>,
    models: &mut ModelSet,
    manifest: &Manifest,
    data_dir: &Path,
    opts: &CalibrateOptions,
) -> Result<CalibrationReport> {
    // Entries with the user's own name or e-mail (`setup`) are the user.
    let mut marked = manifest.clone();
    marked.mark_self(&env.config.setup);
    let manifest = &marked;
    let model = models.embedder.model_id().to_owned();
    let mut report = CalibrationReport {
        model: model.clone(),
        meetings: manifest.meetings.len(),
        issues: manifest.issues.clone(),
        ..CalibrationReport::default()
    };
    let tmp = tempfile::tempdir().map_err(|e| Error::io("creating a temporary library", e))?;
    let temp = Library::open(tmp.path())?;
    let mut registry = Registry::new(env.lib, manifest)?;

    // One entry per recording (by SHA-256): the same file listed twice, by
    // the same path or another, would otherwise sit on both sides of a split.
    let mut hinted: Vec<(&ManifestMeeting, String, u64)> = Vec::new();
    for entry in manifest.meetings.iter().filter(|m| !m.hints.is_empty()) {
        let (sha, bytes) = match file_sha256(&entry.audio_path) {
            Ok(x) => x,
            Err(e) => {
                report.failed.push(failure(entry, &e));
                continue;
            }
        };
        if let Some((first, _, _)) = hinted.iter().find(|(_, s, _)| *s == sha) {
            report.duplicates.push(DuplicateRecording {
                index: entry.index,
                duplicate_of: first.index,
                conflicting: conflicting_fields(first, entry),
            });
            continue;
        }
        hinted.push((entry, sha, bytes));
    }

    // 1. Analyze (no ASR) into the temporary library.
    let mut data: Vec<CalData> = Vec::new();
    for (n, (entry, sha, bytes)) in hinted.iter().enumerate() {
        env.say(format_args!(
            "[{}/{}] {}",
            n + 1,
            hinted.len(),
            entry.audio_path.display()
        ));
        let (sha, bytes) = (sha.as_str(), *bytes);
        let analyzed = (|| -> Result<CalData> {
            let decoded = decode_16k(env.decoders, &entry.audio_path)?;
            let analysis = analyze(&decoded.samples, models, AnalyzeOptions { run_asr: false })?;
            let groups = group_hints(&entry.hints, &registry.people);
            let ids: Vec<PersonId> = groups
                .iter()
                .map(|g| registry.resolve(env.lib, &env.config.setup, g))
                .collect();
            let (people, labelled) = embed_hints(
                &decoded.samples,
                &analysis.diar_segments,
                &analysis.speakers,
                &groups,
                models.embedder.as_ref(),
            )?;
            let started_at = entry
                .started_at
                .or(decoded.info.created_at)
                .or_else(|| crate::finish::file_mtime(&entry.audio_path))
                .unwrap_or_else(schema::now);
            let mut m = Meeting::new(temp.new_meeting_id()?, Source::Import, started_at);
            m.source_id = entry.source_id.clone();
            m.title = entry
                .title
                .clone()
                .or_else(|| title_from_path(&entry.audio_path));
            m.duration_s = analysis.duration_s;
            m.participants = entry
                .participants
                .iter()
                .map(|p| Participant {
                    name: p.name.clone(),
                    email: p.email.clone(),
                    is_self: p.is_self
                        || match_person(&registry.people, p.name.as_deref(), p.email.as_deref())
                            .is_some_and(|k| k.is_self),
                    person_id: None,
                    evidence: Vec::new(),
                })
                .collect();
            m.tracks.push(Track {
                id: TRACK_ID.to_owned(),
                role: TrackRole::Mixed,
            });
            m.speakers = analysis.speakers;
            m.models = models_used(env, &model, false);
            m.processing = Processing {
                status: ProcessingStatus::Analyzed,
                duration_ms: None,
                error: None,
                ..Default::default()
            };
            let ext = crate::finish::audio_ext(&entry.audio_path);
            m.recording = Some(recording(
                &entry.audio_path,
                sha,
                bytes,
                Handling::Reference,
                &ext,
            ));
            temp.write(|tx| {
                tx.create_meeting_dir(&m)?;
                tx.save_meeting(&m)
            })?;
            Ok(CalData {
                meeting: m,
                turns: analysis.diar_segments,
                groups,
                ids,
                people,
                labelled,
            })
        })();
        match analyzed {
            Ok(d) => data.push(d),
            Err(e) => report.failed.push(failure(entry, &e)),
        }
    }
    report.analyzed = data.len();

    // 2. Split by date.
    let cal: Vec<CalMeeting> = data
        .iter()
        .map(|d| CalMeeting {
            id: d.meeting.id.clone(),
            started_at: d.meeting.started_at,
            has_other_hints: d.groups.iter().any(|g| !g.is_self && !g.ranges.is_empty()),
            has_self_hints: d.groups.iter().any(|g| g.is_self && !g.ranges.is_empty()),
        })
        .collect();
    let splits = identity::split_by_date(&cal);
    report.splits = SplitCounts {
        enroll: splits.enroll.len(),
        calibrate: splits.calibrate.len(),
        evaluate: splits.evaluate.len(),
    };
    let set = |list: &[MeetingId]| -> Vec<&CalData> {
        list.iter()
            .filter_map(|id| data.iter().find(|d| &d.meeting.id == id))
            .collect()
    };
    let (enroll_set, cal_set, eval_set) = (
        set(&splits.enroll),
        set(&splits.calibrate),
        set(&splits.evaluate),
    );

    // 3. Enroll from the enrollment set only.
    let (enrolled, people_report) = enroll(&enroll_set, &mut report.enrollment)?;
    if opts.include_names {
        report.people = Some(people_report);
    }
    if enrolled.is_empty() {
        report.reason = Some("nobody could be enrolled from the enrollment set".into());
        return Ok(report);
    }
    let enrolled_ids: BTreeSet<PersonId> = enrolled.iter().map(|p| p.person_id.clone()).collect();

    // 4. Fit merge, then the naming thresholds, on the calibration set.
    let mut pairs: Vec<(f32, bool)> = Vec::new();
    for d in &cal_set {
        let mut labelled: Vec<(Vec<f32>, PersonId)> = Vec::new();
        for s in &d.meeting.speakers {
            let Some(c) = s.centroid.as_ref().filter(|c| c.model == model) else {
                continue;
            };
            report.fitting.clusters += 1;
            match fitting_label(&d.turns, s.id, &d.groups) {
                Label::Pure(g) => {
                    report.fitting.labelled += 1;
                    labelled.push((c.vector()?, d.ids[g].clone()));
                }
                Label::Impure => report.fitting.impure += 1,
                Label::None => report.fitting.unlabelled += 1,
            }
        }
        for (i, (a, pa)) in labelled.iter().enumerate() {
            for (b, pb) in labelled.iter().skip(i + 1) {
                pairs.push((identity::cosine(a, b), pa == pb));
            }
        }
    }
    report.fitting.merge_pairs = pairs.len();
    report.fitting.same_person_pairs = pairs.iter().filter(|p| p.1).count();
    let merge = identity::fit_merge(&pairs);

    let mut trials: Vec<Trial> = Vec::new();
    for d in &cal_set {
        let (speakers, turns) = merged_view(d, &model, merge)?;
        report.fitting.merged_clusters += speakers.len();
        for s in &speakers {
            let Some(c) = s.centroid.as_ref().filter(|c| c.model == model) else {
                continue;
            };
            let Label::Pure(g) = fitting_label(&turns, s.id, &d.groups) else {
                continue;
            };
            let truth = d.ids[g].clone();
            let v = c.vector()?;
            trials.push(Trial {
                scores: enrolled
                    .iter()
                    .filter_map(|p| identity::score(&v, p).map(|sc| (p.person_id.clone(), sc)))
                    .collect(),
                truth: enrolled_ids.contains(&truth).then_some(truth),
                talk_s: s.talk_s,
            });
        }
    }
    if !trials.iter().any(|t| t.truth.is_some()) {
        report.fitting.trials = trials.len();
        report.reason = Some(
            "the calibration set has no hinted speaker of an enrolled person; add more \
             meetings with hints for the same people"
                .into(),
        );
        return Ok(report);
    }
    if !trials.iter().any(|t| t.truth.is_none()) {
        report.fitting.leave_one_out = true;
        let extra: Vec<Trial> = trials
            .iter()
            .filter_map(|t| {
                let truth = t.truth.as_ref()?;
                Some(Trial {
                    scores: t
                        .scores
                        .iter()
                        .filter(|(p, _)| p != truth)
                        .cloned()
                        .collect(),
                    truth: None,
                    talk_s: t.talk_s,
                })
            })
            .collect();
        trials.extend(extra);
    }
    report.fitting.trials = trials.len();
    report.fitting.impostor_trials = trials.iter().filter(|t| t.truth.is_none()).count();
    let Some(mut thresholds) = identity::fit_thresholds(&trials) else {
        report.reason = Some("no calibration trial has an enrolled person".into());
        return Ok(report);
    };
    thresholds.merge = merge;
    report.thresholds = Some(thresholds);

    // 6. Evaluate once with the frozen thresholds.
    let mut intervals = Vec::new();
    for d in &eval_set {
        intervals.extend(eval_intervals(
            d,
            &model,
            &thresholds,
            &enrolled,
            &enrolled_ids,
        )?);
    }
    report.evaluation = Some(identity::evaluate(&intervals));

    // 7. Write (a fresh edit under the config lock, so a concurrent
    // settings change is kept) and re-resolve the user's library.
    let (_, thresholds) = Config::update(data_dir, |config| {
        Ok(config.set_calibrated_thresholds(&model, thresholds))
    })?;
    report.written = true;
    env.say("thresholds written; re-resolving unconfirmed speakers");
    report.changes = crate::reresolve::re_resolve_with_live(
        env.lib,
        &model,
        Some(&thresholds),
        &env.config.live,
    )?
    .into_iter()
    .flat_map(|p| p.changes)
    .collect();
    Ok(report)
}

/// Enrolls from the enrollment set; returns the enrolled people's samples
/// (the production selection: hint agreement, then at most
/// [`MAX_CONFIRMED`] diverse samples) and the per-person outcome.
fn enroll(
    set: &[&CalData],
    counts: &mut EnrollmentCounts,
) -> Result<(Vec<PersonSamples>, Vec<CalibratedPerson>)> {
    let mut pooled: BTreeMap<PersonId, Vec<Vec<f32>>> = BTreeMap::new();
    let mut who: BTreeMap<PersonId, (Option<String>, Option<String>, bool)> = BTreeMap::new();
    let mut labelled: Vec<(PersonId, Vec<f32>)> = Vec::new();
    for d in set {
        for ((g, hp), id) in d.groups.iter().zip(&d.people).zip(&d.ids) {
            who.entry(id.clone())
                .or_insert_with(|| (g.name.clone(), g.email.clone(), g.is_self));
            let slot = pooled.entry(id.clone()).or_default();
            for s in &hp.samples {
                slot.push(s.vector()?);
            }
        }
        for sp in &d.labelled {
            if let Some(id) = d.id_of_key(&sp.person_key) {
                labelled.push((id.clone(), sp.vector()?));
            }
        }
    }
    counts.hinted = pooled.len();
    let mut enrolled = Vec::new();
    let mut people = Vec::new();
    for (id, samples) in &pooled {
        let between: Vec<&[f32]> = pooled
            .iter()
            .filter(|(other, _)| *other != id)
            .flat_map(|(_, s)| s.iter().map(Vec::as_slice))
            .chain(
                labelled
                    .iter()
                    .filter(|(other, _)| other != id)
                    .map(|(_, v)| v.as_slice()),
            )
            .collect();
        let a = identity::hint_agreement(samples, &between);
        if a.enrolled {
            counts.enrolled += 1;
            let kept: Vec<&Vec<f32>> = a.kept.iter().map(|&i| &samples[i]).collect();
            enrolled.push(PersonSamples {
                person_id: id.clone(),
                confirmed: identity::cap_samples(&kept, MAX_CONFIRMED)
                    .into_iter()
                    .map(|k| kept[k].clone())
                    .collect(),
                weak: Vec::new(),
            });
        } else {
            let reason = serde_json::to_value(a.reason)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            *counts.not_enrolled.entry(reason).or_default() += 1;
        }
        let (name, email, is_self) = who.get(id).cloned().unwrap_or_default();
        people.push(CalibratedPerson {
            name,
            email,
            is_self,
            enrolled: a.enrolled,
            reason: a.reason,
        });
    }
    Ok((enrolled, people))
}

enum Label {
    Pure(usize),
    Impure,
    None,
}

/// A cluster's fitting label: the majority hinted group with purity ≥ 0.8
/// of the cluster's hinted speech and ≥ 1 s of overlap.
fn fitting_label(turns: &[Turn], speaker: SpeakerId, groups: &[HintGroup]) -> Label {
    match crate::hints::speaker_label(turns, speaker, groups) {
        None => Label::None,
        Some(l) if l.purity >= LABEL_PURITY && l.overlap_s >= MIN_LABEL_OVERLAP_S => {
            Label::Pure(l.group)
        }
        Some(_) => Label::Impure,
    }
}

/// The meeting's clusters and turns after merging with `merge`, using the
/// import's merge step on a copy (turns stand in for segments).
fn merged_view(d: &CalData, model: &str, merge: Option<f32>) -> Result<(Vec<Speaker>, Vec<Turn>)> {
    let mut m = d.meeting.clone();
    m.segments = d
        .turns
        .iter()
        .enumerate()
        .map(|(i, t)| Segment {
            speech_overlap: Vec::new(),
            id: u32::try_from(i).unwrap_or(u32::MAX),
            track: TRACK_ID.to_owned(),
            speaker: Some(t.speaker),
            start: t.start,
            end: t.end,
            text: String::new(),
            words: Vec::new(),
        })
        .collect();
    let floor = m.roster_floor();
    apply_merge(&mut m, model, merge, floor)?;
    let turns = m
        .segments
        .iter()
        .filter_map(|s| {
            Some(Turn {
                start: s.start,
                end: s.end,
                speaker: s.speaker?,
            })
        })
        .collect();
    Ok((m.speakers, turns))
}

/// Hinted time of one evaluation meeting, cut at turn boundaries so every
/// hinted second is counted once with the prediction of the cluster that
/// covers it (where clusters overlap, the one sharing most time with that
/// person's hints).
fn eval_intervals(
    d: &CalData,
    model: &str,
    thresholds: &Thresholds,
    enrolled: &[PersonSamples],
    enrolled_ids: &BTreeSet<PersonId>,
) -> Result<Vec<EvalInterval>> {
    let (speakers, turns) = merged_view(d, model, thresholds.merge)?;
    let mut predicted: BTreeMap<SpeakerId, Option<(PersonId, NameKind)>> = BTreeMap::new();
    for s in &speakers {
        let p = match s.centroid.as_ref().filter(|c| c.model == model) {
            Some(c) => {
                let r = identity::resolve(&c.vector()?, enrolled, &[], Some(thresholds), None);
                match (r.assignment, r.person_id) {
                    (Assignment::Auto, Some(p)) => Some((p, NameKind::Auto)),
                    (Assignment::Suggested, Some(p)) => Some((p, NameKind::Suggested)),
                    _ => None,
                }
            }
            None => None,
        };
        predicted.insert(s.id, p);
    }
    let mut out = Vec::new();
    for (g, pid) in d.groups.iter().zip(&d.ids) {
        let pid = pid.clone();
        let hinted = if g.is_self {
            PersonOrSelf::Me(pid.clone())
        } else {
            PersonOrSelf::Person(pid.clone())
        };
        let share: BTreeMap<SpeakerId, f64> = speaker_ids(&turns)
            .into_iter()
            .map(|s| (s, ranges::overlap(&speaker_ranges(&turns, s), &g.ranges)))
            .collect();
        for &(a, b) in &g.ranges {
            let mut cuts: Vec<f64> = vec![a, b];
            for t in &turns {
                for x in [t.start, t.end] {
                    if x > a && x < b {
                        cuts.push(x);
                    }
                }
            }
            cuts.sort_by(f64::total_cmp);
            cuts.dedup();
            for w in cuts.windows(2) {
                let (lo, hi) = (w[0], w[1]);
                if hi <= lo {
                    continue;
                }
                let mid = (lo + hi) / 2.0;
                let cover = turns
                    .iter()
                    .filter(|t| t.start <= mid && mid < t.end)
                    .map(|t| t.speaker)
                    .max_by(|x, y| {
                        share
                            .get(x)
                            .unwrap_or(&0.0)
                            .total_cmp(share.get(y).unwrap_or(&0.0))
                            .then(y.cmp(x))
                    });
                out.push(EvalInterval {
                    hinted: hinted.clone(),
                    enrolled: enrolled_ids.contains(&pid),
                    predicted: cover.and_then(|s| predicted.get(&s).cloned().flatten()),
                    covered: cover.is_some(),
                    seconds: hi - lo,
                });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::import::{ImportOptions, import_file};
    use crate::manifest::{ManifestMeeting, ManifestPerson, SpeakerHint};
    use crate::testutil::{Fixture, conversation, models, write_wav};
    use broscribe_platform::DecoderChain;
    use broscribe_platform::fakes::FakeProcessProbe;
    use broscribe_platform::symphonia_decoder::SymphoniaDecoder;
    use chrono::DateTime;

    fn entry(i: usize, path: &Path, day: u32, hints: Vec<SpeakerHint>) -> ManifestMeeting {
        ManifestMeeting {
            index: i,
            audio_path: path.to_owned(),
            sources: Vec::new(),
            per_participant: false,
            source_id: Some(format!("cal-{i}")),
            title: None,
            started_at: Some(
                DateTime::parse_from_rfc3339(&format!("2026-10-{day:02}T10:00:00Z")).unwrap(),
            ),
            ended_at: None,
            duration_s: None,
            language: None,
            participants: Vec::new(),
            hints,
        }
    }

    fn hint(name: &str, ranges: &[(f64, f64)]) -> SpeakerHint {
        SpeakerHint {
            name: Some(name.into()),
            email: None,
            is_self: false,
            ranges: ranges.to_vec(),
        }
    }

    #[test]
    fn guard_writes_nothing_when_the_calibration_set_has_no_enrolled_person() {
        let tmp = tempfile::tempdir().unwrap();
        let lib = Library::open(tmp.path().join("lib")).unwrap();
        let data = tmp.path().join("data");
        let chain = DecoderChain::new(vec![Box::new(SymphoniaDecoder::new())]);
        let probe = FakeProcessProbe::new(std::process::id());
        let config = Config::default();
        let execution = crate::testutil::execution();
        let env = Env {
            lib: &lib,
            config: &config,
            decoders: &chain,
            probe: &probe,
            execution: &execution,
            progress: None,
        };
        // A meeting already in the user's library, unnamed.
        let existing = tmp.path().join("existing.wav");
        write_wav(
            &existing,
            16_000,
            &conversation(&[(Some(0), 4.0), (Some(1), 4.0)]),
        );
        let mut m = models();
        let imported = import_file(&env, &mut m, &existing, &ImportOptions::default()).unwrap();
        let before = lib.load_meeting(&imported.meeting_id).unwrap();

        // Five meetings: Ana and Bea in the enrollment meetings, only Caio
        // and Dora (never enrolled) in the later ones.
        let mut meetings = Vec::new();
        for i in 0..5 {
            let p = tmp.path().join(format!("cal{i}.wav"));
            let (a, b, na, nb) = if i < 3 {
                (0, 1, "Ana Example", "Bea Example")
            } else {
                (2, 3, "Caio Example", "Dora Example")
            };
            // Trailing silence of different lengths keeps the files distinct.
            let pad = 0.25 * (i + 1) as f64;
            write_wav(
                &p,
                16_000,
                &conversation(&[(Some(a), 4.0), (Some(b), 4.0), (None, pad)]),
            );
            meetings.push(entry(
                i,
                &p,
                u32::try_from(i + 1).unwrap(),
                vec![hint(na, &[(0.0, 4.0)]), hint(nb, &[(4.0, 8.0)])],
            ));
        }
        let manifest = Manifest {
            id: "calibration".into(),
            generated_at: None,
            people: Vec::new(),
            meetings,
            issues: Vec::new(),
        };
        let r = calibrate(&env, &mut m, &manifest, &data, &CalibrateOptions::default()).unwrap();
        assert!(!r.written);
        assert!(
            r.reason.as_deref().unwrap().contains("calibration set"),
            "{r:?}"
        );
        assert_eq!(r.analyzed, 5);
        assert_eq!(r.splits.enroll, 3);
        assert_eq!(r.enrollment.enrolled, 2);
        assert!(r.people.is_none());
        // Nothing written: no config file, the user's meeting untouched, no people.
        assert!(!Config::path(&data).exists());
        assert_eq!(lib.load_meeting(&imported.meeting_id).unwrap(), before);
        assert!(lib.list_people().unwrap().is_empty());
        assert_eq!(lib.list_meeting_dirs().unwrap().len(), 1);
    }

    /// Five distinct two-voice recordings (Ana = voice 0, Bea = voice 1).
    fn recordings(fx: &Fixture) -> Vec<PathBuf> {
        (0..5)
            .map(|i| {
                let pad = 0.25 * (i + 1) as f64;
                fx.wav(
                    &format!("r{i}.wav"),
                    &[(Some(0), 4.0), (Some(1), 4.0), (None, pad)],
                )
            })
            .collect()
    }

    #[test]
    fn repeated_recordings_are_analyzed_once() {
        let fx = Fixture::new();
        let hints = || {
            vec![
                hint("Ana Example", &[(0.0, 4.0)]),
                hint("Bea Example", &[(4.0, 8.0)]),
            ]
        };
        let mut meetings: Vec<ManifestMeeting> = recordings(&fx)
            .iter()
            .enumerate()
            .map(|(i, p)| entry(i, p, u32::try_from(i + 1).unwrap(), hints()))
            .collect();
        // The same entry again (same path and metadata) …
        let mut repeat = meetings[0].clone();
        repeat.index = 5;
        // … and recording 1 copied under another path, with other metadata.
        let copy = fx.input.join("copy.wav");
        std::fs::copy(&meetings[1].audio_path, &copy).unwrap();
        let moved = entry(6, &copy, 9, hints());
        meetings.extend([repeat, moved]);
        let manifest = Manifest {
            id: "calibration".into(),
            generated_at: None,
            people: Vec::new(),
            meetings,
            issues: Vec::new(),
        };
        let data = fx._tmp.path().join("data");
        let r = calibrate(
            &fx.env(),
            &mut models(),
            &manifest,
            &data,
            &CalibrateOptions::default(),
        )
        .unwrap();
        assert_eq!(r.analyzed, 5, "{r:?}");
        assert_eq!(r.splits.enroll + r.splits.calibrate + r.splits.evaluate, 5);
        assert_eq!(
            r.duplicates,
            vec![
                DuplicateRecording {
                    index: 5,
                    duplicate_of: 0,
                    conflicting: Vec::new(),
                },
                DuplicateRecording {
                    index: 6,
                    duplicate_of: 1,
                    conflicting: vec!["source_id", "started_at"],
                },
            ]
        );
    }

    #[test]
    fn hints_by_email_and_by_name_are_one_person() {
        let fx = Fixture::new();
        let meetings: Vec<ManifestMeeting> = recordings(&fx)
            .iter()
            .enumerate()
            .map(|(i, p)| {
                // Ana is hinted with her e-mail in some meetings only.
                let ana = SpeakerHint {
                    name: Some("Ana Example".into()),
                    email: (i % 2 == 0).then(|| "ana@example.com".into()),
                    is_self: false,
                    ranges: vec![(0.0, 4.0)],
                };
                entry(
                    i,
                    p,
                    u32::try_from(i + 1).unwrap(),
                    vec![ana, hint("Bea Example", &[(4.0, 8.0)])],
                )
            })
            .collect();
        let manifest = Manifest {
            id: "calibration".into(),
            generated_at: None,
            people: vec![ManifestPerson {
                name: Some("Ana Example".into()),
                email: Some("ana@example.com".into()),
            }],
            meetings,
            issues: Vec::new(),
        };
        let data = fx._tmp.path().join("data");
        let r = calibrate(
            &fx.env(),
            &mut models(),
            &manifest,
            &data,
            &CalibrateOptions {
                include_names: true,
            },
        )
        .unwrap();
        assert_eq!(r.analyzed, 5);
        assert_eq!(r.enrollment.hinted, 2, "{:?}", r.people);
        // The registry is transient: the user's library has no people.
        assert!(fx.lib.list_people().unwrap().is_empty());
    }

    #[test]
    fn partly_hinted_cluster_is_pure_by_its_hinted_speech() {
        let groups = crate::hints::group_hints(
            &[SpeakerHint {
                name: Some("Ana Example".into()),
                email: None,
                is_self: false,
                ranges: vec![(0.0, 7.0)],
            }],
            &[],
        );
        let speaker = SpeakerId::new(1).unwrap();
        let turns = [Turn {
            start: 0.0,
            end: 10.0,
            speaker,
        }];
        assert!(matches!(
            fitting_label(&turns, speaker, &groups),
            Label::Pure(0)
        ));
    }
}
