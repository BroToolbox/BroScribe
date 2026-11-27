//! Bench scoring (design §12): word error rate on hand-checked text,
//! diarization and Jaccard error rates, clusters per true speaker and naming
//! outcomes. Pure functions over references and hypotheses; no I/O.

use std::collections::BTreeMap;

use broscribe_core::schema::Assignment;
use serde::{Deserialize, Serialize};

use crate::ranges::{self, Range};

/// Collar around every reference turn boundary excluded from DER/JER, in
/// seconds on each side.
pub const COLLAR_S: f64 = 0.25;
/// Overlap a hypothesis speaker needs with a reference speaker to count as
/// one of its clusters, or to be labelled with it.
pub const MIN_OVERLAP_S: f64 = 1.0;

/// A figure with its denominator, or the reason it could not be computed.
/// A missing reference is always [`Metric::Unavailable`], never zero.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Metric {
    Value {
        value: f64,
        /// Denominator the value was computed over (`of_unit`).
        of: f64,
        of_unit: String,
    },
    Unavailable {
        unavailable: String,
    },
}

impl Metric {
    pub fn value(value: f64, of: f64, of_unit: &str) -> Self {
        Self::Value {
            value,
            of,
            of_unit: of_unit.to_owned(),
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            unavailable: reason.into(),
        }
    }

    /// `numerator / denominator`, unavailable (with `reason`) when the
    /// denominator is not positive.
    pub fn ratio(numerator: f64, denominator: f64, of_unit: &str, reason: &str) -> Self {
        if denominator > 0.0 && numerator.is_finite() {
            Self::value(numerator / denominator, denominator, of_unit)
        } else {
            Self::unavailable(reason)
        }
    }

    pub fn get(&self) -> Option<f64> {
        match self {
            Self::Value { value, .. } => Some(*value),
            Self::Unavailable { .. } => None,
        }
    }
}

// ---- WER ----

/// Words for WER: lowercase; punctuation becomes a word break except an
/// apostrophe inside a word (dropped: `don't` → `dont`) and `.`/`,` between
/// digits (numbers stay as written: `3.5`, `1,000`; never spelled out).
pub fn normalize_words(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut cleaned = String::with_capacity(text.len());
    for (i, &c) in chars.iter().enumerate() {
        let prev = i.checked_sub(1).and_then(|j| chars.get(j)).copied();
        let next = chars.get(i + 1).copied();
        if c.is_alphanumeric() {
            cleaned.extend(c.to_lowercase());
        } else if matches!(c, '.' | ',')
            && prev.is_some_and(|p| p.is_ascii_digit())
            && next.is_some_and(|n| n.is_ascii_digit())
        {
            cleaned.push(c);
        } else if matches!(c, '\'' | '\u{2019}')
            && prev.is_some_and(char::is_alphanumeric)
            && next.is_some_and(char::is_alphanumeric)
        {
            // Joined: `don't` → `dont`.
        } else {
            cleaned.push(' ');
        }
    }
    cleaned.split_whitespace().map(str::to_owned).collect()
}

/// Edit counts of the minimum-cost word alignment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WordErrors {
    pub substitutions: usize,
    pub deletions: usize,
    pub insertions: usize,
    pub reference_words: usize,
}

impl WordErrors {
    pub fn edits(&self) -> usize {
        self.substitutions + self.deletions + self.insertions
    }

    pub fn add(&mut self, other: &WordErrors) {
        self.substitutions += other.substitutions;
        self.deletions += other.deletions;
        self.insertions += other.insertions;
        self.reference_words += other.reference_words;
    }

    /// WER over the reference words; unavailable without any.
    pub fn wer(&self) -> Metric {
        Metric::ratio(
            self.edits() as f64,
            self.reference_words as f64,
            "reference words",
            "the reference text has no words",
        )
    }
}

/// Levenshtein alignment of normalised words (two rows of memory).
pub fn word_errors(reference: &[String], hypothesis: &[String]) -> WordErrors {
    // Each cell: (cost, substitutions, deletions, insertions).
    type Cell = (usize, usize, usize, usize);
    let mut prev: Vec<Cell> = (0..=hypothesis.len()).map(|j| (j, 0, 0, j)).collect();
    let mut row: Vec<Cell> = vec![(0, 0, 0, 0); hypothesis.len() + 1];
    for (i, r) in reference.iter().enumerate() {
        row[0] = (i + 1, 0, i + 1, 0);
        for (j, h) in hypothesis.iter().enumerate() {
            let diag = prev[j];
            let sub = if r == h {
                diag
            } else {
                (diag.0 + 1, diag.1 + 1, diag.2, diag.3)
            };
            let up = prev[j + 1];
            let del = (up.0 + 1, up.1, up.2 + 1, up.3);
            let left = row[j];
            let ins = (left.0 + 1, left.1, left.2, left.3 + 1);
            row[j + 1] = [sub, del, ins]
                .into_iter()
                .min_by_key(|c| c.0)
                .expect("three candidates");
        }
        std::mem::swap(&mut prev, &mut row);
    }
    let last = prev[hypothesis.len()];
    WordErrors {
        substitutions: last.1,
        deletions: last.2,
        insertions: last.3,
        reference_words: reference.len(),
    }
}

