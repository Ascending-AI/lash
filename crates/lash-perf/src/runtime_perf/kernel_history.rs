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
    pub(crate) result_sha256: String,
    pub(crate) observe: String,
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

fn validate(row: &KernelObservation) -> anyhow::Result<()> {
    if row.cap == 0
        || row.rounds == 0
        || !row.fixed_iterations
        || row.split
        || row.observe != "nothing"
        || row.iterations != row.cap
        || row.samples.len()
            != row
                .cap
                .checked_mul(row.rounds)
                .context("kernel sample count overflow")?
        || row.warmups != row.cap.min(20)
    {
        bail!("{}: incomplete or observed kernel population", row.program);
    }
    for (field, hash) in [
        ("artifact_sha256", &row.artifact_sha256),
        ("workload_sha256", &row.workload_sha256),
        ("result_sha256", &row.result_sha256),
    ] {
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            bail!("{}: missing or invalid {field}", row.program);
        }
    }
    let first = &row.samples[0];
    for (index, sample) in row.samples.iter().enumerate() {
        if sample.round != index / row.cap + 1
            || sample.iteration != index % row.cap
            || sample.ns_run == 0
            || sample.ns_start_and_run < sample.ns_run
            || count_values(sample) != count_values(first)
        {
            bail!(
                "{}: invalid sample coordinates, timing or nondeterministic counts",
                row.program
            );
        }
    }
    Ok(())
}

