//! Retrieval and cost metrics: Recall@k, reciprocal rank, latency percentiles and estimated tokens.

use std::time::Duration;

/// 1 when the first hit is within the first `k` results, else 0.
#[must_use]
pub fn hit_within(first_hit: Option<usize>, k: usize) -> f64 {
    match first_hit {
        Some(rank) if rank <= k => 1.0,
        _ => 0.0,
    }
}

/// 1/rank of the first hit when it is within the first `k` results, else 0.
#[must_use]
pub fn reciprocal_rank(first_hit: Option<usize>, k: usize) -> f64 {
    match first_hit {
        Some(rank) if rank <= k => 1.0 / f64::from(u32::try_from(rank).unwrap_or(u32::MAX)),
        _ => 0.0,
    }
}

/// The arithmetic mean, or `None` for no values.
#[must_use]
pub fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    Some(values.iter().sum::<f64>() / count(values.len()))
}

/// `n` as a float; counts here stay far below 2^52.
#[must_use]
pub fn count(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

/// The nearest-rank percentile `p` (0 to 100) of `values`, or `None` for no values.
#[must_use]
pub fn percentile(values: &[Duration], p: f64) -> Option<Duration> {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let index = nearest_rank(sorted.len(), p)?;
    Some(sorted[index])
}

/// The index of the nearest-rank percentile `p` in `len` sorted values.
#[must_use]
pub fn nearest_rank(len: usize, p: f64) -> Option<usize> {
    let last = len.checked_sub(1)?;
    let rank = (p / 100.0 * count(len)).ceil();
    // The rank lies in 0..=len for p in 0..=100, so the conversion is exact.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rank = rank.max(1.0) as usize;
    Some((rank - 1).min(last))
}

/// The median of `values` (the lower one for an even count), or `None` for no values.
#[must_use]
pub fn median(values: &[usize]) -> Option<usize> {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let index = nearest_rank(sorted.len(), 50.0)?;
    Some(sorted[index])
}

/// The estimated tokens of a text of `chars` characters: `ceil(chars / 4)`, the same estimate for every system.
#[must_use]
pub const fn estimate_tokens(chars: usize) -> usize {
    chars.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_counts_hits_within_k() {
        assert!((hit_within(Some(5), 5) - 1.0).abs() < f64::EPSILON);
        assert!(hit_within(Some(6), 5).abs() < f64::EPSILON);
        assert!(hit_within(None, 5).abs() < f64::EPSILON);
    }

    #[test]
    fn reciprocal_rank_is_zero_past_k() {
        assert!((reciprocal_rank(Some(1), 10) - 1.0).abs() < f64::EPSILON);
        assert!((reciprocal_rank(Some(4), 10) - 0.25).abs() < f64::EPSILON);
        assert!(reciprocal_rank(Some(11), 10).abs() < f64::EPSILON);
        assert!(reciprocal_rank(None, 10).abs() < f64::EPSILON);
    }

    #[test]
    fn synthetic_rankings_give_the_expected_recall_and_mrr() {
        let ranks = [Some(1), Some(3), Some(7), None];
        let recall: Vec<f64> = ranks.iter().map(|rank| hit_within(*rank, 5)).collect();
        let mrr: Vec<f64> = ranks.iter().map(|rank| reciprocal_rank(*rank, 10)).collect();
        assert!((mean(&recall).unwrap() - 0.5).abs() < 1e-12);
        let expected = (1.0 + 1.0 / 3.0 + 1.0 / 7.0) / 4.0;
        assert!((mean(&mrr).unwrap() - expected).abs() < 1e-12);
        assert_eq!(mean(&[]), None);
        assert!((count(3) - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn percentiles_use_the_nearest_rank() {
        let values: Vec<Duration> = [5, 1, 4, 2, 3, 10, 9, 8, 7, 6]
            .into_iter()
            .map(Duration::from_millis)
            .collect();
        assert_eq!(percentile(&values, 50.0), Some(Duration::from_millis(5)));
        assert_eq!(percentile(&values, 95.0), Some(Duration::from_millis(10)));
        assert_eq!(percentile(&values, 0.0), Some(Duration::from_millis(1)));
        assert_eq!(percentile(&[], 50.0), None);
        assert_eq!(nearest_rank(1, 95.0), Some(0));
        assert_eq!(nearest_rank(20, 95.0), Some(18));
        assert_eq!(nearest_rank(20, 100.0), Some(19));
    }

    #[test]
    fn the_median_is_the_lower_middle_value() {
        assert_eq!(median(&[9, 1, 5]), Some(5));
        assert_eq!(median(&[4, 1, 3, 2]), Some(2));
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn tokens_are_a_quarter_of_the_characters_rounded_up() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(1), 1);
        assert_eq!(estimate_tokens(8), 2);
        assert_eq!(estimate_tokens(9), 3);
    }
}
