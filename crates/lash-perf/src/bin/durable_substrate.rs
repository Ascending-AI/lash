//! The durable substrate against L12a's pre-deletion baseline (L12b): L12a's
//! turn, resume, parked-process and concurrency shapes, and the
//! substrate's own cold-resume, snapshot, idle and store measurements, on
//! the production session activation and node runner over SQLite or
//! PostgreSQL. Only the protocol, the model and the tool bodies are the
//! bench's; see `durable_substrate/README.md`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use clap::Parser;
use lash_core_execution::DurableSettings;
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};
use std::alloc::System;

#[path = "durable_substrate/cells.rs"]
mod cells;
#[path = "durable_substrate/deploy.rs"]
mod deploy;
#[path = "durable_substrate/process.rs"]
mod process;
#[path = "durable_substrate/processes.rs"]
mod processes;
#[path = "durable_substrate/recorder.rs"]
mod recorder;
#[path = "durable_substrate/store_bench.rs"]
mod store_bench;
#[path = "durable_substrate/support.rs"]
mod support;
#[path = "durable_substrate/turn.rs"]
mod turn;
#[path = "durable_substrate/turns.rs"]
mod turns;

use deploy::{Database, Deployment, PoolSize};
use support::Report;
use turn::Script;

// Match the runtime performance harness's default allocator instrumentation.
#[global_allocator]
static GLOBAL_ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

#[derive(Parser)]
#[command(about = "Measure the durable substrate against the L12a baseline")]
struct Args {
    /// `sqlite` (a fresh file per case under --sqlite-dir) or `postgres`
    /// (LASH_POSTGRES_DATABASE_URL, provisioned with the 1.0 schema).
    #[arg(long)]
    store: String,
    #[arg(long, env = "LASH_POSTGRES_DATABASE_URL", hide_env_values = true)]
    database_url: Option<String>,
    #[arg(long)]
    sqlite_dir: Option<PathBuf>,
    /// Serving nodes (PostgreSQL only above one).
    #[arg(long, default_value_t = 1)]
    nodes: usize,
    /// Cases, by name; see `cases()`.
    #[arg(long = "case", required = true)]
    cases: Vec<String>,
    /// Batches (or resume samples) per case.
    #[arg(long, default_value_t = 10)]
    samples: usize,
    /// How long a parked process stays parked before its completion.
    #[arg(long, default_value_t = 30)]
    park_seconds: u64,
    /// The idle window of the idle cases.
    #[arg(long, default_value_t = 30)]
    idle_seconds: u64,
    /// Each PostgreSQL node's pool: L12a's runtime pool was 4..32, and
    /// 4..256 for the 100-session case.
    #[arg(long, default_value_t = 32)]
    pool_max: u32,
    #[arg(long, default_value_t = 4)]
    pool_min: u32,
    /// The store cases' node counts and run length.
    #[arg(long, default_value = "1,4,16", value_delimiter = ',')]
    store_nodes: Vec<usize>,
    #[arg(long, default_value_t = 3)]
    store_seconds: u64,
    #[arg(long, default_value_t = 128)]
    wake_events: usize,
    #[arg(long)]
    out: PathBuf,
}

/// A named scenario.
pub struct Case {
    name: String,
    script: Script,
    kind: Kind,
}

enum Kind {
    Rounds { sessions: usize },
    Resume { prior_turns: usize },
    Cell,
    Parked,
    HotWaits { waits: usize },
    Idle { actors: usize },
    Store,
}

fn rounds(rounds: usize, tools_per_round: usize) -> Script {
    Script {
        rounds,
        tools_per_round,
        ..Script::default()
    }
}