// ---- diarization ----

/// A speaker turn: reference turns are labelled with a person (ID or a
/// hint key), hypothesis turns with the candidate's speaker ID.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LabelledTurn {
    pub start: f64,
    pub end: f64,
    pub label: String,
}

/// Times and error components in seconds over the scored region.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DiarErrors {
    /// Reference speech time, overlap counted once per speaker.
    pub reference_s: f64,
    pub missed_s: f64,
    pub false_alarm_s: f64,
    pub confusion_s: f64,
    /// Sum of per-reference-speaker Jaccard errors.
    pub jer_sum: f64,
    pub jer_speakers: usize,
}

impl DiarErrors {
    pub fn add(&mut self, other: &DiarErrors) {
        self.reference_s += other.reference_s;
        self.missed_s += other.missed_s;
        self.false_alarm_s += other.false_alarm_s;
        self.confusion_s += other.confusion_s;
        self.jer_sum += other.jer_sum;
        self.jer_speakers += other.jer_speakers;
    }

    pub fn der(&self) -> Metric {
        Metric::ratio(
            self.missed_s + self.false_alarm_s + self.confusion_s,
            self.reference_s,
            "scored reference seconds",
            "no scored reference speech",
        )
    }

    pub fn jer(&self) -> Metric {
        Metric::ratio(
            self.jer_sum,
            self.jer_speakers as f64,
            "reference speakers",
            "no reference speaker",
        )
    }
}

/// An elementary interval with the labels active in it.
struct Slice {
    dur: f64,
    refs: Vec<usize>,
    hyps: Vec<usize>,
}

/// Turns clipped to `region`, labels interned.
struct Timeline {
    ref_labels: Vec<String>,
    hyp_labels: Vec<String>,
    slices: Vec<Slice>,
}

fn intern(labels: &mut Vec<String>, label: &str) -> usize {
    match labels.iter().position(|l| l == label) {
        Some(i) => i,
        None => {
            labels.push(label.to_owned());
            labels.len() - 1
        }
    }
}

fn clip(turns: &[LabelledTurn], (a, b): Range) -> Vec<LabelledTurn> {
    turns
        .iter()
        .filter_map(|t| {
            let (s, e) = (t.start.max(a), t.end.min(b));
            (e > s).then(|| LabelledTurn {
                start: s,
                end: e,
                label: t.label.clone(),
            })
        })
        .collect()
}

/// Splits `region` at every turn boundary; slices whose midpoint falls in
/// `no_score` are dropped.
fn timeline(
    reference: &[LabelledTurn],
    hypothesis: &[LabelledTurn],
    region: Range,
    no_score: &[Range],
) -> Timeline {
    let reference = clip(reference, region);
    let hypothesis = clip(hypothesis, region);
    let mut points = vec![region.0, region.1];
    for t in reference.iter().chain(&hypothesis) {
        points.extend([t.start, t.end]);
    }
    for &(a, b) in no_score {
        points.extend([a, b]);
    }
    points.retain(|p| p.is_finite() && *p >= region.0 && *p <= region.1);
    points.sort_by(f64::total_cmp);
    points.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    let mut ref_labels = Vec::new();
    let mut hyp_labels = Vec::new();
    let ref_idx: Vec<usize> = reference
        .iter()
        .map(|t| intern(&mut ref_labels, &t.label))
        .collect();
    let hyp_idx: Vec<usize> = hypothesis
        .iter()
        .map(|t| intern(&mut hyp_labels, &t.label))
        .collect();
    let active = |turns: &[LabelledTurn], idx: &[usize], mid: f64| -> Vec<usize> {
        let mut v: Vec<usize> = turns
            .iter()
            .zip(idx)
            .filter(|(t, _)| t.start <= mid && mid < t.end)
            .map(|(_, &i)| i)
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let mut slices = Vec::new();
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        let mid = (a + b) / 2.0;
        if no_score.iter().any(|&(s, e)| s <= mid && mid < e) {
            continue;
        }
        slices.push(Slice {
            dur: b - a,
            refs: active(&reference, &ref_idx, mid),
            hyps: active(&hypothesis, &hyp_idx, mid),
        });
    }
    Timeline {
        ref_labels,
        hyp_labels,
        slices,
    }
}

