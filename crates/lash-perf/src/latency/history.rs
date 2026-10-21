//! Per-turn lash work as one session's history grows (FIG-6889).
//!
//! One same-process session on a SQLite file store set sends `turns` plain
//! fast-provider turns in sequence. Each turn records process instructions,
//! allocations, SQL statements, CPU and RSS. Monotonic span clocks join a
//! sampled CPU profile to the exact history depth.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use lash_vocabulary::identity::SessionId;
use serde::Serialize;

use super::FollowerMode;
use super::provider::{LatencyProviderKind, ProviderTiming};
use super::runner::{CaseSpec, Topology, build_core, durable_backend, latency_llm_profile_spec};
use crate::perf_support::instructions::InstructionCounter;
use crate::perf_support::memory::process_memory_sample;

/// One `lash-perf history` run.
pub struct HistoryRun {
    pub store_dir: PathBuf,
    pub turns: usize,
    pub out: PathBuf,
    /// A `perf record --control fifo:<path>` control FIFO, sent `enable` as
    /// turn `profile_from` starts, so a profile covers only the late turns.
    pub perf_control: Option<(PathBuf, usize)>,
}

#[derive(Serialize)]
struct TurnCost {
    turn: usize,
    instructions: u64,
    allocations: usize,
    allocated_bytes: usize,
    live_bytes: i128,
    statements: u64,
    cpu_ms: Option<f64>,
    start_ns: u64,
    end_ns: u64,
    rss_kb: Option<u64>,
}

#[derive(Serialize)]
struct HistoryReport {
    case: &'static str,
    store: &'static str,
    turns: Vec<TurnCost>,
}

/// Run the session and write the report. `counter` must have been opened
/// before the runtime this future runs on started its threads.
///
/// # Errors
/// Returns an error when the store, a send or the report write fails.
pub async fn run(run: HistoryRun, counter: InstructionCounter) -> Result<()> {
    let work = lash_core_ids::perf_witness::Collector::install()?;
    let core = open_core(&run.store_dir).await?;
    let session_id = SessionId::fixture("history-growth");
    core.session(session_id.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            latency_llm_profile_spec()?.wire_model,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        )))
        .await?;
    let session = core.session(session_id).open().await?;
    let mut turns = Vec::with_capacity(run.turns);
    for turn in 1..=run.turns {
        if let Some((control, from)) = &run.perf_control
            && *from == turn
        {
            std::fs::write(control, b"enable\n")
                .with_context(|| format!("enable perf through {}", control.display()))?;
        }
        let statements_before = work.snapshot().sql_statements;
        let cpu_before = crate::perf_support::scheduler::process_cpu_ms();
        let before = counter.read()?;
        let alloc_before = crate::GLOBAL_ALLOCATOR.stats();
        let start_ns = lash_core_ids::perf_witness::latency::Stamp::now().monotonic_ns;
        let handle = session
            .send(lash::TurnInput::text(format!("history turn {turn}")))
            .into_future()
            .await?;
        let outcome = handle.outcome().await?;
        let end_ns = lash_core_ids::perf_witness::latency::Stamp::now().monotonic_ns;
        let alloc = crate::GLOBAL_ALLOCATOR.stats() - alloc_before;
        let instructions = counter.read()? - before;
        let cpu_ms = crate::perf_support::scheduler::process_cpu_ms()
            .zip(cpu_before)
            .map(|(after, before)| after - before);
        anyhow::ensure!(
            super::runner::status_name(&outcome.status()) == "answered",
            "turn {turn} ended {}",
            super::runner::status_name(&outcome.status())
        );
        turns.push(TurnCost {
            turn,
            instructions,
            allocations: alloc.allocations,
            allocated_bytes: alloc.bytes_allocated,
            live_bytes: alloc.bytes_allocated as i128 - alloc.bytes_deallocated as i128,
            statements: work.snapshot().sql_statements - statements_before,
            cpu_ms,
            start_ns,
            end_ns,
            rss_kb: process_memory_sample().rss_kb,
        });
    }
    session.close().await?;
    let report = HistoryReport {
        case: "fast",
        store: "sqlite-file synchronous=FULL",
        turns,
    };
    std::fs::write(&run.out, serde_json::to_vec_pretty(&report)?)
        .with_context(|| format!("write {}", run.out.display()))?;
    Ok(())
}

async fn open_core(store_dir: &Path) -> Result<lash::LashCore> {
    let _ = std::fs::remove_dir_all(store_dir);
    std::fs::create_dir_all(store_dir)
        .with_context(|| format!("create {}", store_dir.display()))?;
    let stores = lash::sqlite::SqliteStoreSet::open(
        store_dir.join("lash.db"),
        lash::sqlite::SqliteSynchronous::Full,
    )
    .await
    .map_err(|error| anyhow::anyhow!("open the store set: {error}"))?;
    let spec = CaseSpec {
        name: "history",
        topology: Topology::SameProcess,
        provider: LatencyProviderKind::Text,
        follower: FollowerMode::Standard,
        samples: 0,
        lanes: 1,
        busy: false,
        fresh_sessions: false,
    };
    build_core(
        durable_backend(stores)?,
        &spec,
        Arc::new(ProviderTiming::default()),
        None,
        &None,
        None,
    )
}
