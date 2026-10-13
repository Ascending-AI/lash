//! Measurement windows use the replay witness and the existing physical SQL observer.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use lash_sansio::sync::MutexExt as _;
use serde::{Deserialize, Serialize};

use super::{Phase, SampleArgs};
use crate::replay::provider::ScriptBook;
use crate::replay::script::ToolCallSpec;
use lash_core_execution::facade_support::sql;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Resume {
    pub latency_ms: f64,
    pub bytes_read: u64,
    pub restored_nodes: u64,
    pub restored_bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KindBytes {
    pub nodes: u64,
    pub bytes: u64,
}

/// Canonical bytes supplied by an owner, including already stored nodes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StagedCommit {
    pub label: String,
    pub by_kind: BTreeMap<String, KindBytes>,
    pub domain_writes: usize,
}

/// Physical SQL observed by the store, separate from staged graph bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitSql {
    pub label: String,
    pub elapsed_nanos: u64,
    pub installed_graph_bytes: u64,
    pub rows_affected: u64,
    pub bound_bytes: u64,
    pub result_bytes: u64,
    pub statements: u64,
    pub unretained_shape_statements: u64,
    /// Whole-host allocation delta while delegation was open; includes SQL
    /// observation and concurrent host work, not the earlier graph encoding.
    pub delegate_allocations: Option<u64>,
    pub delegate_allocated_bytes: Option<u64>,
    pub delegate_elapsed_nanos: Option<u64>,
    pub shapes: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    pub n: usize,
    pub metrics: BTreeMap<String, f64>,
    pub owner_commit_labels: BTreeMap<String, u64>,
    pub staged_commits: Vec<StagedCommit>,
    pub commit_sql: Vec<CommitSql>,
    pub tool_inputs: Vec<ToolCallSpec>,
    pub resume: Option<Resume>,
    pub prefix: Option<Box<Sample>>,
}

impl Sample {
    pub fn prepend(&mut self, prefix: Self) {
        for (key, value) in &prefix.metrics {
            *self.metrics.entry(key.clone()).or_default() += value;
        }
        for (label, count) in &prefix.owner_commit_labels {
            *self.owner_commit_labels.entry(label.clone()).or_default() += count;
        }
        let mut staged = prefix.staged_commits.clone();
        staged.append(&mut self.staged_commits);
        self.staged_commits = staged;
        let mut sql = prefix.commit_sql.clone();
        sql.append(&mut self.commit_sql);
        self.commit_sql = sql;
        let mut inputs = prefix.tool_inputs.clone();
        inputs.append(&mut self.tool_inputs);
        self.tool_inputs = inputs;
        self.prefix = Some(Box::new(prefix));
    }
}

struct State {
    active: bool,
    started: Instant,
    baseline: lash_core::perf_witness::Snapshot,
    cpu_before: f64,
    worker_cpu_before: f64,
    labels: BTreeMap<String, u64>,
    staged_bytes: u64,
    staged_commits: Vec<StagedCommit>,
    commit_sql: Vec<CommitSql>,
    alloc_before: stats_alloc::Stats,
    read_calls: u64,
    read_operations: BTreeMap<String, u64>,
    ipc_before: BTreeMap<String, f64>,
    bytes_read: u64,
    restored_nodes: u64,
    restored_bytes: u64,
    tool_inputs: Vec<ToolCallSpec>,
    resume: Option<Resume>,
}

pub(super) struct Observation {
    witness: lash_core::perf_witness::Collector,
    state: Mutex<State>,
    pool: Mutex<Option<lash::vm::WorkerPool>>,
    page_size: Mutex<u64>,
    book: Arc<ScriptBook>,
    n: usize,
    phase: Phase,
    out: std::path::PathBuf,
}

