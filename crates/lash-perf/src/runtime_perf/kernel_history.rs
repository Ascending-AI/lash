//! Kernel receipts enter the existing advisory detector; counts retain their units.
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use super::duration_trend::{
    self, BuildMode, DurationHistoryRecord, DurationMetricHistoryValue, ExperimentIdentity,
    HISTORY_RECORD_VERSION,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct KernelSample {
    pub(crate) round: usize,
    pub(crate) iteration: usize,
    pub(crate) ns_start_and_run: u64,
    pub(crate) ns_run: u64,
    pub(crate) allocations_start_and_run: u64,
    pub(crate) allocations_run: u64,
    pub(crate) reallocations_start_and_run: u64,
    pub(crate) reallocations_run: u64,
    pub(crate) charged: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct KernelObservation {
    pub(crate) program: String,
    pub(crate) artifact_sha256: String,
    pub(crate) workload_sha256: String,
    pub(crate) compiler: String,
    pub(crate) build_profile: String,
    pub(crate) debug_assertions: bool,
    pub(crate) allocator: String,
    pub(crate) split: bool,
    pub(crate) fixed_iterations: bool,
    pub(crate) cap: usize,
    pub(crate) iterations: usize,
    pub(crate) warmups: usize,
    pub(crate) rounds: usize,
    pub(crate) samples: Vec<KernelSample>,
}

#[derive(Debug, Deserialize)]
struct KernelReceipt {
    host: String,
    commit: String,
    run_id: String,
    recorded_at: String,
    rows: Vec<KernelObservation>,
}

fn records(receipt: KernelReceipt) -> anyhow::Result<Vec<DurationHistoryRecord>> {
    if receipt.rows.is_empty() || receipt.host.trim().is_empty() {
        bail!("kernel receipt requires host identity and nonempty rows");
    }
    chrono::DateTime::parse_from_rfc3339(&receipt.recorded_at)?;
    let mut selected = std::collections::BTreeSet::new();
    receipt.rows.into_iter().map(|row| {
        if !selected.insert(row.program.clone()) {
            bail!("duplicate kernel row: {}", row.program);
        }
        if row.cap == 0 || row.rounds == 0 || !row.fixed_iterations || row.split
            || row.iterations != row.cap || row.samples.len() != row.cap.checked_mul(row.rounds).context("kernel sample count overflow")?
            || row.warmups != row.cap.min(20)
        {
            bail!("{}: incomplete or observed kernel population", row.program);
        }
        for hash in [&row.artifact_sha256, &row.workload_sha256] {
            if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!("{}: missing artifact/workload SHA256", row.program);
            }
        }
        let first = &row.samples[0];
        for (index, sample) in row.samples.iter().enumerate() {
            if sample.round != index / row.cap + 1 || sample.iteration != index % row.cap
                || sample.ns_run == 0 || sample.ns_start_and_run < sample.ns_run
                || count_values(sample) != count_values(first)
            {
                bail!("{}: invalid sample coordinates, timing or nondeterministic counts", row.program);
            }
        }
        let identity = ExperimentIdentity {
            host: receipt.host.clone(),
            compiler: row.compiler.clone(),
            allocator: row.allocator.clone(),
            build_profile: format!("{};debug_assertions={}", row.build_profile, row.debug_assertions),
            backend: "kernel/in-process/scripted-host".to_string(),
            configured_durability: "not applicable (no store)".to_string(),
            workload: format!("{}:{}", row.program, row.workload_sha256),
            configured_geometry: serde_json::json!({
                "cap": row.cap, "warmups": row.warmups, "rounds": row.rounds,
                "split": row.split, "fixed_iterations": row.fixed_iterations,
                "window": "start+run or run; excludes destruction and receipt construction",
                "bounds": "charge=1000000000;memory=1073741824;call_depth=200;live_tasks=100;requests_per_park=100;join_members=100",
            }).to_string(),
            postgres_settings: None,
        };
        identity.compare(&identity).map_err(|reason| anyhow::anyhow!("kernel identity refused: {reason:?}"))?;
        let run = distribution(row.samples.iter().map(|sample| sample.ns_run));
        let whole = distribution(row.samples.iter().map(|sample| sample.ns_start_and_run));
        Ok(DurationHistoryRecord {
            version: HISTORY_RECORD_VERSION,
            identity,
            scenario: format!("kernel/{}", row.program),
            profile: "kernel-gate6".to_string(),
            runs: row.rounds,
            warmups: row.warmups,
            turns: row.cap,
            build_mode: if row.debug_assertions { BuildMode::Debug } else { BuildMode::Release },
            commit: receipt.commit.clone(),
            run_id: receipt.run_id.clone(),
            recorded_at: receipt.recorded_at.clone(),
            total_ms: whole.median_ms,
            total_p95_ms: Some(whole.p95_ms),
            duration_metrics_ms: BTreeMap::from([("kernel_run_ms".to_string(), run)]),
            kernel: Some(row),
            replay: None,
        })
    }).collect()
}

fn distribution(values: impl Iterator<Item = u64>) -> DurationMetricHistoryValue {
    let mut values: Vec<_> = values.map(|value| value as f64 / 1e6).collect();
    values.sort_by(f64::total_cmp);
    let percentile = |p: f64| {
        let index = p * (values.len() - 1) as f64;
        let low = index.floor() as usize;
        let high = index.ceil() as usize;
        values[low] + (values[high] - values[low]) * (index - low as f64)
    };
    DurationMetricHistoryValue {
        median_ms: percentile(0.5),
        p95_ms: percentile(0.95),
    }
}

fn count_values(sample: &KernelSample) -> [u64; 5] {
    [
        sample.allocations_run,
        sample.allocations_start_and_run,
        sample.reallocations_run,
        sample.reallocations_start_and_run,
        sample.charged,
    ]
}

pub(crate) fn render_counts(history: &[DurationHistoryRecord]) -> String {
    let mut previous = BTreeMap::new();
    let mut out = String::from(
        "kernel counts (diagnostic; delta against preceding comparable receipt; no wall-time gate)\nprogram\tartifact\tallocations_run\tallocations_start_and_run\treallocations_run\treallocations_start_and_run\tcharged\tallocations_run_total\tdeltas\n",
    );
    for record in history {
        let Some(row) = &record.kernel else { continue };
        let Some(sample) = row.samples.first() else {
            continue;
        };
        let values = count_values(sample);
        let counts = [
            values[0],
            values[1],
            values[2],
            values[3],
            values[4],
            row.samples
                .iter()
                .map(|sample| sample.allocations_run)
                .sum(),
        ];
        let deltas = previous
            .insert(&record.identity, counts)
            .map(|old| {
                counts
                    .into_iter()
                    .zip(old)
                    .map(|(new, old)| (i128::from(new) - i128::from(old)).to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_else(|| "no comparable predecessor".to_string());
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            row.program,
            row.artifact_sha256,
            counts[0],
            counts[1],
            counts[2],
            counts[3],
            counts[4],
            counts[5],
            deltas
        ));
    }
    out
}

/// Import one maintained gate-6 invocation, one observation per program.
pub fn record_kernel_receipt(receipt: &Path, history: &Path) -> anyhow::Result<()> {
    let receipt: KernelReceipt = serde_json::from_str(&std::fs::read_to_string(receipt)?)?;
    duration_trend::append_records(history, &records(receipt)?)?;
    let loaded = duration_trend::load_history(history)?;
    let retained = duration_trend::retained_records(&loaded);
    duration_trend::compact_history(history, &retained, &[])?;
    duration_trend::run_duration_trend_cli(history, Some("kernel-gate6"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sustained_kernel_regression_reaches_existing_reader_and_count_delta() {
        let raw = include_str!("../../fixtures/kernel-baseline.json");
        let mut history = Vec::new();
        for index in 0..10 {
            let mut receipt: KernelReceipt = serde_json::from_str(raw).unwrap();
            receipt.recorded_at = format!("2026-10-10T10:{index:02}:00Z");
            if index >= 5 {
                for row in &mut receipt.rows {
                    for sample in &mut row.samples {
                        sample.ns_run *= 2;
                        sample.ns_start_and_run *= 2;
                        sample.allocations_run += 4081;
                        sample.allocations_start_and_run += 4081;
                    }
                }
            }
            history.extend(records(receipt).unwrap());
        }
        let rows = duration_trend::trend_rows(&history, Some("kernel-gate6"));
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|row| row.verdict.is_drifting()));
        assert!(duration_trend::render_trend_table(&rows).contains("DRIFTING"));
        assert!(render_counts(&history).contains("4081,4081,0,0,0"));
        assert!(
            render_counts(&duration_trend::retained_records(&history)).contains("4081,4081,0,0,0")
        );
    }
}
