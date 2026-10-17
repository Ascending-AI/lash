//! Offered-load measurement: scheduled arrivals against real populations.
//!
//! `lash-perf offered-load` sweeps one population across ascending arrival
//! rates. Every step opens a fresh durable store, schedules a finite number
//! of independent sends through [`ledger::run_scheduled`], and reports the
//! ledger: offered against achieved rate, unfinished and failed operations,
//! and latency from the scheduled arrival, so the tail and the saturation
//! knee include queueing. This is the only arrival generator in the crate.
//!
//! The runtime-perf scenarios, the latency lanes and the PostgreSQL
//! live-replay bench are closed loops. They stay as they are and label their
//! receipts [`LoadModel::ServiceDiagnostic`].

pub mod ledger;
mod populations;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use ledger::{ArrivalSchedule, Knee, KneeCriteria, LedgerReport, OperationRecord};

/// How a receipt's operations were generated, and so what its latencies
/// mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LoadModel {
    /// Independent arrivals on a schedule, timed from the scheduled instant.
    OfferedLoad,
    /// A closed loop: the next operation is sent when the last one returns
    /// and is timed from its send. Its latencies are service times under a
    /// fixed concurrency; a slow operation delays the arrivals behind it and
    /// their wait is not recorded.
    ServiceDiagnostic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Population {
    /// The runtime-perf high-traffic turn mix over one served core and a
    /// session population on the selected durable backend.
    HighTraffic,
    /// Send-to-completion through a submitting core that serves no sessions
    /// and a child durable node, sharing one SQLite file.
    CrossWorker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    Sqlite,
    Postgres,
}

#[derive(Debug, clap::Args)]
pub struct Args {
    #[arg(long, value_enum)]
    pub population: Population,
    /// Same single served core and session population for either backend.
    #[arg(long, value_enum, default_value = "sqlite")]
    pub backend: Backend,
    /// Gate-owned base database; each step provisions its own isolated database.
    #[arg(long)]
    pub postgres_url: Option<String>,
    /// Ascending arrival rates to sweep, in operations per second.
    #[arg(long, value_delimiter = ',', required = true, value_name = "RATE")]
    pub rates: Vec<f64>,
    /// Scheduled operations per rate step.
    #[arg(long, default_value_t = 64)]
    pub operations: usize,
    /// Sessions the arrivals are spread over, by ordinal.
    #[arg(long, default_value_t = 4)]
    pub sessions: usize,
    /// Upper bound on operations sent and not yet completed.
    #[arg(long, default_value_t = 256)]
    pub max_in_flight: usize,
    /// How long after the last scheduled arrival a step waits before it
    /// counts the open operations as unfinished.
    #[arg(long, default_value_t = 30_000)]
    pub drain_timeout_ms: u64,
    /// Weighted high-traffic turn mix as comma-separated `kind=weight` pairs.
    #[arg(long, default_value = "plain=1,tool=1,queued=1,child=1")]
    pub mix: String,
    /// A step whose completion rate is below this share of its arrival rate
    /// has fallen behind.
    #[arg(long, default_value_t = 0.95)]
    pub knee_min_achieved: f64,
    /// A step whose scheduled-to-completed p99 exceeds this multiple of the
    /// lowest rate's has fallen behind.
    #[arg(long, default_value_t = 3.0)]
    pub knee_p99_ratio: f64,
    /// Slowest operations each step names.
    #[arg(long, default_value_t = 5)]
    pub slowest: usize,
    /// A fresh directory for per-step store paths.
    #[arg(long)]
    pub store_dir: PathBuf,
    #[arg(long, value_name = "OUT.json")]
    pub out: PathBuf,
    /// Every operation's row; default `<out>.ledger.json`.
    #[arg(long, value_name = "LEDGER.json")]
    pub ledger_out: Option<PathBuf>,
}

/// What each ledger timestamp is, on this population.
#[derive(Debug, Serialize)]
pub struct Marks {
    pub clock: &'static str,
    pub scheduled: &'static str,
    pub sent: &'static str,
    pub admitted: &'static str,
    pub settled: &'static str,
    pub completed: &'static str,
}

