use lash_sansio::sync::MutexExt;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use self::recovery::{
    RecoveryBackendError, RecoveryClaimDisposition, RecoveryCompletionDisposition,
    RecoveryReadDisposition, RecoveryReleaseDisposition,
};
use self::registration::registration_from_record;

mod drain;
mod permit;
mod recovery;
mod registration;

#[cfg(test)]
use permit::{PROCESS_EXECUTION_PERMIT, ProcessExecutionPermit};
pub(crate) use permit::{
    ensure_process_execution_permit, inherit_process_execution_permit,
    release_process_execution_permit_while,
};
pub(super) use permit::{scope_process_execution_permit, scope_queued_work_execution_permit};

pub use self::recovery::{
    ProcessDrainDeferred, ProcessDrainReport, ProcessRecoveryAttemptDisposition,
    ProcessRecoveryOperation,
};

use super::effect::ProcessRunner;
use super::session_manager::RuntimeSessionServices;
use super::worker_capacity::{
    DefaultWorkerSlotSupplier, ObservedWorkerSlotSupplier, WorkerCapacityMetrics,
    WorkerSlotSupplier as _,
};
use super::{EmbeddedRuntimeBuilder, ProcessWorkDriver, QueuedWorkDriver, RuntimeHostConfig};
use crate::InMemorySessionStore;
use crate::{
    AbandonEvidence, AbandonWriter, LashRuntime, PluginError, PluginFactory, PluginHost,
    PluginStack, ProcessAwaitOutput, ProcessExecutionContext, ProcessInput, ProcessLease,
    ProcessRecord, ProcessRegistration, ProcessRegistry, RecoveryDisposition, SessionStoreFactory,
};

/// Default maximum number of processes one [`DurableProcessWorker`] executes
/// inline at once.
pub const DEFAULT_PROCESS_EXECUTION_CONCURRENCY: usize = 64;
const MAX_INTAKE_PAGE: usize = 256;
const WORKLIST_FETCH_ATTEMPTS: usize = 3;
const WORKLIST_FETCH_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(10);

/// Validated per-worker inline process execution concurrency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessExecutionConcurrency(usize);

impl ProcessExecutionConcurrency {
    const DEFAULT: Self = Self(DEFAULT_PROCESS_EXECUTION_CONCURRENCY);

    fn new(concurrency: usize) -> Result<Self, ProcessExecutionConcurrencyError> {
        if !(1..=Semaphore::MAX_PERMITS).contains(&concurrency) {
            return Err(ProcessExecutionConcurrencyError { concurrency });
        }
        Ok(Self(concurrency))
    }

    fn get(self) -> usize {
        self.0
    }
}

/// Invalid inline process execution concurrency supplied by a host.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "process execution concurrency must be between 1 and {max} (inclusive), got {concurrency}",
    max = Semaphore::MAX_PERMITS
)]
pub struct ProcessExecutionConcurrencyError {
    concurrency: usize,
}

/// Deployment-local configuration for rebuilding durable process executions.
///
/// Process rows intentionally carry only portable process input and provenance.
/// Workers provide plugins, providers, stores, secrets, and host capabilities
/// for the deployment that owns those rows.
#[derive(Clone)]
pub struct DurableProcessWorkerConfig {
    pub plugin_host: Arc<PluginHost>,
    pub runtime_host: RuntimeHostConfig,
    pub session_policy: crate::SessionPolicy,
    pub session_store_factory: Arc<dyn SessionStoreFactory>,
    pub process_registry: Arc<dyn ProcessRegistry>,
    pub process_change_hub: Option<crate::ProcessChangeHub>,
    pub trigger_store: Arc<dyn crate::TriggerStore>,
    pub process_work_driver: Option<ProcessWorkDriver>,
    pub queued_work_driver: Option<QueuedWorkDriver>,
    #[doc(hidden)]
    pub turn_phase_probe_slot: crate::runtime::RuntimeTurnPhaseProbeSlot,
    /// Maximum processes this worker executes inline at once. A run holds its
    /// slot while doing its own work and releases it while parked on work that
    /// another process or external owner must complete. This is a per-worker
    /// bound: two workers sharing one registry may execute twice this many.
    process_execution_concurrency: ProcessExecutionConcurrency,
    worker_slot_supplier: Option<Arc<dyn super::WorkerSlotSupplier>>,
    /// Owner identity stem this worker derives per-recovery lease owners from.
    ///
    /// Each recovery attempt claims with a unique `(owner_id, incarnation_id)`
    /// derived from this identity — a live lease held by an earlier attempt
    /// must fence a later sweep pass rather than be re-entered as the same
    /// incarnation. Defaults to a fresh identity per config; recovery waits for
    /// an earlier owner's lease TTL before taking over.
    pub lease_owner: crate::LeaseOwnerIdentity,
}

impl DurableProcessWorkerConfig {
    pub fn validate_process_execution_concurrency(
        concurrency: usize,
    ) -> Result<(), ProcessExecutionConcurrencyError> {
        ProcessExecutionConcurrency::new(concurrency).map(drop)
    }

    pub fn new(
        plugin_host: Arc<PluginHost>,
        runtime_host: RuntimeHostConfig,
        session_store_factory: Arc<dyn SessionStoreFactory>,
        process_registry: Arc<dyn ProcessRegistry>,
    ) -> Self {
        let clock = Arc::clone(&runtime_host.clock);
        Self {
            plugin_host,
            runtime_host,
            session_policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            session_store_factory,
            process_registry,
            process_change_hub: None,
            trigger_store: Arc::new(crate::InMemoryTriggerStore::with_clock(clock)),
            process_work_driver: None,
            queued_work_driver: None,
            turn_phase_probe_slot: crate::runtime::RuntimeTurnPhaseProbeSlot::default(),
            process_execution_concurrency: ProcessExecutionConcurrency::DEFAULT,
            worker_slot_supplier: None,
            lease_owner: crate::LeaseOwnerIdentity::opaque(
                format!("durable-process-worker:{}", uuid::Uuid::new_v4()),
                uuid::Uuid::new_v4().to_string(),
            ),
        }
    }

    pub fn with_trigger_store(mut self, store: Arc<dyn crate::TriggerStore>) -> Self {
        self.trigger_store = store;
        self
    }

    pub fn with_session_policy(mut self, policy: crate::SessionPolicy) -> Self {
        self.session_policy = policy;
        self
    }

    /// Set the maximum processes this worker executes inline at once.
    ///
    /// The minimum is one. The maximum is Tokio's semaphore limit. The bound
    /// applies independently to each worker, not globally to a shared registry.
    pub fn with_process_execution_concurrency(
        mut self,
        concurrency: usize,
    ) -> Result<Self, ProcessExecutionConcurrencyError> {
        self.process_execution_concurrency = ProcessExecutionConcurrency::new(concurrency)?;
        Ok(self)
    }

