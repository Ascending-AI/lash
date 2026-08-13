use super::*;
use std::collections::VecDeque;

/// In-memory process registry for core tests.
pub struct TestLocalProcessRegistry {
    pub(super) transaction: Arc<Mutex<()>>,
    pub(super) managed: Arc<Mutex<ManagedProcessMap>>,
    pub(super) process_read_error: Arc<Mutex<Option<PluginError>>>,
    pub(super) process_read_error_after: Arc<Mutex<Option<(usize, PluginError)>>>,
    pub(super) process_read_absent: Arc<Mutex<bool>>,
    pub(super) process_read_override: Arc<Mutex<Option<ProcessRecord>>>,
    pub(super) process_lease_claim_error: Arc<Mutex<Option<PluginError>>>,
    pub(super) process_lease_renew_error: Arc<Mutex<Option<PluginError>>>,
    pub(super) process_terminal_write_error: Arc<Mutex<Option<PluginError>>>,
    pub(super) process_terminal_write_outcome: Arc<Mutex<Option<ProcessCompletionOutcome>>>,
    pub(super) process_lease_release_error: Arc<Mutex<Option<PluginError>>>,
    pub(super) next_change_seq: Arc<Mutex<u64>>,
    pub(super) observers: Arc<Mutex<HashMap<SessionId, HashSet<String>>>>,
    pub(super) wake_targets: Arc<Mutex<HashMap<String, SessionId>>>,
    pub(super) tombstones: Arc<Mutex<HashMap<String, ProcessTombstone>>>,
    pub(super) leases: Arc<Mutex<ManagedLeaseMap>>,
    pub(super) handovers: Arc<Mutex<HashMap<(String, u64), crate::PersistedSegmentHandover>>>,
    pub(super) execution_write_pause: Arc<std::sync::Mutex<Option<ExecutionWritePause>>>,
    pub(super) wake_mark_pause: Arc<std::sync::Mutex<Option<ExecutionWritePause>>>,
    pub(super) append_target_snapshot_pause: Arc<std::sync::Mutex<Option<ExecutionWritePause>>>,
    pub(super) append_outbox_pause: Arc<std::sync::Mutex<Option<ExecutionWritePause>>>,
    pub(super) prune_managed_removal_pause: Arc<std::sync::Mutex<Option<ExecutionWritePause>>>,
    pub(super) wake_delivery_config: crate::WakeDeliveryConfig,
    pub(super) wake_deliveries: Arc<Mutex<HashMap<String, crate::WakeDelivery>>>,
    pub(super) wake_allocation_floors: Arc<Mutex<HashMap<(SessionId, String), u64>>>,
    pub(super) worklist_page_reads: Arc<Mutex<WorklistPageReads>>,
    pub(super) worklist_page_error_plan: Arc<Mutex<WorklistPageErrorPlan>>,
    pub(super) worklist_page_pause: Arc<std::sync::Mutex<Option<ExecutionWritePause>>>,
    pub(super) clock: Arc<dyn crate::Clock>,
}

/// Concrete in-memory registry rows exposed to raw differential readers.
///
/// This is intentionally not a `ProcessRegistry` read model: it snapshots the
/// maps that the implementation mutates so a differential does not validate a
/// write through the same public query path.
#[doc(hidden)]
pub struct RawProcessRegistryStateForTesting {
    pub records: Vec<(ProcessRecord, u64)>,
    pub events: Vec<(String, ProcessEvent)>,
    pub observers: Vec<(String, String)>,
    pub leases: Vec<ProcessLease>,
    pub wake_deliveries: Vec<crate::WakeDelivery>,
    pub wake_allocation_floors: Vec<(SessionId, String, u64)>,
    pub tombstones: Vec<ProcessTombstone>,
}

pub(super) type ManagedProcessMap = HashMap<String, ManagedProcessRecord>;
pub(super) type ManagedLeaseMap = HashMap<String, ProcessLease>;
type WorklistPageReads = Vec<(usize, Option<crate::ProcessWorklistCursor>)>;
type WorklistPageErrorPlan = Option<(usize, VecDeque<PluginError>)>;

#[derive(Clone)]
pub(super) struct ManagedProcessRecord {
    pub(super) record: ProcessRecord,
    pub(super) change_seq: u64,
    pub(super) events: Vec<ProcessEvent>,
    pub(super) keyed_events: HashMap<String, ProcessEvent>,
    pub(super) parent_end_actions: Option<Vec<crate::ToolIntentParentEndAction>>,
}