/// Refuse invalid cached populations and changed results before any comparison
/// or history mutation. Result changes must not create a new timing cohort.
pub(crate) fn validate_history(history: &[DurationHistoryRecord]) -> anyhow::Result<()> {
    let mut results = BTreeMap::new();
    for record in history {
        let Some(row) = &record.kernel else { continue };
        validate(row)?;
        let key = (&record.identity, &record.profile, &record.scenario);
        if let Some(previous) = results.insert(key, &row.result_sha256)
            && previous != &row.result_sha256
        {
            bail!("{}: result_sha256 identity mismatch", row.program);
        }
    }
    Ok(())
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
        validate(&row)?;
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
                "observe": row.observe,
                "window": "start+run or run; excludes destruction and receipt construction",
                "bounds": format!("charge=1000000000;memory=1073741824;call_depth={};live_tasks=100;requests_per_park=100;join_members=100", lash_kernel_vm::Bounds::DEFAULT_CALL_DEPTH),
            }).to_string(),
            postgres_settings: None,
        };
        identity.compare(&identity).map_err(|reason| anyhow::anyhow!("kernel identity refused: {reason:?}"))?;
        let run = distribution(row.samples.iter().map(|sample| sample.ns_run));
        let whole = distribution(row.samples.iter().map(|sample| sample.ns_start_and_run));
        Ok(DurationHistoryRecord {
            compare_instructions_per_operation: None,
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
    let incoming = records(receipt)?;
    let mut loaded = duration_trend::load_history(history)?;
    loaded.extend(incoming.iter().cloned());
    validate_history(&loaded)?;
    loaded.sort_by(|left, right| left.recorded_at.cmp(&right.recorded_at));
    duration_trend::append_records(history, &incoming)?;
    let retained = duration_trend::retained_records(&loaded);
    duration_trend::compact_history(history, &retained, &[])?;
    duration_trend::run_duration_trend_cli(history, Some("kernel-gate6"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt_json() -> serde_json::Value {
        serde_json::json!({
            "host": "test-host", "commit": "test-commit", "run_id": "test-run",
            "recorded_at": "2026-10-13T00:00:00Z",
            "rows": [{
                "program": "baseline", "artifact_sha256": "a".repeat(64),
                "workload_sha256": "b".repeat(64), "result_sha256": "c".repeat(64),
                "observe": "nothing", "compiler": "test-compiler",
                "build_profile": "test-profile", "debug_assertions": false,
                "allocator": "system+stats_alloc", "split": false,
                "fixed_iterations": true, "cap": 1, "iterations": 1,
                "warmups": 1, "rounds": 1,
                "samples": [{
                    "round": 1, "iteration": 0, "ns_run": 10_000_000,
                    "ns_start_and_run": 20_000_000, "allocations_run": 1,
                    "allocations_start_and_run": 2, "reallocations_run": 0,
                    "reallocations_start_and_run": 0, "charged": 3
                }]
            }]
        })
    }

    #[test]
    fn kernel_history_refuses_mismatched_results_before_append() {
        let dir = tempfile::tempdir().unwrap();
        let history = dir.path().join("history.jsonl");
        let mut raw = receipt_json();
        let initial = records(serde_json::from_value(raw.clone()).unwrap()).unwrap();
        duration_trend::append_records(&history, &initial).unwrap();
        let before = std::fs::read(&history).unwrap();
        raw["rows"][0]["result_sha256"] = "d".repeat(64).into();
        let receipt = dir.path().join("receipt.json");
        std::fs::write(&receipt, serde_json::to_vec(&raw).unwrap()).unwrap();
        let error = record_kernel_receipt(&receipt, &history).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("result_sha256 identity mismatch")
        );
        assert_eq!(std::fs::read(&history).unwrap(), before);
    }

    #[test]
    fn kernel_receipts_refuse_missing_or_observed_identity() {
        for field in ["observe", "result_sha256"] {
            let mut raw = receipt_json();
            raw["rows"][0].as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<KernelReceipt>(raw).is_err());
        }
        for observe in ["work", "context", "invalid"] {
            let mut raw = receipt_json();
            raw["rows"][0]["observe"] = observe.into();
            assert!(records(serde_json::from_value(raw).unwrap()).is_err());
        }
        let mut raw = receipt_json();
        raw["rows"][0]["result_sha256"] = "not-a-digest".into();
        assert!(records(serde_json::from_value(raw).unwrap()).is_err());
    }

    #[test]
    fn kernel_receipts_and_saved_history_refuse_zero_samples() {
        let raw = receipt_json();
        let mut saved = records(serde_json::from_value(raw.clone()).unwrap()).unwrap();
        let mut empty = raw;
        empty["rows"][0]["samples"] = serde_json::json!([]);
        assert!(records(serde_json::from_value(empty).unwrap()).is_err());
        saved[0].kernel.as_mut().unwrap().samples.clear();
        let dir = tempfile::tempdir().unwrap();
        let history = dir.path().join("history.jsonl");
        duration_trend::append_records(&history, &saved).unwrap();
        assert!(duration_trend::load_history(&history).is_err());
    }

    #[test]
    fn cached_kernel_history_cannot_drop_missing_identity_or_compare_changed_results() {
        let dir = tempfile::tempdir().unwrap();
        let history = dir.path().join("history.jsonl");
        let records = records(serde_json::from_value(receipt_json()).unwrap()).unwrap();
        let valid = serde_json::to_value(&records[0]).unwrap();
        for field in ["observe", "result_sha256"] {
            let mut old = valid.clone();
            old["kernel"].as_object_mut().unwrap().remove(field);
            std::fs::write(&history, serde_json::to_vec(&old).unwrap()).unwrap();
            assert!(duration_trend::load_history_lenient(&history).is_err());
        }
        let mut changed = valid.clone();
        changed["kernel"]["result_sha256"] = "d".repeat(64).into();
        std::fs::write(&history, format!("{valid}\n{changed}\n")).unwrap();
        let error = duration_trend::load_history(&history).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("result_sha256 identity mismatch")
        );
    }

    #[test]
    fn sustained_kernel_regression_reaches_existing_reader_and_count_delta() {
        let mut history = Vec::new();
        for index in 0..10 {
            let mut receipt: KernelReceipt = serde_json::from_value(receipt_json()).unwrap();
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
