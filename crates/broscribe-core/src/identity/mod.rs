//! Identity math (design §6.4, plan §5 and §13.3/4/7/8): scoring and resolving
//! speakers against enrolled people, merging over-split clusters, hint
//! agreement and threshold calibration. Pure code: no I/O, no clocks, no
//! randomness. Every vector passed in comes from one embedding model; mixing
//! models is the caller's error (vectors of different lengths score 0).

mod agreement;
mod calibrate;
mod groups;
mod score;

pub use agreement::{Agreement, AgreementReason, hint_agreement};
pub use calibrate::{
    CalMeeting, EvalInterval, EvalReport, MeReport, PersonOrSelf, Splits, Tally, Trial,
    UnenrolledReport, evaluate, fit_merge, fit_thresholds, split_by_date,
};
pub use groups::{auto_groups, merge_input, proven_person};
pub(crate) use score::group_centroid;
pub use score::{
    MAX_CONFIRMED, MAX_WEAK, MergeConstraints, MergeGroup, MergeInput, NameKind, PersonSamples,
    Ranked, Resolution, WEAK_WEIGHT, cap_samples, cap_weak, compatible, kept_speaker,
    merge_clusters, merge_clusters_with, rank, resolve, resolve_ranked, resolve_voice, score,
};

/// Cosine similarity. Returns 0 for vectors of different lengths, empty
/// vectors or a zero vector.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (&x, &y) in a.iter().zip(b) {
        let (x, y) = (f64::from(x), f64::from(y));
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na.sqrt() * nb.sqrt())) as f32
}

/// Whether `v` is a usable embedding: non-empty and every value finite. A
/// vector with a NaN or infinity never ranks, merges, enrolls or is kept
/// over a finite one when samples are capped.
pub fn finite_vector(v: &[f32]) -> bool {
    !v.is_empty() && v.iter().all(|x| x.is_finite())
}

/// A stored vector (`vector_b64` holding `dim` values, as
/// [`crate::schema::decode_vector`] reads it) for identity use: `Some` when
/// it decodes to a usable vector ([`finite_vector`]); `None` when its data
/// is well-formed but unusable (empty, or a NaN or infinity, which the
/// decoder refuses), so it is skipped like an absent vector and can never
/// rank, merge or enroll; an error when the data itself is malformed
/// (invalid base64 or another size), as before.
pub fn usable_vector(vector_b64: &str, dim: usize) -> crate::Result<Option<Vec<f32>>> {
    match crate::schema::decode_vector(vector_b64, dim) {
        Ok(v) => Ok(Some(v).filter(|v| finite_vector(v))),
        Err(e) => {
            use base64::Engine as _;
            // Right size and valid base64: the decoder refused the values.
            let well_formed = base64::engine::general_purpose::STANDARD
                .decode(vector_b64)
                .is_ok_and(|bytes| dim.checked_mul(4) == Some(bytes.len()));
            if well_formed { Ok(None) } else { Err(e) }
        }
    }
}

/// Scales `v` to unit length in place; a zero vector is left unchanged.
pub fn l2_normalize(v: &mut [f32]) {
    let norm = v
        .iter()
        .map(|&x| f64::from(x) * f64::from(x))
        .sum::<f64>()
        .sqrt();
    if norm > 0.0 {
        for x in v {
            *x = (f64::from(*x) / norm) as f32;
        }
    }
}

/// Centroid: the mean of the L2-normalised vectors, L2-normalised again
/// (plan §13.2). `None` when `vectors` is empty or their lengths differ.
pub fn mean_normalized<V: AsRef<[f32]>>(vectors: &[V]) -> Option<Vec<f32>> {
    weighted_mean_normalized(vectors.iter().map(|v| (v.as_ref(), 1.0)))
}

