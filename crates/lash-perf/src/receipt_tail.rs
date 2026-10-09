//! Offline operation tails from the raw ledgers retained by performance receipts.
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::perf_support::metrics::{nearest_rank, percentile_sorted};

#[derive(Default)]
struct Population {
    values: Vec<(String, f64)>,
    missing: usize,
    nearest_rank: bool,
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
    let boundary = rows.values().any(|row| row.nearest_rank);
    let mut out = String::from(
        "operation duration tails; unit=ms; statistic=linear interpolation at p*(n-1); n=contributing operations; missing=absent marks; signed differences retained\nsmall functional populations do not certify rare tails; run_total and phase_total rows describe their named envelope, simulated rows are labelled\npopulation\toperation\tunit\tn\tmissing\tp50\tp99\tp99.9\tmax\tslowest_ids (value_ms)\n",
    );
    if boundary {
        out = out.replace(
            "linear interpolation at p*(n-1)",
            "nearest-rank over exact operation intervals",
        );
    }
    for ((population, operation), mut row) in rows {
        row.values.sort_by(|(left_id, left), (right_id, right)| {
            left.total_cmp(right).then_with(|| left_id.cmp(right_id))
        });
        let values: Vec<_> = row.values.iter().map(|(_, value)| *value).collect();
        let percentile = |fraction| {
            if row.nearest_rank {
                nearest_rank(&values, fraction)
            } else {
                percentile_sorted(&values, fraction)
            }
        };
        let stats = if let Some(max) = values.last() {
            format!(
                "{:.6}\t{:.6}\t{:.6}\t{max:.6}",
                percentile(0.5),
                percentile(0.99),
                percentile(0.999)
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

pub(crate) fn boundary_table(value: &Value, slowest: usize) -> Result<String> {
    let ledger: crate::boundary::Ledger = serde_json::from_value(value.clone())?;
    let mut rows = Populations::new();
    let mut aggregates = String::from(
        "aggregate intervals; excluded from operation percentiles\nbackend\tboundary\tmeasurement\tN\ttotal_ms\tidentity\tresult\n",
    );
    for observation in &ledger.observations {
        let elapsed_ns = observation
            .end_ns
            .checked_sub(observation.start_ns)
            .filter(|elapsed| *elapsed >= 0)
            .context("boundary interval must have monotonic start <= end")?;
        let elapsed_ms = elapsed_ns as f64 / 1_000_000.0;
        if observation.is_operation() {
            let population = format!("boundary/{}", observation.backend);
            add(
                &mut rows,
                &population,
                &observation.boundary,
                format!(
                    "pid:{}/record:{}/{} [{}]",
                    observation.process_id,
                    observation.record_id,
                    observation.operation_id,
                    observation.result
                ),
                Some(elapsed_ms),
            )?;
            rows.get_mut(&(population, observation.boundary.clone()))
                .context("added boundary row")?
                .nearest_rank = true;
        } else {
            aggregates.push_str(&format!(
                "{}\t{}\taggregate\t{}\t{elapsed_ms:.6}\t{}\t{}\n",
                observation.backend,
                observation.boundary,
                observation.operations(),
                observation.operation_id,
                observation.result
            ));
        }
    }
    Ok(format!(
        "boundary ledger cap={} retained_records={} dropped_records={} dropped_operations={}\n{}{}",
        ledger.cap,
        ledger.observations.len(),
        ledger.dropped_records,
        ledger.dropped_operations,
        render(rows, slowest),
        aggregates
    ))
}

/// Read an operation receipt or its retained ledger without running a workload.
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
            Some("lash.boundary-workload") => {
                let path = if let Some(samples) = samples {
                    samples.to_path_buf()
                } else {
                    receipt
                        .parent()
                        .unwrap_or(Path::new("."))
                        .join(label(&value, "ledger_file")?)
                };
                let ledger: Value = serde_json::from_slice(
                    &std::fs::read(&path)
                        .with_context(|| format!("reading boundary ledger {}", path.display()))?,
                )?;
                print!("{}", boundary_table(&ledger, slowest)?);
                return Ok(());
            }
            Some("lash.boundary-observations") => {
                print!("{}", boundary_table(&value, slowest)?);
                return Ok(());
            }
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
                "expected a runtime, latency or boundary receipt, boundary ledger, or raw latency sample array"
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
    fn boundary_percentiles_are_nearest_rank_on_exact_samples() {
        let observations: Vec<_> = (1..=1001).rev().map(|index| json!({
            "boundary": "send.settle", "start_ns": 700, "end_ns": 700 + index * 1_000_000_u64,
            "operation_id": format!("turn:{index}"), "result": "ok", "backend": "sqlite",
            "process_id": 42, "record_id": index, "measurement": "operation",
        })).collect();
        let table = boundary_table(
            &json!({
                "kind": "lash.boundary-observations", "cap": 1001, "dropped_operations": 0,
                "dropped_records": 0, "observations": observations,
            }),
            10,
        )
        .expect("exact boundary samples");
        assert!(table.contains("boundary/sqlite\tsend.settle\tms\t1001\t0\t501.000000\t991.000000\t1000.000000\t1001.000000"), "{table}");
        let row = table
            .lines()
            .find(|line| line.starts_with("boundary/sqlite\t"))
            .expect("boundary row");
        let slowest = row.split('\t').next_back().expect("slowest IDs");
        assert_eq!(slowest.split(", ").count(), 10);
        assert!(slowest.starts_with("pid:42/record:1001/turn:1001 [ok] (1001.000000)"));
        assert!(slowest.ends_with("pid:42/record:992/turn:992 [ok] (992.000000)"));
    }

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
