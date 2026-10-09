//! Offline operation tails from the raw ledgers retained by performance receipts.
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::perf_support::metrics::percentile_sorted;

#[derive(Default)]
struct Population {
    values: Vec<(String, f64)>,
    missing: usize,
}

type Populations = BTreeMap<(String, String), Population>;

fn add(
    rows: &mut Populations,
    population: &str,
    operation: &str,
    id: String,
    value: Option<f64>,
) -> Result<()> {
    let row = rows
        .entry((population.to_string(), operation.to_string()))
        .or_default();
    match value {
        Some(value) if value.is_finite() => row.values.push((id, value)),
        Some(_) => anyhow::bail!("non-finite duration in {population}/{operation}/{id}"),
        None => row.missing += 1,
    }
    Ok(())
}

fn array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value]> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .with_context(|| format!("receipt requires raw {key} array"))
}

fn label<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("raw record requires {key}"))
}

fn duration(value: &Value) -> Result<Option<f64>> {
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_f64()
        .map(Some)
        .context("duration must be numeric milliseconds or null")
}

fn stages(rows: &mut Populations, population: &str, id: &str, value: &Value) -> Result<()> {
    let stages = value
        .get("stages")
        .and_then(Value::as_object)
        .context("raw operation requires stages")?;
    for (name, stage) in stages {
        add(
            rows,
            population,
            &format!("stage/{name}"),
            id.to_string(),
            duration(&stage["duration_ms"])?,
        )?;
    }
    // These are sums of phase spans within ONE turn, never individual spans.
    if let Some(phases) = value.get("phase_profile").and_then(Value::as_object) {
        for (name, phase) in phases {
            add(
                rows,
                population,
                &format!("phase_total/{name}"),
                id.to_string(),
                duration(&phase["duration_ms"])?,
            )?;
        }
    }
    Ok(())
}

fn runtime_rows(receipt: &Value) -> Result<Populations> {
    let mut rows = Populations::new();
    let mut population_sizes = BTreeMap::<String, usize>::new();
    for (run_index, run) in array(receipt, "results")?.iter().enumerate() {
        let scenario = label(run, "scenario")?;
        let population = format!("runtime/{scenario}/run_total");
        *population_sizes.entry(population.clone()).or_default() += 1;
        stages(&mut rows, &population, &format!("run:{run_index}"), run)?;
        let turns = array(run, "turns")?;
        for (position, turn) in turns.iter().enumerate() {
            let index = turn["turn_index"]
                .as_u64()
                .context("turn requires turn_index")?;
            // Knee operation IDs encode the step at 100,000,000. Keep steps
            // separate: their concurrency geometries are deliberately unlike.
            let window = if scenario == "high_traffic_knee_sqlite" {
                format!("step:{}", index / 100_000_000)
            } else if scenario == "high_traffic_load_sqlite" {
                "load".to_string()
            } else if position == 0 {
                "first_turn".to_string()
            } else {
                "subsequent_turn".to_string()
            };
            let population = format!("runtime/{scenario}/{window}");
            *population_sizes.entry(population.clone()).or_default() += 1;
            stages(
                &mut rows,
                &population,
                &format!("run:{run_index}/turn:{index}"),
                turn,
            )?;
        }
        if let Some(metrics) = run.get("metric_samples_ms").and_then(Value::as_object) {
            for (name, values) in metrics {
                let values = values
                    .as_array()
                    .context("metric_samples_ms entries must be arrays")?;
                for (index, value) in values.iter().enumerate() {
                    add(
                        &mut rows,
                        &format!("runtime/{scenario}/sampled_operations"),
                        name,
                        format!("run:{run_index}/sample:{index}"),
                        duration(value)?,
                    )?;
                }
            }
        }
    }
    for ((population, operation), row) in &mut rows {
        if operation.starts_with("stage/") || operation.starts_with("phase_total/") {
            row.missing = population_sizes[population] - row.values.len();
        }
    }
    Ok(rows)
}

const LATENCY_OPERATIONS: &[&str] = &[
    "request_to_accept_ms",
    "accept_to_admission_ms",
    "accept_to_applied_ms",
    "admission_to_first_delta_ms",
    "admission_to_settled_ms",
    "settled_to_complete_ms",
    "send_to_completion_ms",
    "provider_ms",
    "overhead_ms",
    "model_span_ms",
    "poll_detect_ms",
];

fn latency_rows(samples: &[Value]) -> Result<Populations> {
    let mut rows = Populations::new();
    for sample in samples {
        let case = label(sample, "case")?;
        let status = label(sample, "status")?;
        let cold = sample["cold"]
            .as_bool()
            .context("sample requires cold flag")?;
        let lane = sample["lane"].as_u64().context("sample requires lane ID")?;
        let index = sample["index"].as_u64().context("sample requires index")?;
        let timeout = sample["poller_timed_out"]
            .as_bool()
            .context("sample requires poller_timed_out flag")?;
        let population = format!(
            "latency/{case}/{}/{status}/poll_timeout:{timeout}",
            if cold { "cold" } else { "warm" }
        );
        for operation in LATENCY_OPERATIONS {
            let name = if *operation == "poll_detect_ms" {
                "simulated/poll_detect_ms"
            } else {
                operation
            };
            add(
                &mut rows,
                &population,
                name,
                format!("lane:{lane}/sample:{index}"),
                duration(&sample[operation])?,
            )?;
        }
    }
    Ok(rows)
}