/// Seconds where reference `r` and hypothesis `h` are both active.
fn overlap_matrix(t: &Timeline) -> Vec<Vec<f64>> {
    let mut m = vec![vec![0.0; t.hyp_labels.len()]; t.ref_labels.len()];
    for s in &t.slices {
        for &r in &s.refs {
            for &h in &s.hyps {
                m[r][h] += s.dur;
            }
        }
    }
    m
}

/// One-to-one mapping of rows to columns maximising the total weight
/// (Hungarian algorithm); pairs with no weight stay unmapped.
pub fn max_assignment(weights: &[Vec<f64>]) -> Vec<Option<usize>> {
    let rows = weights.len();
    let cols = weights.first().map_or(0, Vec::len);
    if rows == 0 || cols == 0 {
        return vec![None; rows];
    }
    // The algorithm needs rows ≤ columns: transpose otherwise.
    let transposed = rows > cols;
    let (n, m) = if transposed {
        (cols, rows)
    } else {
        (rows, cols)
    };
    let w = |i: usize, j: usize| {
        if transposed {
            weights[j][i]
        } else {
            weights[i][j]
        }
    };
    let cost = |i: usize, j: usize| -w(i - 1, j - 1);
    let inf = f64::INFINITY;
    let mut u = vec![0.0; n + 1];
    let mut v = vec![0.0; m + 1];
    let mut p = vec![0usize; m + 1];
    let mut way = vec![0usize; m + 1];
    for i in 1..=n {
        p[0] = i;
        let mut j0 = 0;
        let mut minv = vec![inf; m + 1];
        let mut used = vec![false; m + 1];
        loop {
            used[j0] = true;
            let i0 = p[j0];
            let mut delta = inf;
            let mut j1 = 0;
            for j in 1..=m {
                if used[j] {
                    continue;
                }
                let cur = cost(i0, j) - u[i0] - v[j];
                if cur < minv[j] {
                    minv[j] = cur;
                    way[j] = j0;
                }
                if minv[j] < delta {
                    delta = minv[j];
                    j1 = j;
                }
            }
            for (j, &is_used) in used.iter().enumerate() {
                if is_used {
                    u[p[j]] += delta;
                    v[j] -= delta;
                } else {
                    minv[j] -= delta;
                }
            }
            j0 = j1;
            if p[j0] == 0 {
                break;
            }
        }
        loop {
            let j1 = way[j0];
            p[j0] = p[j1];
            j0 = j1;
            if j0 == 0 {
                break;
            }
        }
    }
    let mut out = vec![None; rows];
    for (j, &assigned) in p.iter().enumerate().take(m + 1).skip(1) {
        if assigned == 0 {
            continue;
        }
        let (r, c) = if transposed {
            (j - 1, assigned - 1)
        } else {
            (assigned - 1, j - 1)
        };
        if weights[r][c] > 0.0 {
            out[r] = Some(c);
        }
    }
    out
}

/// No-score zones: `collar` seconds on each side of every reference turn
/// boundary, inside `region`.
fn collar_zones(reference: &[LabelledTurn], region: Range, collar: f64) -> Vec<Range> {
    if collar <= 0.0 {
        return Vec::new();
    }
    let clipped = clip(reference, region);
    ranges::union(
        clipped
            .iter()
            .flat_map(|t| [t.start, t.end])
            .map(|b| ((b - collar).max(region.0), (b + collar).min(region.1))),
    )
}

/// DER components and JER over `region` (overlapping speech scored: every
/// active reference speaker counts). DER maximises matched overlap, while
/// JER independently maximises matched Jaccard similarity. `collar` seconds
/// around reference boundaries are excluded from both.
pub fn diarization_errors(
    reference: &[LabelledTurn],
    hypothesis: &[LabelledTurn],
    region: Range,
    collar: f64,
) -> DiarErrors {
    let zones = collar_zones(reference, region, collar);
    let t = timeline(reference, hypothesis, region, &zones);
    let mut overlap = overlap_matrix(&t);
    let map = max_assignment(&overlap);
    let mut out = DiarErrors::default();
    let mut ref_duration = vec![0.0; t.ref_labels.len()];
    let mut hyp_duration = vec![0.0; t.hyp_labels.len()];
    for s in &t.slices {
        let (n_ref, n_hyp) = (s.refs.len(), s.hyps.len());
        let correct = s
            .refs
            .iter()
            .filter(|&&r| map[r].is_some_and(|h| s.hyps.contains(&h)))
            .count();
        out.reference_s += s.dur * n_ref as f64;
        out.missed_s += s.dur * n_ref.saturating_sub(n_hyp) as f64;
        out.false_alarm_s += s.dur * n_hyp.saturating_sub(n_ref) as f64;
        out.confusion_s += s.dur * (n_ref.min(n_hyp) - correct) as f64;
        for &r in &s.refs {
            ref_duration[r] += s.dur;
        }
        for &h in &s.hyps {
            hyp_duration[h] += s.dur;
        }
    }
    for (r, row) in overlap.iter_mut().enumerate() {
        for (h, intersection) in row.iter_mut().enumerate() {
            let union = ref_duration[r] + hyp_duration[h] - *intersection;
            *intersection = if union > 0.0 {
                *intersection / union
            } else {
                0.0
            };
        }
    }
    let jer_map = max_assignment(&overlap);
    for (r, mapped) in jer_map.iter().enumerate() {
        if ref_duration[r] > 0.0 {
            out.jer_sum += 1.0 - mapped.map_or(0.0, |h| overlap[r][h]);
            out.jer_speakers += 1;
        }
    }
    out
}

