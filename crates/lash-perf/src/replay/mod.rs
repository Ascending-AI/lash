//! Standard replayable sessions (FIG-5934): a committed corpus of workload
//! scripts, replayed end to end through the public facade with a scripted
//! provider and deterministic tool fixtures, each turn's counted costs
//! measured, and a per-merge gate over a fast subset.
//!
//! See `docs/perf/instruments/replay-workloads.md`.

pub mod control;
mod effect_phases;
pub mod gate;
pub mod meter;
pub mod provider;
pub mod receipt;
pub mod recorder;
pub mod runner;
pub mod script;
pub mod traffic;
mod workers;

pub use workers::python_worker_entry;

use std::path::PathBuf;

use anyhow::{Context, Result};

use receipt::{Identity, RECEIPT_FORMAT, ReplayReceipt};
use runner::{RunOptions, StoreChoice};

/// The store a replay runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum StoreKind {
    /// A SQLite file store set under `--store-dir`.
    Sqlite,
    /// An isolated database on `LASH_POSTGRES_DATABASE_URL`'s server.
    Postgres,
}

/// `lash-perf replay`: run workloads and write their receipt.
#[derive(Debug, clap::Args)]
pub struct ReplayArgs {
    /// Corpus workloads or script paths; default the whole corpus.
    #[arg(value_delimiter = ',')]
    pub workloads: Vec<String>,
    /// Run only the first this many turns of each workload.
    #[arg(long)]
    pub turns: Option<usize>,
    #[arg(long, value_enum, default_value = "sqlite")]
    pub store: StoreKind,
    /// Where SQLite store sets live (one directory per workload).
    #[arg(long)]
    pub store_dir: PathBuf,
    /// The PostgreSQL server for `--store postgres`.
    #[arg(long, env = "LASH_POSTGRES_DATABASE_URL", hide_env_values = true)]
    pub postgres_url: Option<String>,
    /// Where the JSON receipt lands.
    #[arg(long)]
    pub out: PathBuf,
    /// Append each workload's wall time and counted results to this
    /// duration history (main's trend).
    #[arg(long)]
    pub history: Option<PathBuf>,
    /// SQLite shape × store-origin sidecar (one selected workload).
    #[arg(long)]
    pub sql_shapes: Option<PathBuf>,
    /// Attribute physical SQLite writes and VM transport kinds in the receipt.
    #[arg(long)]
    pub traffic: bool,
    /// Bounded host-clock effect/worker phase JSONL (one SQLite workload).
    #[arg(long, conflicts_with = "history")]
    pub effect_phases: Option<PathBuf>,
    /// Keep worker identity/terminal receipts but omit phase timestamps for calibration.
    #[arg(long, requires = "effect_phases")]
    pub effect_phases_off: bool,
    #[command(flatten)]
    pub heap: ReplayHeapArgs,
}

/// One contiguous host heap window. Setup and earlier history are excluded.
#[derive(Clone, Debug, Default, clap::Args)]
pub struct ReplayHeapArgs {
    #[arg(long, requires = "profile_turn")]
    pub dhat_out: Option<PathBuf>,
    #[arg(long, requires = "dhat_out")]
    pub dhat_frames: Option<usize>,
    /// One-based first turn to profile; requires a dhat-heap build.
    #[arg(long, requires = "dhat_out")]
    pub profile_turn: Option<usize>,
    /// Inclusive last turn (default: profile-turn).
    #[arg(long, requires = "profile_turn")]
    pub profile_through_turn: Option<usize>,
}

/// `lash-perf replay-gate`: the per-merge counted gate.
#[derive(Debug, clap::Args)]
pub struct GateArgs {
    /// Where the subset's SQLite store sets live.
    #[arg(long)]
    pub store_dir: PathBuf,
    /// Where the JSON receipt lands.
    #[arg(long)]
    pub out: PathBuf,
    /// Write the run's medians as the new baseline here instead of checking
    /// the committed one (the re-bless command).
    #[arg(long)]
    pub bless: Option<PathBuf>,
}

/// Run `workloads` in order, rewriting the receipt at `out` after each one so
/// a later failure keeps the finished workloads' evidence.
async fn run_workloads(
    workloads: &[(script::Workload, Option<usize>)],
    store: &StoreChoice,
    out: &std::path::Path,
    sql_shapes: Option<&std::path::Path>,
    heap: &ReplayHeapArgs,
    traffic: bool,
    effect_phases: Option<(&std::path::Path, bool)>,
) -> Result<ReplayReceipt> {
    let mut identity = Identity::current();
    identity.sql_shapes = sql_shapes.is_some();
    identity.traffic = traffic;
    identity.heap_window = heap
        .profile_turn
        .map(|first| [first, heap.profile_through_turn.unwrap_or(first)]);
    let mut receipt = ReplayReceipt {
        format: RECEIPT_FORMAT.to_owned(),
        identity,
        workloads: Vec::with_capacity(workloads.len()),
    };
    for (workload, turns) in workloads {
        let options = RunOptions {
            identity: receipt.identity.clone(),
            store: store.clone(),
            turns: *turns,
            worker_profile_dir: out.with_extension("worker-heaps").join(&workload.name),
            sql_shapes: sql_shapes.map(std::path::Path::to_path_buf),
            heap: heap.clone(),
            traffic,
            effect_phases: effect_phases.map(|(path, _)| path.to_path_buf()),
            effect_phases_off: effect_phases.is_some_and(|(_, off)| off),
        };
        eprintln!(
            "replay: {} ({} turns x {} session(s))",
            workload.name,
            turns.unwrap_or(workload.turn_count()),
            workload.sessions
        );
        receipt.workloads.push(
            runner::run(workload, &options)
                .await
                .with_context(|| format!("workload `{}`", workload.name))?,
        );
        write_receipt(&receipt, out)?;
    }
    Ok(receipt)
}

