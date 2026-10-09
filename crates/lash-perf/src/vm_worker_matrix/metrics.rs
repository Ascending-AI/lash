use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;

pub fn memory(pid: u32) -> Result<(u64, u64)> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let value = |key: &str| -> Result<u64> {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.split_whitespace().next())
            .context("missing Linux memory counter")?
            .parse()
            .map_err(Into::into)
    };
    Ok((value("VmRSS:")?, value("VmHWM:")?))
}
#[derive(Serialize)]
pub struct MetricDistribution {
    pub metric: String,
    pub count: usize,
    pub unit: String,
    pub p50: i64,
    pub p99: i64,
    pub max: i64,
    pub throughput_per_second: Option<f64>,
}
pub struct Samples {
    pub raw: std::io::BufWriter<std::fs::File>,
    pub summaries: Vec<MetricDistribution>,
}
impl Samples {
    pub fn new(path: &Path) -> Result<Self> {
        use std::io::Write;
        std::fs::create_dir_all(path)?;
        let mut raw = std::io::BufWriter::new(std::fs::File::create(path.join("samples.csv"))?);
        writeln!(raw, "metric,sample,value,unit")?;
        Ok(Self {
            raw,
            summaries: Vec::new(),
        })
    }
    pub fn record(&mut self, name: &str, values: &[i64], unit: &str) -> Result<()> {
        use std::io::Write;
        anyhow::ensure!(!values.is_empty(), "empty metric {name}");
        for (i, v) in values.iter().enumerate() {
            writeln!(self.raw, "{name},{i},{v},{unit}")?;
        }
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        let sum: i128 = values.iter().map(|v| i128::from(*v)).sum();
        let summary = MetricDistribution {
            metric: name.into(),
            count: sorted.len(),
            unit: unit.into(),
            p50: sorted[(sorted.len() * 50).div_ceil(100) - 1],
            p99: sorted[(sorted.len() * 99).div_ceil(100) - 1],
            max: sorted[sorted.len() - 1],
            throughput_per_second: (unit == "ns"
                && sum > 0
                && (name.ends_with("/worker")
                    || name.ends_with("/baseline")
                    || name.ends_with("/batch")))
            .then(|| values.len() as f64 * 1e9 / sum as f64),
        };
        println!(
            "{name}: n={} p50={} p99={} max={} {unit}",
            summary.count, summary.p50, summary.p99, summary.max
        );
        self.summaries.push(summary);
        Ok(())
    }
    pub fn finish(&mut self, path: &Path) -> Result<()> {
        use std::io::Write;
        self.raw.flush()?;
        std::fs::write(
            path.join("summary.json"),
            serde_json::to_vec_pretty(&self.summaries)?,
        )?;
        Ok(())
    }
}

#[derive(Clone, Copy, Default, Debug)]
pub struct Phases {
    pub parent_decode: i64,
    pub parent_encode: i64,
    pub ipc_write: i64,
    pub ipc_read_wait: i64,
    pub worker_decode: i64,
    pub worker_encode: i64,
    pub guest: i64,
    pub host: i64,
    pub overlap: i64,
    pub raw_read_wait: i64,
}

pub const RECONCILIATION_TOLERANCE_NS: i64 = 1_000;

impl Phases {
    pub fn measured(
        elapsed: i64,
        decode: i64,
        encode: i64,
        host: i64,
        timing: lash_vm_client::ipc::ExchangeTiming,
    ) -> Self {
        let mut phases = Self {
            parent_decode: decode + timing.parent_decode_ns as i64,
            parent_encode: encode + timing.parent_encode_ns as i64,
            ipc_write: timing.write_ns as i64,
            worker_decode: timing.worker_decode_ns as i64,
            worker_encode: timing.worker_encode_ns as i64,
            guest: timing.guest_ns as i64,
            host,
            raw_read_wait: timing.read_wait_ns as i64,
            ..Self::default()
        };
        // Parent reads overlap worker phases. Attribute worker time first;
        // read/wait owns the remaining wall time, including transport control.
        let attributed = phases.parent_decode
            + phases.parent_encode
            + phases.ipc_write
            + phases.worker_decode
            + phases.worker_encode
            + phases.guest
            + host;
        phases.ipc_read_wait = (elapsed - attributed).max(0);
        phases.overlap = (attributed - elapsed).max(0);
        phases
    }
    pub fn sum(&self) -> i64 {
        self.parent_decode
            + self.parent_encode
            + self.ipc_write
            + self.ipc_read_wait
            + self.worker_decode
            + self.worker_encode
            + self.guest
            + self.host
    }
}

