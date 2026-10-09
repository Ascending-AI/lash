use std::path::PathBuf;
use std::time::Duration;

use crate::PoolError;
use lash_vm_protocol::{ProtocolBounds, RunBounds};

/// An explicit helper executable, or the host executable with an early entry.
#[derive(Clone, Debug)]
pub struct WorkerEntry {
    pub executable: PathBuf,
    pub args: Vec<String>,
}

impl WorkerEntry {
    pub fn helper(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            args: Vec::new(),
        }
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "host explicitly selects its own early re-exec entry"
    )]
    pub fn reexec() -> Result<Self, PoolError> {
        Ok(Self {
            executable: std::env::current_exe().map_err(PoolError::io)?,
            args: Vec::new(),
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Deadlines {
    pub checkout: Duration,
    pub compute: Duration,
    pub serialization: Duration,
    pub cancel_grace: Duration,
    pub cumulative_cpu: Duration,
    pub max_attempts: u32,
}

impl Deadlines {
    /// Bounded host defaults, retained after the FIG-4162 integrated matrix.
    /// Checkout 5 s, compute 30 s, serialization 5 s, cancel grace 100 ms,
    /// cumulative CPU 10 s, and three attempts. These are provisional:
    /// synthetic service times do not bound arbitrary guest computation;
    /// hosts should select deadlines for their admitted workload.
    pub const fn standard() -> Self {
        Self {
            checkout: Duration::from_secs(5),
            compute: Duration::from_secs(30),
            serialization: Duration::from_secs(5),
            cancel_grace: Duration::from_millis(100),
            cumulative_cpu: Duration::from_secs(10),
            max_attempts: 3,
        }
    }
}

/// Worker-local transport and execution working policy.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerTuning {
    /// Parent reads while idle or awaiting a host projection. This is not compute spend.
    pub parent_wait: Duration,
    pub inbound_buffer_bytes: std::num::NonZeroUsize,
    pub parser_stack_base_bytes: usize,
    pub parser_stack_bytes_per_source_byte: usize,
    /// The charge units one `Run` exchange may spend before the machine
    /// returns to the parent, where a run cancel is observed.
    pub slice: u64,
}
impl WorkerTuning {
    /// Existing presets: parent wait 86,400 s; inbound buffer 16 KiB; parser
    /// stack 8 MiB plus 40,000 bytes/source byte; a slice of one million
    /// charge units. Parser slope is measured at 1.8x the worst observed
    /// frames; the other working values have no workload measurements.
    pub const fn standard() -> Self {
        Self {
            parent_wait: Duration::from_secs(86_400),
            inbound_buffer_bytes: std::num::NonZeroUsize::MIN.saturating_add(16 * 1024 - 1),
            parser_stack_base_bytes: 8 * 1024 * 1024,
            parser_stack_bytes_per_source_byte: 40_000,
            slice: 1_000_000,
        }
    }
}
impl Default for WorkerTuning {
    fn default() -> Self {
        Self::standard()
    }
}

#[derive(Clone, Debug)]
pub struct PoolConfig {
    pub entry: WorkerEntry,
    pub min_workers: usize,
    pub max_workers: usize,
    pub max_queue_items: usize,
    pub max_queue_bytes: usize,
    pub protocol: ProtocolBounds,
    /// The most any run's bounds may allow.
    pub run_bounds: RunBounds,
    pub deadlines: Deadlines,
    pub tuning: WorkerTuning,
    pub restart_window: Duration,
    pub max_restarts: usize,
}

impl PoolConfig {
    /// Prewarm one process, admit at most four, and bound waiting input to
    /// two items and eight MiB. Protocol bounds use `ProtocolBounds::standard`;
    /// run bounds are 50 million charge units, 64 MiB, 1,024 nested calls,
    /// 1,024 live tasks, 256 requests a park and 1,024 members a join;
    /// deadlines use `Deadlines::standard`; restarts allow eight per 60 seconds;
    /// working policy uses `WorkerTuning::standard`. FIG-4157/4162 measured
    /// synthetic workloads, not optimal concurrency or arbitrary guest spend.
    pub fn standard(entry: WorkerEntry) -> Self {
        Self {
            entry,
            min_workers: 1,
            max_workers: 4,
            max_queue_items: 2,
            max_queue_bytes: 8 * 1024 * 1024,
            protocol: ProtocolBounds::standard(),
            run_bounds: RunBounds {
                charge: 50_000_000,
                memory: 64 * 1024 * 1024,
                call_depth: 1024,
                live_tasks: 1024,
                requests_per_park: 256,
                join_members: 1024,
            },
            deadlines: Deadlines::standard(),
            tuning: WorkerTuning::standard(),
            restart_window: Duration::from_secs(60),
            max_restarts: 8,
        }
    }

    /// RLM/process preset: 64 MiB state, 128 MiB frames and queued bytes,
    /// 256 MiB decode allocations; other values follow `standard`.
    /// These larger allowances have no workload measurement behind them.
    pub fn rlm(entry: WorkerEntry) -> Self {
        let mut config = Self::standard(entry);
        config.protocol.max_vm_state_bytes = 64 * 1024 * 1024;
        config.protocol.decode.max_frame_bytes = 128 * 1024 * 1024;
        config.protocol.decode.max_allocation_bytes = 256 * 1024 * 1024;
        config.max_queue_bytes = 128 * 1024 * 1024;
        config
    }

    pub(crate) fn validate(&self) -> Result<(), PoolError> {
        if self.tuning.parent_wait.is_zero()
            || self.tuning.inbound_buffer_bytes.get() < lash_vm_protocol::FRAME_HEADER_BYTES
            || self
                .tuning
                .parser_stack_bytes_per_source_byte
                .checked_mul(64 * 1024)
                .and_then(|size| size.checked_add(self.tuning.parser_stack_base_bytes))
                .is_none_or(|size| size == 0)
            || self.protocol.decode.max_frame_bytes < lash_vm_protocol::FRAME_HEADER_BYTES as u32
            || self.protocol.decode.max_depth == 0
            || self.protocol.decode.max_nodes == 0
            || self.protocol.decode.max_allocation_bytes == 0
            || self.tuning.slice == 0
            || self.run_bounds.charge == 0
            || self.run_bounds.memory == 0
            || self.run_bounds.call_depth == 0
            || self.run_bounds.live_tasks == 0
            || self.run_bounds.requests_per_park == 0
            || self.run_bounds.join_members == 0
            || self.max_workers == 0
            || self.min_workers > self.max_workers
            || self.max_restarts == 0
            || self.restart_window.is_zero()
            || self.deadlines.checkout.is_zero()
            || self.deadlines.compute.is_zero()
            || self.deadlines.serialization.is_zero()
            || self.deadlines.cancel_grace.is_zero()
            || self.deadlines.cumulative_cpu.is_zero()
            || self.deadlines.max_attempts == 0
            || self.protocol.no_response_watchdog.is_zero()
        {
            return Err(PoolError::InvalidConfiguration);
        }
        Ok(())
    }
}
