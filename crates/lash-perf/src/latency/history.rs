//! Per-turn lash work as one session's history grows (FIG-6889).
//!
//! One same-process session on a SQLite file store set sends `turns` plain
//! fast-provider turns in sequence. Each turn's `instructions:u` (the whole
//! process, inherited threads included) and the process RSS after it land in
//! one JSON report, so a reader compares turn 600's cost with turn 10's
//! without any wall-clock noise.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use lash_protocol_types::SessionId;
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
        let before = counter.read()?;
        let handle = session
            .send(lash::TurnInput::text(format!("history turn {turn}")))
            .into_future()
            .await?;
        let outcome = handle.outcome().await?;
        let instructions = counter.read()? - before;
        anyhow::ensure!(
            super::runner::status_name(&outcome.status()) == "answered",
            "turn {turn} ended {}",
            super::runner::status_name(&outcome.status())
        );
        turns.push(TurnCost {
            turn,
            instructions,
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
