//! Host-owned pool measurements. Execution receipts are opt-in and bounded.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct PoolCounters {
    pub checkouts: u64,
    pub queue_waits: u64,
    pub queue_delay_ns: u64,
    pub worker_work: lash_vm_protocol::WorkerWork,
    /// Maximum simultaneously owned broker frame buffers (not RSS or heap).
    pub peak_buffer_bytes: u64,
    pub ipc_sent_messages: u64,
    pub ipc_sent_bytes: u64,
    pub ipc_received_messages: u64,
    pub ipc_received_bytes: u64,
    /// Fixed message-kind bins; includes an explicit undecodable receive bin.
    pub ipc: IpcTraffic,
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
        ("worker_work", "mixed; see WorkerWork"),
        ("peak_buffer_bytes", "bytes"),
        ("ipc_sent_messages", "count"),
        ("ipc_sent_bytes", "bytes"),
        ("ipc_received_messages", "count"),
        ("ipc_received_bytes", "bytes"),
        ("ipc", "encoded frames and bytes by message kind"),
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

/// Exact encoded frame traffic, including the transport header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameTraffic {
    pub frames: u64,
    pub bytes: u64,
}
impl FrameTraffic {
    fn record(&mut self, bytes: usize) {
        self.frames += 1;
        self.bytes += bytes as u64;
    }
}

/// Bounded histograms in declaration order, independent of socket read/write
/// boundaries. No decoding, strings or per-frame allocation for classification.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IpcTraffic {
    pub sent: [FrameTraffic; 10],
    pub received: [FrameTraffic; 17],
}
impl IpcTraffic {
    pub(crate) const RUN_KIND: usize = 2;
    pub const SENT_KINDS: [&'static str; 10] = [
        "start",
        "prepare",
        "run",
        "deliver",
        "host_answer",
        "pages",
        "seal",
        "cancel",
        "reset",
        "shutdown",
    ];
    pub const RECEIVED_KINDS: [&'static str; 17] = [
        "refused",
        "progress",
        "limit_exceeded",
        "ready",
        "started",
        "host_read",
        "pages",
        "printed",
        "parked",
        "slice",
        "ended",
        "delivered",
        "sealed",
        "cancelled",
        "reset_done",
        "prepared",
        "undecodable",
    ];
    pub(crate) fn parent_kind(message: &lash_vm_protocol::ParentMessage) -> usize {
        use lash_vm_protocol::ParentMessage as M;
        match message {
            M::Start(_) => 0,
            M::Prepare { .. } => 1,
            M::Run { .. } => Self::RUN_KIND,
            M::Deliver { .. } => 3,
            M::HostAnswer { .. } => 4,
            M::Pages { .. } => 5,
            M::Seal => 6,
            M::Cancel => 7,
            M::Reset => 8,
            M::Shutdown => 9,
        }
    }
    pub(crate) fn sent(&mut self, kind: usize, bytes: usize) {
        self.sent[kind].record(bytes);
    }
    pub(crate) fn received(
        &mut self,
        message: Option<&lash_vm_protocol::WorkerMessage>,
        bytes: usize,
    ) {
        use lash_vm_protocol::WorkerMessage as M;
        let kind = match message {
            Some(M::Refused { .. }) => 0,
            Some(M::Progress { .. }) => 1,
            Some(M::LimitExceeded { .. }) => 2,
            Some(M::Ready { .. }) => 3,
            Some(M::Started) => 4,
            Some(M::HostRead { .. }) => 5,
            Some(M::Pages { .. }) => 6,
            Some(M::Printed { .. }) => 7,
            Some(M::Parked { .. }) => 8,
            Some(M::Slice { .. }) => 9,
            Some(M::Ended { .. }) => 10,
            Some(M::Delivered) => 11,
            Some(M::Sealed { .. }) => 12,
            Some(M::Cancelled) => 13,
            Some(M::ResetDone { .. }) => 14,
            Some(M::Prepared { .. }) => 15,
            None => 16,
        };
        self.received[kind].record(bytes);
    }
    /// Refuse counter resets, missing frames, or a decode failure.
    pub fn difference(&self, before: &Self) -> Option<Self> {
        let mut result = Self::default();
        for ((out, after), before) in result
            .sent
            .iter_mut()
            .chain(&mut result.received)
            .zip(self.sent.iter().chain(&self.received))
            .zip(before.sent.iter().chain(&before.received))
        {
            out.frames = after.frames.checked_sub(before.frames)?;
            out.bytes = after.bytes.checked_sub(before.bytes)?;
        }
        (result.received[16] == FrameTraffic::default()).then_some(result)
    }
    pub fn totals(&self) -> FrameTraffic {
        self.sent
            .iter()
            .chain(&self.received)
            .fold(FrameTraffic::default(), |mut sum, row| {
                sum.frames += row.frames;
                sum.bytes += row.bytes;
                sum
            })
    }
}