impl Observation {
    pub fn new(args: &SampleArgs, book: Arc<ScriptBook>) -> Result<Self> {
        if args.phase == Phase::Resume {
            crate::perf_support::time::instruction_control("enable")?;
        }
        let witness = lash_core::perf_witness::Collector::install()?;
        let baseline = witness.snapshot();
        Ok(Self {
            witness,
            state: Mutex::new(State {
                active: args.phase == Phase::Resume,
                started: Instant::now(),
                baseline,
                cpu_before: cpu_ms()?,
                worker_cpu_before: 0.0,
                labels: BTreeMap::new(),
                staged_bytes: 0,
                staged_commits: Vec::new(),
                commit_sql: Vec::new(),
                alloc_before: crate::GLOBAL_ALLOCATOR.stats(),
                read_calls: 0,
                read_operations: BTreeMap::new(),
                ipc_before: BTreeMap::new(),
                bytes_read: 0,
                restored_nodes: 0,
                restored_bytes: 0,
                tool_inputs: Vec::new(),
                resume: None,
            }),
            pool: Mutex::new(None),
            page_size: Mutex::new(0),
            book,
            n: args.n,
            phase: args.phase,
            out: args.out.clone(),
        })
    }

    pub fn set_page_size(&self, stores: &lash_sqlite_store::SqliteStoreSet) -> Result<()> {
        let connection = rusqlite::Connection::open_with_flags(
            stores.database_uri(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )?;
        *self.page_size.lock_recover() =
            connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        Ok(())
    }

    pub fn set_pool(&self, pool: lash::vm::WorkerPool) {
        *self.pool.lock_recover() = Some(pool);
    }

    fn worker_cpu_ms(&self) -> f64 {
        self.pool
            .lock_recover()
            .as_ref()
            .map_or(0.0, |pool| pool.measured_cpu().as_secs_f64() * 1000.0)
    }

    fn ipc_metrics(&self) -> BTreeMap<String, f64> {
        let pool = self.pool.lock_recover();
        let Some(pool) = pool.as_ref() else {
            return BTreeMap::new();
        };
        let counters = pool.measurements().counters;
        BTreeMap::from([
            (
                "ipc_sent_messages".into(),
                counters.ipc_sent_messages as f64,
            ),
            (
                "ipc_received_messages".into(),
                counters.ipc_received_messages as f64,
            ),
            ("ipc_sent_bytes".into(), counters.ipc_sent_bytes as f64),
            (
                "ipc_received_bytes".into(),
                counters.ipc_received_bytes as f64,
            ),
        ])
    }

    pub fn start(&self) -> Result<()> {
        crate::perf_support::time::instruction_control("enable")?;
        let mut state = self.state.lock_recover();
        state.baseline = self.witness.snapshot();
        state.cpu_before = cpu_ms().unwrap_or_default();
        state.worker_cpu_before = self.worker_cpu_ms();
        state.started = Instant::now();
        state.active = true;
        state.labels.clear();
        state.staged_bytes = 0;
        state.staged_commits.clear();
        state.commit_sql.clear();
        state.alloc_before = crate::GLOBAL_ALLOCATOR.stats();
        state.read_calls = 0;
        state.read_operations.clear();
        state.ipc_before = self.ipc_metrics();
        state.bytes_read = 0;
        state.restored_nodes = 0;
        state.restored_bytes = 0;
        Ok(())
    }

    pub fn stop_instructions(&self) -> Result<()> {
        crate::perf_support::time::instruction_control("disable")
    }

    pub fn tool(&self, tool: &str, args: &serde_json::Value) {
        let mut state = self.state.lock_recover();
        if !state.active {
            return;
        }
        if self.phase == Phase::Resume && state.resume.is_none() {
            state.resume = Some(Resume {
                latency_ms: state.started.elapsed().as_secs_f64() * 1000.0,
                bytes_read: state.bytes_read,
                restored_nodes: state.restored_nodes,
                restored_bytes: state.restored_bytes,
            });
        }
        state.tool_inputs.push(ToolCallSpec {
            tool: tool.to_owned(),
            args: args.clone(),
        });
    }

    pub fn staged(&self, commit: StagedCommit) {
        let mut state = self.state.lock_recover();
        if state.active {
            state.staged_bytes += commit
                .by_kind
                .values()
                .map(|share| share.bytes)
                .sum::<u64>();
            state.staged_commits.push(commit);
        }
    }

    pub fn delegated(
        &self,
        label: lash_durable::CommitLabel,
        before: stats_alloc::Stats,
        started: Instant,
    ) {
        let elapsed = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        let alloc = crate::GLOBAL_ALLOCATOR.stats() - before;
        let mut state = self.state.lock_recover();
        if state.active
            && let Some(commit) = state
                .commit_sql
                .iter_mut()
                .rev()
                .find(|commit| commit.label == label.as_str())
        {
            commit.delegate_allocations = Some(alloc.allocations as u64);
            commit.delegate_allocated_bytes = Some(alloc.bytes_allocated as u64);
            commit.delegate_elapsed_nanos = Some(elapsed);
        }
    }

    pub fn read(&self, receipt: &sql::Receipt) {
        let mut state = self.state.lock_recover();
        if state.active {
            state.bytes_read += receipt.result_bytes;
            state.read_calls += 1;
            *state
                .read_operations
                .entry(receipt.operation.clone())
                .or_default() += 1;
        }
    }

    pub fn hydrated(&self, nodes: u64, bytes: u64) {
        let mut state = self.state.lock_recover();
        if state.active {
            state.restored_nodes += nodes;
            state.restored_bytes += bytes;
        }
    }

    pub fn before_commit(&self, label: lash_durable::CommitLabel) {
        if std::env::var_os("LASH_PERF_COMMIT_TRACE").is_some() {
            eprintln!("MEASURE_COMMIT_BEGIN {}", label.as_str());
        }
        // Admission also writes x_start: a Once member cut immediately after
        // admission must be interrupted, even if its body has not run. A zero
        // completed-call prefix therefore cuts before that transaction.
        if self.phase == Phase::Prefix
            && self.n / 2 == 0
            && zero_prefix_cut_label(label)
            && self.state.lock_recover().active
        {
            self.kill_at_prefix();
        }
    }

    pub fn commit(&self, receipt: &sql::Receipt, outcome: &str) {
        if std::env::var_os("LASH_PERF_COMMIT_TRACE").is_some() {
            eprintln!("MEASURE_COMMIT_END {} {outcome}", receipt.operation);
        }
        let cut = {
            let mut state = self.state.lock_recover();
            if !state.active || outcome != "success" || receipt.owner.is_none() {
                return;
            }
            *state.labels.entry(receipt.operation.clone()).or_default() += 1;
            state.commit_sql.push(CommitSql {
                label: receipt.operation.clone(),
                elapsed_nanos: receipt.elapsed_nanos,
                installed_graph_bytes: receipt.newly_installed_graph_bytes,
                rows_affected: receipt.rows_affected,
                bound_bytes: receipt.bound_bytes,
                result_bytes: receipt.result_bytes,
                statements: receipt.statements,
                unretained_shape_statements: receipt.unretained_shape_statements,
                delegate_allocations: None,
                delegate_allocated_bytes: None,
                delegate_elapsed_nanos: None,
                shapes: receipt
                    .shapes
                    .iter()
                    .filter_map(|(key, shape)| {
                        serde_json::to_value(shape)
                            .ok()
                            .map(|value| (key.clone(), value))
                    })
                    .collect(),
            });
            self.phase == Phase::Prefix
                && self.n / 2 > 0
                && receipt.operation == lash_durable::CommitLabel::ROUND_OUTCOME.as_str()
                && state.tool_inputs.len() == self.n / 2
        };
        if cut {
            // The killed call cannot return through the decorator; retain its
            // final SQL payload here, exactly once.
            self.read(receipt);
            self.kill_at_prefix();
        }
    }

    fn kill_at_prefix(&self) {
        let written = self
            .stop_instructions()
            .and_then(|()| super::write(&self.out, &self.snapshot()));
        if let Err(error) = written {
            eprintln!("write prefix evidence before kill: {error:#}");
            std::process::exit(1);
        }
        // This callback never returns to the drive; workers and owner are
        // SIGKILLed without draining or completing any more admitted work.
        crate::replay::workers::kill_code_workers();
        #[expect(
            unsafe_code,
            reason = "raise SIGKILL in this benchmark owner at its prefix cut"
        )]
        unsafe {
            libc::raise(libc::SIGKILL);
        }
        std::process::exit(1);
    }

    pub fn snapshot(&self) -> Sample {
        let state = self.state.lock_recover();
        let work = self.witness.snapshot();
        let before = &state.baseline;
        let wall_ms = state.started.elapsed().as_secs_f64() * 1000.0;
        let host_cpu = cpu_ms().unwrap_or_default() - state.cpu_before;
        let worker_cpu = self.worker_cpu_ms() - state.worker_cpu_before;
        let frames = work
            .store_pages_written
            .saturating_sub(before.store_pages_written);
        let alloc = crate::GLOBAL_ALLOCATOR.stats() - state.alloc_before;
        let mut metrics = self.ipc_metrics();
        for (key, value) in &mut metrics {
            *value -= state.ipc_before.get(key).copied().unwrap_or_default();
        }
        for (operation, calls) in &state.read_operations {
            metrics.insert(format!("durable_calls.{operation}"), *calls as f64);
        }
        metrics.extend(BTreeMap::from([
            ("wall_ms".to_owned(), wall_ms),
            ("host_allocations".to_owned(), alloc.allocations as f64),
            (
                "host_allocated_bytes".to_owned(),
                alloc.bytes_allocated as f64,
            ),
            ("durable_calls".to_owned(), state.read_calls as f64),
            ("cpu_ms".to_owned(), host_cpu + worker_cpu),
            ("host_cpu_ms".to_owned(), host_cpu),
            ("worker_cpu_ms".to_owned(), worker_cpu),
            (
                "sql_statements".to_owned(),
                work.sql_statements.saturating_sub(before.sql_statements) as f64,
            ),
            (
                "sql_rows_read".to_owned(),
                work.sql_rows_read.saturating_sub(before.sql_rows_read) as f64,
            ),
            (
                "sql_rows_written".to_owned(),
                work.sql_rows_written
                    .saturating_sub(before.sql_rows_written) as f64,
            ),
            ("wal_frames".to_owned(), frames as f64),
            (
                "wal_bytes".to_owned(),
                (frames * (*self.page_size.lock_recover() + 24)) as f64,
            ),
            ("staged_bytes".to_owned(), state.staged_bytes as f64),
            ("bytes_read".to_owned(), state.bytes_read as f64),
            (
                "logical_owner_commits".to_owned(),
                state.labels.values().sum::<u64>() as f64,
            ),
            ("model_calls".to_owned(), self.book.total_calls() as f64),
        ]));
        Sample {
            n: self.n,
            metrics,
            staged_commits: state.staged_commits.clone(),
            commit_sql: state.commit_sql.clone(),
            owner_commit_labels: state.labels.clone(),
            tool_inputs: state.tool_inputs.clone(),
            resume: state.resume.clone(),
            prefix: None,
        }
    }
}

/// Nanosecond-resolution host CPU, unlike /proc's coarse scheduler ticks.
fn cpu_ms() -> Result<f64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    #[expect(
        unsafe_code,
        reason = "clock_gettime writes only this initialized timespec"
    )]
    let status = unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) };
    (status == 0)
        .then_some(time.tv_sec as f64 * 1000.0 + time.tv_nsec as f64 / 1_000_000.0)
        .context("read the host process CPU clock")
}

fn zero_prefix_cut_label(label: lash_durable::CommitLabel) -> bool {
    matches!(
        label,
        lash_durable::CommitLabel::CELL_SNAPSHOT_ADMIT
            | lash_durable::CommitLabel::MODEL_DONE_CELL_SNAPSHOT_ADMIT
    )
}

#[cfg(test)]
mod tests {
    /// Zero-prefix recovery must cut before the first Once x_start, including
    /// a first cell snapshot that carries the model's response.
    #[test]
    fn zero_prefix_cuts_before_the_snapshot_carrying_model_done() {
        assert!(super::zero_prefix_cut_label(
            lash_durable::CommitLabel::MODEL_DONE_CELL_SNAPSHOT_ADMIT
        ));
    }
}
