//! Host-owned pool measurements. Execution receipts are opt-in and bounded.
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct PoolCounters {
    pub checkouts: u64,
    pub queue_waits: u64,
    pub queue_delay_ns: u64,
    pub ipc_sent_messages: u64,
    pub ipc_sent_bytes: u64,
    pub ipc_received_messages: u64,
    pub ipc_received_bytes: u64,
    pub resets: u64,
    pub reuses: u64,
    /// Supervisor-observed EOF/exit, distinct from a guest error or limit.
    pub crashes: u64,
    pub discards: u64,
    pub replacements: u64,
    pub cell_executions: u64,
    pub process_executions: u64,
    pub receipts_dropped: u64,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionClass {
    Cell,
    Process,
}

/// Recorded on the worker's first acknowledged compute phase, once per lease.
#[derive(Clone, Debug, Serialize)]
pub struct ExecutionReceipt {
    pub lease: u64,
    pub class_name: ExecutionClass,
    pub owner: String,
    pub pid: u32,
    /// Linux process start ticks, matching the independent /proc sampler.
    pub process_epoch: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PoolMeasurements {
    pub exporter: &'static str,
    pub epoch: u64,
    pub workers: usize,
    pub idle: usize,
    pub queued_items: usize,
    pub queued_bytes: usize,
    pub counters: PoolCounters,
    pub units: BTreeMap<&'static str, &'static str>,
    pub executions: Vec<ExecutionReceipt>,
}

#[derive(Default)]
pub(crate) struct Measurements {
    pub counters: PoolCounters,
    pub executions: Vec<ExecutionReceipt>,
    pub receipt_capacity: usize,
    pub pending_replacements: usize,
}
pub(crate) type SharedMeasurements = Arc<Mutex<Measurements>>;

impl Measurements {
    pub fn execution(&mut self, receipt: ExecutionReceipt) {
        match receipt.class_name {
            ExecutionClass::Cell => self.counters.cell_executions += 1,
            ExecutionClass::Process => self.counters.process_executions += 1,
        }
        if self.receipt_capacity == 0 {
            return;
        }
        if self.executions.len() < self.receipt_capacity {
            self.executions.push(receipt);
        } else {
            self.counters.receipts_dropped += 1;
        }
    }
}

pub(crate) fn units() -> BTreeMap<&'static str, &'static str> {
    [
        ("workers", "count"),
        ("idle", "count"),
        ("queued_items", "count"),
        ("queued_bytes", "bytes"),
        ("checkouts", "count"),
        ("queue_waits", "count"),
        ("queue_delay_ns", "nanoseconds"),
        ("ipc_sent_messages", "count"),
        ("ipc_sent_bytes", "bytes"),
        ("ipc_received_messages", "count"),
        ("ipc_received_bytes", "bytes"),
        ("resets", "count"),
        ("reuses", "count"),
        ("crashes", "count"),
        ("discards", "count"),
        ("replacements", "count"),
        ("cell_executions", "count"),
        ("process_executions", "count"),
        ("receipts_dropped", "count"),
    ]
    .into()
}
