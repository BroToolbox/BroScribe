//! Time-range arithmetic on `(start, end)` pairs in seconds.

/// A half-open time range in seconds.
pub type Range = (f64, f64);

/// Sorted, merged ranges with positive length.
pub fn union(ranges: impl IntoIterator<Item = Range>) -> Vec<Range> {
    let mut v: Vec<Range> = ranges
        .into_iter()
        .filter(|(a, b)| b > a && a.is_finite() && b.is_finite())
        .collect();
    v.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.total_cmp(&y.1)));
    let mut out: Vec<Range> = Vec::with_capacity(v.len());
    for (a, b) in v {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// Total length of the union of `ranges`.
pub fn total(ranges: &[Range]) -> f64 {
    union(ranges.iter().copied())
        .iter()
        .map(|(a, b)| b - a)
        .sum()
}

/// `a ∩ b` as merged ranges.
pub fn intersect(a: &[Range], b: &[Range]) -> Vec<Range> {
    let (a, b) = (union(a.iter().copied()), union(b.iter().copied()));
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let lo = a[i].0.max(b[j].0);
        let hi = a[i].1.min(b[j].1);
        if hi > lo {
            out.push((lo, hi));
        }
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

/// `a − b` as merged ranges, in one pass over both: the merged ranges of
/// each side are sorted and disjoint, so a piece of `b` that ends before one
/// range of `a` starts ends before every later one too.
pub fn subtract(a: &[Range], b: &[Range]) -> Vec<Range> {
    let b = union(b.iter().copied());
    let mut out = Vec::new();
    let mut first = 0;
    for (start, end) in union(a.iter().copied()) {
        while first < b.len() && b[first].1 <= start {
            first += 1;
        }
        let mut cur = start;
        for &(bs, be) in &b[first..] {
            if bs >= end {
                break;
            }
            if be <= cur {
                continue;
            }
            if bs > cur {
                out.push((cur, bs));
            }
            cur = cur.max(be);
            if cur >= end {
                break;
            }
        }
        if cur < end {
            out.push((cur, end));
        }
    }
    out
}

/// Length of `a ∩ b`.
pub fn overlap(a: &[Range], b: &[Range]) -> f64 {
    intersect(a, b).iter().map(|(x, y)| y - x).sum()
}

/// Overlap of two single ranges.
pub fn overlap1(a: Range, b: Range) -> f64 {
    (a.1.min(b.1) - a.0.max(b.0)).max(0.0)
}

/// Keeps ranges of at least `min_len`, then the `cap` longest (ties: earlier),
/// returned in time order.
pub fn longest(ranges: Vec<Range>, min_len: f64, cap: usize) -> Vec<Range> {
    let mut v: Vec<Range> = ranges
        .into_iter()
        .filter(|(a, b)| b - a >= min_len)
        .collect();
    v.sort_by(|x, y| {
        (y.1 - y.0)
            .total_cmp(&(x.1 - x.0))
            .then(x.0.total_cmp(&y.0))
    });
    v.truncate(cap);
    v.sort_by(|x, y| x.0.total_cmp(&y.0));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_operations() {
        assert_eq!(
            union([(3.0, 4.0), (0.0, 1.0), (0.5, 2.0)]),
            vec![(0.0, 2.0), (3.0, 4.0)]
        );
        assert_eq!(
            subtract(&[(0.0, 10.0)], &[(2.0, 3.0), (5.0, 6.0)]),
            vec![(0.0, 2.0), (3.0, 5.0), (6.0, 10.0)]
        );
        assert_eq!(total(&[(0.0, 2.0), (1.0, 3.0)]), 3.0);
        assert_eq!(
            intersect(&[(0.0, 5.0), (6.0, 9.0)], &[(4.0, 7.0)]),
            vec![(4.0, 5.0), (6.0, 7.0)]
        );
        assert_eq!(overlap(&[(0.0, 5.0)], &[(4.0, 7.0)]), 1.0);
        assert!(subtract(&[(0.0, 1.0)], &[(0.0, 2.0)]).is_empty());
        assert_eq!(
            longest(vec![(0.0, 1.0), (2.0, 5.0), (6.0, 8.0)], 1.5, 1),
            vec![(2.0, 5.0)]
        );
    }

    /// The quadratic definition `subtract` replaced.
    fn subtract_reference(a: &[Range], b: &[Range]) -> Vec<Range> {
        let b = union(b.iter().copied());
        let mut out = Vec::new();
        for (start, end) in union(a.iter().copied()) {
            let mut cur = start;
            for &(bs, be) in &b {
                if be <= cur || bs >= end {
                    continue;
                }
                if bs > cur {
                    out.push((cur, bs));
                }
                cur = cur.max(be);
                if cur >= end {
                    break;
                }
            }
            if cur < end {
                out.push((cur, end));
            }
        }
        out
    }

    #[test]
    fn linear_subtract_matches_the_reference_on_synthetic_turns() {
        // Deterministic LCG: overlapping, nested, touching and spanning
        // ranges on a 0-60 s timeline, including non-finite junk.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = |m: f64| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 11) as f64 / (1u64 << 53) as f64 * m
        };
        for round in 0..200 {
            let mut set = |n: usize| -> Vec<Range> {
                (0..n)
                    .map(|_| {
                        let a = (next(60.0) * 4.0).round() / 4.0;
                        (a, a + (next(8.0) * 4.0).round() / 4.0)
                    })
                    .collect()
            };
            let mut a = set(1 + round % 9);
            let b = set(round % 13);
            if round % 17 == 0 {
                a.push((f64::NAN, 3.0));
                a.push((1.0, f64::INFINITY));
            }
            assert_eq!(
                subtract(&a, &b),
                subtract_reference(&a, &b),
                "{a:?} − {b:?}"
            );
        }
        assert_eq!(
            subtract(
                &[(0.0, 2.0), (3.0, 9.0)],
                &[(1.0, 4.0), (5.0, 6.0), (8.0, 12.0)]
            ),
            vec![(0.0, 1.0), (4.0, 5.0), (6.0, 8.0)]
        );
    }
}