fn case(name: &str) -> Result<Case> {
    let (kind, script) = match name {
        "rounds-1" => (Kind::Rounds { sessions: 1 }, rounds(1, 1)),
        "rounds-5" => (Kind::Rounds { sessions: 1 }, rounds(5, 1)),
        "rounds-20" => (Kind::Rounds { sessions: 1 }, rounds(20, 1)),
        "concurrent-1" => (Kind::Rounds { sessions: 1 }, rounds(5, 3)),
        "concurrent-10" => (Kind::Rounds { sessions: 10 }, rounds(5, 3)),
        "concurrent-100" => (Kind::Rounds { sessions: 100 }, rounds(5, 3)),
        "resume-1" => (Kind::Resume { prior_turns: 0 }, rounds(1, 1)),
        "resume-5" => (Kind::Resume { prior_turns: 0 }, rounds(5, 1)),
        "resume-20" => (Kind::Resume { prior_turns: 0 }, rounds(20, 1)),
        "prior-0" => (Kind::Resume { prior_turns: 0 }, rounds(1, 1)),
        "prior-10" => (Kind::Resume { prior_turns: 10 }, rounds(1, 1)),
        "prior-100" => (Kind::Resume { prior_turns: 100 }, rounds(1, 1)),
        "prior-300" => (Kind::Resume { prior_turns: 300 }, rounds(1, 1)),
        "cell-0" | "cell-1024" | "cell-16384" => {
            let payload = name.trim_start_matches("cell-").parse()?;
            (
                Kind::Cell,
                Script {
                    cell_calls: 10,
                    cell_payload: payload,
                    ..Script::default()
                },
            )
        }
        "parked-process" => (Kind::Parked, Script::default()),
        "process-waits-10" => (Kind::HotWaits { waits: 10 }, Script::default()),
        "idle-0" => (Kind::Idle { actors: 0 }, Script::default()),
        "idle-1000" => (Kind::Idle { actors: 1000 }, Script::default()),
        "store" => (Kind::Store, Script::default()),
        other => bail!("no case `{other}`"),
    };
    Ok(Case {
        name: name.to_owned(),
        script,
        kind,
    })
}

/// One case's run: its database, nodes, samples and report.
pub struct Run<'a> {
    report: &'a Report,
    database: Database,
    nodes: usize,
    samples: usize,
    pool: PoolSize,
}

impl Run<'_> {
    fn deployment(&self) -> Deployment {
        Deployment::new(self.database.clone(), DurableSettings::default(), self.pool)
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let report = Report::open(&args.out)?;
    report.write(
        "run",
        &serde_json::json!({
            "store": args.store,
            "nodes": args.nodes,
            "cases": args.cases,
            "samples": args.samples,
            "pool_max": args.pool_max,
            "pool_min": args.pool_min,
            "settings": format!("{:?}", DurableSettings::default()),
            "debug_assertions": cfg!(debug_assertions),
        }),
    )?;
    for name in &args.cases {
        let case = case(name)?;
        let database = match args.store.as_str() {
            "postgres" => Database::Postgres(
                args.database_url
                    .clone()
                    .context("LASH_POSTGRES_DATABASE_URL is not set")?,
            ),
            "sqlite" => {
                let dir = args
                    .sqlite_dir
                    .clone()
                    .context("--sqlite-dir is required")?;
                std::fs::create_dir_all(&dir)?;
                let path = dir.join(format!("{name}.sqlite"));
                if path.exists() {
                    bail!("{} exists: each case takes a fresh file", path.display());
                }
                Database::Sqlite(path)
            }
            other => bail!("no store `{other}`"),
        };
        let run = Run {
            report: &report,
            database,
            nodes: args.nodes,
            samples: args.samples,
            pool: PoolSize {
                max: args.pool_max,
                min: args.pool_min,
            },
        };
        eprintln!("case {name} on {} x{}", args.store, args.nodes);
        match case.kind {
            Kind::Rounds { sessions } => turns::rounds(&run, &case, sessions).await?,
            Kind::Resume { prior_turns } => turns::resume(&run, &case, prior_turns).await?,
            Kind::Cell => turns::cell(&run, &case).await?,
            Kind::Parked => {
                processes::parked(&run, &case, Duration::from_secs(args.park_seconds)).await?;
            }
            Kind::HotWaits { waits } => processes::hot_waits(&run, &case, waits).await?,
            Kind::Idle { actors } => {
                processes::idle(&run, &case, actors, Duration::from_secs(args.idle_seconds))
                    .await?;
            }
            Kind::Store => {
                let Database::Postgres(url) = &run.database else {
                    bail!("the store case runs on PostgreSQL");
                };
                store_bench::run(
                    &report,
                    url,
                    &args.store_nodes,
                    args.store_seconds,
                    args.wake_events,
                )
                .await?;
            }
        }
        eprintln!("case {name} done");
    }
    Ok(())
}