/// Like [`mean_normalized`] with a non-negative weight per vector.
pub(crate) fn weighted_mean_normalized<'a>(
    items: impl IntoIterator<Item = (&'a [f32], f64)>,
) -> Option<Vec<f32>> {
    let mut sum: Option<Vec<f64>> = None;
    for (v, w) in items {
        let acc = sum.get_or_insert_with(|| vec![0.0; v.len()]);
        if acc.len() != v.len() || v.is_empty() {
            return None;
        }
        let norm = v
            .iter()
            .map(|&x| f64::from(x) * f64::from(x))
            .sum::<f64>()
            .sqrt();
        if norm == 0.0 {
            continue;
        }
        for (a, &x) in acc.iter_mut().zip(v) {
            *a += w * f64::from(x) / norm;
        }
    }
    let mut out: Vec<f32> = sum?.into_iter().map(|x| x as f32).collect();
    l2_normalize(&mut out);
    Some(out)
}

/// Median of `values` (mean of the two middle values for an even count).
/// `None` when empty. NaNs sort last.
pub(crate) fn median(values: &[f32]) -> Option<f32> {
    percentile(values, 0.5)
}

/// Percentile `p` in `[0, 1]` with linear interpolation between closest ranks
/// (the common "type 7" definition). `None` when empty.
pub(crate) fn percentile(values: &[f32], p: f64) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f32::total_cmp);
    let pos = p.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    let frac = pos - lo as f64;
    let (a, b) = (f64::from(sorted[lo]), f64::from(sorted[hi]));
    Some((a + (b - a) * frac) as f32)
}

#[cfg(test)]
pub(crate) mod testutil {
    //! Deterministic synthetic voices: unit vectors scattered around per-person
    //! means (orthogonal basis directions), from a seeded SplitMix64 + Box–Muller.

    pub const DIM: usize = 32;

    pub struct Rng(u64);

    impl Rng {
        pub fn new(seed: u64) -> Self {
            Self(seed)
        }

        fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// Uniform in (0, 1).
        fn uniform(&mut self) -> f64 {
            ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
        }

        fn normal(&mut self) -> f64 {
            let (u1, u2) = (self.uniform(), self.uniform());
            (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
        }

        /// A unit vector near basis direction `person` (noise σ per component).
        pub fn voice(&mut self, person: usize, sigma: f64) -> Vec<f32> {
            let mut v: Vec<f32> = (0..DIM).map(|_| (self.normal() * sigma) as f32).collect();
            v[person] += 1.0;
            super::l2_normalize(&mut v);
            v
        }

        pub fn voices(&mut self, person: usize, n: usize, sigma: f64) -> Vec<Vec<f32>> {
            (0..n).map(|_| self.voice(person, sigma)).collect()
        }
    }

    pub fn basis(i: usize) -> Vec<f32> {
        let mut v = vec![0.0; DIM];
        v[i] = 1.0;
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_and_normalisation() {
        assert!((cosine(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 3.0]).abs() < 1e-6);
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
        let mut zero = vec![0.0, 0.0];
        l2_normalize(&mut zero);
        assert_eq!(zero, vec![0.0, 0.0]);
    }

    #[test]
    fn centroid_is_unit_mean_of_unit_vectors() {
        let c = mean_normalized(&[vec![10.0, 0.0], vec![0.0, 1.0]]).unwrap();
        let h = std::f32::consts::FRAC_1_SQRT_2;
        assert!((c[0] - h).abs() < 1e-6 && (c[1] - h).abs() < 1e-6);
        assert!(mean_normalized::<Vec<f32>>(&[]).is_none());
        assert!(mean_normalized(&[vec![1.0], vec![1.0, 0.0]]).is_none());
    }

    #[test]
    fn median_and_percentile() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), Some(2.5));
        assert_eq!(percentile(&[0.0, 1.0, 2.0, 3.0, 4.0], 0.75), Some(3.0));
        assert_eq!(percentile(&[0.0, 1.0], 0.75), Some(0.75));
        assert_eq!(median(&[]), None);
    }
}
