//! Optional future scheduling and physical queue diagnostics for existing
//! populations. `LASH_PERF_ASYNC_RECEIPT` selects a sidecar; setting
//! `LASH_PERF_ASYNC_TIMING=off` runs the same population without these probes.
use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use lash_core::perf_witness::queues;
use serde::Serialize;
use tokio_metrics::TaskMonitor;

const KIND_LIMIT: usize = 64;
static ENABLED: AtomicBool = AtomicBool::new(false);
type SharedMonitors = Arc<Mutex<Monitors>>;
static CURRENT: LazyLock<Mutex<Option<SharedMonitors>>> = LazyLock::new(|| Mutex::new(None));
#[derive(Default)]
struct Monitors {
    kinds: BTreeMap<&'static str, TaskMonitor>,
    unmonitored_operations: u64,
}

/// Wrap the actual operation future, below the population's aggregate root.
/// Monitors survive a dropped future, retaining its partial scheduling metrics.
pub fn observe<T>(kind: &'static str, future: impl Future<Output = T>) -> impl Future<Output = T> {
    let monitor = if ENABLED.load(Ordering::Acquire) {
        CURRENT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|state| {
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if !state.kinds.contains_key(kind) && state.kinds.len() == KIND_LIMIT {
                    state.unmonitored_operations = state.unmonitored_operations.saturating_add(1);
                    return None;
                }
                Some(state.kinds.entry(kind).or_default().clone())
            })
    } else {
        None
    };
    match monitor {
        Some(monitor) => futures_util::future::Either::Left(monitor.instrument(future)),
        None => futures_util::future::Either::Right(future),
    }
}

#[derive(Debug, Serialize)]
pub struct TaskReceipt {
    pub operation_kind: &'static str,
    pub instrumented_futures_count: u64,
    /// Futures dropped, including those dropped after successful completion.
    pub dropped_futures_count: u64,
    pub polls_count: u64,
    pub busy_poll_ns_sum: u64,
    pub scheduled_delay_ns_sum: u64,
    pub idle_ns_sum: u64,
    pub first_poll_delay_ns_sum: u64,
}
impl TaskReceipt {
    fn new(kind: &'static str, monitor: &TaskMonitor) -> Self {
        let metrics = monitor.cumulative();
        let nanos =
            |duration: std::time::Duration| duration.as_nanos().min(u128::from(u64::MAX)) as u64;
        Self {
            operation_kind: kind,
            instrumented_futures_count: metrics.instrumented_count,
            dropped_futures_count: metrics.dropped_count,
            polls_count: metrics.total_poll_count,
            busy_poll_ns_sum: nanos(metrics.total_poll_duration),
            scheduled_delay_ns_sum: nanos(metrics.total_scheduled_duration),
            idle_ns_sum: nanos(metrics.total_idle_duration),
            first_poll_delay_ns_sum: nanos(metrics.total_first_poll_delay),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Receipt {
    pub kind: &'static str,
    pub enabled: bool,
    pub process_id: u32,
    pub window: &'static str,
    pub window_elapsed_ns_sample: u128,
    pub timing_semantics: &'static str,
    pub configured_queue_record_limit: usize,
    pub configured_operation_kind_limit: usize,
    pub unmonitored_operations_count: u64,
    pub tasks: Vec<TaskReceipt>,
    pub queues: queues::Snapshot,
}

/// One command's process window. Each child starts its own window and writes
/// its own sidecar; async task totals do not imply child-thread CPU coverage.
pub struct Capture {
    path: PathBuf,
    started: Instant,
    state: Option<SharedMonitors>,
    queues: Option<queues::Collector>,
}
impl Capture {
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let Some(path) = std::env::var_os("LASH_PERF_ASYNC_RECEIPT") else {
            return Ok(None);
        };
        let enabled = std::env::var("LASH_PERF_ASYNC_TIMING").as_deref() != Ok("off");
        let mut path = PathBuf::from(path);
        if std::env::args().any(|arg| matches!(arg.as_str(), "boundary-worker" | "latency-worker"))
        {
            let name = path
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("async receipt needs a filename"))?
                .to_string_lossy();
            path.set_file_name(format!("{name}.pid-{}.json", std::process::id()));
        }
        Self::open(path, enabled).map(Some)
    }

    fn open(path: PathBuf, enabled: bool) -> anyhow::Result<Self> {
        let started = Instant::now();
        let (state, queues) = if enabled {
            let mut current = CURRENT
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            anyhow::ensure!(
                current.is_none(),
                "an async diagnostic window is already active"
            );
            let queues = queues::Collector::install()?;
            let state = Arc::new(Mutex::new(Monitors::default()));
            *current = Some(Arc::clone(&state));
            ENABLED.store(true, Ordering::Release);
            (Some(state), Some(queues))
        } else {
            (None, None)
        };
        Ok(Self {
            path,
            started,
            state,
            queues,
        })
    }

    pub fn finish(self) -> anyhow::Result<()> {
        let mut tasks = Vec::new();
        let mut unmonitored = 0;
        if let Some(state) = &self.state {
            let state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            tasks = state
                .kinds
                .iter()
                .map(|(kind, monitor)| TaskReceipt::new(kind, monitor))
                .collect();
            unmonitored = state.unmonitored_operations;
        }
        let receipt = Receipt {
            kind: "lash.async-operations",
            enabled: self.state.is_some(),
            process_id: std::process::id(),
            window: "command invocation in this process, excluding receipt serialization",
            window_elapsed_ns_sample: self.started.elapsed().as_nanos(),
            timing_semantics: "task durations are per-kind sums over instrumented futures, not CPU; queue durations are physical-operation samples; gate children overlap their parent; worker service excludes measured gate waits; postgres service includes pool checkout and SQL; incomplete records include cancelled waits",
            configured_queue_record_limit: queues::RECORD_LIMIT,
            configured_operation_kind_limit: KIND_LIMIT,
            unmonitored_operations_count: unmonitored,
            tasks,
            queues: self
                .queues
                .as_ref()
                .map_or_else(Default::default, queues::Collector::snapshot),
        };
        std::fs::write(
            &self.path,
            format!("{}\n", serde_json::to_string_pretty(&receipt)?),
        )?;
        Ok(())
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        if self.state.is_some() {
            ENABLED.store(false, Ordering::Release);
            CURRENT
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The receipt rule is per-operation-kind scheduling, including futures
    // dropped while pending. No shared-host timing ceiling is asserted.
    #[tokio::test]
    async fn scheduling_receipt_keeps_kind_and_cancelled_future_metrics() {
        let dir = tempfile::tempdir().expect("receipt directory");
        let path = dir.path().join("async.json");
        let capture = Capture::open(path.clone(), true).expect("capture");
        observe("ready", async {}).await;
        let mut pending = Box::pin(observe("cancelled", std::future::pending::<()>()));
        std::future::poll_fn(|cx| {
            assert!(pending.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(pending);
        capture.finish().expect("write receipt");
        let receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("read receipt"))
                .expect("decode receipt");
        let tasks = receipt["tasks"].as_array().expect("task records");
        assert_eq!(tasks.len(), 2);
        for task in tasks {
            assert_eq!(task["instrumented_futures_count"], 1);
            assert_eq!(task["polls_count"], 1);
            assert_eq!(task["dropped_futures_count"], 1);
        }
        assert_eq!(tasks[0]["operation_kind"], "cancelled");
        assert_eq!(tasks[1]["operation_kind"], "ready");
    }
}