    pub fn process_execution_concurrency(&self) -> usize {
        self.process_execution_concurrency.get()
    }

    /// Replace fixed process admission with a host-owned worker slot supplier.
    #[doc(hidden)]
    pub fn with_worker_slot_supplier(
        mut self,
        supplier: Arc<dyn super::WorkerSlotSupplier>,
    ) -> Self {
        self.worker_slot_supplier = Some(supplier);
        self
    }

    pub fn with_process_work_driver(mut self, driver: ProcessWorkDriver) -> Self {
        self.process_work_driver = Some(driver);
        self
    }

    pub fn with_change_hub(mut self, hub: crate::ProcessChangeHub) -> Self {
        self.process_change_hub = Some(hub);
        self
    }

    /// Set the owner identity this worker presents when claiming process
    /// leases. See [`DurableProcessWorkerConfig::lease_owner`].
    pub fn with_lease_owner(mut self, lease_owner: crate::LeaseOwnerIdentity) -> Self {
        self.lease_owner = lease_owner;
        self
    }

    pub fn with_queued_work_driver(mut self, driver: QueuedWorkDriver) -> Self {
        self.queued_work_driver = Some(driver);
        self
    }

    #[doc(hidden)]
    pub fn with_turn_phase_probe_slot(
        mut self,
        slot: crate::runtime::RuntimeTurnPhaseProbeSlot,
    ) -> Self {
        self.turn_phase_probe_slot = slot;
        self
    }

    pub fn from_plugin_factories(
        plugin_factories: impl IntoIterator<Item = Arc<dyn PluginFactory>>,
        runtime_host: RuntimeHostConfig,
        session_store_factory: Arc<dyn SessionStoreFactory>,
        process_registry: Arc<dyn ProcessRegistry>,
    ) -> Self {
        Self::new(
            Arc::new(PluginHost::new(plugin_factories.into_iter().collect())),
            runtime_host,
            session_store_factory,
            process_registry,
        )
    }

    pub fn from_plugin_stack(
        plugin_stack: PluginStack,
        runtime_host: RuntimeHostConfig,
        session_store_factory: Arc<dyn SessionStoreFactory>,
        process_registry: Arc<dyn ProcessRegistry>,
    ) -> Self {
        Self::from_plugin_factories(
            plugin_stack.into_factories(),
            runtime_host,
            session_store_factory,
            process_registry,
        )
    }
}

/// Reconstructable background-process worker.
pub struct DurableProcessWorker {
    config: Arc<DurableProcessWorkerConfig>,
    execution_scheduler: Arc<ProcessExecutionScheduler>,
    lifetime: Option<Arc<ProcessWorkerLifetime>>,
}

impl Clone for DurableProcessWorker {
    fn clone(&self) -> Self {
        Self {
            config: Arc::clone(&self.config),
            execution_scheduler: Arc::clone(&self.execution_scheduler),
            lifetime: self.lifetime.clone(),
        }
    }
}

struct ProcessWorkerLifetime {
    shutdown: CancellationToken,
}

impl Drop for ProcessWorkerLifetime {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

#[derive(Default)]
struct ProcessExecutionSchedulerState {
    pending: VecDeque<ProcessRecord>,
    scheduled: BTreeSet<String>,
    rerun: BTreeMap<String, ProcessRecord>,
    active: usize,
    dispatcher_running: bool,
    worklist_scan: ProcessWorklistScan,
    rescan_requested: bool,
    scan_incomplete: Option<PluginError>,
}

#[derive(Default)]
enum ProcessWorklistScan {
    #[default]
    Idle,
    Fetching(Option<crate::ProcessWorklistCursor>),
    Ready(Option<crate::ProcessWorklistCursor>),
}

struct ProcessExecutionScheduler {
    slots: Arc<dyn super::WorkerSlotSupplier>,
    metrics: WorkerCapacityMetrics,
    state: std::sync::Mutex<ProcessExecutionSchedulerState>,
    changed: Arc<tokio::sync::Notify>,
    shutdown: CancellationToken,
}

impl ProcessExecutionScheduler {
    fn new(
        concurrency: ProcessExecutionConcurrency,
        supplier: Option<Arc<dyn super::WorkerSlotSupplier>>,
    ) -> Self {
        let supplier = supplier.unwrap_or_else(|| {
            Arc::new(DefaultWorkerSlotSupplier::new(
                concurrency.get(),
                super::DEFAULT_QUEUED_WORK_EXECUTION_CONCURRENCY,
            ))
        });
        let metrics = WorkerCapacityMetrics::default();
        let slots = ObservedWorkerSlotSupplier::new(supplier, metrics.clone());
        metrics.slots(
            super::WorkerSlotKind::Process,
            0,
            slots.available_slots(super::WorkerSlotKind::Process),
        );
        metrics.intake_depth(super::WorkerSlotKind::Process, 0);
        Self {
            slots,
            metrics,
            state: std::sync::Mutex::new(ProcessExecutionSchedulerState::default()),
            changed: Arc::new(tokio::sync::Notify::new()),
            shutdown: CancellationToken::new(),
        }
    }

