//! Endpoint counters: store-owned bytes, allocator traffic and Linux resources.
use anyhow::{Context as _, Result};
use lash_protocol_types::sync::MutexExt as _;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

pub(super) struct Meter {
    database: PathBuf,
    out: Mutex<std::fs::File>,
    started: Instant,
    closed: std::sync::atomic::AtomicBool,
    pub rounds: usize,
    pub bytes: usize,
    pub every: usize,
    previous: Mutex<Option<stats_alloc::Stats>>,
    request_text_bytes: std::sync::atomic::AtomicUsize,
    cumulative_request_text_bytes: std::sync::atomic::AtomicUsize,
    pub profile_from_round: usize,
    profile_args: lash_perf::perf_support::dhat::ProfileArgs,
    profile: Mutex<Option<lash_perf::perf_support::dhat::ProfileWindow>>,
    native_memory_dir: Option<PathBuf>,
}
impl Meter {
    pub fn new(args: &super::Args) -> Result<Self> {
        if let Some(dir) = &args.native_memory_dir {
            std::fs::create_dir_all(dir)?;
        }
        Ok(Self {
            database: args.database.clone(),
            out: Mutex::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&args.out)?,
            ),
            started: Instant::now(),
            closed: std::sync::atomic::AtomicBool::new(false),
            rounds: args.rounds,
            bytes: args.payload_bytes,
            every: args.checkpoint_every,
            previous: Mutex::new(None),
            request_text_bytes: std::sync::atomic::AtomicUsize::new(0),
            cumulative_request_text_bytes: std::sync::atomic::AtomicUsize::new(0),
            profile_from_round: args.profile_from_round,
            profile_args: lash_perf::perf_support::dhat::ProfileArgs {
                dhat_out: args.profile.dhat_out.clone(),
                dhat_frames: args.profile.dhat_frames,
                future_out: args.profile.future_out.clone(),
                future_top: args.profile.future_top,
            },
            profile: Mutex::new(None),
            native_memory_dir: args.native_memory_dir.clone(),
        })
    }
    pub fn opened(&self) {
        self.closed
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn closed(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn install_profile(&self, window: Option<lash_perf::perf_support::dhat::ProfileWindow>) {
        *self.profile.lock_recover() = window;
    }
    pub fn start_profile(&self, done: usize) -> Result<()> {
        if self.profile_from_round > 0 && done >= self.profile_from_round {
            let mut window = self.profile.lock_recover();
            if window.is_none() {
                *window = Some(lash_perf::perf_support::dhat::ProfileWindow::start(
                    "growturn-host",
                    "late-round-through-close-and-core-shutdown; earlier allocations excluded",
                    &self.profile_args,
                )?);
                self.write(&json!({"kind":"profile-start", "pid":std::process::id(),
                    "rounds_done":done, "earlier_allocations_captured":false}))?;
            }
        }
        Ok(())
    }
    pub fn finish_profile(&self, completed: bool) -> Result<()> {
        if let Some(window) = self.profile.lock_recover().take() {
            window.finish(completed)?;
        }
        Ok(())
    }
    fn write(&self, row: &Value) -> Result<()> {
        let mut out = self.out.lock_recover();
        serde_json::to_writer(&mut *out, row)?;
        writeln!(out)?;
        out.flush()?;
        Ok(())
    }
    pub fn failure(&self, error: &anyhow::Error) -> Result<()> {
        self.write(&json!({"kind":"failure", "pid":std::process::id(), "error":format!("{error:#}"), "errors":1}))
    }
    /// Text actually carried by the model request, without encoding or
    /// copying it: the inherent transcript payload beside allocation traffic.
    pub fn model_request(&self, request: &super::LlmRequest) {
        let bytes = request
            .messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                super::LlmContentBlock::Text { text, .. } => Some(text.len()),
                _ => None,
            })
            .sum();
        self.request_text_bytes
            .store(bytes, std::sync::atomic::Ordering::Relaxed);
        self.cumulative_request_text_bytes
            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn sample(&self, phase: &str, done: usize) -> Result<()> {
        let alloc = lash_perf::GLOBAL_ALLOCATOR.stats();
        let native = self
            .native_memory_dir
            .as_ref()
            .map(|dir| super::native::snapshot(dir, phase, done))
            .transpose()?;
        let traffic =
            {
                let mut previous = self.previous.lock_recover();
                let traffic = previous.map(|before| json!({
                "allocated_bytes":alloc.bytes_allocated.saturating_sub(before.bytes_allocated),
                "freed_bytes":alloc.bytes_deallocated.saturating_sub(before.bytes_deallocated),
                "allocations":alloc.allocations.saturating_sub(before.allocations),
                "deallocations":alloc.deallocations.saturating_sub(before.deallocations),
            }));
                *previous = Some(alloc);
                traffic
            };
        let processes = processes()?;
        let store = if self.database.exists() {
            Some(store(&self.database)?)
        } else {
            None
        };
        self.write(&json!({"kind":"checkpoint", "phase":phase, "rounds":self.rounds,
            "rounds_done":done, "payload_bytes":self.bytes, "pid":std::process::id(),
            "model_request_text_bytes":self.request_text_bytes.load(std::sync::atomic::Ordering::Relaxed),
            "cumulative_model_request_text_bytes":self.cumulative_request_text_bytes.load(std::sync::atomic::Ordering::Relaxed),
            "elapsed_ns":self.started.elapsed().as_nanos(),
            "unix_time_ns":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos(),
            "loadavg":std::fs::read_to_string("/proc/loadavg")?,
            "cpu_pressure":std::fs::read_to_string("/proc/pressure/cpu")?,
            "traffic_since_previous_checkpoint":traffic, "allocator":lash_perf::ALLOCATION_MODE,
            "live_requested_bytes":alloc.bytes_allocated as i128 - alloc.bytes_deallocated as i128,
            "allocated_bytes":alloc.bytes_allocated, "freed_bytes":alloc.bytes_deallocated,
            "allocations":alloc.allocations, "deallocations":alloc.deallocations,
            "profile_heap":lash_perf::perf_support::dhat::profiled_heap_sample(),
            "native_memory":native,
            "store":store, "processes":processes,
            "close_tombstone_observed":self.closed.load(std::sync::atomic::Ordering::Relaxed),
            "endpoint_scope":"host Rust allocator including harness; proc includes worker native memory",
            "allocation_scope":"cumulative traffic, not temporary live bytes or unique payload bytes"}))
    }
}
fn store(path: &std::path::Path) -> Result<Value> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::from_secs(15))?;
    // One read transaction makes all store quantities describe the same head.
    db.execute_batch("BEGIN")?;
    let scalar = |sql: &str| -> Result<i64> { Ok(db.query_row(sql, [], |r| r.get(0))?) };
    let counts = |condition: &str| {
        scalar(&format!(
            "SELECT count(*) FROM session_runs WHERE {condition}"
        ))
    };
    let mut by_kind = std::collections::BTreeMap::<String, (usize, usize)>::new();
    let mut outcome_refs = Vec::new();
    let mut marker_leaf_bytes = 0;
    {
        let mut statement = db.prepare("SELECT hash, content FROM blobs WHERE encoding=1")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(1)?;
            let envelope = lash_core_store::checkpoint_graph::parse_envelope(&bytes, None)?;
            let name = envelope
                .kind
                .spec()
                .context("unknown canonical node kind")?
                .name;
            let tally = by_kind.entry(name.to_owned()).or_default();
            tally.0 += 1;
            tally.1 += bytes.len();
            if envelope.kind == lash_core_store::checkpoint_graph::NodeKind::CALL_OUTCOME {
                outcome_refs.push(row.get::<_, String>(0)?);
            }
            if envelope.kind == lash_core_store::checkpoint_graph::NodeKind::TEXT_LEAF
                && bytes
                    .windows(b"grow-payload:".len())
                    .any(|w| w == b"grow-payload:")
            {
                marker_leaf_bytes += bytes.len();
            }
        }
    }
    // CallOutcome roots include worker effects, which need not use the host
    // run_records table. UNION deduplicates shared descendants across roots.
    let outcome_bytes: i64 = db.query_row(
        "WITH RECURSIVE owned(hash) AS (
        SELECT value FROM json_each(?1)
        UNION SELECT child_hash FROM blob_edges JOIN owned ON parent_hash=owned.hash
    ) SELECT COALESCE(sum(length(content)),0) FROM blobs JOIN owned USING(hash)",
        [serde_json::to_string(&outcome_refs)?],
        |r| r.get(0),
    )?;
    let row = json!({
        "history_nodes":scalar("SELECT count(*) FROM graph_nodes")?,
        "history_body_bytes":scalar("SELECT COALESCE(sum(body_bytes),0) FROM graph_nodes")?,
        "blob_nodes":scalar("SELECT count(*) FROM blobs")?,
        "blob_bytes":scalar("SELECT COALESCE(sum(length(content)),0) FROM blobs")?,
        "tool_and_cell_outcome_reachable_bytes":outcome_bytes,
        "tool_marker_text_leaf_bytes":marker_leaf_bytes,
        "canonical_by_kind_nodes_bytes":by_kind,
        "completed":counts("terminal_kind='answered'")?,
        "errors":counts("terminal_kind IN ('failed','cancelled')")?,
        "unfinished":counts("terminal_kind IS NULL")?,
    });
    db.execute_batch("ROLLBACK")?;
    Ok(row)
}
fn process(pid: u32, role: &str) -> Result<Value> {
    let base = PathBuf::from(format!("/proc/{pid}"));
    let status = std::fs::read_to_string(base.join("status"))?;
    let kb = |key: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse::<u64>().ok())
    };
    let count = |name| -> Result<usize> { Ok(std::fs::read_dir(base.join(name))?.count()) };
    Ok(
        json!({"pid":pid, "role":role, "rss_kib":kb("VmRSS:"), "hwm_kib":kb("VmHWM:"),
        "descriptors":count("fd")?, "threads":count("task")?}),
    )
}
fn processes() -> Result<Vec<Value>> {
    let host = std::process::id();
    let mut rows = vec![process(host, "host")?];
    // Children can be spawned by any Tokio thread, so inspect every task's
    // child list, deduplicate PIDs, and identify the actual worker argv.
    let mut children = std::collections::BTreeSet::new();
    for task in std::fs::read_dir(format!("/proc/{host}/task"))? {
        if let Ok(text) = std::fs::read_to_string(task?.path().join("children")) {
            children.extend(
                text.split_whitespace()
                    .filter_map(|s| s.parse::<u32>().ok()),
            );
        }
    }
    for pid in children {
        let Ok(argv) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if argv.split(|b| *b == 0).any(|s| s == b"--lash-vm-worker") {
            match process(pid, "vm-worker") {
                Ok(row) => rows.push(row),
                Err(error) => rows.push(
                    json!({"pid":pid,"role":"vm-worker","observation_error":error.to_string()}),
                ),
            }
        }
    }
    Ok(rows)
}
