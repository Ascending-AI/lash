//! Per-turn work by component and held frame, on a fresh FULL SQLite session.
use super::FollowerMode;
use super::provider::{LatencyProviderKind, ProviderTiming};
use super::runner::{CaseSpec, Topology, build_core, durable_backend, latency_llm_profile_spec};
use crate::perf_support::instructions::InstructionCounter;
use crate::perf_support::memory::process_memory_sample;
use anyhow::{Context, Result};
use lash_core_ids::perf_witness::history::{Collector, Component, Cost, process_cpu_ns};
use lash_protocol_types::sync::MutexExt;
use lash_vocabulary::identity::SessionId;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

pub struct HistoryRun {
    pub store_dir: PathBuf,
    pub turns: usize,
    pub out: PathBuf,
    pub perf_control: Option<(PathBuf, usize)>,
    pub compact_every: Option<usize>,
    pub protocol: HistoryProtocol,
    pub continue_as_every: Option<usize>,
    pub check: bool,
    pub growing_heap: bool,
}
/// The protocol is explicit so the same reset rule covers code-mode turns.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryProtocol {
    Standard,
    Rlm,
}
impl HistoryRun {
    fn reset_every(&self) -> Result<Option<usize>> {
        anyhow::ensure!(
            self.compact_every.is_none() || self.continue_as_every.is_none(),
            "choose only one frame-change geometry"
        );
        anyhow::ensure!(
            self.continue_as_every.is_none() || self.protocol == HistoryProtocol::Rlm,
            "continue-as requires --protocol rlm"
        );
        let every = self.compact_every.or(self.continue_as_every);
        if let Some(k) = every {
            anyhow::ensure!(
                k >= 60 && self.turns >= 2 * k,
                "frame changes need K >= 60 and at least two frames"
            );
        }
        Ok(every)
    }
}
pub(super) const CONTINUE_TASK: &str = "Continue auditing the daily order totals. Use the carried memo and receipt as the handover, verify the next batch, and return its checked total.";
const MEMO_BYTES: usize = 3072;
fn seed_memo() -> String {
    "Order audit: verified invoices and reconciled receipts. "
        .repeat(64)
        .chars()
        .take(MEMO_BYTES)
        .collect()
}
pub(super) fn cell_text(switch: bool, follow_on: bool, growing: bool, first: bool) -> String {
    let end = if switch {
        format!(
            "await control.continueAs({{task: {}, seed: {{memo: {}, handover_receipt: receipt}}}});",
            serde_json::json!(CONTINUE_TASK),
            serde_json::json!(seed_memo())
        )
    } else {
        "await control.finish({total: total, receipt: receipt.value});".into()
    };
    let handover = if follow_on {
        "console.log({memo_bytes: memo.length, prior_receipt: handover_receipt.value});\n"
    } else {
        ""
    };
    let growth = if growing {
        format!(
            "{}checkpointHeap.push(1);\n",
            if first {
                "let checkpointHeap = [];\n"
            } else {
                ""
            }
        )
    } else {
        String::new()
    };
    format!(
        "{handover}{growth}const receipt = await tools.benchmarkEcho({{value: \"history turn\", ordinal: 1}});\nconst total = [125, 250, 375].reduce((sum, amount) => sum + amount, 0);\nconsole.log({{total: total, receipt: receipt.value}});\n{end}"
    )
}
#[derive(Serialize)]
pub(super) struct RequestHistory {
    pub frame: String,
    pub held_bytes: usize,
    pub held_messages: usize,
    // Preserve the actual request at frame resets for independent seed accounting.
    pub first_in_frame_messages: Option<serde_json::Value>,
    pub first_in_frame_instructions: Option<Arc<str>>,
}
impl RequestHistory {
    pub(super) fn new(
        request: &lash_core::llm::types::LlmRequest,
        held_bytes: usize,
        first: bool,
    ) -> Self {
        Self {
            frame: request.scope.agent_frame_id.clone(),
            held_bytes,
            held_messages: request.messages.len(),
            first_in_frame_messages: first.then(|| serde_json::json!(request.messages)),
            first_in_frame_instructions: first.then(|| request.instructions.clone()).flatten(),
        }
    }
}
#[derive(Serialize)]
pub(super) struct TurnCost {
    pub turn: usize,
    pub switch: bool,
    pub requests: Vec<RequestHistory>,
    pub vm: BTreeMap<String, u64>,
    pub ownership: Option<super::history_ownership::Ownership>,
    switch_root_observed_cpu_ns: Option<u64>,
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
            switch: false,
            requests: Vec::new(),
            vm: BTreeMap::new(),
            ownership: None,
            switch_root_observed_cpu_ns: None,
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
    continue_as_every: Option<usize>,
    protocol: HistoryProtocol,
    growing_heap: bool,
    seed_memo_bytes: usize,
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
    let reset_every = run.reset_every()?;
    anyhow::ensure!(
        !run.growing_heap || run.protocol == HistoryProtocol::Rlm,
        "growing heap requires RLM"
    );
    let work = lash_core_ids::perf_witness::Collector::install()?;
    let attribution = Collector::install()?;
    attribution.pause();
    let timing = Arc::new(ProviderTiming::default());
    let core = open_core(&run, Arc::clone(&timing)).await?;
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
        let vm_before = vm_snapshot(&timing)?;
        let statements_before = work.snapshot().sql_statements;
        let before_components = attribution.snapshot();
        let overhead_before = attribution.overhead_cpu_ns();
        let _ = attribution.take_sql();
        let before = counter.read()?;
        let alloc_before = crate::GLOBAL_ALLOCATOR.stats();
        let cpu_before = process_cpu_ns();
        let start_ns = lash_core_ids::perf_witness::latency::Stamp::now().monotonic_ns;
        attribution.resume();
        let switch = run
            .continue_as_every
            .is_some_and(|k| turn < run.turns && turn.is_multiple_of(k));
        let outcome = session
            .send(lash::TurnInput::text("history turn"))
            .into_future()
            .await?
            .outcome()
            .await?;
        let switch_root_observed_cpu_ns = switch.then(|| process_cpu_ns() - cpu_before);
        if switch {
            let lash::SendOutcome::Settled { output, .. } = &outcome else {
                anyhow::bail!("switch turn {turn} did not settle");
            };
            let lash::TurnOutcome::AgentFrameSwitch { frame_key, .. } = &output.outcome else {
                anyhow::bail!("turn {turn} did not call continue_as");
            };
            let follow_on = session
                .attach_id(lash_core::runtime::durable::session_mail::frame_task_run(
                    frame_key,
                ))
                .outcome()
                .await?;
            anyhow::ensure!(
                super::runner::status_name(&follow_on.status()) == "answered",
                "follow-on at {turn} failed: {follow_on:?}"
            );
        }
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
        let vm: BTreeMap<String, u64> = vm_snapshot(&timing)?
            .into_iter()
            .map(|(key, value)| {
                let before = vm_before.get(&key).copied().unwrap_or(0);
                Ok((
                    key.clone(),
                    value
                        .checked_sub(before)
                        .with_context(|| format!("VM counter {key} regressed"))?,
                ))
            })
            .collect::<Result<_>>()?;
        let requests = std::mem::take(&mut *timing.history_requests.lock_recover());
        anyhow::ensure!(
            requests.len() == if switch { 2 } else { 1 },
            "turn {turn} has an unexpected provider-call population"
        );
        if switch {
            anyhow::ensure!(
                requests[0].frame != requests[1].frame,
                "continue-as did not open a fresh frame at {turn}"
            );
        }
        if run.protocol == HistoryProtocol::Rlm {
            anyhow::ensure!(
                vm.get("cell_executions").copied().unwrap_or(0) > 0
                    && vm.get("vm_steps").copied().unwrap_or(0) > 0,
                "turn {turn} has an empty cell/VM work population"
            );
        }
        let ownership = (run.protocol == HistoryProtocol::Rlm)
            .then(|| {
                let staged_bytes = timing
                    .history_vm_service
                    .lock_recover()
                    .as_ref()
                    .context("RLM worker service missing")?
                    .spools()?
                    .retained_page_bytes();
                let mut owned = super::history_ownership::snapshot(&run.store_dir.join("lash.db"))?;
                owned.staged_bytes = staged_bytes;
                Ok::<_, anyhow::Error>(owned)
            })
            .transpose()?;
        turns.push(TurnCost {
            turn,
            switch,
            requests,
            vm,
            ownership,
            switch_root_observed_cpu_ns,
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
        .then(|| super::history_gate::evaluate(&turns, reset_every))
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
        continue_as_every: run.continue_as_every,
        protocol: run.protocol,
        growing_heap: run.growing_heap,
        seed_memo_bytes: if run.continue_as_every.is_some() {
            MEMO_BYTES
        } else {
            0
        },
        held_bytes_definition: "UTF-8 JSON bytes of the provider request messages, including structural overhead; not resident heap capacity",
        attribution: "exclusive thread CPU during polls and synchronous scopes; all other process CPU is residual; Rust allocations only (SQLite C heap excluded); measurement CPU excluded; compaction outside turn window; continue-as window includes its automatically mailed follow-on",
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
async fn open_core(run: &HistoryRun, timing: Arc<ProviderTiming>) -> Result<lash::LashCore> {
    let store_dir = &run.store_dir;
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
        provider: match run.protocol {
            HistoryProtocol::Standard => LatencyProviderKind::Text,
            HistoryProtocol::Rlm => LatencyProviderKind::HistoryCell {
                continue_as_every: run.continue_as_every,
                turns: run.turns,
                growing_heap: run.growing_heap,
            },
        },
        follower: FollowerMode::Standard,
        samples: 0,
        lanes: 1,
        busy: false,
        fresh_sessions: false,
    };
    build_core(durable_backend(stores)?, &spec, timing, None, &None, None)
}

// WorkerWork owns its counter membership. Project its cumulative numeric fields;
// maxima describe capacity rather than per-turn work and are not differenced.
fn vm_snapshot(timing: &ProviderTiming) -> Result<BTreeMap<String, u64>> {
    let pool = timing.vm_pool.lock_recover();
    let Some(pool) = pool.as_ref() else {
        return Ok(BTreeMap::new());
    };
    let counters = pool.measurements().counters;
    let work = serde_json::to_value(counters.worker_work)?;
    let mut values: BTreeMap<String, u64> = work
        .as_object()
        .context("worker work must be a record")?
        .iter()
        .filter(|(key, _)| !key.contains("peak"))
        .filter_map(|(key, value)| value.as_u64().map(|value| (key.clone(), value)))
        .collect();
    values.insert("cpu_ns".into(), pool.measured_cpu().as_nanos().try_into()?);
    values.insert("ipc_sent_bytes".into(), counters.ipc_sent_bytes);
    values.insert("ipc_received_bytes".into(), counters.ipc_received_bytes);
    values.insert("cell_executions".into(), counters.cell_executions);
    Ok(values)
}

/// A bounded observer: construct and publish real records, then release them.
pub(super) struct DiscardTrace;
impl lash::tracing::TraceSink for DiscardTrace {
    fn append(&self, _: &lash::tracing::TraceRecord) -> Result<(), lash::tracing::TraceSinkError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // FIG-7226: the matrix admits continue_as only for RLM and one reset owner.
    #[test]
    fn continue_as_requires_rlm_and_excludes_compaction() {
        let mut run = HistoryRun {
            store_dir: PathBuf::new(),
            turns: 1000,
            out: PathBuf::new(),
            perf_control: None,
            compact_every: None,
            protocol: HistoryProtocol::Standard,
            continue_as_every: Some(100),
            check: true,
            growing_heap: false,
        };
        assert!(run.reset_every().is_err());
        run.protocol = HistoryProtocol::Rlm;
        assert_eq!(run.reset_every().unwrap(), Some(100));
        run.compact_every = Some(100);
        assert!(run.reset_every().is_err());
        run.continue_as_every = None;
        assert_eq!(run.reset_every().unwrap(), Some(100));
        run.protocol = HistoryProtocol::Standard;
        assert_eq!(run.reset_every().unwrap(), Some(100));
        run.compact_every = None;
        assert_eq!(run.reset_every().unwrap(), None);
        run.protocol = HistoryProtocol::Rlm;
        assert_eq!(run.reset_every().unwrap(), None);
    }
}
