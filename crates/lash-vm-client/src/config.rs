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

    /// What an execution meets past its cumulative CPU budget, whether the
    /// parent's accounting or the worker's own ceiling saw it.
    pub(crate) fn cpu_limit(&self) -> lash_vm_protocol::WorkerLimit {
        lash_vm_protocol::WorkerLimit::Cpu {
            bound_ms: millis(self.cumulative_cpu),
        }
    }
}

/// What an exchange meets when it computes or serializes past `bound`.
pub(crate) fn deadline_limit(bound: Duration) -> lash_vm_protocol::WorkerLimit {
    lash_vm_protocol::WorkerLimit::Deadline {
        bound_ms: millis(bound),
    }
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

/// Worker-local transport and execution working policy.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerTuning {
    pub diagnostic_bytes: lash_vm_protocol::DetailLimit,
    pub kernel_data: lash_kernel_doc::KernelDataLimits,
    /// Parent reads while idle or awaiting a host projection. This is not compute spend.
    pub parent_wait: Duration,
    pub inbound_buffer_bytes: std::num::NonZeroUsize,
    /// TypeScript source admission. Defaults: 64 KiB and 28 nesting levels.
    pub typescript_source_bytes: usize,
    pub typescript_source_nesting: usize,
    /// Python source admission. Defaults: 1 MiB and 96 nesting levels.
    pub python_source_bytes: usize,
    pub python_source_nesting: usize,
    pub parser_stack_base_bytes: usize,
    pub parser_stack_bytes_per_source_byte: usize,
    /// The charge units one `Run` exchange may spend before the machine
    /// returns to the parent, where a run cancel is observed.
    pub slice: u64,
    /// The worker's page residency on the persistent heap (FIG-6190).
    pub pages: PageBudgets,
    /// Whether a function among a tool call's arguments crosses as a
    /// function value (`K-EFF-002`, `K-CLO-004`) in every run the pool
    /// hosts, cells and processes alike. Off, a closure in a tool call's
    /// arguments raises `not_data`. Internal until the process tools take
    /// function values: standard is off.
    #[doc(hidden)]
    pub crosses_functions: bool,
}

/// A worker's host budgets for heap pages (`D-CAP`): resident bytes, never
/// the guest's logical memory. Passing one is a host outcome
/// (`HeapFault::Exhausted`), and residency never changes a verdict or a
/// charge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageBudgets {
    /// Clean pages the worker keeps read: evicted first, and read again
    /// from the parent on demand.
    pub clean_bytes: u64,
    /// Changed pages the heap holds before it seals and spills them to the
    /// parent's staging.
    pub dirty_bytes: u64,
    /// Pages a native call or a visit holds pinned at once.
    pub pinned_bytes: u64,
}

impl PageBudgets {
    /// The `D-CAP` start values: 64 MiB clean, 32 MiB dirty, 8 MiB pinned.
    pub const fn standard() -> Self {
        Self {
            clean_bytes: 64 << 20,
            dirty_bytes: 32 << 20,
            pinned_bytes: 8 << 20,
        }
    }
}
impl WorkerTuning {
    /// Existing presets: parent wait 86,400 s; inbound buffer 16 KiB; parser
    /// stack 8 MiB plus 40,000 bytes/source byte; a slice of one million
    /// charge units. Parser slope is measured at 1.8x the worst observed
    /// frames; the other working values have no workload measurements.
    pub const fn standard() -> Self {
        Self {
            diagnostic_bytes: lash_vm_protocol::DetailLimit::standard(),
            kernel_data: lash_kernel_doc::KernelDataLimits::standard(),
            parent_wait: Duration::from_secs(86_400),
            inbound_buffer_bytes: std::num::NonZeroUsize::MIN.saturating_add(16 * 1024 - 1),
            typescript_source_bytes: 64 * 1024,
            typescript_source_nesting: 28,
            python_source_bytes: 1 << 20,
            python_source_nesting: 96,
            parser_stack_base_bytes: 8 * 1024 * 1024,
            parser_stack_bytes_per_source_byte: 40_000,
            slice: 1_000_000,
            pages: PageBudgets::standard(),
            crosses_functions: false,
        }
    }
}
impl Default for WorkerTuning {
    fn default() -> Self {
        Self::standard()
    }
}

