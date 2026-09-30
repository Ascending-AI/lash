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

pub fn measure(path: &Path, warm: usize, cold: usize) -> Result<()> {
    ensure!(
        warm >= 10_000 && cold >= 200,
        "measurement needs 10,000 warm observations and 200 cold starts"
    );
    let pool = WorkerPool::new(config()?)?;
    let reference = baseline::Reference::new()?;
    let mut samples = Samples::new(path)?;
    for case in workload::cases() {
        println!("measuring {}", case.name);
        let mut reference_times = Vec::with_capacity(warm);
        let mut isolated = Vec::with_capacity(warm);
        let mut queue = Vec::with_capacity(warm);
        let mut exchange = Vec::new();
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
            exchange.extend(observation.exchange_ns.into_iter().map(|v| v as i64));
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
            samples.record(
                &format!("warm/{}/effect-exchange", case.name),
                &exchange,
                "ns",
            )?;
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
    let zero = samples
        .summaries
        .iter()
        .find(|s| s.metric == "warm/zero-effects/paired-overhead")
        .context("missing zero overhead")?;
    let exchanges: Vec<_> = samples.summaries.iter()
        .filter(|s| s.metric.ends_with("/effect-exchange"))
        .map(|s| serde_json::json!({"metric":s.metric,"p50_ns":s.p50,"p99_ns":s.p99,"over_budget":s.p50>100_000 || s.p99>500_000}))
        .collect();
    let failures = serde_json::json!({"zero_effect_overhead": {"p50_ns":zero.p50,"p99_ns":zero.p99,"over_budget":zero.p50>1_000_000 || zero.p99>5_000_000},"effect_exchanges": exchanges});
    std::fs::write(
        path.join("budgets.json"),
        serde_json::to_vec_pretty(&failures)?,
    )?;
    Ok(())
}
use anyhow::Context;

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