    fn complete_execution(&self, process_id: &str) {
        let mut state = self.state.lock_recover();
        state.active = state
            .active
            .checked_sub(1)
            .expect("a completed process execution was active");
        if let Some(record) = state.rerun.remove(process_id) {
            state.pending.push_back(record);
        } else {
            state.scheduled.remove(process_id);
        }
        self.metrics.intake_depth(
            super::WorkerSlotKind::Process,
            state.pending.len() + state.rerun.len(),
        );
        drop(state);
        self.changed.notify_one();
    }
}

/// Clears the single-dispatcher latch if the fire-and-forget dispatcher
/// unwinds. Without this guard, one panic permanently leaves queued work with
/// no task allowed to drain it.
struct ProcessExecutionDispatcherGuard {
    scheduler: Arc<ProcessExecutionScheduler>,
    armed: bool,
}

impl ProcessExecutionDispatcherGuard {
    fn new(scheduler: Arc<ProcessExecutionScheduler>) -> Self {
        Self {
            scheduler,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessExecutionDispatcherGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(mut state) = self.scheduler.state.try_lock_recover() {
            state.dispatcher_running = false;
            if let ProcessWorklistScan::Fetching(continuation) = &state.worklist_scan {
                state.worklist_scan = ProcessWorklistScan::Ready(continuation.clone());
            }
            self.scheduler.changed.notify_one();
            return;
        }
        let scheduler = Arc::clone(&self.scheduler);
        crate::task::spawn(async move {
            let mut state = scheduler.state.lock_recover();
            state.dispatcher_running = false;
            if let ProcessWorklistScan::Fetching(continuation) = &state.worklist_scan {
                state.worklist_scan = ProcessWorklistScan::Ready(continuation.clone());
            }
            drop(state);
            scheduler.changed.notify_one();
        });
    }
}

struct ProcessExecutionTaskCompletion {
    process_id: String,
    scheduler: Arc<ProcessExecutionScheduler>,
}

impl Drop for ProcessExecutionTaskCompletion {
    fn drop(&mut self) {
        self.scheduler.complete_execution(&self.process_id);
    }
}

/// Why a recovery run did not produce a terminal outcome under the lease.
enum RecoverFailure {
    /// The lease was lost mid-run (another owner reclaimed an expired lease).
    /// The losing worker must not write a terminal outcome — the new owner is
    /// now the single writer.
    LeaseLost(PluginError),
    /// A process-registry operation failed through the typed recovery seam.
    BackendError(RecoveryBackendError),
    /// The process could not be run (rebuild/store-facet failure). The lease is
    /// still held, so this worker terminalizes the row.
    Run(PluginError),
}

impl DurableProcessWorker {
    pub fn new(config: DurableProcessWorkerConfig) -> Self {
        let execution_scheduler = Arc::new(ProcessExecutionScheduler::new(
            config.process_execution_concurrency,
            config.worker_slot_supplier.clone(),
        ));
        let lifetime = Arc::new(ProcessWorkerLifetime {
            shutdown: execution_scheduler.shutdown.clone(),
        });
        Self {
            config: Arc::new(config),
            execution_scheduler,
            lifetime: Some(lifetime),
        }
    }

    pub fn from_shared_config(config: Arc<DurableProcessWorkerConfig>) -> Self {
        let execution_scheduler = Arc::new(ProcessExecutionScheduler::new(
            config.process_execution_concurrency,
            config.worker_slot_supplier.clone(),
        ));
        let lifetime = Arc::new(ProcessWorkerLifetime {
            shutdown: execution_scheduler.shutdown.clone(),
        });
        Self {
            config,
            execution_scheduler,
            lifetime: Some(lifetime),
        }
    }

    fn detached_for_task(&self) -> Self {
        Self {
            config: Arc::clone(&self.config),
            execution_scheduler: Arc::clone(&self.execution_scheduler),
            lifetime: None,
        }
    }

    pub fn config(&self) -> &DurableProcessWorkerConfig {
        &self.config
    }

