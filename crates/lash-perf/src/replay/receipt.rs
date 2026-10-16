//! The replay receipt: per-turn counted metrics, per-workload totals and
//! slopes against history depth, and the identity every comparison checks.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::meter::{METRICS, MetricKind, metric};

/// The receipt format this module writes and the gate reads.
pub const RECEIPT_FORMAT: &str = "lash-perf-replay-receipt/1";

/// One replay run: its identity and every workload it ran.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplayReceipt {
    pub format: String,
    pub identity: Identity,
    pub workloads: Vec<WorkloadReceipt>,
}

/// What a receipt's numbers depend on. Counted metrics compare across hosts;
/// allocation and SQL baselines require one build profile and allocator;
/// timed metrics additionally compare only within one host.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Identity {
    pub commit: Option<String>,
    pub dirty: bool,
    /// `debug` or `release`: allocation counts differ between them.
    pub build: String,
    pub build_profile: String,
    pub allocator: String,
    pub host: String,
    pub cpus: usize,
    pub load1_at_start: Option<f64>,
    pub started_at: String,
}

impl Identity {
    pub fn current() -> Self {
        Self {
            commit: crate::perf_support::git::head_commit(),
            dirty: crate::perf_support::git::git_dirty(),
            build: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .to_owned(),
            build_profile: format!(
                "{};debug_assertions={}",
                env!("LASH_PERF_BUILD_PROFILE"),
                cfg!(debug_assertions)
            ),
            allocator: crate::ALLOCATION_MODE.to_owned(),
            host: std::fs::read_to_string("/proc/sys/kernel/hostname")
                .map(|name| name.trim().to_owned())
                .unwrap_or_default(),
            cpus: std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get),
            load1_at_start: std::fs::read_to_string("/proc/loadavg")
                .ok()
                .and_then(|text| text.split_whitespace().next()?.parse().ok()),
            started_at: chrono::Utc::now().to_rfc3339(),
        }
    }
}

/// One workload's run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkloadReceipt {
    pub workload: String,
    /// SHA-256 of the script: receipts of different scripts never compare.
    pub script_digest: String,
    pub protocol: String,
    pub backend: String,
    pub sessions: usize,
    pub turns: usize,
    pub compact_every: Option<usize>,
    /// Every metric's unit; absent metrics were not countable on this backend.
    pub units: BTreeMap<String, String>,
    /// Statistic of `rounds[*].metrics`: the sum over the round's sessions.
    pub round_statistic: String,
    pub rounds: Vec<RoundRecord>,
    pub compactions: Vec<RoundRecord>,
    pub summary: BTreeMap<String, MetricSummary>,
}

/// One round: every session's turn `turn`, sent at once.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoundRecord {
    pub turn: usize,
    /// Turns each session committed before this one.
    pub history_depth: usize,
    /// Turns since the session's last compaction (its frame's depth).
    pub frame_depth: usize,
    pub sessions: usize,
    /// A setup turn: counted in totals, left out of medians and slopes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub setup: bool,
    pub metrics: BTreeMap<String, f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub traffic: Option<super::traffic::Report>,
}

/// One metric over a workload's rounds, per session turn.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetricSummary {
    pub unit: String,
    pub kind: MetricKind,
    pub total: f64,
    pub per_turn_median: f64,
    pub per_turn_p95: f64,
    pub per_turn_max: f64,
    /// Least-squares slope of the per-turn value against history depth, in
    /// the metric's unit per prior turn.
    pub slope_per_history_turn: f64,
    /// Theil-Sen (median pairwise) slope against history depth: the same
    /// quantity, robust to the odd sweep or retention turn.
    pub robust_slope_per_history_turn: f64,
    /// Least-squares slope against the frame's depth: O(frame) work in a
    /// compacting session. Equal to the history slope without compaction.
    pub slope_per_frame_turn: f64,
    /// Theil-Sen slope against the frame's depth: the depth slope the gate
    /// binds, robust to sweep turns, and bounded work in a compacting
    /// session whose frames reset.
    pub robust_slope_per_frame_turn: f64,
}

/// The per-session-turn values of `name` over the steady (non-setup)
/// `rounds`, with their depths.
fn series(rounds: &[RoundRecord], name: &str) -> Vec<(f64, f64, f64)> {
    rounds
        .iter()
        .filter(|round| !round.setup)
        .filter_map(|round| {
            round.metrics.get(name).map(|value| {
                (
                    round.history_depth as f64,
                    round.frame_depth as f64,
                    value / round.sessions.max(1) as f64,
                )
            })
        })
        .collect()
}

/// Least-squares slope of `y` on `x`; zero for fewer than two distinct `x`.
pub fn least_squares_slope(points: &[(f64, f64)]) -> f64 {
    let n = points.len() as f64;
    if points.len() < 2 {
        return 0.0;
    }
    let mean_x = points.iter().map(|(x, _)| x).sum::<f64>() / n;
    let mean_y = points.iter().map(|(_, y)| y).sum::<f64>() / n;
    let (covariance, variance) = points.iter().fold((0.0, 0.0), |(cov, var), (x, y)| {
        (
            cov + (x - mean_x) * (y - mean_y),
            var + (x - mean_x) * (x - mean_x),
        )
    });
    if variance == 0.0 {
        0.0
    } else {
        covariance / variance
    }
}

