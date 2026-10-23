//! Per-turn work by component and held frame, on a fresh FULL SQLite session.
use super::FollowerMode;
use super::provider::{LatencyProviderKind, ProviderTiming};
use super::runner::{CaseSpec, Topology, build_core, durable_backend, latency_llm_profile_spec};
use crate::perf_support::instructions::InstructionCounter;
use crate::perf_support::memory::process_memory_sample;
use anyhow::{Context, Result};
use lash_core_ids::perf_witness::history::{Collector, Component, Cost, process_cpu_ns};
use lash_vocabulary::identity::SessionId;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct HistoryRun {
    pub store_dir: PathBuf,
    pub turns: usize,
    pub out: PathBuf,
    pub perf_control: Option<(PathBuf, usize)>,
    pub compact_every: Option<usize>,
    pub check: bool,
}
#[derive(Serialize)]
pub(super) struct TurnCost {
    pub turn: usize,
    instructions: u64,
    allocations: usize,
    allocated_bytes: usize,
    live_bytes: i128,
    statements: u64,
    cpu_ms: f64,
    start_ns: u64,
    end_ns: u64,
    rss_kb: Option<u64>,
    pub held_bytes: usize,
    pub held_messages: usize,
    pub frame: String,
    pub components: BTreeMap<Component, Cost>,
    pub sql: BTreeMap<String, Cost>,
    measurement_cpu_ns: u64,
}
#[cfg(test)]
impl TurnCost {
    pub(super) fn fixture(turn: usize, components: BTreeMap<Component, Cost>) -> Self {
        Self {
            turn,
            instructions: 0,
            allocations: 0,
            allocated_bytes: 0,
            live_bytes: 0,
            statements: 0,
            cpu_ms: 0.0,
            start_ns: 0,
            end_ns: 0,
            rss_kb: None,
            held_bytes: turn,
            held_messages: turn,
            frame: "one".into(),
            components,
            sql: BTreeMap::new(),
            measurement_cpu_ns: 0,
        }
    }
}
#[derive(Serialize)]
struct HistoryReport {
    case: &'static str,
    store: &'static str,
    source_sha: Option<String>,
    compact_every: Option<usize>,
    held_bytes_definition: &'static str,
    attribution: &'static str,
    turns: Vec<TurnCost>,
    verdict: Option<super::history_gate::Verdict>,
}
/// Counters open before runtime threads. Only sequential turns enter the meter;
/// compaction and report construction stay outside it.
///
/// # Errors
/// Store/send failures, an incomplete probe or a failed requested check.
pub async fn run(run: HistoryRun, counter: InstructionCounter) -> Result<()> {
    if let Some(k) = run.compact_every {
        anyhow::ensure!(
            k >= 60 && run.turns >= 2 * k,
            "compaction needs K >= 60 and at least two frames"
        );
    }
    let work = lash_core_ids::perf_witness::Collector::install()?;
    let attribution = Collector::install()?;
    attribution.pause();
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
        if turn > 1
            && run
                .compact_every
                .is_some_and(|k| (turn - 1).is_multiple_of(k))
        {
            let admin = session.admin();
            let opened = admin
                .state()
                .compact_context(None, format!("history-compact-{turn}"))
                .await?
                .settle_with(&admin.commands(), lash::testing::admin_fixture_outcome)
                .await?;
            anyhow::ensure!(opened, "compaction before turn {turn} opened no frame");
        }
        if let Some((control, from)) = &run.perf_control
            && *from == turn
        {
            std::fs::write(control, b"enable\n")
                .with_context(|| format!("enable perf through {}", control.display()))?;
        }
        let statements_before = work.snapshot().sql_statements;
        let before_components = attribution.snapshot();
        let overhead_before = attribution.overhead_cpu_ns();
        let _ = attribution.take_sql();
        let before = counter.read()?;
        let alloc_before = crate::GLOBAL_ALLOCATOR.stats();
        let cpu_before = process_cpu_ns();
        let start_ns = lash_core_ids::perf_witness::latency::Stamp::now().monotonic_ns;
        attribution.resume();
        let outcome = session
            .send(lash::TurnInput::text("history turn"))
            .into_future()
            .await?
            .outcome()
            .await?;
        attribution.pause();
        let end_ns = lash_core_ids::perf_witness::latency::Stamp::now().monotonic_ns;
        let cpu_ns = process_cpu_ns() - cpu_before;
        let alloc = crate::GLOBAL_ALLOCATOR.stats() - alloc_before;
        let instructions = counter.read()? - before;
        anyhow::ensure!(
            super::runner::status_name(&outcome.status()) == "answered",
            "turn {turn} ended {}",
            super::runner::status_name(&outcome.status())
        );
        let measurement_cpu_ns = attribution.overhead_cpu_ns() - overhead_before;
        let mut components: BTreeMap<_, _> = attribution
            .snapshot()
            .into_iter()
            .map(|(component, cost)| (component, cost.since(before_components[&component])))
            .collect();
        let attributed: u64 = components.values().map(|c| c.cpu_ns).sum();
        anyhow::ensure!(
            attributed + measurement_cpu_ns <= cpu_ns,
            "component CPU exceeds process CPU at turn {turn}"
        );
        components.entry(Component::Other).or_default().cpu_ns =
            cpu_ns - attributed - measurement_cpu_ns;
        let sql = attribution.take_sql();
        let durable = components.entry(Component::DurableCommitSql).or_default();
        durable.statements = sql.values().map(|c| c.statements).sum();
        durable.vm_steps = sql.values().map(|c| c.vm_steps).sum();
        durable.rows = sql.values().map(|c| c.rows).sum();
        let (held_bytes, held_messages, frame) = attribution.held();
        turns.push(TurnCost {
            turn,
            instructions,
            allocations: alloc.allocations,
            allocated_bytes: alloc.bytes_allocated,
            live_bytes: alloc.bytes_allocated as i128 - alloc.bytes_deallocated as i128,
            statements: work.snapshot().sql_statements - statements_before,
            cpu_ms: cpu_ns as f64 / 1_000_000.0,
            start_ns,
            end_ns,
            rss_kb: process_memory_sample().rss_kb,
            held_bytes,
            held_messages,
            frame,
            components,
            sql,
            measurement_cpu_ns,
        });
    }
    session.close().await?;
    core.shutdown().await?;
    let verdict = run
        .check
        .then(|| super::history_gate::evaluate(&turns, run.compact_every))
        .transpose()?;
    let pass = verdict.as_ref().is_none_or(|v| v.pass);
    if let Some(verdict) = &verdict {
        for row in verdict.comparisons.iter().filter(|row| !row.pass) {
            eprintln!(
                "history growth FAIL {} {} early={} late={} ratio={:.6} per_held_byte={}",
                row.component, row.metric, row.early, row.late, row.ratio, row.per_held_byte
            );
        }
        eprintln!(
            "history growth {} turns={} comparisons={}",
            if pass { "PASS" } else { "FAIL" },
            turns.len(),
            verdict.comparisons.len()
        );
    }
    let report = HistoryReport {
        case: "fast",
        store: "sqlite-file synchronous=FULL",
        source_sha: crate::perf_support::git::head_commit(),
        compact_every: run.compact_every,
        held_bytes_definition: "UTF-8 JSON bytes of the provider request messages, including structural overhead; not resident heap capacity",
        attribution: "exclusive thread CPU during polls and synchronous scopes; all other process CPU is residual; Rust allocations only (SQLite C heap excluded); measurement CPU excluded; compaction outside turn window",
        turns,
        verdict,
    };
    std::fs::write(&run.out, serde_json::to_vec_pretty(&report)?)
        .with_context(|| format!("write {}", run.out.display()))?;
    anyhow::ensure!(
        pass,
        "history component growth check failed; see {}",
        run.out.display()
    );
    Ok(())
}
async fn open_core(store_dir: &Path) -> Result<lash::LashCore> {
    if store_dir.exists() {
        std::fs::remove_dir_all(store_dir)?;
    }
    std::fs::create_dir_all(store_dir)?;
    let stores = lash::sqlite::SqliteStoreSet::open(
        store_dir.join("lash.db"),
        lash::sqlite::SqliteSynchronous::Full,
    )
    .await?;
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

/// A bounded observer: construct and publish real records, then release them.
pub(super) struct DiscardTrace;
impl lash::tracing::TraceSink for DiscardTrace {
    fn append(&self, _: &lash::tracing::TraceRecord) -> Result<(), lash::tracing::TraceSinkError> {
        Ok(())
    }
}
