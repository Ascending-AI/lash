//! Replay receipts enter the duration history: each workload's turn wall
//! time feeds the advisory detector, and its counted per-turn medians and
//! depth slopes ride along in their own units (FIG-5934).
use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::duration_trend::{
    self, BuildMode, DurationHistoryRecord, DurationMetricHistoryValue, ExperimentIdentity,
    HISTORY_RECORD_VERSION,
};
use crate::replay::meter::MetricKind;
use crate::replay::receipt::{ReplayReceipt, WorkloadReceipt};

/// One counted metric of a replay workload: never a duration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReplayCount {
    pub(crate) unit: String,
    pub(crate) per_turn_median: f64,
    pub(crate) robust_slope_per_history_turn: f64,
}

/// A replay workload's counted results, keyed by metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReplayCounts {
    pub(crate) script_digest: String,
    pub(crate) counts: BTreeMap<String, ReplayCount>,
}

fn distribution(workload: &WorkloadReceipt, metric: &str) -> Option<DurationMetricHistoryValue> {
    let summary = workload.summary.get(metric)?;
    Some(DurationMetricHistoryValue {
        median_ms: summary.per_turn_median,
        p95_ms: summary.per_turn_p95,
    })
}

fn record(receipt: &ReplayReceipt, workload: &WorkloadReceipt) -> Option<DurationHistoryRecord> {
    let wall = distribution(workload, "wall_ms")?;
    let mut identity = ExperimentIdentity::capture(serde_json::json!({
        "turns": workload.turns,
        "sessions": workload.sessions,
        "compact_every": workload.compact_every,
    }));
    identity.backend = workload.backend.clone();
    identity.configured_durability = "SQLite synchronous=NORMAL; PostgreSQL fixture".to_owned();
    identity.workload = format!("replay/{}:{}", workload.workload, workload.script_digest);
    Some(DurationHistoryRecord {
        version: HISTORY_RECORD_VERSION,
        identity,
        scenario: format!("replay/{}", workload.workload),
        profile: "replay".to_owned(),
        runs: 1,
        warmups: 0,
        turns: workload.turns,
        build_mode: if receipt.identity.build == "debug" {
            BuildMode::Debug
        } else {
            BuildMode::Release
        },
        commit: duration_trend::history_commit(),
        run_id: duration_trend::history_run_id(),
        recorded_at: receipt.identity.started_at.clone(),
        total_ms: wall.median_ms,
        total_p95_ms: Some(wall.p95_ms),
        duration_metrics_ms: ["host_cpu_ms", "worker_cpu_ms"]
            .into_iter()
            .filter_map(|metric| {
                distribution(workload, metric).map(|value| (format!("turn_{metric}"), value))
            })
            .collect(),
        kernel: None,
        replay: Some(ReplayCounts {
            script_digest: workload.script_digest.clone(),
            counts: workload
                .summary
                .iter()
                .filter(|(_, summary)| summary.kind == MetricKind::Counted)
                .map(|(name, summary)| {
                    (
                        name.clone(),
                        ReplayCount {
                            unit: summary.unit.clone(),
                            per_turn_median: summary.per_turn_median,
                            robust_slope_per_history_turn: summary.robust_slope_per_history_turn,
                        },
                    )
                })
                .collect(),
        }),
    })
}

/// Append one observation per workload of `receipt` to `history`, compact it
/// and print the replay trend.
pub fn record_replay_receipt(receipt: &ReplayReceipt, history: &Path) -> anyhow::Result<()> {
    let records: Vec<_> = receipt
        .workloads
        .iter()
        .filter_map(|workload| record(receipt, workload))
        .collect();
    duration_trend::append_records(history, &records)?;
    let loaded = duration_trend::load_history(history)?;
    let retained = duration_trend::retained_records(&loaded);
    duration_trend::compact_history(history, &retained, &[])?;
    duration_trend::run_duration_trend_cli(history, Some("replay"))
}
