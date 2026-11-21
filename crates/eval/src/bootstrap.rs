//! Percentile bootstrap confidence intervals with a seeded `SplitMix64`, so every report is reproducible.

use serde::Serialize;

use crate::metrics::{count, mean, nearest_rank};

/// Resamples per interval.
pub const RESAMPLES: usize = 10_000;

/// The seed of every interval.
pub const SEED: u64 = 42;

/// The `SplitMix64` generator (Steele, Lea and Flood, 2014), as in Vigna's reference implementation.
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// A generator seeded with `seed`.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// The next 64 random bits.
    pub const fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n`, by Lemire's multiply-shift; `n` must be at least 1.
    pub fn below(&mut self, n: usize) -> usize {
        let wide = u128::from(self.next_u64()) * n as u128;
        usize::try_from(wide >> 64).expect("the result is below n, which fits in usize")
    }
}

/// A 95% confidence interval.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Interval {
    /// The 2.5th percentile of the resampled means.
    pub low: f64,
    /// The 97.5th percentile of the resampled means.
    pub high: f64,
}

/// The 95% percentile-bootstrap interval of the mean of `values`, or `None` for no values.
#[must_use]
pub fn mean_interval(values: &[f64], resamples: usize, seed: u64) -> Option<Interval> {
    if values.is_empty() || resamples == 0 {
        return None;
    }
    let mut rng = SplitMix64::new(seed);
    let n = values.len();
    let mut means: Vec<f64> = (0..resamples)
        .map(|_| (0..n).map(|_| values[rng.below(n)]).sum::<f64>() / count(n))
        .collect();
    means.sort_unstable_by(f64::total_cmp);
    let at = |p: f64| means[nearest_rank(resamples, p).expect("resamples is at least 1")];
    Some(Interval {
        low: at(2.5),
        high: at(97.5),
    })
}

/// The interval of the mean paired difference `a[i] - b[i]`, or `None` for no pairs.
#[must_use]
pub fn paired_difference(a: &[f64], b: &[f64], resamples: usize, seed: u64) -> Option<Interval> {
    let differences: Vec<f64> = a.iter().zip(b).map(|(x, y)| x - y).collect();
    mean_interval(&differences, resamples, seed)
}

/// A point estimate with its interval.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Estimate {
    /// The mean.
    pub value: f64,
    /// Its 95% interval.
    pub interval: Interval,
}

/// The mean of `values` with its interval from [`RESAMPLES`] resamples seeded with [`SEED`].
#[must_use]
pub fn estimate(values: &[f64]) -> Option<Estimate> {
    Some(Estimate {
        value: mean(values)?,
        interval: mean_interval(values, RESAMPLES, SEED)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix64_matches_the_reference_sequence() {
        let mut rng = SplitMix64::new(1_234_567);
        let first: Vec<u64> = (0..5).map(|_| rng.next_u64()).collect();
        assert_eq!(
            first,
            [
                6_457_827_717_110_365_317,
                3_203_168_211_198_807_973,
                9_817_491_932_198_370_423,
                4_593_380_528_125_082_431,
                16_408_922_859_458_223_821,
            ]
        );
    }

    #[test]
    fn draws_stay_below_the_bound_and_cover_it() {
        let mut rng = SplitMix64::new(SEED);
        let mut seen = [0_usize; 3];
        for _ in 0..3000 {
            seen[rng.below(3)] += 1;
        }
        assert!(seen.iter().all(|n| (900..1100).contains(n)), "{seen:?}");
        assert_eq!(SplitMix64::new(SEED).below(1), 0);
    }

    #[test]
    fn a_seeded_interval_is_reproducible_and_brackets_the_mean() {
        let values: Vec<f64> = (0..40).map(|n| if n % 4 == 0 { 0.0 } else { 1.0 }).collect();
        let interval = mean_interval(&values, RESAMPLES, SEED).unwrap();
        assert_eq!(Some(interval), mean_interval(&values, RESAMPLES, SEED));
        assert_ne!(Some(interval), mean_interval(&values, RESAMPLES, SEED + 1));
        assert!(interval.low < 0.75 && 0.75 < interval.high, "{interval:?}");
        assert!(interval.low > 0.5 && interval.high <= 0.9, "{interval:?}");
        let constant = mean_interval(&[1.0; 10], 100, SEED).unwrap();
        assert_eq!(constant, Interval { low: 1.0, high: 1.0 });
        assert_eq!(mean_interval(&[], RESAMPLES, SEED), None);
        assert_eq!(mean_interval(&[1.0], 0, SEED), None);
    }

    #[test]
    fn paired_differences_subtract_question_by_question() {
        let a = [1.0, 1.0, 0.0, 1.0];
        let b = [1.0, 0.0, 0.0, 1.0];
        let interval = paired_difference(&a, &b, 1000, SEED).unwrap();
        assert!(interval.low >= 0.0 && interval.high <= 1.0, "{interval:?}");
        assert_eq!(
            paired_difference(&a, &a, 1000, SEED),
            Some(Interval { low: 0.0, high: 0.0 })
        );
        assert_eq!(paired_difference(&[], &[], 1000, SEED), None);
    }

    #[test]
    fn an_estimate_pairs_the_mean_with_its_interval() {
        let found = estimate(&[1.0, 0.0]).unwrap();
        assert!((found.value - 0.5).abs() < f64::EPSILON);
        assert!(found.interval.low <= 0.5 && found.interval.high >= 0.5);
        assert_eq!(estimate(&[]), None);
    }
}
