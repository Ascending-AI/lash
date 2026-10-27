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
pub mod oracle;
pub mod provider;
pub mod receipt;
pub mod recorder;
pub mod runner;
pub mod script;
mod sql_work;
pub mod traffic;
pub mod turn_profile;
mod workers;

pub use workers::python_worker_entry;

mod capabilities;
pub(crate) use capabilities::HostTools;

use std::path::PathBuf;

use anyhow::{Context, Result};

use receipt::{Identity, RECEIPT_FORMAT, ReplayReceipt};
use runner::{RunOptions, StoreChoice};

/// SQLite embeds its canonical path in package publication identities. Keep
/// that input's byte length constant, then preserve the closed stores beside
/// the receipt, outside every measured window.
struct GateStores(PathBuf);

impl GateStores {
    fn create() -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        let path = PathBuf::from("/tmp").join(format!("lash-counted-{}", uuid::Uuid::new_v4()));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }

    fn preserve(&self, destination: &std::path::Path) -> Result<()> {
        fn copy(source: &std::path::Path, destination: &std::path::Path) -> Result<()> {
            std::fs::create_dir_all(destination)?;
            for entry in std::fs::read_dir(source)? {
                let entry = entry?;
                let target = destination.join(entry.file_name());
                if entry.file_type()?.is_dir() {
                    copy(&entry.path(), &target)?;
                } else {
                    std::fs::copy(entry.path(), target)?;
                }
            }
            Ok(())
        }
        copy(&self.0, destination)
    }
}

impl Drop for GateStores {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

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
    /// SQL execution joins plus shape × store-origin sidecar (one workload).
    #[arg(long)]
    pub sql_shapes: Option<PathBuf>,
    /// Attribute physical SQLite writes and VM transport kinds in the receipt.
    #[arg(long)]
    pub traffic: bool,
    #[arg(long)]
    pub turn_report: Option<PathBuf>,
    /// Count qualified hardware instructions on this local host, including worker receipts.
    #[arg(long)]
    pub instructions: bool,
    /// Count worker allocator deltas through the harness worker.
    #[arg(long)]
    pub worker_resources: bool,
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
    /// Explicit counted reference for a local injection/control experiment.
    #[arg(long, conflicts_with = "bless")]
    pub baseline: Option<PathBuf>,
    /// One raw population for the gate coordinator.
    #[arg(long, hide = true, conflicts_with = "bless")]
    pub capture: bool,
}

#[derive(Default)]
struct Observers<'a> {
    sql_shapes: Option<&'a std::path::Path>,
    turn_report: Option<&'a std::path::Path>,
    worker_resources: bool,
    instructions: bool,
    traffic: bool,
    effect_phases: Option<(&'a std::path::Path, bool)>,
}