#[derive(Debug, Serialize)]
pub struct PopulationReceipt {
    pub name: Population,
    pub engine: &'static str,
    pub store: &'static str,
    pub topology: &'static str,
    /// Configured.
    pub sessions: usize,
    /// Configured; `null` where the population has one operation kind.
    pub mix: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Receipt {
    pub kind: &'static str,
    pub load_model: LoadModel,
    pub crate_version: &'static str,
    /// Timings on a shared host are not a baseline.
    pub evidence: &'static str,
    pub population: PopulationReceipt,
    pub marks: Marks,
    pub steps: Vec<StepReceipt>,
    pub knee: Knee,
    /// Resource with the largest measured aggregate queue wait at the knee.
    /// This ranks observed contention; it does not establish product causality.
    pub first_saturated_resource: Option<&'static str>,
    pub resource_attribution: &'static str,
}

/// One timing window includes its own bounded queue and task evidence.
#[derive(Debug, Serialize)]
pub struct StepReceipt {
    #[serde(flatten)]
    pub ledger: LedgerReport,
    pub diagnostics: crate::perf_support::async_operations::Receipt,
    pub host_samples: Vec<HostSample>,
    pub timing_qualified: bool,
    pub scheduler: std::collections::BTreeMap<String, Vec<f64>>,
}

#[derive(Debug, Serialize)]
pub struct HostSample {
    pub unix_ms: u128,
    pub load1: Option<f64>,
    pub cpu_psi: Option<String>,
}
impl HostSample {
    fn capture() -> Self {
        Self {
            unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
            load1: std::fs::read_to_string("/proc/loadavg")
                .ok()
                .and_then(|text| text.split_whitespace().next()?.parse().ok()),
            cpu_psi: std::fs::read_to_string("/proc/pressure/cpu").ok(),
        }
    }
}

#[derive(Debug, Serialize)]
struct StepLedger<'a> {
    offered_rate_per_second: f64,
    timestamp_unit: &'static str,
    operations: &'a [OperationRecord],
}

pub async fn run(args: &Args) -> Result<Receipt> {
    ensure!(args.sessions > 0, "the population needs a session");
    ensure!(!args.rates.is_empty(), "the sweep needs a rate");
    ensure!(
        args.backend != Backend::Postgres || args.population == Population::HighTraffic,
        "PostgreSQL uses --population high-traffic for matched single-core geometry"
    );
    ensure!(
        args.backend != Backend::Postgres || args.postgres_url.is_some(),
        "--backend postgres requires --postgres-url from a private kiln gate"
    );
    ensure!(
        args.rates.windows(2).all(|pair| pair[0] < pair[1]),
        "rates must ascend"
    );
    let schedules = args
        .rates
        .iter()
        .map(|rate| {
            let schedule = ArrivalSchedule {
                rate_per_second: *rate,
                operations: args.operations,
                max_in_flight: args.max_in_flight,
                drain_timeout: Duration::from_millis(args.drain_timeout_ms),
            };
            schedule.validate().map(|()| schedule)
        })
        .collect::<Result<Vec<_>>>()?;
    let mix = crate::runtime_perf::HighTrafficMix::parse(&args.mix)?;
    std::fs::create_dir(&args.store_dir)
        .with_context(|| format!("create fresh {}", args.store_dir.display()))?;

    let mut ledgers = Vec::with_capacity(schedules.len());
    let mut steps = Vec::with_capacity(schedules.len());
    for (step, schedule) in schedules.into_iter().enumerate() {
        let store_dir = args.store_dir.join(format!("step-{step}"));
        let population = populations::Opened::open(
            args.population,
            &store_dir,
            args.sessions,
            &mix,
            args.backend,
            args.postgres_url.as_deref(),
        )
        .await?;
        let capture = crate::perf_support::async_operations::Capture::step()?;
        let scheduler = crate::perf_support::scheduler::RuntimeSchedulerSample::capture();
        let mut host_samples = vec![HostSample::capture()];
        let ledger = {
            let prefix = format!("step{step}");
            let scheduled = ledger::run_scheduled(schedule, &prefix, population.service());
            tokio::pin!(scheduled);
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.tick().await;
            loop {
                tokio::select! {
                    ledger = &mut scheduled => break ledger,
                    _ = tick.tick() => host_samples.push(HostSample::capture()),
                }
            }
        };
        host_samples.push(HostSample::capture());
        let scheduler = scheduler.window_metric_samples(
            &crate::perf_support::scheduler::RuntimeSchedulerSample::capture(),
        );
        let diagnostics = capture.finish_receipt();
        let timing_qualified = host_samples
            .iter()
            .all(|sample| sample.load1.is_some_and(|load| load < 8.0) && sample.cpu_psi.is_some());
        let summary = ledger.summary(args.slowest);
        ensure!(
            summary.counts.scheduled
                == summary.counts.completed + summary.counts.errors + summary.counts.unfinished,
            "scheduled operation accounting does not conserve"
        );
        steps.push(StepReceipt {
            ledger: summary,
            diagnostics,
            host_samples,
            timing_qualified,
            scheduler,
        });
        population.close().await?;
        println!(
            "offered-load step {step}: {} operations/s scheduled, {} of {} completed",
            schedule.rate_per_second,
            ledger
                .operations
                .iter()
                .filter(|record| record.outcome == ledger::OperationOutcome::Completed)
                .count(),
            schedule.operations,
        );
        ledgers.push(ledger);
    }

    let summaries = steps
        .iter()
        .map(|step| step.ledger.clone())
        .collect::<Vec<_>>();
    let knee = ledger::knee(
        &summaries,
        KneeCriteria {
            min_achieved_to_offered: args.knee_min_achieved,
            max_p99_to_lowest_rate: args.knee_p99_ratio,
        },
    );
    let first_saturated_resource = knee
        .first_saturated_rate_per_second
        .and_then(|rate| {
            steps
                .iter()
                .find(|step| step.ledger.offered_rate.value == rate)
        })
        .and_then(|step| {
            step.diagnostics
                .queues
                .sites
                .iter()
                .filter(|(_, site)| site.queue_wait.sum_ns > 0)
                .max_by_key(|(_, site)| site.queue_wait.sum_ns)
                .map(|(name, _)| *name)
        });
    let receipt = Receipt {
        kind: "lash.offered-load",
        load_model: LoadModel::OfferedLoad,
        crate_version: env!("CARGO_PKG_VERSION"),
        evidence: "functional receipt; timings are not a baseline unless taken on a qualified quiet host",
        population: PopulationReceipt {
            name: args.population,
            engine: "lash-durable",
            store: match args.backend {
                Backend::Sqlite => "sqlite-file, one fresh store per step",
                Backend::Postgres => "postgres, one fresh database and pool set per step",
            },
            topology: match args.population {
                Population::HighTraffic => "one core submits and serves",
                Population::CrossWorker => {
                    "the submitting core serves no sessions; a child process serves"
                }
            },
            sessions: args.sessions,
            mix: (args.population == Population::HighTraffic).then(|| args.mix.clone()),
        },
        marks: populations::MARKS,
        steps,
        knee,
        first_saturated_resource,
        resource_attribution: "largest aggregate observed direct physical queue wait at the first saturated rate; a contention candidate, not a causal product bottleneck inference; null if no observed wait or knee; lock-containing SQL includes execution and is an upper bound on lock wait",
    };

    write_json(&args.out, &receipt)?;
    let ledger_out = args.ledger_out.clone().unwrap_or_else(|| {
        let mut name = args.out.file_name().unwrap_or_default().to_os_string();
        name.push(".ledger.json");
        args.out.with_file_name(name)
    });
    write_json(
        &ledger_out,
        &ledgers
            .iter()
            .map(|ledger| StepLedger {
                offered_rate_per_second: ledger.schedule.rate_per_second,
                timestamp_unit: "us since the step's window opened, generator monotonic clock",
                operations: &ledger.operations,
            })
            .collect::<Vec<_>>(),
    )?;
    println!("offered-load knee: {}", receipt.knee.verdict);
    Ok(receipt)
}

