//! Independent 1.0 boundary workloads. Receipts prove operations; elapsed time
//! is diagnostic until collected on a qualified quiet host.
mod attachments;
mod facade;
mod observation;
mod seeded;
mod tokens;
mod waves;
mod workers;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use lash_sansio::sync::MutexExt;
use serde::Serialize;

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum Case {
    WireSlots,
    TokenHealthy,
    TokenExpiring,
    TokenRejected,
    RootRedrive,
    ParkedTakeover,
    SqliteProcesses,
    PgFacade,
    TypedHistory,
    ProcessLifecycle,
    SeededPlan,
    /// One served node stays alive across operations waves of callers turns.
    PersistentNodeWaves,
    ProcessDispatcher,
    ProcessFeeds,
    ProcessBurst,
    ProcessConvergence,
    ProcessReconcile,
    ProcessRoster,
    SessionReplay,
    SessionResume,
    TraceSinkOmitted,
    TraceSinkCaptured,
    TraceSinkCustom,
    TraceSinkOtel,
    TraceSinkSlow,
    OverlayFold,
    OverlayAttribution,
}

/// Run exactly one population so setup and boundary costs cannot be conflated.
#[derive(Debug, clap::Args)]
pub struct Args {
    #[arg(long, value_enum)]
    pub case: Case,
    #[arg(long)]
    pub out: PathBuf,
    /// A fresh directory; refusing reuse protects existing product stores.
    #[arg(long)]
    pub store_dir: PathBuf,
    #[arg(long, default_value_t = 8)]
    pub operations: usize,
    #[arg(long, default_value_t = 4)]
    pub callers: usize,
    /// Private, baseline-initialized PG18 database; never the sketch schema.
    #[arg(long)]
    pub postgres_url: Option<String>,
    #[arg(long, default_value = "smoke-v1")]
    pub workload: String,
    /// Write concrete future sizes collected before tracing and boxing.
    #[arg(long)]
    pub future_out: Option<PathBuf>,
    #[arg(long, default_value_t = 20, requires = "future_out")]
    pub future_top: usize,
    /// Write a dhat heap profile of the population; needs the dhat-heap build.
    #[arg(long, value_name = "OUT.json")]
    pub dhat_out: Option<PathBuf>,
    /// Trim dhat backtraces to this many frames.
    #[arg(long, value_name = "FRAMES", requires = "dhat_out")]
    pub dhat_frames: Option<usize>,
    /// Tokio worker stack size for the population's runtime.
    #[arg(long, value_name = "BYTES")]
    pub worker_stack_bytes: Option<usize>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Phase {
    pub boundary: String,
    pub operations: usize,
    pub elapsed_us: u128,
}

/// One boundary's per-operation latency over its recorded intervals.
#[derive(Clone, Debug, Serialize)]
pub struct Latency {
    pub boundary: String,
    pub samples: usize,
    pub p50_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
    pub max_us: f64,
    pub mean_us: f64,
}

/// Operations over one wall-clock window. Concurrent intervals overlap, so a
/// rate is stated only for a window the workload measured as one.
#[derive(Clone, Debug, Serialize)]
pub struct Throughput {
    pub window: String,
    pub operations: usize,
    pub elapsed_us: u128,
    pub per_second: f64,
}

/// Allocator counters over the population, in the binary's allocation mode.
#[derive(Clone, Debug, Serialize)]
pub struct Allocations {
    pub mode: &'static str,
    pub allocations: usize,
    pub deallocations: usize,
    pub reallocations: usize,
    pub bytes_allocated: usize,
    pub bytes_deallocated: usize,
}

#[derive(Clone, Debug, Default)]
struct Meter {
    phases: Arc<Mutex<Vec<Phase>>>,
    /// High-rate intervals in nanoseconds, summarized in the receipt and
    /// never listed.
    samples: Arc<Mutex<BTreeMap<String, Vec<u64>>>>,
    windows: Arc<Mutex<Vec<Throughput>>>,
}
impl Meter {
    fn record(&self, boundary: &str, operations: usize, start: Instant) {
        self.phases.lock_recover().push(Phase {
            boundary: boundary.into(),
            operations,
            elapsed_us: start.elapsed().as_micros(),
        });
    }
    fn count(&self, boundary: &str) -> usize {
        self.phases
            .lock_recover()
            .iter()
            .filter(|p| p.boundary == boundary)
            .map(|p| p.operations)
            .sum()
    }
    fn sample(&self, boundary: &str, elapsed: Duration) {
        self.samples
            .lock_recover()
            .entry(boundary.into())
            .or_default()
            .push(u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX));
    }
    fn window(&self, window: &str, operations: usize, start: Instant) {
        let elapsed = start.elapsed();
        self.windows.lock_recover().push(Throughput {
            window: window.into(),
            operations,
            elapsed_us: elapsed.as_micros(),
            per_second: crate::perf_support::time::round3(
                operations as f64 / elapsed.as_secs_f64().max(f64::EPSILON),
            ),
        });
    }
    fn latency(&self) -> Vec<Latency> {
        let mut by_boundary = self.samples.lock_recover().clone();
        for phase in self.phases.lock_recover().iter() {
            if phase.operations > 0 {
                let each = phase.elapsed_us * 1000 / phase.operations as u128;
                by_boundary
                    .entry(phase.boundary.clone())
                    .or_default()
                    .push(u64::try_from(each).unwrap_or(u64::MAX));
            }
        }
        by_boundary
            .into_iter()
            .map(|(boundary, values)| {
                let samples = values.len();
                let stats = crate::perf_support::metrics::percentile_summary(
                    values
                        .into_iter()
                        .map(|nanos| nanos as f64 / 1000.0)
                        .collect(),
                );
                Latency {
                    boundary,
                    samples,
                    p50_us: stats.p50,
                    p95_us: stats.p95,
                    p99_us: stats.p99,
                    max_us: stats.max,
                    mean_us: stats.mean,
                }
            })
            .collect()
    }
}