/// What confines a worker process before it serves (FIG-5858). The worker
/// installs it from its bootstrap, the pool's recorded configuration, and
/// reads nothing live. Its system calls are always the worker's fixed
/// allowlist; a call outside it kills the process
/// ([`lash_vm_protocol::SupervisorEvidence::ForbiddenSyscall`]).
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfinement {
    /// The worker's address-space ceiling (`RLIMIT_AS`), soft and hard: an
    /// allocation past it fails in the worker, whatever the run's memory
    /// bound. It must hold the parser stack for the largest admitted source.
    pub address_space_bytes: u64,
}
impl WorkerConfinement {
    /// A 4 GiB address-space ceiling. Its largest measured tenant is the
    /// parser stack for a 64 KiB source under `WorkerTuning::standard`
    /// (2,508 MiB reserved); a worker's peak with it was 2,678 MiB. The rest
    /// is headroom for the run's heap and decoding, unmeasured.
    pub const fn standard() -> Self {
        Self {
            address_space_bytes: 4 * 1024 * 1024 * 1024,
        }
    }
}
impl Default for WorkerConfinement {
    fn default() -> Self {
        Self::standard()
    }
}

/// Host policy for worker processes and their parent's temporary page storage.
///
/// # Staging capacity and lifetime
///
/// [`Self::staging_dir`] holds content-addressed checkpoint pages sealed by
/// workers or the parent, plus candidate-publication manifests and temporary
/// walk, sort and reachability files. Attachment uploads, tool attachment
/// staging and attachment-store blob spooling use the host's attachment store;
/// they do not consume this directory. Tool results encoded into checkpoint
/// pages do consume it, under the same quota as other pages.
///
/// Let `Q = staging_quota_bytes`. During successful spool operations, peak
/// file contents per spool are bounded by `Q` bytes: a spool admits a page
/// only while its staged page bytes plus 99 bytes for each staged page and
/// each distinct external reference stay within `Q` (FIG-6512). The 99
/// bytes are an entry's manifest scratch: sorting holds at most three
/// copies of each 33-byte digest record (walk queue, input runs and merge
/// output). The reference index uses empty files with counts in their
/// names. A partial page write replaces its temporary name by rename; it
/// does not add a second copy of the page. At the standard quota the bound
/// is **1,073,741,824 bytes** (1 GiB) per spool.
///
/// Pages are as small as one binding's value, so the quota bounds a spool
/// by what its files hold, never by a count of pages: a session of 50,000
/// bindings stages about 50,000 pages and 10 MB of the quota.
///
/// A code-mode session's cells share one turn spool, so this is also its
/// per-active-session bound. Multiply by the host's maximum number of live
/// spools for a directory budget: independently staged process activations
/// each add a spool. [`Self::max_workers`] limits checked-out workers, not
/// live spools; a spool can remain while work waits on a tool or publication.
/// Add filesystem block rounding, directories and inode headroom when sizing
/// an `emptyDir`; the quota bounds file contents, not filesystem allocation.
/// Failed filesystem operations can leave scratch files until the spool is
/// dropped; the successful-operation bound does not cover repeated I/O failures.
///
/// An authoritative publication deletes its candidate's pages and manifest;
/// unrelated staged pages remain until retention or the turn ends. Ambiguous
/// publication keeps its candidate for resolution. Dropping an activation's
/// spool (or the last handle to a shared turn spool) removes its whole spool
/// directory, including unpublished pages and scratch files. Cleanup after an
/// I/O error is best effort. A dead parent's files are reaped when the next
/// parent opens the same staging directory; committed state lives in the store.
#[derive(Clone, Debug)]
pub struct PoolConfig {
    pub entry: WorkerEntry,
    pub min_workers: usize,
    pub max_workers: usize,
    pub max_queue_items: usize,
    pub max_queue_bytes: usize,
    pub graph_reads: lash_core_store::checkpoint_graph::GraphReadPolicy,
    pub protocol: ProtocolBounds,
    /// The most any run's bounds may allow.
    pub run_bounds: RunBounds,
    pub deadlines: Deadlines,
    pub tuning: WorkerTuning,
    pub confinement: WorkerConfinement,
    pub restart_window: Duration,
    pub max_restarts: usize,
    /// The file-content quota per spool (`D-CAP`), shared by a session's
    /// turn or owned by one independent activation: staged page bytes plus
    /// 99 bytes of manifest scratch for each staged page and distinct
    /// external reference. Duplicate seals and shared references count
    /// once; retention and settlement release their bytes. Passing it is
    /// the host outcome `StageFault::Quota` (or
    /// `PageRefusal::StagingExhausted` over the worker wire). See
    /// [`PoolConfig`] for capacity. The standard quota is 1 GiB.
    pub staging_quota_bytes: u64,
    /// The directory for the parent's page spools. `None` uses
    /// `std::env::temp_dir().join("lash-staging")`. See [`PoolConfig`] for
    /// contents, per-session peak capacity, measurement and cleanup lifetime.
    /// Spools of dead parents are deleted when the next parent opens it.
    pub staging_dir: Option<std::path::PathBuf>,
}

