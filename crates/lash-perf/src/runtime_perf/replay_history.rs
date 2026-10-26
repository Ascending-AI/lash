//! Replay receipts enter the duration history: each workload's turn wall
//! time feeds the advisory detector, and its counted per-turn medians and
//! depth slopes ride along in their own units (FIG-5934).
use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::duration_trend::{
    self, BuildMode, DurationHistoryRecord, DurationMetricHistoryValue, HISTORY_RECORD_VERSION,
};
use crate::replay::meter::MetricKind;
use crate::replay::receipt::{ReplayReceipt, WorkloadReceipt};

/// One counted metric of a replay workload: never a duration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReplayCount {
    pub(crate) unit: String,
    pub(crate) per_turn_median: f64,
    pub(crate) robust_slope_per_history_turn: Option<f64>,
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
        p95_ms: summary.round_average_p95,
    })
}

fn record(receipt: &ReplayReceipt, workload: &WorkloadReceipt) -> Option<DurationHistoryRecord> {
    let wall = distribution(workload, "wall_ms")?;
    let identity = workload.experiment.clone();
    Some(DurationHistoryRecord {
        compare_instructions_per_operation: None,
        version: HISTORY_RECORD_VERSION,
        identity,
        scenario: format!("replay/{}/round_average", workload.workload),
        profile: "replay".to_owned(),
        runs: 1,
        warmups: 0,
        turns: workload.turns,
        build_mode: if receipt.identity.build == "debug" {
            BuildMode::Debug
        } else {
            BuildMode::Release
        },
        commit: receipt.identity.commit.clone().unwrap_or_default(),
        run_id: receipt.identity.run_id.clone(),
        recorded_at: receipt.identity.started_at.clone(),
        total_ms: wall.median_ms,
        total_p95_ms: Some(wall.p95_ms),
        duration_metrics_ms: ["host_cpu_ms", "worker_cpu_ms"]
            .into_iter()
            .filter_map(|metric| {
                distribution(workload, metric)
                    .map(|value| (format!("round_average_{metric}"), value))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replay::receipt::{Identity, RECEIPT_FORMAT, RoundRecord, summarize};

    /// FIG-6620: history preserves the producer's immutable settings and
    /// separates SQL shape, traffic and heap windows; concurrent percentiles
    /// describe round averages, never individual session tails.
    #[test]
    fn replay_history_preserves_actual_observer_backend_and_window_identity() {
        let mut producer = Identity::current();
        producer.host = "producer-host".to_owned();
        producer.compiler = "producer-compiler".to_owned();
        producer.commit = Some("producer-commit".to_owned());
        producer.run_id = "producer-run".to_owned();
        let geometry = serde_json::json!({
            "sql_shapes": false, "traffic": false, "heap_window": null, "heap_frames": null,
            "worker": "producer-worker", "window": "round send through outcomes",
            "terminal": "successful scripted outcomes", "sessions": 2, "turns": 2,
        });
        let mut experiment = producer.experiment(geometry.clone());
        experiment.backend = "postgres".to_owned();
        experiment.configured_durability = "actual selected pool settings".to_owned();
        experiment.workload = "replay/control:digest".to_owned();
        let rounds: Vec<_> = (0..2)
            .map(|depth| RoundRecord {
                turn: depth + 1,
                history_depth: depth,
                frame_depth: depth,
                sessions: 2,
                setup: false,
                traffic: None,
                metrics: BTreeMap::from([("wall_ms".to_owned(), 100.0 + depth as f64 * 100.0)]),
            })
            .collect();
        let workload = WorkloadReceipt {
            experiment: experiment.clone(),
            workload: "control".to_owned(),
            script_digest: "digest".to_owned(),
            protocol: "standard".to_owned(),
            backend: "postgres".to_owned(),
            sessions: 2,
            turns: 2,
            compact_every: None,
            units: BTreeMap::new(),
            round_statistic: "round sum".to_owned(),
            summary: summarize(&rounds),
            rounds,
            compactions: Vec::new(),
        };
        let receipt = ReplayReceipt {
            format: RECEIPT_FORMAT.to_owned(),
            identity: producer,
            workloads: vec![workload],
        };
        let recorded = record(&receipt, &receipt.workloads[0]).unwrap();
        assert_eq!(recorded.identity, experiment);
        assert_eq!(recorded.commit, "producer-commit");
        assert_eq!(recorded.run_id, "producer-run");
        let table = crate::replay::receipt::table(&receipt);
        assert!(table.contains("producer-host"));
        assert!(table.contains("actual selected pool settings"));
        assert!(table.contains("p95(round-average)"));
        assert!(table.contains("steady_rounds history_depths frame_depths"));
        assert!(recorded.scenario.ends_with("/round_average"));
        assert_eq!(recorded.total_p95_ms, Some(100.0));
        let mut refusals = Vec::new();
        for (field, value) in [
            ("sql_shapes", serde_json::json!(true)),
            ("traffic", serde_json::json!(true)),
            ("heap_window", serde_json::json!([1, 2])),
            ("heap_frames", serde_json::json!(2)),
            ("worker", serde_json::json!("other-worker")),
            ("window", serde_json::json!("other-window")),
        ] {
            let mut other = geometry.clone();
            other[field] = value;
            let mut candidate = experiment.clone();
            candidate.configured_geometry = other.to_string();
            assert!(experiment.compare(&candidate).is_err());
            refusals.push(field);
        }
        let mut unavailable = experiment.clone();
        unavailable.configured_durability.clear();
        assert!(unavailable.compare(&unavailable).is_err());
        println!(
            "replay-observer-identity.json={}",
            serde_json::json!({"producer": experiment, "preserved": recorded.identity, "mixed_refused": refusals, "missing_policy_refused": true})
        );
    }
}
