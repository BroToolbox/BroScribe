//! Hint agreement (plan §13.4, §16.2): decides whether a person's hinted
//! speech pieces agree enough to enroll their voice. Two independent steps,
//! with no absolute similarity threshold anywhere (design §6.5):
//!
//! 1. **Trim.** The medoid is the sample with the highest mean cosine to the
//!    others; dᵢ = cos(vᵢ, medoid) for every other sample. Sample i is dropped
//!    when dᵢ < median(d) − 3·max(1.4826·MAD(d), 0.02). This only bites for
//!    N ≥ 4 (with 2 or 3 samples no dᵢ can fall below the cutoff) and only
//!    removes a minority contamination (below roughly 45%): when a large share
//!    of the pieces belongs to someone else, the median and MAD follow them and
//!    nothing useful is trimmed; the gate below is what rejects such groups.
//! 2. **Gate.** Enroll iff N ≥ 2, the survivors (medoid included) are a strict
//!    majority of N, and the median dᵢ over surviving non-medoid samples
//!    exceeds the 75th percentile of cosines between survivors and `between`.
//!
//! `between` holds samples of *other* people known to be different, supplied
//! by the caller: other hinted people's samples and centroids of diarized
//! speakers that majority-overlap another person's hints, never unhinted
//! clusters (they may be over-splits of this person). Samples are pooled per
//! person over the whole batch; splitting a single ≥3 s range into halves is
//! the caller's job.

use serde::Serialize;

use super::{cosine, finite_vector, median, percentile};

const MAD_SCALE: f32 = 1.4826;
const MAD_FLOOR: f32 = 0.02;
const MAD_K: f32 = 3.0;
const BETWEEN_PERCENTILE: f64 = 0.75;

/// Why a hinted person was or was not enrolled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgreementReason {
    /// Enrolled: the survivors agree more with each other than with others.
    Agreed,
    /// Fewer than 2 samples.
    TooFewSamples,
    /// Trimming left no strict majority.
    NoMajority,
    /// No samples of other people to compare against.
    InsufficientEvidence,
    /// The survivors are no closer to each other than to other people.
    NotDistinct,
}

/// Result of [`hint_agreement`]. `kept` (medoid included) and `dropped` are
/// ascending indices into `samples`; `within` is the median cosine of the kept
/// samples to the medoid and `between_p75` the comparison value, when computed.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Agreement {
    pub enrolled: bool,
    pub kept: Vec<usize>,
    pub dropped: Vec<usize>,
    pub reason: AgreementReason,
    pub medoid: Option<usize>,
    pub within: Option<f32>,
    pub between_p75: Option<f32>,
}

/// Trims and gates one person's hint embeddings (see the module docs). A
/// sample that is not a finite vector ([`finite_vector`]) is dropped before
/// both steps and still counts in N, so it can never help a group reach
/// its majority; non-finite `between` vectors are ignored.
pub fn hint_agreement<S: AsRef<[f32]>, B: AsRef<[f32]>>(samples: &[S], between: &[B]) -> Agreement {
    let n = samples.len();
    let usable: Vec<usize> = (0..n)
        .filter(|&i| finite_vector(samples[i].as_ref()))
        .collect();
    let usable_between: Vec<&[f32]> = between
        .iter()
        .map(|b| b.as_ref())
        .filter(|b| finite_vector(b))
        .collect();
    if usable.len() == n && usable_between.len() == between.len() {
        return agreement_of(samples, between);
    }
    let subset: Vec<&[f32]> = usable.iter().map(|&i| samples[i].as_ref()).collect();
    let mut out = agreement_of(&subset, &usable_between);
    out.kept = out.kept.iter().map(|&k| usable[k]).collect();
    out.medoid = out.medoid.map(|k| usable[k]);
    let mut dropped: Vec<usize> = out.dropped.iter().map(|&k| usable[k]).collect();
    dropped.extend((0..n).filter(|i| !usable.contains(i)));
    dropped.sort_unstable();
    out.dropped = dropped;
    if out.reason != AgreementReason::TooFewSamples && out.kept.len() * 2 <= n {
        out.enrolled = false;
        out.reason = AgreementReason::NoMajority;
    }
    out
}