/// Theil-Sen slope: the median of every pairwise slope.
pub fn theil_sen_slope(points: &[(f64, f64)]) -> f64 {
    let mut slopes = Vec::with_capacity(points.len() * points.len().saturating_sub(1) / 2);
    for (index, (x1, y1)) in points.iter().enumerate() {
        for (x2, y2) in &points[index + 1..] {
            if x2 != x1 {
                slopes.push((y2 - y1) / (x2 - x1));
            }
        }
    }
    median(&mut slopes)
}

fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        f64::midpoint(values[mid - 1], values[mid])
    } else {
        values[mid]
    }
}

fn round3(value: f64) -> f64 {
    (value * 1_000.0).round() / 1_000.0
}

/// Summarize every metric `rounds` carry: totals over every round, the
/// other statistics over the steady rounds.
pub fn summarize(rounds: &[RoundRecord]) -> BTreeMap<String, MetricSummary> {
    METRICS
        .iter()
        .filter_map(|def| {
            let points = series(rounds, def.name);
            if points.is_empty() {
                return None;
            }
            let mut values: Vec<f64> = points.iter().map(|(_, _, value)| *value).collect();
            let total = rounds
                .iter()
                .filter_map(|round| round.metrics.get(def.name))
                .sum::<f64>();
            let history: Vec<(f64, f64)> = points.iter().map(|(x, _, y)| (*x, *y)).collect();
            let frame: Vec<(f64, f64)> = points.iter().map(|(_, x, y)| (*x, *y)).collect();
            let per_turn_median = median(&mut values);
            let p95 = values[((values.len() - 1) as f64 * 0.95).round() as usize];
            Some((
                def.name.to_owned(),
                MetricSummary {
                    unit: def.unit.to_owned(),
                    kind: def.kind,
                    total: round3(total),
                    per_turn_median: round3(per_turn_median),
                    per_turn_p95: round3(p95),
                    per_turn_max: round3(values.last().copied().unwrap_or_default()),
                    slope_per_history_turn: round3(least_squares_slope(&history)),
                    robust_slope_per_history_turn: round3(theil_sen_slope(&history)),
                    slope_per_frame_turn: round3(least_squares_slope(&frame)),
                    robust_slope_per_frame_turn: round3(theil_sen_slope(&frame)),
                },
            ))
        })
        .collect()
}

/// The units of the metrics `rounds` carry.
pub fn units(rounds: &[RoundRecord]) -> BTreeMap<String, String> {
    rounds
        .iter()
        .flat_map(|round| round.metrics.keys())
        .filter_map(|name| metric(name).map(|def| (name.clone(), def.unit.to_owned())))
        .collect()
}

/// A plain-text table of every workload's per-turn medians and slopes.
pub fn table(receipt: &ReplayReceipt) -> String {
    let mut out = String::new();
    for workload in &receipt.workloads {
        out.push_str(&format!(
            "replay {} ({} on {}, {} session(s) x {} turns{})\n",
            workload.workload,
            workload.protocol,
            workload.backend,
            workload.sessions,
            workload.turns,
            workload
                .compact_every
                .map(|every| format!(", compacting every {every}"))
                .unwrap_or_default(),
        ));
        out.push_str(
            "  metric                  unit         median/turn    max/turn  \
             lsq-slope/history-turn  robust/history-turn  robust/frame-turn\n",
        );
        for (name, summary) in &workload.summary {
            out.push_str(&format!(
                "  {name:<23} {:<12} {:>12} {:>11} {:>22} {:>20} {:>18}\n",
                summary.unit,
                summary.per_turn_median,
                summary.per_turn_max,
                summary.slope_per_history_turn,
                summary.robust_slope_per_history_turn,
                summary.robust_slope_per_frame_turn,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rounds(value: impl Fn(usize) -> f64) -> Vec<RoundRecord> {
        (0..200)
            .map(|depth| RoundRecord {
                turn: depth + 1,
                history_depth: depth,
                frame_depth: depth % 50,
                sessions: 2,
                setup: depth == 0,
                traffic: None,
                metrics: BTreeMap::from([("store_bytes_written".to_owned(), 2.0 * value(depth))]),
            })
            .collect()
    }

    /// REPLAY-SLOPE: a per-session-turn cost growing by `k` per prior turn
    /// has slope `k` against history depth, both least-squares and robust; a
    /// cost that restarts with every frame has its slope against frame depth;
    /// and one sweep turn moves the least-squares slope but not the robust
    /// ones the gate binds. A setup turn, however far off, moves neither.
    #[test]
    fn slopes_measure_history_and_frame_growth_and_ignore_a_sweep_turn() {
        let history =
            &summarize(&rounds(|depth| 100.0 + 7.0 * depth as f64))["store_bytes_written"];
        assert_eq!(history.slope_per_history_turn, 7.0);
        assert_eq!(history.robust_slope_per_history_turn, 7.0);
        assert_eq!(history.per_turn_median, 100.0 + 7.0 * 100.0);
        let frame = &summarize(&rounds(|depth| 3.0 * (depth % 50) as f64))["store_bytes_written"];
        assert_eq!(frame.robust_slope_per_frame_turn, 3.0);
        assert_eq!(frame.slope_per_frame_turn, 3.0);
        let swept = &summarize(&rounds(|depth| match depth {
            0 => -1_000_000.0,
            150 => 1_000_100.0,
            _ => 100.0,
        }))["store_bytes_written"];
        assert_eq!(swept.per_turn_median, 100.0);
        assert_ne!(swept.slope_per_history_turn, 0.0);
        assert_eq!(swept.robust_slope_per_history_turn, 0.0);
        assert_eq!(swept.robust_slope_per_frame_turn, 0.0);
    }
}