/// For each reference speaker with at least [`MIN_OVERLAP_S`] of speech in
/// `region`: how many hypothesis speakers overlap it by at least
/// [`MIN_OVERLAP_S`] (0 when it was missed).
pub fn clusters_per_speaker(
    reference: &[LabelledTurn],
    hypothesis: &[LabelledTurn],
    region: Range,
) -> BTreeMap<String, usize> {
    let t = timeline(reference, hypothesis, region, &[]);
    let m = overlap_matrix(&t);
    let mut talk = vec![0.0; t.ref_labels.len()];
    for s in &t.slices {
        for &r in &s.refs {
            talk[r] += s.dur;
        }
    }
    t.ref_labels
        .iter()
        .enumerate()
        .filter(|(r, _)| talk[*r] >= MIN_OVERLAP_S)
        .map(|(r, label)| {
            let n = m[r].iter().filter(|&&o| o >= MIN_OVERLAP_S).count();
            (label.clone(), n)
        })
        .collect()
}

// ---- naming ----

/// A hypothesis speaker's naming state after resolve.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HypIdentity {
    /// Speaker label used in the hypothesis turns.
    pub label: String,
    pub assignment: Assignment,
    /// Named person (`auto` or `confirmed`).
    pub person: Option<String>,
    /// Suggested person (`suggested`).
    pub suggested: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NamingOutcome {
    /// Reference speech seconds (overlap counted per speaker).
    pub reference_s: f64,
    /// Of which the speaking cluster carries the right outcome.
    pub correct_s: f64,
    /// Estimated corrections: clusters named wrongly, left unnamed or
    /// suggested wrongly.
    pub fixes: usize,
    /// Hypothesis speakers matched to a reference speaker.
    pub speakers: usize,
}

/// Naming outcome over `region`. A reference label for which `known`
/// returns `false` is someone absent from the library: the right outcome is
/// no automatic name. Hypothesis speakers overlapping no reference speaker
/// by [`MIN_OVERLAP_S`] are not judged.
pub fn naming(
    reference: &[LabelledTurn],
    hypothesis: &[LabelledTurn],
    identities: &[HypIdentity],
    known: &dyn Fn(&str) -> bool,
    region: Range,
) -> NamingOutcome {
    let t = timeline(reference, hypothesis, region, &[]);
    let identity = |h: usize| identities.iter().find(|i| i.label == t.hyp_labels[h]);
    let named = |h: usize| {
        identity(h).and_then(|i| match i.assignment {
            Assignment::Auto | Assignment::Confirmed => i.person.as_deref(),
            _ => None,
        })
    };
    let right_for = |h: usize, r: usize| {
        let truth = t.ref_labels[r].as_str();
        match named(h) {
            Some(p) => known(truth) && p == truth,
            None => !known(truth),
        }
    };
    let mut out = NamingOutcome::default();
    for s in &t.slices {
        for &r in &s.refs {
            out.reference_s += s.dur;
            if s.hyps.iter().any(|&h| right_for(h, r)) {
                out.correct_s += s.dur;
            }
        }
    }
    let m = overlap_matrix(&t);
    for (h, _) in t.hyp_labels.iter().enumerate() {
        let best = (0..t.ref_labels.len())
            .filter(|&r| m[r][h] >= MIN_OVERLAP_S)
            .max_by(|&a, &b| m[a][h].total_cmp(&m[b][h]));
        // A label without identity is a cluster merged away without any
        // words: nothing in the transcript to correct.
        let (Some(r), Some(i)) = (best, identity(h)) else {
            continue;
        };
        out.speakers += 1;
        let truth = t.ref_labels[r].as_str();
        let fix = match i.assignment {
            Assignment::Auto | Assignment::Confirmed => i.person.as_deref() != Some(truth),
            Assignment::Suggested => i.suggested.as_deref() != Some(truth),
            Assignment::None => true,
        };
        if fix {
            out.fixes += 1;
        }
    }
    out
}