pub fn record_exchanges(
    samples: &mut Samples,
    population: &str,
    exchange: &[i64],
    leaves: usize,
    baseline: &[i64],
    phases: &[Phases],
) -> Result<serde_json::Value> {
    anyhow::ensure!(leaves > 0, "zero-leaf exchange");
    let name = format!("warm/{population}");
    samples.record(&format!("{name}/effect-exchange"), exchange, "ns")?;
    let raw_index = samples.summaries.len() - 1;
    let per_leaf: Vec<_> = exchange.iter().map(|v| v / leaves as i64).collect();
    samples.record(&format!("{name}/exchange-per-leaf"), &per_leaf, "ns/leaf")?;
    let mut judged = per_leaf;
    let method = if baseline.is_empty() {
        "none"
    } else {
        "paired_per_sample"
    };
    if !baseline.is_empty() {
        anyhow::ensure!(
            baseline.len() == exchange.len(),
            "unpaired codec baseline for {population}"
        );
        samples.record(&format!("{name}/codec-socket-baseline"), baseline, "ns")?;
        judged = exchange
            .iter()
            .zip(baseline)
            .map(|(e, b)| (e - b) / leaves as i64)
            .collect();
        samples.record(&format!("{name}/baseline-subtracted"), &judged, "ns/leaf")?;
    }
    let judged_index = samples.summaries.len() - 1;
    if !phases.is_empty() {
        anyhow::ensure!(
            phases.len() == exchange.len(),
            "missing phase samples for {population}"
        );
        for (phase, values) in [
            (
                "parent-decode",
                phases.iter().map(|p| p.parent_decode).collect::<Vec<_>>(),
            ),
            (
                "parent-encode",
                phases.iter().map(|p| p.parent_encode).collect(),
            ),
            ("ipc-write", phases.iter().map(|p| p.ipc_write).collect()),
            (
                "ipc-read-wait",
                phases.iter().map(|p| p.ipc_read_wait).collect(),
            ),
            (
                "worker-decode",
                phases.iter().map(|p| p.worker_decode).collect(),
            ),
            (
                "worker-encode",
                phases.iter().map(|p| p.worker_encode).collect(),
            ),
            ("guest", phases.iter().map(|p| p.guest).collect()),
            ("host", phases.iter().map(|p| p.host).collect()),
            (
                "raw-ipc-read-wait",
                phases.iter().map(|p| p.raw_read_wait).collect(),
            ),
            ("overlap", phases.iter().map(|p| p.overlap).collect()),
            ("sum", phases.iter().map(Phases::sum).collect()),
            (
                "reconciliation-error",
                phases
                    .iter()
                    .zip(exchange)
                    .map(|(p, e)| p.sum() - p.overlap - e)
                    .collect(),
            ),
        ] {
            samples.record(&format!("{name}/phase/{phase}"), &values, "ns")?;
        }
    }
    let raw = &samples.summaries[raw_index];
    let summary = &samples.summaries[judged_index];
    Ok(serde_json::json!({
        "metric":summary.metric,"unit":"per_leaf","leaves":leaves,
        "p50_ns":summary.p50,"p99_ns":summary.p99,
        "raw_batch_p50_ns":raw.p50,"raw_batch_p99_ns":raw.p99,
        "baseline_method":method,"baseline_order":"after_exchange",
        "negative_samples":judged.iter().filter(|v| **v<0).count(),
        "threshold_p50_ns":100_000,"threshold_p99_ns":500_000,
        "over_budget":summary.p50>100_000 || summary.p99>500_000,
        "reconciliation_tolerance_ns": RECONCILIATION_TOLERANCE_NS,
        "reconciliation_failures":phases.iter().zip(exchange).filter(|(p,e)| (p.sum()-p.overlap-**e).abs()>RECONCILIATION_TOLERANCE_NS).count(),
        "overlapping_samples":phases.iter().filter(|p| p.overlap>0).count(),
    }))
}