fn render(rows: Populations, slowest: usize) -> String {
    let mut out = String::from(
        "operation duration tails; unit=ms; statistic=linear interpolation at p*(n-1); n=contributing operations; missing=absent marks; signed differences retained\nsmall functional populations do not certify rare tails; run_total and phase_total rows describe their named envelope, simulated rows are labelled\npopulation\toperation\tunit\tn\tmissing\tp50\tp99\tp99.9\tmax\tslowest_ids (value_ms)\n",
    );
    for ((population, operation), mut row) in rows {
        row.values.sort_by(|(left_id, left), (right_id, right)| {
            left.total_cmp(right).then_with(|| left_id.cmp(right_id))
        });
        let values: Vec<_> = row.values.iter().map(|(_, value)| *value).collect();
        let stats = if let Some(max) = values.last() {
            format!(
                "{:.6}\t{:.6}\t{:.6}\t{max:.6}",
                percentile_sorted(&values, 0.5),
                percentile_sorted(&values, 0.99),
                percentile_sorted(&values, 0.999)
            )
        } else {
            "-\t-\t-\t-".to_string()
        };
        let ids = row
            .values
            .iter()
            .rev()
            .take(slowest)
            .map(|(id, value)| format!("{id} ({value:.6})"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "{population}\t{operation}\tms\t{}\t{}\t{stats}\t{ids}\n",
            values.len(),
            row.missing
        ));
    }
    out
}

/// Read a runtime receipt, latency receipt or raw latency ledger without running a workload.
pub fn run(receipt: &Path, samples: Option<&Path>, slowest: usize) -> Result<()> {
    let value: Value = serde_json::from_slice(&std::fs::read(receipt)?)?;
    let rows = if value.is_array() {
        latency_rows(
            value
                .as_array()
                .context("latency ledger must be an array")?,
        )?
    } else {
        match value["kind"].as_str() {
            Some("runtime-perf") => runtime_rows(&value)?,
            Some("lash.send-latency") => {
                let path = if let Some(samples) = samples {
                    samples.to_path_buf()
                } else if let Some(path) = value["samples_file"].as_str() {
                    receipt.parent().unwrap_or(Path::new(".")).join(path)
                } else {
                    receipt.with_file_name(format!(
                        "{}.samples.json",
                        receipt
                            .file_name()
                            .context("receipt requires a filename")?
                            .to_string_lossy()
                    ))
                };
                let ledger: Vec<Value> = serde_json::from_slice(&std::fs::read(&path).with_context(|| format!("reading raw samples {}; supply --samples for a separately retained ledger", path.display()))?)?;
                latency_rows(&ledger)?
            }
            _ => anyhow::bail!(
                "expected a runtime-perf or lash.send-latency receipt, or a raw latency sample array"
            ),
        }
    };
    print!("{}", render(rows, slowest));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn raw_operation_tails_preserve_p999_max_ids_and_populations() {
        let turns: Vec<_> = (0..1001).map(|index| json!({"turn_index":index, "stages":{"total":{"duration_ms": index as f64}}, "phase_profile":{}})).collect();
        let receipt = json!({"results":[{"scenario":"standard", "stages":{"total":{"duration_ms":12345.0}}, "turns":turns, "metric_samples_ms":{}}]});
        let table = render(runtime_rows(&receipt).expect("raw runtime receipt"), 2);
        // Steady operations are 1..=1000, so rank .999*999 gives 999.001.
        assert!(table.contains("runtime/standard/subsequent_turn\tstage/total\tms\t1000\t0\t500.500000\t990.010000\t999.001000\t1000.000000\trun:0/turn:1000 (1000.000000), run:0/turn:999 (999.000000)"), "{table}");
        let mut samples: Vec<_> = (0..1000).map(|index| json!({"case":"fast", "lane":0, "index":index, "cold":false, "status":"answered", "poller_timed_out":false, "send_to_completion_ms":index as f64, "settled_to_complete_ms":-2.0})).collect();
        samples.push(json!({"case":"fast", "lane":1, "index":0, "cold":true, "status":"failed", "poller_timed_out":true, "send_to_completion_ms":50000.0}));
        let table = render(latency_rows(&samples).expect("latency ledger"), 1);
        assert!(table.contains("latency/fast/warm/answered/poll_timeout:false\tsend_to_completion_ms\tms\t1000\t0\t499.500000\t989.010000\t998.001000\t999.000000\tlane:0/sample:999 (999.000000)"), "{table}");
        assert!(table.contains("-2.000000\t-2.000000\t-2.000000\t-2.000000"));
        assert!(table.contains("latency/fast/cold/failed/poll_timeout:true"));
        assert!(table.contains("overhead_ms\tms\t0\t1000\t-\t-\t-\t-"));
        let root = tempfile::tempdir().expect("artifact root");
        let original = root.path().join("original");
        std::fs::create_dir(&original).expect("artifact directory");
        let receipt = original.join("latency.json");
        let ledger = original.join("custom.samples.json");
        std::fs::write(&ledger, serde_json::to_vec(&samples).expect("ledger JSON"))
            .expect("write ledger");
        let reference =
            crate::latency::samples_reference(&receipt, &ledger).expect("relative reference");
        assert_eq!(reference, std::path::Path::new("custom.samples.json"));
        std::fs::write(
            &receipt,
            json!({"kind":"lash.send-latency", "samples_file":reference}).to_string(),
        )
        .expect("write receipt");
        let retained = root.path().join("retained");
        std::fs::rename(&original, &retained).expect("retain artifacts elsewhere");
        run(&retained.join("latency.json"), None, 1)
            .expect("one command reads a relocated custom-ledger receipt");
    }
}
