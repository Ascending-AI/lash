//! Independent OS writers share the product SQLite database. The parent
//! releases a stdin barrier only after every child opened its own core.
use super::{Args, Case, Meter, Receipt, facade};
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Debug, clap::Args)]
pub struct WorkerArgs {
    #[arg(long)]
    pub store_dir: PathBuf,
    #[arg(long)]
    pub out: PathBuf,
    #[arg(long)]
    pub lane: usize,
    #[arg(long)]
    pub operations: usize,
}

/// One writer: its own core over the shared database, serving as its own
/// node, and the session it sends to.
pub(super) struct Writer {
    lane: usize,
    core: lash::LashCore,
    session: lash::DurableSession,
    meter: Meter,
}

impl Writer {
    pub(super) async fn open(store_dir: &Path, lane: usize) -> Result<Self> {
        let meter = Meter::default();
        let stores = lash_sqlite_store::SqliteStoreSet::open(
            store_dir.join("lash.db"),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await?;
        let core = facade::build(
            facade::backend(Arc::new(stores))?,
            &format!("writer-{lane}"),
            true,
            true,
            facade::provider(&meter, false),
        )?;
        let session = facade::create(&core, &format!("writer-{lane}")).await?;
        Ok(Self {
            lane,
            core,
            session,
            meter,
        })
    }

    /// Send and settle `operations` inputs, then stop the writer's node.
    pub(super) async fn finish(self, operations: usize) -> Result<Meter> {
        let result = async {
            for n in 0..operations {
                facade::send(
                    &self.session,
                    &format!("writer-{}-{n}", self.lane),
                    &self.meter,
                )
                .await?;
            }
            anyhow::Ok(())
        }
        .await;
        self.core.shutdown().await?;
        result?;
        Ok(self.meter)
    }
}

pub async fn run_worker(args: &WorkerArgs) -> Result<()> {
    use std::io::Write;
    let writer = Writer::open(&args.store_dir, args.lane).await?;
    println!("boundary writer ready");
    std::io::stdout().flush()?;
    let mut line = String::new();
    BufReader::new(tokio::io::stdin())
        .read_line(&mut line)
        .await?;
    ensure!(line == "go\n", "writer barrier was not released");
    let meter = writer.finish(args.operations).await?;
    super::write_receipt(
        &args.out,
        &Receipt::new(
            Case::SqliteProcesses,
            "facade-child-writer",
            "sqlite-file-product",
            args.operations,
            &meter,
            serde_json::json!({"lane": args.lane, "pid": std::process::id(), "settled": args.operations}),
        ),
    )
}

pub(super) async fn run(args: &Args) -> Result<Receipt> {
    ensure!(
        args.callers >= 2,
        "simultaneous SQLite requires at least two processes"
    );
    // Establish the product baseline once; all child connections open it again.
    let initialized = lash_sqlite_store::SqliteStoreSet::open(
        args.store_dir.join("lash.db"),
        lash_sqlite_store::SqliteSynchronous::Normal,
    )
    .await?;
    drop(initialized);
    let mut children = Vec::new();
    let mut paths = Vec::new();
    let meter = Meter::default();
    let start = Instant::now();
    for lane in 0..args.callers {
        let path = args.store_dir.join(format!("writer-{lane}.json"));
        let mut child = tokio::process::Command::new(std::env::current_exe()?)
            .arg("boundary-worker")
            .arg("--store-dir")
            .arg(&args.store_dir)
            .arg("--out")
            .arg(&path)
            .arg("--lane")
            .arg(lane.to_string())
            .arg("--operations")
            .arg(args.operations.to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child.stdout.take().context("writer stdout")?;
        let mut lines = BufReader::new(stdout).lines();
        let ready = tokio::time::timeout(Duration::from_secs(60), lines.next_line()).await??;
        ensure!(
            ready.as_deref() == Some("boundary writer ready"),
            "writer startup failed: {ready:?}"
        );
        paths.push(path);
        children.push(child);
    }
    meter.record("sqlite.process.boot", args.callers, start);
    let start = Instant::now();
    for child in &mut children {
        let mut stdin = child.stdin.take().context("writer stdin")?;
        stdin.write_all(b"go\n").await?;
    }
    meter.record("sqlite.process.barrier_release", args.callers, start);
    let start = Instant::now();
    let statuses = futures_util::future::try_join_all(children.iter_mut().map(|child| async {
        Ok::<_, anyhow::Error>(tokio::time::timeout(Duration::from_secs(120), child.wait()).await??)
    }))
    .await?;
    ensure!(
        statuses.iter().all(|status| status.success()),
        "SQLite writer failed: {statuses:?}"
    );
    meter.record("sqlite.process.join", args.callers, start);
    let mut pids = std::collections::BTreeSet::new();
    let mut settled = 0;
    for path in paths {
        let receipt: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
        pids.insert(receipt["evidence"]["pid"].as_u64().context("writer pid")?);
        settled += receipt["evidence"]["settled"]
            .as_u64()
            .context("writer settles")? as usize;
        for phase in receipt["phases"].as_array().context("writer phases")? {
            // Preserve the child-measured intervals rather than timing a parent
            // read and calling it a send.
            meter
                .0
                .lock()
                .map_err(|_| anyhow::anyhow!("meter poisoned"))?
                .push(super::Phase {
                    boundary: phase["boundary"].as_str().context("boundary")?.into(),
                    operations: phase["operations"].as_u64().context("count")? as usize,
                    elapsed_us: u128::from(phase["elapsed_us"].as_u64().context("elapsed")?),
                });
        }
    }
    ensure!(
        pids.len() == args.callers && settled == args.operations * args.callers,
        "SQLite population incomplete"
    );
    Ok(Receipt::new(
        Case::SqliteProcesses,
        "facade-multiple-os-processes",
        "sqlite-file-product",
        settled,
        &meter,
        serde_json::json!({"pids": pids, "processes": args.callers, "settled": settled,
            "operations_per_process": args.operations, "barrier": "all-writers-open-before-go"}),
    ))
}