fn write_json(path: &std::path::Path, value: &impl Serialize) -> Result<()> {
    std::fs::write(path, format!("{}\n", serde_json::to_string_pretty(value)?))
        .with_context(|| format!("write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OFFERED-STEP-EVIDENCE: each measured step owns all real sends and a
    /// fresh queue budget; population setup and shutdown cannot fill it.
    #[tokio::test]
    async fn measured_steps_keep_real_send_evidence_and_conserve_arrivals() {
        let directory = tempfile::tempdir().expect("experiment directory");
        let args = Args {
            population: Population::HighTraffic,
            backend: Backend::Sqlite,
            postgres_url: None,
            rates: vec![100.0, 200.0],
            operations: 4,
            sessions: 2,
            max_in_flight: 4,
            drain_timeout_ms: 30_000,
            mix: "plain=1".into(),
            knee_min_achieved: 0.95,
            knee_p99_ratio: 3.0,
            slowest: 2,
            store_dir: directory.path().join("stores"),
            out: directory.path().join("receipt.json"),
            ledger_out: None,
        };
        let receipt = run(&args).await.expect("scheduled real sends");
        assert_eq!(receipt.steps.len(), 2);
        for step in &receipt.steps {
            let counts = &step.ledger.counts;
            assert_eq!(
                (
                    counts.scheduled,
                    counts.completed,
                    counts.errors,
                    counts.unfinished
                ),
                (4, 4, 0, 0)
            );
            let tasks = &step.diagnostics.tasks;
            assert_eq!(tasks.len(), 1);
            assert_eq!(tasks[0].operation_kind, "offered-load.send");
            assert_eq!(tasks[0].instrumented_futures_count, 4);
            assert!(tasks[0].polls_count >= 4);
            let queue = &step.diagnostics.queues;
            assert_eq!(queue.sites["generator.in_flight"].enqueued, 4);
            assert_eq!(queue.sites["generator.in_flight"].completed, 4);
            assert!(queue.sites.contains_key("sqlite.worker"));
            assert!(
                queue
                    .records
                    .iter()
                    .all(|record| record.enqueued_ns <= record.finished_ns)
            );
            let tail = step
                .ledger
                .intervals
                .scheduled_to_completed
                .as_ref()
                .expect("tail");
            assert_eq!(tail.n, 4);
            assert!(tail.p50 <= tail.p95 && tail.p95 <= tail.p99 && tail.p99 <= tail.max);
            assert!(step.host_samples.len() >= 2);
        }
    }
}