/// Run `workloads` in order, rewriting the receipt at `out` after each one so
/// a later failure keeps the finished workloads' evidence.
async fn run_workloads(
    workloads: &[(script::Workload, Option<usize>)],
    store: &StoreChoice,
    out: &std::path::Path,
    heap: &ReplayHeapArgs,
    observers: Observers<'_>,
) -> Result<ReplayReceipt> {
    let Observers {
        sql_shapes,
        turn_report,
        worker_resources,
        instructions,
        traffic,
        effect_phases,
    } = observers;
    let mut identity = Identity::current();
    identity.sql_shapes = sql_shapes.is_some();
    identity.turn_profile = turn_report.is_some();
    identity.worker_resources = worker_resources;
    identity.instructions = instructions;
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
            worker_resources,
            instructions,
            store: match store {
                StoreChoice::CountedSqliteFile(dir) if workload.name != "ts-codemode" => {
                    StoreChoice::SqliteFile(dir.clone())
                }
                _ => store.clone(),
            },
            turns: *turns,
            worker_profile_dir: out.with_extension("worker-heaps").join(&workload.name),
            sql_shapes: sql_shapes.map(std::path::Path::to_path_buf),
            heap: heap.clone(),
            turn_report: turn_report.map(std::path::Path::to_path_buf),
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
        let result = if matches!(store, StoreChoice::CountedSqliteFile(_)) {
            let workload = workload.clone();
            let counted = matches!(options.store, StoreChoice::CountedSqliteFile(_));
            std::thread::spawn(move || {
                let _identities =
                    counted.then(lash_core_store::environment::sequential_identities_for_testing);
                let mut runtime = if counted {
                    tokio::runtime::Builder::new_current_thread()
                } else {
                    let mut runtime = tokio::runtime::Builder::new_multi_thread();
                    runtime.worker_threads(8);
                    runtime
                };
                // Dropping this runtime finishes detached observers from this
                // workload before the next workload's process-wide counters.
                runtime
                    .enable_all()
                    .build()?
                    .block_on(runner::run(&workload, &options))
            })
            .join()
            .map_err(|_| anyhow::anyhow!("the counted replay thread panicked"))?
        } else {
            runner::run(workload, &options).await
        };
        receipt
            .workloads
            .push(result.with_context(|| format!("workload `{}`", workload.name))?);
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
    if args.sql_shapes.is_some()
        || args.turn_report.is_some()
        || args.heap.dhat_out.is_some()
        || args.effect_phases.is_some()
    {
        anyhow::ensure!(
            workloads.len() == 1,
            "attribution selects exactly one workload"
        );
    }
    anyhow::ensure!(
        (!args.traffic && args.effect_phases.is_none()) || args.store == StoreKind::Sqlite,
        "--traffic and --effect-phases are SQLite only"
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
        &args.heap,
        Observers {
            sql_shapes: args.sql_shapes.as_deref(),
            turn_report: args.turn_report.as_deref(),
            worker_resources: args.worker_resources || args.instructions,
            instructions: args.instructions,
            traffic: args.traffic,
            effect_phases: args
                .effect_phases
                .as_deref()
                .map(|path| (path, args.effect_phases_off)),
        },
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
    let mut receipts = Vec::new();
    if !args.capture {
        // A scheduled gate starts a fresh process. Calibrating within this
        // runtime misses process-local allocator/cache variation.
        for run in 0..5 {
            let out = args.out.with_extension(format!("{run}.json"));
            match std::fs::remove_file(&out) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            let captured = std::process::Command::new(std::env::current_exe()?)
                .arg("replay-gate")
                .arg("--capture")
                .arg("--store-dir")
                .arg(args.store_dir.join(format!("run-{run}")))
                .arg("--out")
                .arg(&out)
                .output()?;
            let mut log = captured.stdout;
            log.extend_from_slice(&captured.stderr);
            std::fs::write(args.out.with_extension(format!("{run}.log")), log)?;
            // Failed or partial producers never enter the five-run statistic.
            anyhow::ensure!(
                captured.status.success() && out.exists(),
                "calibration child {run} did not produce a receipt"
            );
            let receipt: ReplayReceipt = serde_json::from_slice(&std::fs::read(out)?)?;
            anyhow::ensure!(
                receipt.workloads.len() == workloads.len(),
                "incomplete calibration population"
            );
            for (script, turns) in &workloads {
                let row = receipt
                    .workloads
                    .iter()
                    .find(|w| w.workload == script.name)
                    .context("missing calibration workload")?;
                anyhow::ensure!(
                    row.script_digest == script.digest()
                        && row.turns
                            == turns
                                .unwrap_or(script.turn_count())
                                .min(script.turn_count())
                        && row.sessions == script.sessions,
                    "calibration workload geometry changed"
                );
            }
            receipts.push(receipt);
        }
    } else {
        let stores = GateStores::create()?;
        receipts.push(
            run_workloads(
                &workloads,
                &StoreChoice::CountedSqliteFile(stores.0.clone()),
                &args.out,
                &ReplayHeapArgs::default(),
                Observers {
                    worker_resources: true,
                    ..Observers::default()
                },
            )
            .await?,
        );
        stores.preserve(&args.store_dir)?;
    }
    let raw = receipts.last().context("no replay runs")?;
    if args.capture {
        write_receipt(raw, &args.out)?;
        return Ok(0);
    }
    let population = gate::candidate_population(&receipts)?;
    let receipt = &population;
    write_receipt(receipt, &args.out)?;
    print!("{}", receipt::table(receipt));
    if let Some(path) = &args.bless {
        let mut baseline = gate::Baseline::calibrate(&receipts)?;
        let old = gate::Baseline::committed()?;
        baseline.tighten_against(&old)?;
        std::fs::write(path, serde_json::to_string_pretty(&baseline)? + "\n")
            .with_context(|| format!("write {}", path.display()))?;
        println!("replay gate: blessed baseline {}", path.display());
        return Ok(0);
    }
    let baseline = match &args.baseline {
        Some(path) => gate::Baseline::parse(&std::fs::read_to_string(path)?)?,
        None => gate::Baseline::committed()?,
    };
    let verdict = gate::evaluate(receipt, &budgets, &baseline);
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