fn write_receipt(receipt: &ReplayReceipt, out: &std::path::Path) -> Result<()> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, serde_json::to_string_pretty(receipt)? + "\n")
        .with_context(|| format!("write {}", out.display()))
}

/// Run `lash-perf replay`.
pub async fn run(args: &ReplayArgs) -> Result<()> {
    let names = if args.workloads.is_empty() {
        script::CORPUS
            .iter()
            .map(|(name, _)| (*name).to_owned())
            .collect()
    } else {
        args.workloads.clone()
    };
    let workloads = names
        .iter()
        .map(|name| Ok((script::resolve(name)?, args.turns)))
        .collect::<Result<Vec<_>>>()?;
    if args.sql_shapes.is_some() || args.heap.dhat_out.is_some() || args.effect_phases.is_some() {
        anyhow::ensure!(
            workloads.len() == 1,
            "attribution selects exactly one workload"
        );
    }
    anyhow::ensure!(
        (args.sql_shapes.is_none() && !args.traffic && args.effect_phases.is_none())
            || args.store == StoreKind::Sqlite,
        "--sql-shapes and --traffic are SQLite only"
    );
    if let Some(first) = args.heap.profile_turn {
        let end = args.heap.profile_through_turn.unwrap_or(first);
        let (workload, limit) = &workloads[0];
        let count = limit
            .unwrap_or(workload.turn_count())
            .min(workload.turn_count());
        anyhow::ensure!(
            first > 0 && end >= first && end <= count,
            "profile turn range must lie within the replay"
        );
        anyhow::ensure!(
            !workload
                .compact_every
                .is_some_and(|every| (first..end).any(|t| t % every == 0)),
            "heap range cannot include between-turn compaction; select one side"
        );
        anyhow::ensure!(
            cfg!(feature = "dhat-heap"),
            "--dhat-out requires a dhat-heap build"
        );
    }
    let store = match args.store {
        StoreKind::Sqlite => StoreChoice::SqliteFile(args.store_dir.clone()),
        StoreKind::Postgres => StoreChoice::Postgres(
            args.postgres_url
                .clone()
                .context("--store postgres needs LASH_POSTGRES_DATABASE_URL or --postgres-url")?,
        ),
    };
    let receipt = run_workloads(
        &workloads,
        &store,
        &args.out,
        args.sql_shapes.as_deref(),
        &args.heap,
        args.traffic,
        args.effect_phases
            .as_deref()
            .map(|path| (path, args.effect_phases_off)),
    )
    .await?;
    print!("{}", receipt::table(&receipt));
    if let Some(history) = &args.history {
        crate::runtime_perf::record_replay_receipt(&receipt, history)?;
    }
    println!("replay: receipt {}", args.out.display());
    Ok(())
}

/// Run `lash-perf replay-gate`; the exit code is the verdict.
pub async fn run_gate(args: &GateArgs) -> Result<i32> {
    let budgets = gate::Budgets::committed()?;
    let corpus = script::corpus()?;
    let workloads = budgets
        .subset
        .iter()
        .map(|entry| {
            corpus
                .get(&entry.workload)
                .cloned()
                .map(|workload| (workload, entry.turns))
                .with_context(|| format!("the gate names unknown workload `{}`", entry.workload))
        })
        .collect::<Result<Vec<_>>>()?;
    let receipt = run_workloads(
        &workloads,
        &StoreChoice::SqliteFile(args.store_dir.clone()),
        &args.out,
        None,
        &ReplayHeapArgs::default(),
        false,
        None,
    )
    .await?;
    print!("{}", receipt::table(&receipt));
    if let Some(path) = &args.bless {
        let baseline = gate::Baseline::bless(&receipt)?;
        std::fs::write(path, serde_json::to_string_pretty(&baseline)? + "\n")
            .with_context(|| format!("write {}", path.display()))?;
        println!("replay gate: blessed baseline {}", path.display());
        return Ok(0);
    }
    let verdict = gate::evaluate(&receipt, &budgets, &gate::Baseline::committed()?);
    for line in &verdict.known_breaches {
        println!("replay gate: KNOWN BREACH {line}");
    }
    for line in &verdict.improvements {
        println!("replay gate: improved {line}");
    }
    for line in &verdict.violations {
        println!("replay gate: VIOLATION {line}");
    }
    println!(
        "replay gate: {} ({} violation(s), {} known breach(es))",
        if verdict.passed() { "passed" } else { "failed" },
        verdict.violations.len(),
        verdict.known_breaches.len()
    );
    Ok(if verdict.passed() { 0 } else { 1 })
}
