//! Independent 1.0 boundary workloads. Receipts prove operations; elapsed time
//! is diagnostic until collected on a qualified quiet host.
mod attachments;
mod facade;
mod seeded;
mod tokens;
mod workers;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
}

#[derive(Clone, Debug, Serialize)]
pub struct Phase {
    pub boundary: String,
    pub operations: usize,
    pub elapsed_us: u128,
}

#[derive(Clone, Debug, Default)]
struct Meter(Arc<Mutex<Vec<Phase>>>);
impl Meter {
    fn record(&self, boundary: &str, operations: usize, start: Instant) {
        self.0.lock_recover().push(Phase {
            boundary: boundary.into(),
            operations,
            elapsed_us: start.elapsed().as_micros(),
        });
    }
    fn count(&self, boundary: &str) -> usize {
        self.0
            .lock_recover()
            .iter()
            .filter(|p| p.boundary == boundary)
            .map(|p| p.operations)
            .sum()
    }
}

#[derive(Debug, Serialize)]
pub struct Receipt {
    pub kind: &'static str,
    pub case: String,
    pub surface: &'static str,
    pub store: &'static str,
    pub population: usize,
    pub phases: Vec<Phase>,
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
        Self {
            kind: "lash.boundary-workload",
            case: format!("{case:?}"),
            surface,
            store,
            population,
            phases: meter.0.lock_recover().clone(),
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
    let receipt = match args.case {
        Case::WireSlots => attachments::run(args.operations).await?,
        Case::TokenHealthy | Case::TokenExpiring | Case::TokenRejected => {
            tokens::run(args.case, args.operations, args.callers).await?
        }
        Case::SqliteProcesses => workers::run(args).await?,
        Case::SeededPlan => seeded::run(args).await?,
        _ => facade::run(args).await?,
    };
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
