use super::*;
use std::collections::VecDeque;

/// In-memory process registry for core tests.
///
/// Every durable map lives in [`RegistryState`] behind one lock, and every
/// mutation runs through [`TestLocalProcessRegistry::write`], which stages the
/// changes on a clone and publishes them only on success — a failed write
/// leaves no half-applied row for the cross-backend differential to grade.
/// Error-injection knobs, read counters, and pause points are not durable
/// state, so they keep their own slots and stay settable while a write holds
/// the state lock.
pub struct TestLocalProcessRegistry {
    pub(super) state: Arc<Mutex<RegistryState>>,
    pub(super) process_read_error: Arc<Mutex<Option<PluginError>>>,
    pub(super) process_read_error_after: Arc<Mutex<Option<(usize, PluginError)>>>,
    pub(super) process_events_read_error: Arc<Mutex<Option<PluginError>>>,
    pub(super) process_read_absent: Arc<Mutex<bool>>,
    pub(super) process_read_override: Arc<Mutex<Option<ProcessRecord>>>,
    pub(super) process_lease_claim_error: Arc<Mutex<Option<PluginError>>>,
    pub(super) process_lease_renew_error: Arc<Mutex<Option<PluginError>>>,
    pub(super) process_terminal_write_error: Arc<Mutex<Option<PluginError>>>,
    pub(super) process_terminal_write_outcome: Arc<Mutex<Option<ProcessCompletionOutcome>>>,
    pub(super) external_ref_write_error: Arc<Mutex<Option<PluginError>>>,
    pub(super) cancel_request_write_error: Arc<Mutex<Option<PluginError>>>,
    pub(super) process_lease_release_error: Arc<Mutex<Option<PluginError>>>,
    pub(crate) process_lease_point_reads: Arc<Mutex<usize>>,
    pub(crate) process_lease_batch_reads: Arc<Mutex<usize>>,
    pub(super) execution_write_pause: Arc<std::sync::Mutex<Option<ExecutionWritePause>>>,
    pub(super) wake_mark_pause: Arc<std::sync::Mutex<Option<ExecutionWritePause>>>,
    pub(super) append_target_snapshot_pause: Arc<std::sync::Mutex<Option<ExecutionWritePause>>>,
    pub(super) append_outbox_pause: Arc<std::sync::Mutex<Option<ExecutionWritePause>>>,
    pub(super) prune_managed_removal_pause: Arc<std::sync::Mutex<Option<ExecutionWritePause>>>,
    pub(super) wake_delivery_config: crate::WakeDeliveryConfig,
    pub(super) worklist_page_reads: Arc<Mutex<WorklistPageReads>>,
    pub(super) worklist_page_error_plan: Arc<Mutex<WorklistPageErrorPlan>>,
    pub(super) worklist_page_pause: Arc<std::sync::Mutex<Option<ExecutionWritePause>>>,
    pub(super) clock: Arc<dyn crate::Clock>,
    pub(super) scope_fence_hosts: super::super::ProcessScopeFenceHosts,
}

/// The registry's durable state: the maps a committed mutation can touch.
///
/// Mutations stage on a clone under one lock and swap in atomically, so no
/// failure path can leave a pending wake delivery for an event tail that was
/// rolled back — the divergence the durable backends' transactions prevent.
#[derive(Clone, Default)]
pub(super) struct RegistryState {
    pub(super) managed: ManagedProcessMap,
    pub(super) next_change_seq: u64,
    pub(super) observers: HashMap<SessionId, HashSet<ProcessId>>,
    pub(super) wake_targets: HashMap<ProcessId, SessionId>,
    pub(super) tombstones: HashMap<(String, ProcessIncarnation), ProcessTombstone>,
    pub(super) artifact_cleanup:
        HashMap<(ProcessId, ProcessIncarnation), crate::ProcessArtifactCleanup>,
    pub(super) leases: ManagedLeaseMap,
    pub(super) handovers: HashMap<(ProcessId, u64), crate::PersistedSegmentHandover>,
    pub(super) tool_intent_submissions: HashMap<String, crate::ToolIntentSubmissionRecord>,
    pub(super) wake_deliveries: HashMap<String, crate::WakeDelivery>,
    pub(super) wake_allocation_floors: HashMap<(SessionId, ProcessId), u64>,
    pub(super) tombstone_compaction_horizon: u64,
    pub(super) parent_end_plans: ParentEndLedger,
}

impl TestLocalProcessRegistry {
    /// The one write path: run `body` against a staged clone of the state and
    /// publish it only on success. The lock is held across `body` so pauses
    /// inside a write still serialize concurrent writers exactly as the old
    /// `transaction` mutex did.
    pub(super) async fn write<T>(
        &self,
        body: impl AsyncFnOnce(&mut RegistryState) -> Result<T, PluginError>,
    ) -> Result<T, PluginError> {
        let mut state = self.state.lock().await;
        let mut staged = state.clone();
        let result = body(&mut staged).await;
        if result.is_ok() {
            *state = staged;
        }
        result
    }
}

/// Concrete in-memory registry rows exposed to raw differential readers.
///
/// This is intentionally not a `ProcessRegistry` read model: it snapshots the
/// maps that the implementation mutates so a differential does not validate a
/// write through the same public query path.
pub struct RawProcessRegistryStateForTesting {
    pub records: Vec<(ProcessRecord, u64)>,
    pub events: Vec<(ProcessId, ProcessEvent)>,
    pub observers: Vec<(SessionId, ProcessId, u64)>,
    pub leases: Vec<ProcessLease>,
    pub wake_deliveries: Vec<crate::WakeDelivery>,
    pub wake_allocation_floors: Vec<(SessionId, ProcessId, u64)>,
    pub tombstones: Vec<ProcessTombstone>,
}

/// Entries sit behind `Arc` so the staged clone in `write` bumps refcounts
/// instead of deep-copying every record, event log, and keyed-event map on
/// each mutation — under a live workload that clone dominates the write-lock
/// hold and serializes unrelated operations into millisecond waits.
pub(super) type ManagedProcessMap = HashMap<ProcessId, Arc<ManagedProcessRecord>>;
/// Parent-end ledger keyed by the scope's storage kind and id, so a turn
/// parent is representable alongside a process parent.
pub(super) type ParentEndLedger = HashMap<(String, String), crate::ParentEndPlan>;
pub(super) type ManagedLeaseMap = HashMap<ProcessId, ProcessLease>;
type WorklistPageReads = Vec<(usize, Option<crate::ProcessWorklistCursor>)>;
type WorklistPageErrorPlan = Option<(usize, VecDeque<PluginError>)>;

#[derive(Clone)]
pub(super) struct ManagedProcessRecord {
    pub(super) record: ProcessRecord,
    pub(super) change_seq: u64,
    pub(super) events: Vec<ProcessEvent>,
    pub(super) keyed_events: HashMap<String, ProcessEvent>,
}
