use serde::Serialize;

use super::time::round3;

#[derive(Debug, Clone, Serialize)]
pub struct BasicMetricSummary {
    pub min: f64,
    pub median: f64,
    pub max: f64,
    pub mean: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PercentileMetricSummary {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
    pub mean: f64,
}

pub fn basic_summary(mut values: Vec<f64>) -> BasicMetricSummary {
    values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let min = *values.first().unwrap_or(&0.0);
    let max = *values.last().unwrap_or(&0.0);
    let median = if values.is_empty() {
        0.0
    } else if values.len().is_multiple_of(2) {
        (values[values.len() / 2 - 1] + values[values.len() / 2]) / 2.0
    } else {
        values[values.len() / 2]
    };
    let mean = if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    };
    let p50 = percentile_sorted(&values, 0.50);
    let p95 = percentile_sorted(&values, 0.95);
    let p99 = percentile_sorted(&values, 0.99);
    BasicMetricSummary {
        min: round3(min),
        median: round3(median),
        max: round3(max),
        mean: round3(mean),
        p50: round3(p50),
        p95: round3(p95),
        p99: round3(p99),
    }
}

pub fn optional_basic_summary(values: Vec<f64>) -> Option<BasicMetricSummary> {
    if values.is_empty() {
        None
    } else {
        Some(basic_summary(values))
    }
}

pub fn percentile_summary(mut values: Vec<f64>) -> PercentileMetricSummary {
    values.sort_by(f64::total_cmp);
    PercentileMetricSummary {
        p50: round3(percentile_sorted(&values, 0.50)),
        p95: round3(percentile_sorted(&values, 0.95)),
        p99: round3(percentile_sorted(&values, 0.99)),
        max: round3(*values.last().unwrap_or(&0.0)),
        mean: round3(values.iter().sum::<f64>() / values.len().max(1) as f64),
    }
}

/// Returns a linearly interpolated percentile from sorted samples.
///
/// The percentile rank is the zero-based index `p * (n - 1)`. A fractional
/// rank is interpolated between its two neighboring samples. Empty input
/// returns zero, one sample returns that sample, and two samples interpolate
/// directly between their endpoints. Runtime report p50/p95/p99 fields and
/// `scripts/runtime_perf_percentiles.py` use this same definition.
pub fn percentile_sorted(values: &[f64], percentile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    if values.len() == 1 {
        return values[0];
    }
    let rank = percentile.clamp(0.0, 1.0) * (values.len() - 1) as f64;
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    if lower == upper {
        values[lower]
    } else {
        let weight = rank - lower as f64;
        values[lower] * (1.0 - weight) + values[upper] * weight
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_uses_half_away_from_zero() {
        assert_eq!(round3(1.5025), 1.503);
    }

    #[test]
    fn percentiles_use_interpolation_for_odd_samples() {
        let summary = basic_summary(vec![3.0, 1.0, 2.0]);

        assert_eq!(summary.median, 2.0);
        assert_eq!(summary.p50, 2.0);
        assert_eq!(summary.p95, 2.9);
        assert_eq!(summary.p99, 2.98);
    }

    #[test]
    fn percentiles_use_interpolation_for_even_samples() {
        let summary = basic_summary(vec![4.0, 1.0, 3.0, 2.0]);

        assert_eq!(summary.median, 2.5);
        assert_eq!(summary.p50, 2.5);
        assert_eq!(summary.p95, 3.85);
        assert_eq!(summary.p99, 3.97);
    }

    #[test]
    fn percentiles_handle_empty_and_single_sample_inputs() {
        let empty = basic_summary(Vec::new());
        assert_eq!(empty.p50, 0.0);
        assert_eq!(empty.p95, 0.0);
        assert_eq!(empty.p99, 0.0);

        let single = basic_summary(vec![7.25]);
        assert_eq!(single.p50, 7.25);
        assert_eq!(single.p95, 7.25);
        assert_eq!(single.p99, 7.25);
    }

    #[test]
    fn percentiles_interpolate_two_sample_inputs() {
        let summary = basic_summary(vec![10.0, 20.0]);

        assert_eq!(summary.median, 15.0);
        assert_eq!(summary.p50, 15.0);
        assert_eq!(summary.p95, 19.5);
        assert_eq!(summary.p99, 19.9);
    }
}