    /// Run exactly one engine segment. Durable substrates use this method so a
    /// non-terminal boundary can end the current substrate invocation; the
    /// inline worker's lease-fenced drive loops over segment boundaries
    /// internally.
    pub async fn run_process_segment_with_scoped_effect_controller(
        &self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        execution_write_authority: crate::ProcessExecutionWriteAuthority,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
        cancellation: CancellationToken,
        handover: Option<crate::SegmentHandover>,
    ) -> Result<crate::ProcessRunOutcome, PluginError> {
        self.ensure_stable_process_id(&registration)?;
        // Externally-owned rows are never executed by lash (ADR 0019). Reject the
        // disposition before touching a runtime — the old fabricated-success path
        // for External inputs is deleted.
        if registration.disposition == RecoveryDisposition::ExternallyOwned {
            return Err(PluginError::Session(format!(
                "process `{}` is externally-owned and must not be executed by lash",
                registration.id
            )));
        }
        let current = self
            .config
            .process_registry
            .get_process(&registration.id)
            .await?
            .ok_or_else(|| {
                PluginError::Session(format!("unknown process `{}`", registration.id))
            })?;
        self.run_process_segment_from_current(
            registration,
            current,
            execution_context,
            execution_write_authority,
            scoped_effect_controller,
            cancellation,
            handover,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_process_segment_from_current(
        &self,
        registration: ProcessRegistration,
        current: ProcessRecord,
        execution_context: ProcessExecutionContext,
        execution_write_authority: crate::ProcessExecutionWriteAuthority,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
        cancellation: CancellationToken,
        handover: Option<crate::SegmentHandover>,
    ) -> Result<crate::ProcessRunOutcome, PluginError> {
        let (owner, fencing_token) = match &execution_write_authority {
            crate::ProcessExecutionWriteAuthority::Lease(lease) => {
                (self.config.lease_owner.clone(), lease.fencing_token)
            }
            crate::ProcessExecutionWriteAuthority::Invocation {
                process_id,
                execution_id,
                ..
            } => (
                crate::LeaseOwnerIdentity::restate_process_execution(process_id, execution_id),
                0,
            ),
        };
        let attempt = current.first_started.as_deref().map_or(1, |started| {
            if started.owner.same_incarnation(&owner) && started.fencing_token == fencing_token {
                started.attempt
            } else {
                started.attempt.saturating_add(1)
            }
        });
        let execution_write_authority = execution_write_authority.bind_attempt(attempt);
        self.config
            .process_registry
            .record_first_started_with_authority(
                &registration.id,
                crate::ProcessStarted {
                    owner,
                    fencing_token,
                    attempt,
                    started_at_ms: self.now_ms(),
                },
                &execution_write_authority,
            )
            .await?
            .into_record()?;
        let execution_context =
            execution_context.with_execution_write_authority(execution_write_authority);
        let mut runtime = Box::pin(self.runtime_for_registration(&registration)).await?;
        let _attachment_owner_binding = matches!(
            registration.input.as_ref(),
            ProcessInput::ToolCall { .. } | ProcessInput::Engine { .. }
        )
        .then(|| {
            runtime
                .host
                .core
                .durability
                .attachment_store
                .bind_process_scoped(registration.id.clone())
        });
        let originator_scope = if let crate::ProcessOriginator::Session { session_id, .. } =
            &registration.provenance.originator
        {
            Some(crate::SessionScope::new(session_id))
        } else {
            None
        };
        let wake_scope = registration
            .wake_session_id
            .as_ref()
            .map(crate::SessionScope::new);
        let probe_scope = wake_scope.as_ref().or(originator_scope.as_ref());
        if let Some(probe) =
            probe_scope.and_then(|scope| self.config.turn_phase_probe_slot.get_for_scope(scope))
        {
            runtime.set_turn_phase_probe(probe);
        }
        let manager = RuntimeSessionServices::new(&runtime, true, None, None).map_err(|err| {
            PluginError::Session(format!(
                "failed to build runtime env for process `{}`: {err}",
                registration.id
            ))
        })?;
        manager
            .run_process(
                registration,
                execution_context,
                Arc::clone(&self.config.process_registry),
                scoped_effect_controller,
                cancellation,
                handover,
            )
            .await
            .map_err(crate::ProcessInfraError::into_plugin_error)
    }

    /// Page through non-terminal processes as execution capacity frees and run
    /// the claimable rows inline, driving each to a terminal state.
    ///
    /// This is the sole inline executor for every process start: live tool and
    /// subagent starts, trigger deliveries, admin starts, session-open passes,
    /// and crash recovery all enter through this worklist. The drive:
    ///
    /// 1. reads bounded pages from the non-terminal worklist
    ///    ([`ProcessRegistry::list_non_terminal_page`]);
    /// 2. claims the durable single-owner [`ProcessLease`] over each — a process
    ///    already leased live by *another* owner is skipped until its TTL
    ///    expires; a non-terminal process is re-run by exactly one owner (lease
    ///    fencing);
    /// 3. keeps at most one capacity-sized intake page pending, fetching the
    ///    continuation only after dispatch capacity frees; the worker-scoped
    ///    scheduler's shared execution budget spans repeated host-driven passes
    ///    and runs claimed processes on this worker's wired controller while renewing
    ///    the lease across the long-running execution so a healthy recovery is
    ///    not swept out from under itself;
    /// 4. atomically writes the terminal outcome and releases the validated lease.
    ///
    /// Idempotent by `process_id`: terminal processes are never in the worklist,
    /// and a process that became terminal between the list and the claim is
    /// detected after claiming and skipped, so re-running a recovery sweep does
    /// not double-execute completed work.
    pub async fn drive_pending_processes(&self) -> Result<(), PluginError> {
        self.reconcile_trigger_deliveries().await?;
        let available = std::num::NonZeroUsize::new(
            self.execution_scheduler
                .slots
                .available_slots(super::WorkerSlotKind::Process)
                .clamp(1, MAX_INTAKE_PAGE),
        )
        .expect("the clamped intake page bound is non-zero");
        let (fetch_initial_page, should_start_dispatcher, scan_incomplete) = {
            let mut state = self.execution_scheduler.state.lock_recover();
            let fetch_initial_page = if matches!(state.worklist_scan, ProcessWorklistScan::Idle) {
                state.worklist_scan = ProcessWorklistScan::Fetching(None);
                Some(available)
            } else {
                state.rescan_requested = true;
                None
            };
            let should_start_dispatcher = if state.dispatcher_running {
                false
            } else {
                state.dispatcher_running = true;
                true
            };
            (
                fetch_initial_page,
                should_start_dispatcher,
                state.scan_incomplete.take(),
            )
        };
        if let Some(limit) = fetch_initial_page {
            let page = match self
                .config
                .process_registry
                .list_non_terminal_page(limit, None)
                .await
            {
                Ok(page) => page,
                Err(error) => {
                    let mut state = self.execution_scheduler.state.lock_recover();
                    state.worklist_scan = if state.rescan_requested {
                        ProcessWorklistScan::Ready(None)
                    } else {
                        ProcessWorklistScan::Idle
                    };
                    let restart_dispatcher = should_start_dispatcher && state.rescan_requested;
                    if should_start_dispatcher && !restart_dispatcher {
                        state.dispatcher_running = false;
                    }
                    drop(state);
                    self.execution_scheduler.changed.notify_one();
                    if restart_dispatcher {
                        let worker = self.detached_for_task();
                        crate::task::spawn(async move {
                            worker.run_process_execution_dispatcher().await
                        });
                    }
                    return Err(error);
                }
            };
            self.install_worklist_page(page);
        }
        self.execution_scheduler.changed.notify_one();
        if should_start_dispatcher {
            let worker = self.detached_for_task();
            crate::task::spawn(async move { worker.run_process_execution_dispatcher().await });
        }
        match scan_incomplete {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn run_process_execution_dispatcher(&self) {
        let mut dispatcher_guard =
            ProcessExecutionDispatcherGuard::new(Arc::clone(&self.execution_scheduler));
        loop {
            if self.execution_scheduler.shutdown.is_cancelled() {
                return;
            }
            while let Some((record, permit)) = self.next_process_execution().await {
                let worker = self.clone();
                let completion = ProcessExecutionTaskCompletion {
                    process_id: record.id.clone(),
                    scheduler: Arc::clone(&self.execution_scheduler),
                };
                crate::task::spawn(async move {
                    let _completion = completion;
                    // Install the execution budget only at the inline worker
                    // boundary, never in the shared process-segment path.
                    Box::pin(scope_process_execution_permit(
                        Arc::clone(&worker.execution_scheduler.slots),
                        permit,
                        Arc::clone(&worker.execution_scheduler.changed),
                        worker.recover_process(record),
                    ))
                    .await;
                });
            }

            if let Some((limit, continuation)) = self.next_worklist_page_request() {
                match self
                    .fetch_worklist_page_with_retry(limit, continuation.clone())
                    .await
                {
                    Ok(page) => self.install_worklist_page(page),
                    Err(error) => {
                        tracing::warn!(error = %error, "process worklist scan remains incomplete after retry exhaustion");
                        let mut state = self.execution_scheduler.state.lock_recover();
                        state.worklist_scan = ProcessWorklistScan::Ready(continuation);
                        state.rescan_requested = true;
                        state.scan_incomplete = Some(error);
                        state.dispatcher_running = false;
                        dispatcher_guard.disarm();
                        return;
                    }
                }
                continue;
            }

            {
                let mut state = self.execution_scheduler.state.lock_recover();
                if state.pending.is_empty()
                    && state.active == 0
                    && matches!(state.worklist_scan, ProcessWorklistScan::Idle)
                {
                    state.dispatcher_running = false;
                    dispatcher_guard.disarm();
                    return;
                }
            }

            tokio::select! {
                biased;
                () = self.execution_scheduler.shutdown.cancelled() => return,
                _ = self.execution_scheduler.changed.notified() => {}
            }
        }
    }

    fn install_worklist_page(&self, page: crate::ProcessWorklistPage) {
        let mut state = self.execution_scheduler.state.lock_recover();
        for record in page.records {
            if state.scheduled.insert(record.id.clone()) {
                state.pending.push_back(record);
            } else {
                // Coalesce a newer host-driven pass instead of dropping it.
                // The row may have gained an Abandon Request or other
                // execution-relevant state while its prior attempt was still
                // queued or finishing.
                state.rerun.insert(record.id.clone(), record);
            }
        }
        state.worklist_scan = match page.continuation {
            Some(continuation) => ProcessWorklistScan::Ready(Some(continuation)),
            None if state.rescan_requested => {
                state.rescan_requested = false;
                ProcessWorklistScan::Ready(None)
            }
            None => ProcessWorklistScan::Idle,
        };
        self.execution_scheduler.metrics.intake_depth(
            super::WorkerSlotKind::Process,
            state.pending.len() + state.rerun.len(),
        );
    }

    fn next_worklist_page_request(
        &self,
    ) -> Option<(std::num::NonZeroUsize, Option<crate::ProcessWorklistCursor>)> {
        let available = self
            .execution_scheduler
            .slots
            .available_slots(super::WorkerSlotKind::Process);
        let mut state = self.execution_scheduler.state.lock_recover();
        if !state.pending.is_empty() {
            return None;
        }
        if available == 0 && state.active != 0 {
            return None;
        }
        let limit = std::num::NonZeroUsize::new(available.clamp(1, MAX_INTAKE_PAGE))
            .expect("the clamped intake page bound is non-zero");
        let ProcessWorklistScan::Ready(continuation) = &state.worklist_scan else {
            return None;
        };
        let continuation = continuation.clone();
        state.worklist_scan = ProcessWorklistScan::Fetching(continuation.clone());
        Some((limit, continuation))
    }

    async fn fetch_worklist_page_with_retry(
        &self,
        limit: std::num::NonZeroUsize,
        continuation: Option<crate::ProcessWorklistCursor>,
    ) -> Result<crate::ProcessWorklistPage, PluginError> {
        let mut retry_after = WORKLIST_FETCH_RETRY_BASE;
        for attempt in 1..=WORKLIST_FETCH_ATTEMPTS {
            match self
                .config
                .process_registry
                .list_non_terminal_page(limit, continuation.clone())
                .await
            {
                Ok(page) => return Ok(page),
                Err(error) if attempt == WORKLIST_FETCH_ATTEMPTS => return Err(error),
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        attempt,
                        "retrying incomplete process worklist scan"
                    );
                    tokio::time::sleep(retry_after).await;
                    retry_after = retry_after.saturating_mul(2);
                }
            }
        }
        unreachable!("the non-zero worklist retry budget returns from the loop")
    }

    async fn next_process_execution(&self) -> Option<(ProcessRecord, super::WorkerSlotPermit)> {
        if self
            .execution_scheduler
            .state
            .lock_recover()
            .pending
            .is_empty()
        {
            return None;
        }
        let reserve = self
            .execution_scheduler
            .slots
            .reserve_slot(super::WorkerSlotKind::Process);
        tokio::pin!(reserve);
        let permit = tokio::select! {
            biased;
            () = self.execution_scheduler.shutdown.cancelled() => return None,
            permit = &mut reserve => permit,
        };
        if self.execution_scheduler.shutdown.is_cancelled() {
            drop(permit);
            return None;
        }
        let mut state = self.execution_scheduler.state.lock_recover();
        let Some(record) = state.pending.pop_front() else {
            drop(permit);
            return None;
        };
        state.active += 1;
        self.execution_scheduler.metrics.intake_depth(
            super::WorkerSlotKind::Process,
            state.pending.len() + state.rerun.len(),
        );
        Some((record, permit))
    }

    async fn reconcile_trigger_deliveries(&self) -> Result<(), PluginError> {
        let candidates = self.config.trigger_store.list_deliveries().await?;
        if candidates.is_empty() {
            return Ok(());
        }
        let candidate_process_ids = candidates
            .iter()
            .map(|delivery| delivery.process_id.clone())
            .collect::<Vec<_>>();
        let missing_process_ids = self
            .config
            .process_registry
            .filter_unregistered_process_ids(&candidate_process_ids)
            .await?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let router = crate::TriggerRouter::new(
            Arc::clone(&self.config.trigger_store),
            Some(Arc::clone(&self.config.process_registry)),
            self.config.process_work_driver.clone(),
        );
        let mut started_any = false;
        for delivery in candidates {
            if missing_process_ids.contains(&delivery.process_id) {
                let Some(scoped_effect_controller) = self
                    .config
                    .runtime_host
                    .control
                    .effect_host
                    .scoped_static(crate::ExecutionScope::runtime_operation(format!(
                        "trigger-delivery-reconcile:{}",
                        delivery.process_id
                    )))
                    .map_err(|err| PluginError::Session(err.to_string()))?
                else {
                    return Err(PluginError::Session(
                        "process worker effect host must provide a static trigger delivery reconcile scope"
                            .to_string(),
                    ));
                };
                match router
                    .start_delivery(
                        &delivery,
                        Arc::clone(&self.config.process_registry),
                        scoped_effect_controller.controller(),
                    )
                    .await
                {
                    Ok(()) => started_any = true,
                    Err(err) => tracing::warn!(
                        process_id = %delivery.process_id,
                        occurrence_id = %delivery.occurrence.occurrence_id,
                        subscription_id = %delivery.subscription.subscription_id,
                        error = %err,
                        "failed to reconcile trigger delivery",
                    ),
                }
            }
        }
        if started_any && let Some(driver) = self.config.process_work_driver.as_ref() {
            driver
                .claim_and_run_pending("trigger_delivery_reconcile")
                .await?;
        }
        Ok(())
    }

    /// Terminalize one of this host's started OwnerBound rows as
    /// `Abandoned{OwnerDrain}` under a freshly claimed drain lease. Returns
    /// a typed disposition distinguishing an acknowledged terminal write from
    /// contention, absence, peer settlement, lease loss, and backend failure.
    async fn drain_one_owner_bound(
        &self,
        process_id: &str,
        owner: crate::LeaseOwnerIdentity,
    ) -> RecoveryCompletionDisposition {
        let lease_ttl_ms = self.lease_timings().ttl_ms();
        let drain_owner = self.recovery_lease_owner();
        let lease = match self
            .claim_for_recovery(process_id, &drain_owner, lease_ttl_ms)
            .await
        {
            RecoveryClaimDisposition::Acquired(lease) => lease,
            RecoveryClaimDisposition::Busy => return RecoveryCompletionDisposition::Busy,
            RecoveryClaimDisposition::BackendError(error) => {
                return RecoveryCompletionDisposition::BackendError(error);
            }
        };
        let current = match self.read_for_recovery(process_id).await {
            RecoveryReadDisposition::Found(current) => *current,
            RecoveryReadDisposition::Absent => {
                return match self.release_for_recovery(&lease).await {
                    RecoveryReleaseDisposition::Released => RecoveryCompletionDisposition::Absent,
                    RecoveryReleaseDisposition::BackendError(error) => {
                        RecoveryCompletionDisposition::BackendError(error)
                    }
                };
            }
            RecoveryReadDisposition::BackendError(error) => {
                self.release_or_log(&lease).await;
                return RecoveryCompletionDisposition::BackendError(error);
            }
        };
        if current.is_terminal() {
            return match self.release_for_recovery(&lease).await {
                RecoveryReleaseDisposition::Released => {
                    RecoveryCompletionDisposition::SettledByPeer(current.status)
                }
                RecoveryReleaseDisposition::BackendError(error) => {
                    RecoveryCompletionDisposition::BackendError(error)
                }
            };
        }
        let evidence = AbandonEvidence {
            writer: AbandonWriter::OwnerDrain,
            owner: Some(owner),
            epoch_ms: self.now_ms(),
        };
        self.complete_and_release(
            &lease,
            process_id,
            ProcessAwaitOutput::Abandoned {
                evidence: Box::new(evidence),
                control: None,
            },
        )
        .await
    }

    /// Unique lease owner for one recovery attempt.
    ///
    /// Derived from [`DurableProcessWorkerConfig::lease_owner`]: a fresh
    /// `(owner_id, incarnation_id)` per attempt keeps sweeps idempotent (a
    /// still-running attempt's live lease fences later passes instead of being
    /// re-entered as "own lease").
    fn recovery_lease_owner(&self) -> crate::LeaseOwnerIdentity {
        let attempt = uuid::Uuid::new_v4();
        crate::LeaseOwnerIdentity {
            owner_id: format!("{}:recovery:{attempt}", self.config.lease_owner.owner_id),
            incarnation_id: attempt.to_string(),
        }
    }

    /// Recover one non-terminal row, obeying its declared recovery disposition
    /// (ADR 0019). The verdict per disposition:
    ///
    /// - **ExternallyOwned**: never claimed, never run. If a pending Abandon
    ///   Request is present it is reconciled into `Abandoned{reconciled_request}`.
    /// - **Rerunnable**: exactly today's behavior — claim, (re-)run, complete.
    /// - **OwnerBound, never started**: any worker may run it (first execution is
    ///   not re-execution); the runner records `first_started` before executing.
    /// - **OwnerBound, started**: never re-run. A silent or expired holder is
    ///   left non-terminal unless an Abandon Request is present and the lease
    ///   has lapsed, which yields `Abandoned{reconciled_request}`. Elapsed time
    ///   alone never terminalizes.
    ///
    /// Every Abandoned write goes through `complete_process_with_lease`, which
    /// atomically validates this sweep's fence, appends the terminal, and clears
    /// the lease so a revenant's stale token is rejected.
    async fn recover_process(&self, record: ProcessRecord) {
        let process_id = record.id.clone();
        // ExternallyOwned: lash never executes the row. The only recovery action
        // is reconciling a pending Abandon Request; there is no owner lease to
        // wait out.
        if record.disposition == RecoveryDisposition::ExternallyOwned {
            if record.abandon_request.is_some() {
                self.reconcile_externally_owned_abandon(&process_id).await;
            }
            return;
        }

        let lease_ttl_ms = self.lease_timings().ttl_ms();
        let owner = self.recovery_lease_owner();
        // Claim the single-owner lease. A live holder or a claim error leaves
        // the row to its owner until the lease TTL expires.
        let lease = match self
            .claim_for_recovery(&process_id, &owner, lease_ttl_ms)
            .await
        {
            RecoveryClaimDisposition::Acquired(lease) => lease,
            RecoveryClaimDisposition::Busy | RecoveryClaimDisposition::BackendError(_) => return,
        };
        // Terminal between the list and the claim. Idempotent by process_id: do
        // not re-execute or re-terminalize a finished process.
        let record = match self.read_for_recovery(&process_id).await {
            RecoveryReadDisposition::Found(record) => *record,
            RecoveryReadDisposition::Absent | RecoveryReadDisposition::BackendError(_) => {
                self.release_or_log(&lease).await;
                return;
            }
        };
        if record.is_terminal() {
            self.release_or_log(&lease).await;
            return;
        }
        if record.disposition == RecoveryDisposition::Rerunnable
            && let (Some(max_attempts), Some(started)) =
                (record.max_attempts, record.first_started.as_deref())
            && started.attempt >= max_attempts
        {
            Self::observe_recovery_completion(
                self.complete_and_release(
                    &lease,
                    &process_id,
                    ProcessAwaitOutput::Abandoned {
                        evidence: Box::new(AbandonEvidence {
                            writer: AbandonWriter::EngineGaveUp,
                            owner: Some(started.owner.clone()),
                            epoch_ms: self.now_ms(),
                        }),
                        control: None,
                    },
                )
                .await,
            );
            return;
        }

        match record.disposition {
            // Rerunnable: claim, (re-)run, complete — exactly today's behavior.
            RecoveryDisposition::Rerunnable => Box::pin(self.run_and_complete(record, lease)).await,
            RecoveryDisposition::OwnerBound if record.first_started.is_some() => {
                // Started OwnerBound work is NEVER re-run — abandonment is the
                // only recovery. `first_started`'s owner is the lapsed owner the
                // reconciled-request evidence names.
                let lapsed_owner = record
                    .first_started
                    .as_ref()
                    .map(|started| started.owner.clone());
                let evidence = if record.abandon_request.is_some() {
                    // Silent/expired holder, with an
                    // operator authorized abandonment and the lease has lapsed
                    // (we acquired a free/expired lease) ⇒ Abandoned{reconciled}.
                    Some(AbandonEvidence {
                        writer: AbandonWriter::ReconciledRequest,
                        owner: lapsed_owner,
                        epoch_ms: self.now_ms(),
                    })
                } else {
                    // No authorization: elapsed time alone never terminalizes.
                    // Leave the row non-terminal.
                    None
                };
                match evidence {
                    Some(evidence) => {
                        Self::observe_recovery_completion(
                            self.complete_and_release(
                                &lease,
                                &process_id,
                                ProcessAwaitOutput::Abandoned {
                                    evidence: Box::new(evidence),
                                    control: None,
                                },
                            )
                            .await,
                        );
                    }
                    None => self.release_or_log(&lease).await,
                }
            }
            // OwnerBound, never started: first execution is not re-execution, so
            // any worker may run it; the runner records first_started first.
            RecoveryDisposition::OwnerBound => Box::pin(self.run_and_complete(record, lease)).await,
            // Filtered above; releasing keeps the lease honest if reached.
            RecoveryDisposition::ExternallyOwned => self.release_or_log(&lease).await,
        }
    }

    /// Wall-clock epoch ms from the worker's configured clock.
    fn now_ms(&self) -> u64 {
        self.config.runtime_host.clock.timestamp_ms()
    }

    /// Reconcile a pending Abandon Request on an externally-owned row into an
    /// `Abandoned{reconciled_request}` terminal. Lash never executed the row, so
    /// there is no owner lease to wait out — but the sweep claims its own lease
    /// and completes through the atomic fenced path so it stays the single writer.
    async fn reconcile_externally_owned_abandon(&self, process_id: &str) {
        let lease_ttl_ms = self.lease_timings().ttl_ms();
        let owner = self.recovery_lease_owner();
        let lease = match self
            .claim_for_recovery(process_id, &owner, lease_ttl_ms)
            .await
        {
            RecoveryClaimDisposition::Acquired(lease) => lease,
            RecoveryClaimDisposition::Busy | RecoveryClaimDisposition::BackendError(_) => return,
        };
        let current = match self.read_for_recovery(process_id).await {
            RecoveryReadDisposition::Found(current) => *current,
            RecoveryReadDisposition::Absent | RecoveryReadDisposition::BackendError(_) => {
                self.release_or_log(&lease).await;
                return;
            }
        };
        if current.is_terminal() {
            self.release_or_log(&lease).await;
            return;
        }
        let evidence = AbandonEvidence {
            writer: AbandonWriter::ReconciledRequest,
            // Externally-owned work has no lash execution owner to name.
            owner: None,
            epoch_ms: self.now_ms(),
        };
        Self::observe_recovery_completion(
            self.complete_and_release(
                &lease,
                process_id,
                ProcessAwaitOutput::Abandoned {
                    evidence: Box::new(evidence),
                    control: None,
                },
            )
            .await,
        );
    }

    /// (Re-)run a claimed row under its renewed lease and write the terminal
    /// outcome, the same live-owner-is-single-writer path used before ADR 0019.
    async fn run_and_complete(&self, record: ProcessRecord, lease: ProcessLease) {
        let process_id = record.id.clone();
        let registration = registration_from_record(record);
        let execution_context = ProcessExecutionContext::default();
        let mut handover = None;
        loop {
            match Box::pin(self.run_process_with_lease_renewal(
                registration.clone(),
                execution_context.clone(),
                lease.clone(),
                handover,
            ))
            .await
            {
                // Ran to a terminal outcome (success or a process-level failure) while
                // holding the lease: this owner is the single writer of the terminal.
                Ok(crate::ProcessRunOutcome::Terminal(output)) => {
                    Self::observe_recovery_completion(
                        self.complete_and_release(&lease, &process_id, *output)
                            .await,
                    );
                    return;
                }
                Ok(crate::ProcessRunOutcome::SegmentBoundary(next)) => {
                    tracing::debug!(
                        process_id = %process_id,
                        reason = ?next.reason,
                        "process crossed an in-memory segment boundary",
                    );
                    handover = Some(next);
                }
                // The lease was lost mid-run — another owner reclaimed the expired
                // lease and is now running this process. Do NOT write a terminal
                // outcome or release the lease: that would race the new owner and
                // could record a succeeded process as Failed. Leave the row to the
                // lease holder; it will finish (or another sweep retries it).
                Err(RecoverFailure::LeaseLost(_error)) => return,
                Err(RecoverFailure::BackendError(_error)) => {
                    // The typed backend event was emitted at the failing
                    // operation. Token-fenced release makes the row claimable
                    // for a healthy retry without risking a successor's lease.
                    self.release_or_log(&lease).await;
                    return;
                }
                // Rebuild/store-facet failures are infrastructure failures, not
                // producer outcomes. Release the claim without a terminal so a
                // later sweep can retry.
                Err(RecoverFailure::Run(err)) => {
                    tracing::warn!(
                        process_id = %process_id,
                        error = %err,
                        "process execution infrastructure failed; leaving process claimable",
                    );
                    self.release_or_log(&lease).await;
                    return;
                }
            }
        }
    }

    /// Run a recovered process while renewing its lease across the execution,
    /// mirroring the turn-lease renewal that keeps a long-running effect's lease
    /// from expiring under the live owner.
    async fn run_process_with_lease_renewal(
        &self,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        mut lease: ProcessLease,
        handover: Option<crate::SegmentHandover>,
    ) -> Result<crate::ProcessRunOutcome, RecoverFailure> {
        let process_id = registration.id.clone();
        self.ensure_stable_process_id(&registration)
            .map_err(RecoverFailure::Run)?;
        if registration.disposition == RecoveryDisposition::ExternallyOwned {
            return Err(RecoverFailure::Run(PluginError::Session(format!(
                "process `{}` is externally-owned and must not be executed by lash",
                registration.id
            ))));
        }
        let current = match self.read_for_recovery(&process_id).await {
            RecoveryReadDisposition::Found(current) => *current,
            RecoveryReadDisposition::Absent => {
                return Err(RecoverFailure::Run(PluginError::Session(format!(
                    "unknown process `{process_id}`"
                ))));
            }
            RecoveryReadDisposition::BackendError(error) => {
                return Err(RecoverFailure::BackendError(error));
            }
        };
        let cancellation = CancellationToken::new();
        let cancel_watcher = {
            let awaiter = self
                .config
                .process_change_hub
                .clone()
                .map(|hub| {
                    crate::ProcessAwaiter::new(Arc::clone(&self.config.process_registry), hub)
                })
                .unwrap_or_else(|| {
                    crate::ProcessAwaiter::polling(Arc::clone(&self.config.process_registry))
                });
            let process_id = process_id.clone();
            let cancellation = cancellation.clone();
            crate::task::spawn(async move {
                match awaiter
                    .await_event(&process_id, "process.cancel_requested", 0)
                    .await
                {
                    Ok(_) => cancellation.cancel(),
                    Err(err) => tracing::warn!(
                        process_id = %process_id,
                        error = %err,
                        "process cancel watcher stopped before observing cancellation",
                    ),
                }
            })
        };
        let scoped_effect_controller = self
            .config
            .runtime_host
            .control
            .effect_host
            .scoped_static(crate::ExecutionScope::process(registration.id.clone()))
            .map_err(|err| RecoverFailure::Run(PluginError::Session(err.to_string())))?
            .ok_or_else(|| {
                RecoverFailure::Run(PluginError::Session(
                    "process worker effect host must provide a static process scope".to_string(),
                ))
            })?;
        let pending = self.run_process_segment_from_current(
            registration,
            current,
            execution_context,
            crate::ProcessExecutionWriteAuthority::lease(lease.clone()),
            scoped_effect_controller,
            cancellation.clone(),
            handover,
        );
        tokio::pin!(pending);
        loop {
            tokio::select! {
                outcome = &mut pending => {
                    cancel_watcher.abort();
                    return outcome.map_err(RecoverFailure::Run);
                }
                _ = self.config.runtime_host.clock.sleep(self.lease_timings().renew_interval()) => {
                    match self
                        .config
                        .process_registry
                        .renew_process_lease(&lease, self.lease_timings().ttl_ms())
                        .await
                    {
                        Ok(renewed) => lease = renewed,
                        Err(err) => {
                            cancellation.cancel();
                            cancel_watcher.abort();
                            if matches!(&err, PluginError::ProcessLeaseSuperseded { .. }) {
                                self.recovery_lease_lost(
                                    &process_id,
                                    ProcessRecoveryOperation::RenewLease,
                                    &err,
                                );
                                return Err(RecoverFailure::LeaseLost(err));
                            }
                            return Err(RecoverFailure::BackendError(
                                self.recovery_backend_error(
                                    &process_id,
                                    ProcessRecoveryOperation::RenewLease,
                                    err,
                                ),
                            ));
                        }
                    }
                }
            }
        }
    }

    pub async fn request_process_cancel(
        &self,
        process_id: &str,
        reason: Option<String>,
    ) -> Result<(), PluginError> {
        self.config
            .process_registry
            .append_event(
                process_id,
                crate::ProcessEventAppendRequest::cancel_requested(process_id, reason),
            )
            .await
            .map(|_| ())
    }

    async fn runtime_for_registration(
        &self,
        registration: &ProcessRegistration,
    ) -> Result<LashRuntime, PluginError> {
        match registration.input.as_ref() {
            ProcessInput::SessionTurn { create_request, .. } => {
                Box::pin(self.runtime_for_session_turn(registration, create_request.as_ref())).await
            }
            ProcessInput::ToolCall { .. } | ProcessInput::Engine { .. } => {
                Box::pin(self.runtime_for_process_env(registration)).await
            }
            // Externally-owned rows are rejected before dispatch (ADR 0019), so an
            // External input has no execution runtime; fail loudly rather than
            // fabricate one.
            ProcessInput::External { .. } => Err(PluginError::Session(format!(
                "process `{}` is externally-owned and has no execution runtime",
                registration.id
            ))),
        }
    }

    async fn runtime_for_session_turn(
        &self,
        registration: &ProcessRegistration,
        create_request: &crate::SessionCreateRequest,
    ) -> Result<LashRuntime, PluginError> {
        let mut policy = create_request
            .policy
            .clone()
            .unwrap_or_else(|| self.config.session_policy.clone());
        if policy.recorded_provider_id().is_empty() {
            policy.provider_id = self.config.session_policy.provider_id.clone();
        }
        // Boxed: building a process runtime is a rare, cold path whose future
        // holds a whole session policy, so it stays off the caller's stack.
        Box::pin(self.build_process_runtime(
            crate::process_runtime_session_ids(&registration.id)[1].clone(),
            policy,
            create_request.plugin_options.clone(),
            "session turn request",
        ))
        .await
    }

    async fn runtime_for_process_env(
        &self,
        registration: &ProcessRegistration,
    ) -> Result<LashRuntime, PluginError> {
        let Some(env_ref) = registration.env_ref.as_ref() else {
            return Err(PluginError::Session(format!(
                "process `{}` is missing a captured execution env",
                registration.id
            )));
        };
        let env = crate::load_process_execution_env(
            self.config
                .runtime_host
                .durability
                .process_env_store
                .as_ref(),
            env_ref,
        )
        .await?;
        Box::pin(self.build_process_runtime(
            crate::process_runtime_session_ids(&registration.id)[0].clone(),
            env.policy,
            env.plugin_options,
            env_ref.as_str(),
        ))
        .await
    }

    async fn build_process_runtime(
        &self,
        session_id: String,
        policy: crate::SessionPolicy,
        plugin_options: crate::PluginOptions,
        source_label: &str,
    ) -> Result<LashRuntime, PluginError> {
        let attachment_manifest_store = self
            .config
            .session_store_factory
            .create_store(&crate::SessionStoreCreateRequest {
                session_id: session_id.clone(),
                relation: crate::SessionRelation::default(),
                policy: policy.clone(),
            })
            .await
            .map_err(|err| {
                PluginError::Session(format!(
                    "failed to open process attachment owner store for `{session_id}`: {err}"
                ))
            })?;
        // Process execution sessions are reconstruction-only and must not alias
        // a parent-bound factory's runtime state. Attachment intents still use
        // the factory store so their process owner remains durable.
        let store = Arc::new(InMemorySessionStore::default());
        let process_work_driver = self.config.process_work_driver.clone().unwrap_or_else(|| {
            if let Some(hub) = self.config.process_change_hub.clone() {
                ProcessWorkDriver::from_watched(
                    Arc::clone(&self.config.process_registry),
                    hub,
                    Arc::new(crate::InlineProcessRunHandle::new(self.clone())),
                )
            } else {
                ProcessWorkDriver::inline(Arc::clone(&self.config.process_registry), self.clone())
            }
        });
        let mut builder = EmbeddedRuntimeBuilder::new(
            self.config.runtime_host.durability.commit_budget,
            self.config.runtime_host.durability.queued_work_batching,
        )
        .with_session_id(session_id.to_string())
        .with_plugin_host(self.config.plugin_host.as_ref().clone())
        .with_runtime_host(self.config.runtime_host.clone())
        .with_policy(policy)
        .with_plugin_options(plugin_options)
        .with_session_store_factory(Arc::clone(&self.config.session_store_factory))
        .with_trigger_store(Arc::clone(&self.config.trigger_store))
        .with_process_registry(Arc::clone(&self.config.process_registry))
        .with_process_work_driver(process_work_driver)
        .with_attachment_manifest_store(attachment_manifest_store)
        .with_store(store);
        if let Some(driver) = self.config.queued_work_driver.clone() {
            builder = builder.with_queued_work_driver(driver);
        }
        Box::pin(builder.build()).await.map_err(|err| {
            PluginError::Session(format!(
                "failed to build process worker runtime for {source_label}: {err}"
            ))
        })
    }

    /// Enforce the stable-process-id invariant at every (re-)execution: process
    /// execution identity is the persisted `process_id`, so a retry — a Restate
    /// `run` re-invocation (keyed `LashProcessWorkflow/{process_id}`) or a
    /// recovery sweep re-running a non-terminal row — must present that stable
    /// id. An empty/fresh id has lost its idempotency anchor and is rejected
    /// loudly here, mirroring how `ExecutionScope` rejects an
    /// empty turn id at the durable-effect boundary.
    fn ensure_stable_process_id(
        &self,
        registration: &ProcessRegistration,
    ) -> Result<(), PluginError> {
        if registration.id.trim().is_empty() {
            return Err(PluginError::Session(
                crate::RuntimeError::missing_process_execution_id().to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod permit_tests;
#[cfg(test)]
mod recovery_tests;
