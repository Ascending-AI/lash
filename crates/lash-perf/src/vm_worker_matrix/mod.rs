#![expect(
    clippy::expect_used,
    reason = "the fixed benchmark workload inventory contains its scalar reference"
)]
mod baseline;
mod metrics;
mod worker;
mod workload;

use anyhow::{Result, ensure};
use lash_vm_client::{PoolConfig, WorkerEntry, WorkerPool, WorkerPoolRuntimeOps as _};
use lash_vm_protocol::VmOwner;
use metrics::Samples;
use std::path::Path;
use std::time::Instant;

pub fn nanos(start: Instant) -> u64 {
    start.elapsed().as_nanos() as u64
}

fn config() -> Result<PoolConfig> {
    let mut config = PoolConfig::standard(WorkerEntry::reexec()?);
    // Fix concurrency at one for the paired service-time population.
    config.max_workers = 1;
    config.entry.args.push("--lash-vm-measure".into());
    Ok(config)
}

pub fn verify() -> Result<()> {
    let pool = WorkerPool::new(config()?)?;
    let reference = baseline::Reference::new()?;
    for case in workload::cases() {
        reference.run(&case)?;
        worker::run(&case, &pool, &VmOwner::new("verification"))?;
        println!("verified {}", case.name);
    }
    let a = worker::run(&workload::cases()[1], &pool, &VmOwner::new("owner-A"))?;
    let b = worker::run(&workload::cases()[1], &pool, &VmOwner::new("owner-B"))?;
    ensure!(
        a.pid == b.pid,
        "clean sessions did not reuse the reset worker"
    );
    println!("verified reset reuse across owners");
    for index in 0..256 {
        worker::run(
            &workload::cases()[0],
            &pool,
            &VmOwner::new(format!("churn-{index}")),
        )?;
    }
    println!("verified reservation across growing lease IDs");
    queue_once(&pool, &workload::cases()[1])?;
    println!("verified bounded queued checkout");
    Ok(())
}

/// Warm effect exchanges only, for a paired before/after comparison in one
/// run (FIG-4433). The timings are a population on a shared host; the frame
/// and byte counts per exchange are exact.
pub fn exchanges(path: &Path, warm: usize, enforce_budgets: bool) -> Result<()> {
    ensure!(warm > 0, "empty exchange population");
    let pool = WorkerPool::new(config()?)?;
    let mut codec = baseline::CodecSocket::new()?;
    let mut samples = Samples::new(path)?;
    let mut budgets = Vec::new();
    for case in workload::cases().into_iter().filter(|c| c.effects > 0) {
        let before = pool.measurements().counters;
        let mut exchange = Vec::new();
        let mut phases = Vec::new();
        let mut baseline = Vec::new();
        for index in 0..warm {
            let observation = worker::run(&case, &pool, &VmOwner::new(format!("owner-{index}")))?;
            baseline.extend(
                observation
                    .value_fixtures
                    .iter()
                    .map(|(request, answer)| codec.measure(request, answer))
                    .collect::<Result<Vec<_>>>()?,
            );
            exchange.extend(observation.exchange_ns.into_iter().map(|v| v as i64));
            phases.extend(observation.exchange_phases);
        }
        let mut row = metrics::record_exchanges(
            &mut samples,
            &case.name,
            &exchange,
            leaves(&case),
            &baseline,
            &phases,
        )?;
        let after = pool.measurements().counters;
        let per_case = |after: u64, before: u64| (after - before) as f64 / warm as f64;
        row["sent_frames_per_case"] =
            serde_json::json!(per_case(after.ipc_sent_messages, before.ipc_sent_messages));
        row["received_frames_per_case"] = serde_json::json!(per_case(
            after.ipc_received_messages,
            before.ipc_received_messages
        ));
        row["sent_bytes_per_case"] =
            serde_json::json!(per_case(after.ipc_sent_bytes, before.ipc_sent_bytes));
        row["received_bytes_per_case"] = serde_json::json!(per_case(
            after.ipc_received_bytes,
            before.ipc_received_bytes
        ));
        println!(
            "exchange {} samples={} p50_ns={} p99_ns={} sent_frames_per_case={} received_frames_per_case={} sent_bytes_per_case={} received_bytes_per_case={}",
            case.name,
            exchange.len(),
            row["raw_batch_p50_ns"],
            row["raw_batch_p99_ns"],
            row["sent_frames_per_case"],
            row["received_frames_per_case"],
            row["sent_bytes_per_case"],
            row["received_bytes_per_case"]
        );
        budgets.push(row);
        samples.finish(path)?;
    }
    finish_budgets(path, &samples, &budgets, enforce_budgets)
}