pub fn write_exchange_report(
    path: &Path,
    samples: &Samples,
    budgets: &[serde_json::Value],
    enforce_budgets: bool,
) -> Result<()> {
    use std::fmt::Write as _;
    let mut report = String::from(
        "# Worker exchange measurements\n\nLoaded-host diagnostic samples. Exchange spans start before worker request serialization and end after the next request arrives, using the shared machine CLOCK_MONOTONIC clock. All times are nearest-rank p50 / p99 in microseconds. Configured thresholds are 100 / 500 us per leaf.\n\n| Population | Samples | Raw batch | Per leaf | Codec/socket baseline | Subtracted per leaf | Negative samples | Over budget |\n| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |\n",
    );
    report.insert_str(
        report.find("| Population").context("missing table")?,
        if enforce_budgets {
            "Certifying mode: every selected budget binds exit status.\n\n"
        } else {
            "Report-only mode: does not certify any budget.\n\n"
        },
    );
    let pair = |population: &str, metric: &str| {
        samples
            .summaries
            .iter()
            .find(|s| s.metric == format!("warm/{population}/{metric}"))
            .map_or_else(
                || "n/a".into(),
                |s| format!("{:.3} / {:.3}", s.p50 as f64 / 1e3, s.p99 as f64 / 1e3),
            )
    };
    for row in budgets {
        let metric = row["metric"].as_str().context("missing budget metric")?;
        let population = metric.split('/').nth(1).context("missing population")?;
        let raw = samples
            .summaries
            .iter()
            .find(|s| s.metric == format!("warm/{population}/effect-exchange"))
            .context("missing raw exchange")?;
        writeln!(
            report,
            "| {population} | {} | {} | {} | {} | {} | {} | {} |",
            raw.count,
            pair(population, "effect-exchange"),
            pair(population, "exchange-per-leaf"),
            pair(population, "codec-socket-baseline"),
            pair(population, "baseline-subtracted"),
            row["negative_samples"],
            row["over_budget"]
        )?;
    }
    report.push_str("\nThe baseline is paired per sample, immediately after its worker exchange. Both endpoints live in the measuring process and use production MessagePack payload and bounded frame codecs. The socket peer echoes the identical value. Each direction encodes, moves bytes through the socket pair, and decodes. Thread startup, fixture preparation and answer validation are outside baseline timing.\n\n| Population | Parent decode | Parent encode | IPC write | IPC read/wait | Worker decode | Worker encode | Guest | Echo host |\n| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for row in budgets {
        let population = row["metric"]
            .as_str()
            .context("missing metric")?
            .split('/')
            .nth(1)
            .context("missing population")?;
        write!(report, "| {population}")?;
        for phase in [
            "parent-decode",
            "parent-encode",
            "ipc-write",
            "ipc-read-wait",
            "worker-decode",
            "worker-encode",
            "guest",
            "host",
        ] {
            write!(report, " | {}", pair(population, &format!("phase/{phase}")))?;
        }
        report.push_str(" |\n");
    }
    report.push_str("\nWorker and parent durations come from clocks around their codec, write and guest operations. IPC read/wait is the exclusive end-to-end remainder after those phases and echo host work; raw blocking read/wait is also retained. Concurrent work exceeding the wall interval is reported as overlap. Reconciliation uses the sum of the eight attributed phases minus overlap, sample by sample, with a 1 us tolerance. Percentile phase sums need not equal percentile total times.\n\n| Population | Phase sum | Raw IPC read/wait | Overlap | Reconciliation error | Failures | Overlapping samples |\n| --- | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for row in budgets {
        let population = row["metric"]
            .as_str()
            .context("missing metric")?
            .split('/')
            .nth(1)
            .context("missing population")?;
        writeln!(
            report,
            "| {population} | {} | {} | {} | {} | {} | {} |",
            pair(population, "phase/sum"),
            pair(population, "phase/raw-ipc-read-wait"),
            pair(population, "phase/overlap"),
            pair(population, "phase/reconciliation-error"),
            row["reconciliation_failures"],
            row["overlapping_samples"]
        )?;
    }
    report.push_str("\nInstrumentation uses const-generic hooks. Normal client calls and worker entry select the false specialization, which compiles out all measurement clocks and telemetry emission. The matrix opts into the measured worker at startup and uses the measured effect-answer method. These instrumented, closed-loop results are diagnostic; quiet-host final numbers belong to FIG-4172. No benchmark threshold may change to make observed results pass.\n");
    std::fs::write(path.join("report.md"), report)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_budget_is_per_leaf_and_keeps_the_raw_batch() {
        let directory = tempfile::tempdir().unwrap();
        let mut samples = Samples::new(directory.path()).unwrap();
        let row = record_exchanges(
            &mut samples,
            "parallel-10",
            &[900_000, 4_900_000],
            10,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(row["unit"], "per_leaf");
        assert_eq!(row["leaves"], 10);
        assert_eq!(row["p50_ns"], 90_000);
        assert_eq!(row["p99_ns"], 490_000);
        assert_eq!(row["over_budget"], false);
        assert_eq!(samples.summaries[0].p99, 4_900_000);
    }

    #[test]
    fn paired_value_baseline_retains_and_flags_negative_samples() {
        let directory = tempfile::tempdir().unwrap();
        let mut samples = Samples::new(directory.path()).unwrap();
        let row =
            record_exchanges(&mut samples, "value-32", &[100, 200], 1, &[110, 10], &[]).unwrap();
        assert_eq!(row["baseline_method"], "paired_per_sample");
        assert_eq!(row["negative_samples"], 1);
        assert_eq!(row["p50_ns"], -10);
        assert_eq!(row["p99_ns"], 190);
        assert!(
            samples
                .summaries
                .iter()
                .any(|s| s.metric.ends_with("/codec-socket-baseline"))
        );
    }

    #[test]
    fn measured_phases_cover_every_exchange_and_reconcile() {
        use lash_vm_client::{WorkerEntry, WorkerPool};
        use lash_vm_protocol::VmOwner;
        let helper = std::env::var_os("LASH_VM_WORKER").expect("worker helper");
        let mut config = lash_vm_client::PoolConfig::standard(WorkerEntry::helper(helper));
        config.entry.args.push("--lash-vm-measure".into());
        let pool = WorkerPool::new(config).unwrap();
        let mut codec = crate::vm_worker_matrix::baseline::CodecSocket::new().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let mut samples = Samples::new(directory.path()).unwrap();
        for case in crate::vm_worker_matrix::workload::cases()
            .into_iter()
            .filter(|case| case.effects > 0)
        {
            let observation =
                crate::vm_worker_matrix::worker::run(&case, &pool, &VmOwner::new("phase-law"))
                    .unwrap();
            assert_eq!(
                observation.exchange_phases.len(),
                observation.exchange_ns.len(),
                "{} missing phase samples",
                case.name
            );
            let baseline: Vec<_> = observation
                .value_fixtures
                .iter()
                .map(|(request, answer)| codec.measure(request, answer).unwrap())
                .collect();
            if case.name.starts_with("value-") {
                assert_eq!(baseline.len(), observation.exchange_ns.len());
                assert!(baseline.iter().all(|v| *v > 0));
            }
            let exchanges: Vec<_> = observation.exchange_ns.iter().map(|v| *v as i64).collect();
            let row = record_exchanges(
                &mut samples,
                &case.name,
                &exchanges,
                crate::vm_worker_matrix::leaves(&case),
                &baseline,
                &observation.exchange_phases,
            )
            .unwrap();
            assert_eq!(row["reconciliation_failures"], 0);
            let phase_rows: Vec<_> = samples
                .summaries
                .iter()
                .filter(|s| s.metric.starts_with(&format!("warm/{}/phase/", case.name)))
                .collect();
            assert_eq!(phase_rows.len(), 12);
            assert!(phase_rows.iter().all(|s| s.count == exchanges.len()));
            for (phase, &elapsed) in observation
                .exchange_phases
                .iter()
                .zip(&observation.exchange_ns)
            {
                assert!(
                    phase.worker_decode > 0 && phase.worker_encode > 0 && phase.guest > 0,
                    "{} missing worker measurement",
                    case.name
                );
                let sum = phase.parent_decode
                    + phase.parent_encode
                    + phase.ipc_write
                    + phase.ipc_read_wait
                    + phase.worker_decode
                    + phase.worker_encode
                    + phase.guest
                    + phase.host
                    - phase.overlap;
                assert!(
                    (sum - elapsed as i64).abs() <= 1_000,
                    "{} phases do not reconcile: {sum} vs {elapsed}",
                    case.name
                );
            }
        }
    }
}