fn agreement_of<S: AsRef<[f32]>, B: AsRef<[f32]>>(samples: &[S], between: &[B]) -> Agreement {
    let n = samples.len();
    let mut out = Agreement {
        enrolled: false,
        kept: (0..n).collect(),
        dropped: Vec::new(),
        reason: AgreementReason::TooFewSamples,
        medoid: None,
        within: None,
        between_p75: None,
    };
    if n < 2 {
        return out;
    }

    // Pairwise cosines; the diagonal stays 0 so a row sum is the sum over the others.
    let mut sim = vec![vec![0.0f32; n]; n];
    for (i, si) in samples.iter().enumerate() {
        for (j, sj) in samples.iter().enumerate().skip(i + 1) {
            let c = cosine(si.as_ref(), sj.as_ref());
            sim[i][j] = c;
            sim[j][i] = c;
        }
    }
    // argmax of the mean cosine to the others (= row sum / (n − 1)); ties: lowest index.
    let mut medoid = 0;
    let mut best = f32::NEG_INFINITY;
    for (i, row) in sim.iter().enumerate() {
        let total: f32 = row.iter().sum();
        if total > best {
            best = total;
            medoid = i;
        }
    }
    out.medoid = Some(medoid);

    // Step 1: trim.
    let others: Vec<usize> = (0..n).filter(|&i| i != medoid).collect();
    let d: Vec<f32> = others.iter().map(|&i| sim[i][medoid]).collect();
    let med = median(&d).unwrap_or(0.0);
    let deviations: Vec<f32> = d.iter().map(|&x| (x - med).abs()).collect();
    let mad = median(&deviations).unwrap_or(0.0);
    let cutoff = med - MAD_K * (MAD_SCALE * mad).max(MAD_FLOOR);
    let mut kept_d = Vec::with_capacity(d.len());
    out.kept = vec![medoid];
    out.dropped.clear();
    for (&i, &di) in others.iter().zip(&d) {
        if di < cutoff {
            out.dropped.push(i);
        } else {
            out.kept.push(i);
            kept_d.push(di);
        }
    }
    out.kept.sort_unstable();

    // Step 2: gate.
    if out.kept.len() * 2 <= n {
        out.reason = AgreementReason::NoMajority;
        return out;
    }
    let within = median(&kept_d);
    out.within = within;
    if between.is_empty() {
        out.reason = AgreementReason::InsufficientEvidence;
        return out;
    }
    let cross: Vec<f32> = out
        .kept
        .iter()
        .flat_map(|&i| {
            between
                .iter()
                .map(move |b| cosine(samples[i].as_ref(), b.as_ref()))
        })
        .collect();
    let p75 = percentile(&cross, BETWEEN_PERCENTILE);
    out.between_p75 = p75;
    out.enrolled = matches!((within, p75), (Some(w), Some(b)) if w > b);
    out.reason = if out.enrolled {
        AgreementReason::Agreed
    } else {
        AgreementReason::NotDistinct
    };
    out
}

#[cfg(test)]
mod tests {
    use super::super::testutil::{Rng, basis};
    use super::*;

    const SIGMA: f64 = 0.03;

    fn others(rng: &mut Rng) -> Vec<Vec<f32>> {
        let mut b = rng.voices(1, 3, SIGMA);
        b.extend(rng.voices(2, 3, SIGMA));
        b
    }

    #[test]
    fn far_outlier_among_many_is_dropped() {
        let mut rng = Rng::new(42);
        let mut samples = rng.voices(0, 7, SIGMA);
        samples.insert(3, rng.voice(1, SIGMA));
        let between = others(&mut rng);
        let a = hint_agreement(&samples, &between);
        assert_eq!(a.dropped, vec![3]);
        assert_eq!(a.kept, vec![0, 1, 2, 4, 5, 6, 7]);
        assert!(a.enrolled);
        assert_eq!(a.reason, AgreementReason::Agreed);
    }

