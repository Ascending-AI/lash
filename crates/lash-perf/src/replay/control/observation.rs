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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sample {
    pub n: usize,
    pub metrics: BTreeMap<String, f64>,
    pub owner_commit_labels: BTreeMap<String, u64>,
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

    pub fn start(&self) {
        let mut state = self.state.lock_recover();
        state.baseline = self.witness.snapshot();
        state.cpu_before = cpu_ms().unwrap_or_default();
        state.worker_cpu_before = self.worker_cpu_ms();
        state.started = Instant::now();
        state.active = true;
        state.labels.clear();
        state.staged_bytes = 0;
        state.bytes_read = 0;
        state.restored_nodes = 0;
        state.restored_bytes = 0;
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

    pub fn staged(&self, bytes: u64) {
        let mut state = self.state.lock_recover();
        if state.active {
            state.staged_bytes += bytes;
        }
    }

    pub fn read(&self, receipt: &sql::Receipt) {
        let mut state = self.state.lock_recover();
        if state.active {
            state.bytes_read += receipt.result_bytes;
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
        // Admission also writes x_start: a Once member cut immediately after
        // admission must be interrupted, even if its body has not run. A zero
        // completed-call prefix therefore cuts before that transaction.
        if self.phase == Phase::Prefix
            && self.n / 2 == 0
            && label == lash_durable::CommitLabel::CELL_SNAPSHOT_ADMIT
            && self.state.lock_recover().active
        {
            self.kill_at_prefix();
        }
    }

    pub fn commit(&self, receipt: &sql::Receipt, outcome: &str) {
        let cut = {
            let mut state = self.state.lock_recover();
            if !state.active || outcome != "success" || receipt.owner.is_none() {
                return;
            }
            *state.labels.entry(receipt.operation.clone()).or_default() += 1;
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
        let sample = self.snapshot();
        if let Err(error) = super::write(&self.out, &sample) {
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
        Sample {
            n: self.n,
            metrics: BTreeMap::from([
                ("wall_ms".to_owned(), wall_ms),
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
            ]),
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
