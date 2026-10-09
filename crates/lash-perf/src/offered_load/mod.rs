//! Offered-load measurement: scheduled arrivals against real populations.
//!
//! `lash-perf offered-load` sweeps one population across ascending arrival
//! rates. Every step opens a fresh SQLite store, schedules a finite number
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
    /// session population on a SQLite file.
    HighTraffic,
    /// Send-to-completion through a submitting core that serves no sessions
    /// and a child durable node, sharing one SQLite file.
    CrossWorker,
}

#[derive(Debug, clap::Args)]
pub struct Args {
    #[arg(long, value_enum)]
    pub population: Population,
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
    /// A fresh directory for the per-step SQLite stores.
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
    pub steps: Vec<LedgerReport>,
    pub knee: Knee,
}

#[derive(Debug, Serialize)]
struct StepLedger<'a> {
    offered_rate_per_second: f64,
    timestamp_unit: &'static str,
    operations: &'a [OperationRecord],
}

pub async fn run(args: &Args) -> Result<Receipt> {
    ensure!(args.sessions > 0, "the population needs a session");
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
    for (step, schedule) in schedules.into_iter().enumerate() {
        let store_dir = args.store_dir.join(format!("step-{step}"));
        let population =
            populations::Opened::open(args.population, &store_dir, args.sessions, &mix).await?;
        let ledger =
            ledger::run_scheduled(schedule, &format!("step{step}"), population.service()).await;
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

    let steps: Vec<LedgerReport> = ledgers
        .iter()
        .map(|ledger| ledger.summary(args.slowest))
        .collect();
    let knee = ledger::knee(
        &steps,
        KneeCriteria {
            min_achieved_to_offered: args.knee_min_achieved,
            max_p99_to_lowest_rate: args.knee_p99_ratio,
        },
    );
    let receipt = Receipt {
        kind: "lash.offered-load",
        load_model: LoadModel::OfferedLoad,
        crate_version: env!("CARGO_PKG_VERSION"),
        evidence: "functional receipt; timings are not a baseline unless taken on a qualified quiet host",
        population: PopulationReceipt {
            name: args.population,
            engine: "lash-durable",
            store: "sqlite-file, one fresh store per step",
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