fn allocator_stats() -> stats_alloc::Stats {
    crate::GLOBAL_ALLOCATOR.stats()
}

#[derive(Debug, Serialize)]
pub struct Receipt {
    pub kind: &'static str,
    pub case: String,
    pub surface: &'static str,
    pub store: &'static str,
    pub population: usize,
    pub phases: Vec<Phase>,
    pub latency: Vec<Latency>,
    pub throughput: Vec<Throughput>,
    /// Filled by [`run`] over the whole population, setup included.
    pub allocations: Option<Allocations>,
    pub stack_profile: Option<crate::perf_support::stack::StackProfile>,
    pub counters: serde_json::Value,
    pub evidence: serde_json::Value,
}
impl Receipt {
    fn new(
        case: Case,
        surface: &'static str,
        store: &'static str,
        population: usize,
        meter: &Meter,
        evidence: serde_json::Value,
    ) -> Self {
        Self::measured(
            case,
            surface,
            store,
            population,
            meter,
            serde_json::json!({}),
            evidence,
        )
    }

    fn measured(
        case: Case,
        surface: &'static str,
        store: &'static str,
        population: usize,
        meter: &Meter,
        counters: serde_json::Value,
        evidence: serde_json::Value,
    ) -> Self {
        // Each guard ends with its statement; `latency` locks the phases again.
        let phases = meter.phases.lock_recover().clone();
        let throughput = meter.windows.lock_recover().clone();
        Self {
            kind: "lash.boundary-workload",
            case: format!("{case:?}"),
            surface,
            store,
            population,
            phases,
            latency: meter.latency(),
            throughput,
            allocations: None,
            stack_profile: None,
            counters,
            evidence,
        }
    }
}

pub async fn run(args: &Args) -> Result<Receipt> {
    ensure!(
        args.operations > 0 && args.callers > 0,
        "population must be positive"
    );
    std::fs::create_dir(&args.store_dir)?;
    let before = allocator_stats();
    let mut receipt = match args.case {
        Case::WireSlots => attachments::run(args.operations).await?,
        Case::TokenHealthy | Case::TokenExpiring | Case::TokenRejected => {
            tokens::run(args.case, args.operations, args.callers).await?
        }
        Case::SqliteProcesses => workers::run(args).await?,
        Case::SeededPlan => seeded::run(args).await?,
        Case::PersistentNodeWaves => waves::run(args).await?,
        Case::RootRedrive
        | Case::ParkedTakeover
        | Case::PgFacade
        | Case::TypedHistory
        | Case::ProcessLifecycle => facade::run(args).await?,
        _ => Box::pin(observation::run(args)).await?,
    };
    let after = allocator_stats();
    receipt.allocations = Some(Allocations {
        mode: crate::ALLOCATION_MODE,
        allocations: after.allocations - before.allocations,
        deallocations: after.deallocations - before.deallocations,
        reallocations: after.reallocations - before.reallocations,
        bytes_allocated: after.bytes_allocated - before.bytes_allocated,
        bytes_deallocated: after.bytes_deallocated - before.bytes_deallocated,
    });
    receipt.stack_profile = args
        .worker_stack_bytes
        .map(|bytes| crate::perf_support::stack::StackProfile::capture(Some(bytes), None));
    write_receipt(&args.out, &receipt)?;
    Ok(receipt)
}

fn write_receipt(path: &Path, receipt: &Receipt) -> Result<()> {
    std::fs::write(
        path,
        format!("{}\n", serde_json::to_string_pretty(receipt)?),
    )?;
    Ok(())
}

pub use workers::{WorkerArgs, run_worker};

#[cfg(test)]
mod tests;