impl PoolConfig {
    /// Prewarm one process, admit at most four, and bound waiting input to
    /// two items and eight MiB. Protocol bounds use `ProtocolBounds::standard`;
    /// run bounds are 50 million charge units, 64 MiB, 1,024 nested calls,
    /// 1,024 live tasks, 256 requests a park and 1,024 members a join;
    /// deadlines use `Deadlines::standard`; restarts allow eight per 60 seconds;
    /// startup uses one protocol silence window, or four with the development
    /// `dhat-heap` feature (20 s at standard bounds). Serving-worker silence
    /// and checkout deadlines keep their configured bounds;
    /// working policy uses `WorkerTuning::standard`; confinement uses
    /// `WorkerConfinement::standard`; a spool's files hold at most 1 GiB.
    /// FIG-4157/4162 measured
    /// synthetic workloads, not optimal concurrency or arbitrary guest spend.
    pub fn standard(entry: WorkerEntry) -> Self {
        Self {
            entry,
            min_workers: 1,
            max_workers: 4,
            max_queue_items: 2,
            max_queue_bytes: 8 * 1024 * 1024,
            graph_reads: lash_core_store::checkpoint_graph::GraphReadPolicy::standard(),
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
            confinement: WorkerConfinement::standard(),
            restart_window: Duration::from_secs(60),
            max_restarts: 8,
            staging_quota_bytes: 1 << 30,
            staging_dir: None,
        }
    }

    /// code mode/process preset: 64 MiB state, 128 MiB frames and queued bytes,
    /// 256 MiB decode allocations; other values follow `standard`.
    /// These larger allowances have no workload measurement behind them.
    pub fn codemode(entry: WorkerEntry) -> Self {
        let mut config = Self::standard(entry);
        config.protocol.max_vm_state_bytes = 64 * 1024 * 1024;
        config.protocol.decode.max_frame_bytes = 128 * 1024 * 1024;
        config.protocol.decode.max_allocation_bytes = 256 * 1024 * 1024;
        config.max_queue_bytes = 128 * 1024 * 1024;
        config
    }

    pub(crate) fn validate(&self) -> Result<(), PoolError> {
        let largest_parser_stack = usize::try_from(self.protocol.max_source_bytes)
            .ok()
            .and_then(|bytes| {
                let bytes = bytes.max(self.tuning.typescript_source_bytes);
                self.tuning
                    .parser_stack_bytes_per_source_byte
                    .checked_mul(bytes)
            })
            .and_then(|size| size.checked_add(self.tuning.parser_stack_base_bytes))
            .and_then(|size| u64::try_from(size).ok());
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
            || largest_parser_stack
                .is_none_or(|stack| stack >= self.confinement.address_space_bytes)
        {
            return Err(PoolError::InvalidConfiguration);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    /// WORKER-PARSER-RESERVATION: confinement covers the frontend's admitted source, even under a smaller wire cap.
    #[test]
    fn confinement_honors_the_configured_frontend_source_allowance() {
        let mut config = PoolConfig::standard(WorkerEntry::helper("worker"));
        config.tuning.typescript_source_bytes = 128 * 1024;
        assert!(matches!(
            config.validate(),
            Err(PoolError::InvalidConfiguration)
        ));
        config.confinement.address_space_bytes = 8 * 1024 * 1024 * 1024;
        assert!(config.validate().is_ok());
    }
}