    #[test]
    fn two_disagreeing_samples_are_rejected_by_the_gate() {
        let mut rng = Rng::new(43);
        let samples = vec![rng.voice(0, SIGMA), rng.voice(1, SIGMA)];
        let mut between = rng.voices(1, 3, SIGMA);
        between.push(rng.voice(2, SIGMA));
        let a = hint_agreement(&samples, &between);
        assert_eq!(a.kept, vec![0, 1], "nothing is trimmed with two samples");
        assert!(!a.enrolled);
        assert_eq!(a.reason, AgreementReason::NotDistinct);
    }

    #[test]
    fn inconsistent_group_is_rejected() {
        let mut rng = Rng::new(44);
        let samples: Vec<Vec<f32>> = (0..6).map(|i| rng.voice(i % 2, SIGMA)).collect();
        let mut between = rng.voices(0, 3, SIGMA);
        between.extend(rng.voices(1, 3, SIGMA));
        let a = hint_agreement(&samples, &between);
        assert!(!a.enrolled);
        assert_ne!(a.reason, AgreementReason::Agreed);
    }

    #[test]
    fn few_consistent_samples_are_enrolled() {
        let mut rng = Rng::new(45);
        for n in [2, 3] {
            let samples = rng.voices(0, n, SIGMA);
            let between = others(&mut rng);
            let a = hint_agreement(&samples, &between);
            assert!(a.enrolled, "n = {n}: {a:?}");
            assert_eq!(a.kept, (0..n).collect::<Vec<_>>());
            assert!(a.dropped.is_empty());
        }
    }

    #[test]
    fn empty_between_is_insufficient_evidence() {
        let mut rng = Rng::new(46);
        let samples = rng.voices(0, 4, SIGMA);
        let a = hint_agreement(&samples, &[] as &[Vec<f32>]);
        assert!(!a.enrolled);
        assert_eq!(a.reason, AgreementReason::InsufficientEvidence);
        assert_eq!(a.kept.len(), 4);
    }

    #[test]
    fn zero_mad_drops_nothing() {
        let samples = vec![basis(0); 5];
        let a = hint_agreement(&samples, &[basis(1)]);
        assert!(a.dropped.is_empty());
        assert_eq!(a.kept, vec![0, 1, 2, 3, 4]);
        assert!(a.enrolled);
    }

    #[test]
    fn single_sample_is_too_few() {
        let a = hint_agreement(&[basis(0)], &[basis(1)]);
        assert!(!a.enrolled);
        assert_eq!(a.reason, AgreementReason::TooFewSamples);
    }

    #[test]
    fn non_finite_samples_never_enroll_and_count_against_the_majority() {
        use super::super::testutil::DIM;
        let mut rng = Rng::new(47);
        let mut samples = rng.voices(0, 3, SIGMA);
        samples.extend([
            vec![f32::NAN; DIM],
            vec![f32::INFINITY; DIM],
            vec![f32::NAN; DIM],
        ]);
        let between = others(&mut rng);
        let a = hint_agreement(&samples, &between);
        assert!(!a.enrolled);
        assert_eq!(a.reason, AgreementReason::NoMajority);
        assert_eq!(a.dropped, vec![3, 4, 5]);
        // One bad piece among clean ones is dropped; the rest still agree,
        // and a non-finite comparison vector is ignored.
        let mut samples = rng.voices(0, 5, SIGMA);
        samples.insert(2, vec![f32::INFINITY; DIM]);
        let mut between = others(&mut rng);
        between.push(vec![f32::NAN; DIM]);
        let a = hint_agreement(&samples, &between);
        assert!(a.enrolled, "{a:?}");
        assert!(a.dropped.contains(&2) && !a.kept.contains(&2));
        assert!(a.between_p75.is_some_and(f32::is_finite));
    }
}