fn leaves(case: &workload::Case) -> usize {
    if case.name.starts_with("parallel-") {
        case.effects
    } else {
        1
    }
}

fn finish_budgets(
    path: &Path,
    samples: &Samples,
    exchanges: &[serde_json::Value],
    enforce_budgets: bool,
) -> Result<()> {
    let zero = samples.summaries.iter().find(|s| s.metric == "warm/zero-effects/paired-overhead").map(|s| serde_json::json!({"p50_ns":s.p50,"p99_ns":s.p99,"over_budget":s.p50>1_000_000 || s.p99>5_000_000,"threshold_p50_ns":1_000_000,"threshold_p99_ns":5_000_000}));
    let report = serde_json::json!({
        "certification_mode":if enforce_budgets { "certifying" } else { "report_only" },
        "zero_effect_overhead":zero,"effect_exchanges":exchanges,
        "instrumentation":"const_generic_zero_cost_hook",
        "exchange_boundary":"worker_request_serialization_start_to_next_request_received",
        "exchange_clock":"CLOCK_MONOTONIC",
        "baseline_process_id":std::process::id(),
        "baseline_transport":"same_process_socket_pair",
        "budget_statistic":"nearest_rank_p50_and_p99",
        "exchange_budget_unit":"nanoseconds_per_leaf",
        "zero_effect_budget_unit":"nanoseconds_per_case",
        "threshold_kind":"configured_upper_bound",
        "phase_attribution":"worker_and_parent_phases_measured; ipc_read_wait_is_exclusive_wall_remainder; overlap_reported",
    });
    std::fs::write(
        path.join("budgets.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    metrics::write_exchange_report(path, samples, exchanges, enforce_budgets)?;
    if enforce_budgets {
        ensure!(
            zero.is_some() || !exchanges.is_empty(),
            "no selected worker budgets"
        );
        ensure!(
            zero.iter()
                .chain(exchanges)
                .all(|row| row["over_budget"].as_bool() == Some(false)
                    && row
                        .get("reconciliation_failures")
                        .is_none_or(|value| value.as_u64() == Some(0))),
            "worker matrix budget failed; see budgets.json"
        );
    } else {
        println!("Report-only mode: does not certify any worker budget.");
    }
    Ok(())
}

pub fn measure(path: &Path, warm: usize, cold: usize, enforce_budgets: bool) -> Result<()> {
    ensure!(
        warm >= 10_000 && cold >= 200,
        "measurement needs 10,000 warm observations and 200 cold starts"
    );
    let pool = WorkerPool::new(config()?)?;
    let reference = baseline::Reference::new()?;
    let mut codec = baseline::CodecSocket::new()?;
    let mut budgets = Vec::new();
    let mut samples = Samples::new(path)?;
    for case in workload::cases() {
        println!("measuring {}", case.name);
        let mut reference_times = Vec::with_capacity(warm);
        let mut isolated = Vec::with_capacity(warm);
        let mut queue = Vec::with_capacity(warm);
        let mut exchange = Vec::new();
        let mut exchange_phases = Vec::new();
        let mut codec_baseline = Vec::new();
        let mut reset = Vec::new();
        let mut parent_rss = Vec::new();
        let mut parent_peak = Vec::new();
        let mut worker_rss = Vec::new();
        let mut worker_peak = Vec::new();
        let mut state_bytes = Vec::new();
        for index in 0..warm {
            let reference_run = || -> Result<u64> {
                let t = Instant::now();
                reference.run(&case)?;
                Ok(nanos(t))
            };
            let isolated_run = || -> Result<(u64, worker::Observation)> {
                let t = Instant::now();
                let observation =
                    worker::run(&case, &pool, &VmOwner::new(format!("owner-{index}")))?;
                Ok((nanos(t), observation))
            };
            let (base_ns, (worker_ns, observation)) = if index % 2 == 0 {
                (reference_run()?, isolated_run()?)
            } else {
                let result = isolated_run()?;
                (reference_run()?, result)
            };
            reference_times.push(base_ns as i64);
            isolated.push(worker_ns as i64);
            queue.push(observation.queue_ns as i64);
            codec_baseline.extend(
                observation
                    .value_fixtures
                    .iter()
                    .map(|(request, answer)| codec.measure(request, answer))
                    .collect::<Result<Vec<_>>>()?,
            );
            exchange.extend(observation.exchange_ns.into_iter().map(|v| v as i64));
            exchange_phases.extend(observation.exchange_phases);
            reset.extend(observation.resets_ns.into_iter().map(|v| v as i64));
            if index.is_multiple_of(100) && !case.error {
                let memory =
                    worker::run_observed(&case, &pool, &VmOwner::new("memory-probe"), true)?;
                if let Some(value) = memory.worker_rss_kib {
                    worker_rss.push(value as i64);
                }
                if let Some(value) = memory.worker_peak_kib {
                    worker_peak.push(value as i64);
                }
            }
            state_bytes.push(observation.state_bytes as i64);
            let memory = metrics::memory(std::process::id())?;
            parent_rss.push(memory.0 as i64);
            parent_peak.push(memory.1 as i64);
        }
        let overhead: Vec<_> = isolated
            .iter()
            .zip(&reference_times)
            .map(|(w, b)| w - b)
            .collect();
        for (metric, values, unit) in [
            ("baseline", &reference_times, "ns"),
            ("worker", &isolated, "ns"),
            ("paired-overhead", &overhead, "ns"),
            ("checkout", &queue, "ns"),
            ("reset-or-discard", &reset, "ns"),
            ("parent-rss", &parent_rss, "KiB"),
            ("parent-peak", &parent_peak, "KiB"),
            ("worker-rss", &worker_rss, "KiB"),
            ("worker-peak", &worker_peak, "KiB"),
            ("state", &state_bytes, "bytes"),
        ] {
            if !values.is_empty() {
                samples.record(&format!("warm/{}/{metric}", case.name), values, unit)?;
            }
        }
        if !exchange.is_empty() {
            budgets.push(metrics::record_exchanges(
                &mut samples,
                &case.name,
                &exchange,
                leaves(&case),
                &codec_baseline,
                &exchange_phases,
            )?);
        }
        let mut cold_ns = Vec::new();
        let mut cold_queue = Vec::new();
        let mut cold_parent_rss = Vec::new();
        let mut cold_parent_peak = Vec::new();
        let mut cold_worker_rss = Vec::new();
        let mut cold_worker_peak = Vec::new();
        for index in 0..cold {
            let t = Instant::now();
            let cold_pool = WorkerPool::new(config()?)?;
            let observation =
                worker::run(&case, &cold_pool, &VmOwner::new(format!("cold-{index}")))?;
            cold_ns.push(nanos(t) as i64);
            cold_queue.push(observation.queue_ns as i64);
            let parent = metrics::memory(std::process::id())?;
            cold_parent_rss.push(parent.0 as i64);
            cold_parent_peak.push(parent.1 as i64);
            if !case.error {
                let child = metrics::memory(observation.pid)?;
                cold_worker_rss.push(child.0 as i64);
                cold_worker_peak.push(child.1 as i64);
            }
        }
        samples.record(&format!("cold/{}/worker", case.name), &cold_ns, "ns")?;
        samples.record(&format!("cold/{}/checkout", case.name), &cold_queue, "ns")?;
        for (metric, values) in [
            ("parent-rss", cold_parent_rss),
            ("parent-peak", cold_parent_peak),
            ("worker-rss", cold_worker_rss),
            ("worker-peak", cold_worker_peak),
        ] {
            if !values.is_empty() {
                samples.record(&format!("cold/{}/{metric}", case.name), &values, "KiB")?;
            }
        }
        samples.finish(path)?;
    }
    measure_concurrency(&mut samples, warm)?;
    samples.finish(path)?;
    finish_budgets(path, &samples, &budgets, enforce_budgets)?;
    Ok(())
}

fn measure_concurrency(samples: &mut Samples, count: usize) -> Result<()> {
    let case = workload::cases()[1].clone();
    for width in [1, 2, 4] {
        let mut config = config()?;
        config.min_workers = width;
        config.max_workers = width;
        let pool = WorkerPool::new(config)?;
        let mut latency = Vec::new();
        let mut checkout = Vec::new();
        for _ in 0..count {
            let t = Instant::now();
            let observations = std::thread::scope(|scope| -> Result<Vec<_>> {
                let workers: Vec<_> = (0..width)
                    .map(|index| {
                        let pool = &pool;
                        let case = &case;
                        scope.spawn(move || {
                            worker::run(
                                case,
                                pool,
                                &VmOwner::new(format!("parallel-owner-{index}")),
                            )
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .map(|w| {
                        w.join()
                            .map_err(|_| anyhow::anyhow!("measurement thread panicked"))?
                    })
                    .collect()
            })?;
            latency.push(nanos(t) as i64);
            checkout.extend(observations.iter().map(|o| o.queue_ns as i64));
        }
        samples.record(&format!("concurrency/{width}/batch"), &latency, "ns")?;
        samples.record(&format!("concurrency/{width}/checkout"), &checkout, "ns")?;
        if let Some(summary) = samples
            .summaries
            .iter_mut()
            .find(|s| s.metric == format!("concurrency/{width}/batch"))
        {
            summary.throughput_per_second = summary.throughput_per_second.map(|v| v * width as f64);
        }
    }
    let pool = WorkerPool::new(config()?)?;
    let mut queued = Vec::with_capacity(count);
    for _ in 0..count {
        queued.push(queue_once(&pool, &case)?.queue_ns as i64);
    }
    samples.record("saturated/one-slot/checkout", &queued, "ns")?;
    Ok(())
}

fn queue_once(pool: &WorkerPool, case: &workload::Case) -> Result<worker::Observation> {
    let held = pool.checkout(
        1,
        lash_vm_protocol::OwnerEpoch(0),
        lash_vm_protocol::FrameEpoch(0),
        lash_vm_client::ExecutionBudget::default(),
    )?;
    std::thread::scope(|scope| -> Result<_> {
        let waiter = scope.spawn(|| worker::run(case, pool, &VmOwner::new("queued-owner")));
        let deadline = Instant::now() + pool.config().deadlines.checkout;
        while pool.stats().queued_items != 1 {
            ensure!(
                Instant::now() < deadline,
                "waiter never entered the bounded queue"
            );
            std::thread::yield_now();
        }
        ensure!(
            pool.stats().queued_bytes > 0,
            "queued input bytes were not charged"
        );
        held.release()?;
        waiter
            .join()
            .map_err(|_| anyhow::anyhow!("queue measurement thread panicked"))?
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn certification_rejects_any_selected_worker_budget() {
        let directory = tempfile::tempdir().unwrap();
        let mut samples = super::Samples::new(directory.path()).unwrap();
        let rows = [
            super::metrics::record_exchanges(&mut samples, "fast", &[1], 1, &[], &[]).unwrap(),
            super::metrics::record_exchanges(&mut samples, "slow", &[600_000], 1, &[], &[])
                .unwrap(),
        ];
        let result = super::finish_budgets(directory.path(), &samples, &rows, true);
        assert!(
            result.is_err(),
            "a failing worker budget must refuse certification"
        );
        let receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.path().join("budgets.json")).unwrap())
                .unwrap();
        assert_eq!(receipt["certification_mode"], "certifying");
        assert!(super::finish_budgets(directory.path(), &samples, &rows, false).is_ok());
        assert!(super::finish_budgets(directory.path(), &samples, &rows[..1], true).is_ok());
        let mut unreconciled = rows[..1].to_vec();
        unreconciled[0]["reconciliation_failures"] = serde_json::json!(1);
        assert!(super::finish_budgets(directory.path(), &samples, &unreconciled, true).is_err());
        samples
            .record("warm/zero-effects/paired-overhead", &[6_000_000], "ns")
            .unwrap();
        assert!(super::finish_budgets(directory.path(), &samples, &[], true).is_err());
    }
}
