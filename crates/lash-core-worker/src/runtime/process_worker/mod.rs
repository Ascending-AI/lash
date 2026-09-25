use crate::ProcessId;
use crate::SessionId;
use lash_core::runtime::coalescing_scheduler::{
    CoalescingDispatcherGuard, CoalescingExtra, CoalescingMutex, CoalescingSchedulerHandle,
    CoalescingSchedulerState,
};
use lash_core::runtime::process_permit::SharedNotify;
use lash_sansio::sync::MutexExt;
use std::collections::BTreeSet;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use self::recovery::{RecoveryBackendError, RecoveryReadDisposition};
use self::registration::registration_from_record;
use self::worklist::{ProcessPassBegin, ProcessWorklistScan};

mod drain;
mod parent_end;
mod recovery;
mod registration;
#[path = "../native_substrate/worklist.rs"]
mod worklist;

use self::recovery::ProcessRecoveryOutcome;
#[cfg(test)]
use crate::{
    PROCESS_EXECUTION_PERMIT, ProcessExecutionPermit, ensure_process_execution_permit,
    inherit_process_execution_permit, release_process_execution_permit_while,
};

pub use self::recovery::{
    ProcessAdmissionDeferred, ProcessAdmissionIntake, ProcessAdmissionReport, ProcessDrainDeferred,
    ProcessDrainReport, ProcessRecoveryAttemptOutcome, ProcessRecoveryOperation,
    ProcessWorkerFault,
};
pub use crate::DEFAULT_PROCESS_EXECUTION_CONCURRENCY;

use crate::RuntimeHostConfig;
use crate::runtime::EmbeddedRuntimeBuilder;
use crate::{
    AbandonEvidence, AbandonWriter, LashRuntime, PluginError, PluginFactory, PluginHost,
    PluginStack, ProcessAwaitOutput, ProcessExecutionContext, ProcessInput, ProcessLease,
    ProcessRecord, ProcessRegistration, ProcessRegistry, RecoveryContract, SessionStoreFactory,
};
use lash_core::core_internal::RuntimeSessionServices;
use lash_core::core_internal::{
    DefaultWorkerSlotSupplier, ObservedWorkerSlotSupplier, WorkerCapacityMetrics,
    WorkerSlotSupplier as _,
};
use lash_core_execution::runtime::effect::ProcessRunner;

/// Validated per-worker native process execution concurrency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProcessExecutionConcurrency(usize);

impl ProcessExecutionConcurrency {
    const DEFAULT: Self = Self(crate::DEFAULT_PROCESS_EXECUTION_CONCURRENCY);

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

/// Invalid native process execution concurrency supplied by a host.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error(
    "process execution concurrency must be between 1 and {max} (inclusive), got {concurrency}",
    max = Semaphore::MAX_PERMITS
)]
pub struct ProcessExecutionConcurrencyError {
    concurrency: usize,
}

/// Who executes this worker's nested process work.
#[derive(Clone)]
pub enum WorkerProcessWork {
    /// Compose native process work over this worker.
    SelfNative(crate::WatchedRegistry),
    /// Use an externally composed process-work port.
    External(crate::ProcessWorkWiring),
}

/// Deployment-local configuration for rebuilding durable process executions.
///
/// Process rows intentionally carry only portable process input and provenance.
/// Workers provide plugins, providers, stores, secrets, and host capabilities
/// for the deployment that owns those rows.
#[derive(Clone)]
pub struct DurableProcessWorkerConfig {
    #[cfg(test)]
    cancel_watcher_ready: Option<Arc<tokio::sync::Notify>>,
    pub plugin_host: Arc<PluginHost>,
    /// The host config and its one backend, which supplies the session
    /// catalog and trigger store this worker reaches (ADR 0102, D2).
    pub runtime_host: RuntimeHostConfig,
    pub session_policy: crate::SessionPolicy,
    /// Host-facing sink this worker reports [`ProcessWorkerFault`]s on.
    ///
    /// Wire the same sink the registry decorator was built with: a drive
    /// admits rows and returns, so the faults that strand an admitted row
    /// afterwards have no other honest way back to the host.
    pub process_event_sink: Option<Arc<dyn crate::ProcessEventSink>>,
    pub native_substrate: crate::NativeSubstrateConfig,
    process_work: WorkerProcessWork,
    queued_work: Arc<dyn crate::QueuedWorkSubstrate>,
    pub turn_phase_probe_slot: crate::runtime::RuntimeTurnPhaseProbeSlot,
    /// A run holds its slot while doing its own work and releases it while parked on work that
    /// another process or external owner must complete.
    /// This is a per-worker bound: two workers sharing one registry may execute twice this
    /// many.
    process_execution_concurrency: ProcessExecutionConcurrency,
    worker_slot_supplier: Option<Arc<dyn super::WorkerSlotSupplier>>,
    /// Required host owner identity this worker derives per-recovery lease owners from.
    ///
    /// Each recovery attempt claims with a unique `(owner_id, incarnation_id)`
    /// derived from this identity — a live lease held by an earlier attempt
    /// must fence a later sweep pass rather than be re-entered as the same
    /// incarnation. Recovery waits for an earlier owner's lease TTL before
    /// taking over.
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
        process_work: WorkerProcessWork,
        queued_work: Arc<dyn crate::QueuedWorkSubstrate>,
        lease_owner: crate::LeaseOwnerIdentity,
    ) -> Self {
        Self {
            plugin_host,
            runtime_host,
            session_policy: crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
            process_event_sink: None,
            #[cfg(test)]
            cancel_watcher_ready: None,
            native_substrate: crate::NativeSubstrateConfig::default(),
            process_work,
            queued_work,
            turn_phase_probe_slot: crate::runtime::RuntimeTurnPhaseProbeSlot::default(),
            process_execution_concurrency: ProcessExecutionConcurrency::DEFAULT,
            worker_slot_supplier: None,
            lease_owner,
        }
    }

    /// The backend's session catalog, which every session this worker
    /// creates, opens or reconstructs goes through.
    pub fn session_store_factory(&self) -> Arc<dyn SessionStoreFactory> {
        self.runtime_host.session_store_factory()
    }

    /// The backend's trigger store, whose deliveries this worker drives.
    pub fn trigger_store(&self) -> Arc<dyn crate::TriggerStore> {
        self.runtime_host.trigger_store()
    }

    pub fn with_session_policy(mut self, policy: crate::SessionPolicy) -> Self {
        self.session_policy = policy;
        self
    }

    pub fn process_registry(&self) -> &Arc<dyn ProcessRegistry> {
        self.watched_registry().registry()
    }

    pub(crate) fn watched_registry(&self) -> &crate::WatchedRegistry {
        match &self.process_work {
            WorkerProcessWork::SelfNative(watched) => watched,
            WorkerProcessWork::External(wiring) => wiring.watched(),
        }
    }

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
    pub fn with_worker_slot_supplier(
        mut self,
        supplier: Arc<dyn super::WorkerSlotSupplier>,
    ) -> Self {
        self.worker_slot_supplier = Some(supplier);
        self
    }

    pub fn with_process_event_sink(mut self, sink: Arc<dyn crate::ProcessEventSink>) -> Self {
        self.process_event_sink = Some(sink);
        self
    }

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
        process_work: WorkerProcessWork,
        queued_work: Arc<dyn crate::QueuedWorkSubstrate>,
        lease_owner: crate::LeaseOwnerIdentity,
    ) -> Self {
        Self::new(
            Arc::new(PluginHost::new(plugin_factories.into_iter().collect())),
            runtime_host,
            process_work,
            queued_work,
            lease_owner,
        )
    }

    pub fn from_plugin_stack(
        plugin_stack: PluginStack,
        runtime_host: RuntimeHostConfig,
        process_work: WorkerProcessWork,
        queued_work: Arc<dyn crate::QueuedWorkSubstrate>,
        lease_owner: crate::LeaseOwnerIdentity,
    ) -> Self {
        Self::from_plugin_factories(
            plugin_stack.into_factories(),
            runtime_host,
            process_work,
            queued_work,
            lease_owner,
        )
    }
}

/// Reconstructable background-process worker.
pub struct DurableProcessWorker {
    config: Arc<DurableProcessWorkerConfig>,
    execution_scheduler: Arc<ProcessExecutionScheduler>,
    lifetime: Option<Arc<ProcessWorkerLifetime>>,
    /// The parent-end recovery pass's state, shared by every clone of one
    /// worker.
    parent_end: Arc<parent_end::ParentEndRecovery>,
}

impl Clone for DurableProcessWorker {
    fn clone(&self) -> Self {
        Self {
            config: Arc::clone(&self.config),
            execution_scheduler: Arc::clone(&self.execution_scheduler),
            lifetime: self.lifetime.clone(),
            parent_end: Arc::clone(&self.parent_end),
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

type ProcessExecutionSchedulerState =
    CoalescingSchedulerState<ProcessId, ProcessRecord, ProcessWorklistScan>;

/// `lock_recover` for the loom mutex the shared protocol's state swaps to
/// under `--cfg loom` (FIG-1161). Module-local like every other seam shim;
/// `MutexExt` still covers the real `std::sync::Mutex` sites in this file.
#[cfg(loom)]
mod loom_ext {
    pub trait LoomMutexExt<T: ?Sized> {
        fn lock_recover(&self) -> loom::sync::MutexGuard<'_, T>;
    }

    impl<T: ?Sized> LoomMutexExt<T> for loom::sync::Mutex<T> {
        fn lock_recover(&self) -> loom::sync::MutexGuard<'_, T> {
            self.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }
}

#[cfg(loom)]
use loom_ext::LoomMutexExt as _;

struct ProcessExecutionScheduler {
    slots: Arc<dyn super::WorkerSlotSupplier>,
    metrics: WorkerCapacityMetrics,
    state: CoalescingMutex<ProcessExecutionSchedulerState>,
    changed: Arc<SharedNotify>,
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
            state: CoalescingMutex::new(ProcessExecutionSchedulerState::default()),
            changed: Arc::new(SharedNotify::new()),
            shutdown: CancellationToken::new(),
        }
    }

    fn complete_execution(&self, process_id: &ProcessId) {
        // A completion with no running entry cannot be produced by the
        // protocol: a task exists only for a popped (running) record.
        self.complete(process_id);
    }
}

impl CoalescingSchedulerHandle for ProcessExecutionScheduler {
    type Key = ProcessId;
    type Work = ProcessRecord;
    type Extra = ProcessWorklistScan;

    fn state(
        &self,
    ) -> &CoalescingMutex<CoalescingSchedulerState<ProcessId, ProcessRecord, ProcessWorklistScan>>
    {
        &self.state
    }

    fn changed(&self) -> &Arc<SharedNotify> {
        &self.changed
    }

    fn slot_kind(&self) -> super::WorkerSlotKind {
        super::WorkerSlotKind::Process
    }

    fn metrics(&self) -> &WorkerCapacityMetrics {
        &self.metrics
    }
}

type ProcessExecutionDispatcherGuard = CoalescingDispatcherGuard<ProcessExecutionScheduler>;

struct ProcessExecutionTaskCompletion {
    process_id: ProcessId,
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
    pub fn new(
        config: DurableProcessWorkerConfig,
    ) -> Result<Self, crate::NativeSubstrateConfigError> {
        config.native_substrate.validate()?;
        let execution_scheduler = Arc::new(ProcessExecutionScheduler::new(
            config.process_execution_concurrency,
            config.worker_slot_supplier.clone(),
        ));
        let lifetime = Arc::new(ProcessWorkerLifetime {
            shutdown: execution_scheduler.shutdown.clone(),
        });
        Ok(Self {
            config: Arc::new(config),
            execution_scheduler,
            lifetime: Some(lifetime),
            parent_end: Arc::default(),
        })
    }

    pub fn from_shared_config(
        config: Arc<DurableProcessWorkerConfig>,
    ) -> Result<Self, crate::NativeSubstrateConfigError> {
        config.native_substrate.validate()?;
        let execution_scheduler = Arc::new(ProcessExecutionScheduler::new(
            config.process_execution_concurrency,
            config.worker_slot_supplier.clone(),
        ));
        let lifetime = Arc::new(ProcessWorkerLifetime {
            shutdown: execution_scheduler.shutdown.clone(),
        });
        Ok(Self {
            config,
            execution_scheduler,
            lifetime: Some(lifetime),
            parent_end: Arc::default(),
        })
    }

    fn detached_for_task(&self) -> Self {
        Self {
            config: Arc::clone(&self.config),
            execution_scheduler: Arc::clone(&self.execution_scheduler),
            lifetime: None,
            parent_end: Arc::clone(&self.parent_end),
        }
    }

    pub fn config(&self) -> &DurableProcessWorkerConfig {
        &self.config
    }

    #[expect(
        clippy::expect_used,
        reason = "the substrate config was validated when the worker was built"
    )]
    fn process_wiring(&self) -> crate::ProcessWorkWiring {
        let wiring = match &self.config.process_work {
            WorkerProcessWork::SelfNative(watched) => {
                let port: Arc<dyn crate::ProcessWorkSubstrate> =
                    Arc::new(crate::NativeProcessWork::new(watched, self.clone()));
                crate::ProcessWorkWiring::new(watched.clone(), port)
            }
            WorkerProcessWork::External(wiring) => wiring.clone(),
        };
        wiring
            .with_work_cadence(self.config.native_substrate.work_cadence.clone())
            .expect("native substrate config was validated when the worker was built")
    }

    /// The replay-key grammar `registration`'s engine journals under
    /// (FIG-3586): what the incarnation's start record must name. A durable
    /// substrate that records the start itself, before this worker runs the
    /// segment, stamps it from here so the record matches the one this worker
    /// would write (FIG-3588).
    pub fn replay_key_grammar(&self, registration: &ProcessRegistration) -> Option<u32> {
        match registration.input.as_ref() {
            crate::ProcessInput::Engine { kind, .. } => self
                .config
                .runtime_host
                .process_engines
                .require(kind)
                .ok()
                .and_then(|engine| engine.replay_key_grammar()),
            _ => None,
        }
    }

    /// Durable substrates use this method so a non-terminal boundary can end the current
    /// substrate invocation; the native worker's lease-fenced drive loops over segment
    /// boundaries internally.
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
        if registration.disposition == RecoveryContract::ExternallyOwned {
            return Err(PluginError::Session(format!(
                "process `{}` is externally-owned and must not be executed by lash",
                registration.id
            )));
        }
        let current = self
            .config
            .process_registry()
            .get_process(&registration.id)
            .await?
            .ok_or_else(|| {
                crate::runtime::registry_transitions::unknown_process(&registration.id)
            })?;
        self.run_process_segment_from_current(
            registration,
            current,
            execution_context,
            execution_write_authority,
            scoped_effect_controller,
            cancellation,
            handover,
            SegmentAdmissionOwner::Substrate,
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
        admission: SegmentAdmissionOwner,
    ) -> Result<crate::ProcessRunOutcome, PluginError> {
        let attachment_owner = crate::ProcessRef::from_record(&current);
        let (owner, fencing_token) = match &execution_write_authority {
            crate::ProcessExecutionWriteAuthority::Lease { lease, .. } => {
                (self.config.lease_owner.clone(), lease.fencing_token)
            }
            crate::ProcessExecutionWriteAuthority::Invocation {
                process_id,
                execution_id,
                ..
            } => (
                crate::LeaseOwnerIdentity::engine_process_execution(process_id, execution_id),
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
        // The start record's replay-key grammar (FIG-3586): the grammar the
        // incarnation's first attempt ran under, stamped from its engine once
        // and inherited by every later attempt, so an incarnation a
        // pre-cutover build started stays unstamped and is refused by an
        // engine that keys its journal by grammar.
        let replay_grammar = match current.first_started.as_deref() {
            Some(started) => started.replay_grammar,
            None => self.replay_key_grammar(&registration),
        };
        // A durable substrate admits a segment in its own journal before the
        // worker runs it (FIG-3588): the start record it wrote is read here,
        // never written again. A second write on every redrive would be live
        // registry I/O that answers differently once the process is terminal,
        // and a redrive reaches it after the terminal is stored (FIG-3673).
        // Only a record that holds no start yet is started here.
        let admitted = match (admission, current.first_started.is_some()) {
            (SegmentAdmissionOwner::Substrate, true) => current.clone(),
            _ => self
                .config
                .process_registry()
                .record_first_started_with_authority(
                    &registration.id,
                    crate::ProcessStarted {
                        owner,
                        fencing_token,
                        attempt,
                        started_at_ms: self.now_ms(),
                        replay_grammar,
                    },
                    &execution_write_authority,
                )
                .await?
                .into_record()?,
        };
        // The attachment owner was read from `current`, before the start that
        // admitted this execution: this worker's authority CAS, or the
        // substrate's own journaled start, which recorded the same record's
        // incarnation. Either refuses a superseded incarnation, so reaching here
        // means the admitted record still carries the incarnation we are about
        // to bind under. Pinned because an owner bound from a stale incarnation would
        // root the earlier incarnation's blobs forever (FIG-2980), and because
        // the binding below must never be hoisted above this call.
        debug_assert_eq!(
            admitted.incarnation, attachment_owner.incarnation,
            "attachment owner must carry the incarnation the authority CAS admitted"
        );
        let admitted_incarnation = admitted.incarnation;
        // The authority CAS above is the admission: the controller must
        // already carry the exact pair it returned, because the caller pinned
        // it from the same record read. A mismatch means the pin was minted
        // from a stale record — a same-name successor — which is refused as a
        // admission error, never relabelled (ADR 0099 §1).
        let cas_admission =
            crate::AdmittedScope::process(crate::ProcessRef::from_record(&admitted));
        if scoped_effect_controller.admitted_scope() != &cas_admission {
            return Err(PluginError::Runtime(crate::RuntimeError::new(
                crate::RuntimeErrorCode::ExecutionScopeAdmissionRefused,
                format!(
                    "process worker for `{}` was pinned to {:?} but the admission CAS admitted {:?}",
                    registration.id,
                    scoped_effect_controller.admitted_scope(),
                    cas_admission,
                ),
            )));
        }
        // A parked process (FIG-3586) is re-run to find out whether this build
        // can replay its journal: its park is lifted for the run and recorded
        // again if the run refuses again. A run that goes on to fail for any
        // other reason spends its attempt budget as usual.
        if current
            .wait
            .as_ref()
            .is_some_and(crate::WaitState::is_parked)
        {
            self.config
                .process_registry()
                .clear_process_wait_with_authority(&registration.id, &execution_write_authority)
                .await?;
        }
        let park_authority = execution_write_authority.clone();
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
                .bind_process_scoped(attachment_owner)
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
        let manager = RuntimeSessionServices::for_worker(&runtime, true).map_err(|err| {
            PluginError::Session(format!(
                "failed to build runtime env for process `{}`: {err}",
                registration.id
            ))
        })?;
        let process_id = registration.id.clone();
        let result = manager
            .run_process(
                // The opener is the name bound to the incarnation this run was
                // admitted under, read off the record the authority CAS
                // returned rather than re-read later (ADR 0099 §1).
                crate::execution::runtime::effect::AdmittedProcess {
                    registration,
                    incarnation: admitted_incarnation,
                },
                execution_context,
                Arc::clone(self.config.process_registry()),
                scoped_effect_controller,
                cancellation,
                handover,
            )
            .await
            .map_err(crate::ProcessInfraError::into_plugin_error);
        if let Err(error) = &result
            && let Some(refusal) = parking_refusal(error)
        {
            // The body refused to replay its journal with nothing dispatched
            // (FIG-3586): the process parks — non-terminal, with no terminal
            // evidence — until an operator acts. The park is what exempts its
            // later sweeps from the attempt budget.
            let wait = crate::WaitState {
                kind: crate::WaitKind::Parked {
                    code: refusal.code.as_str().to_string(),
                    message: refusal.message.clone(),
                },
                since_ms: self.now_ms(),
            };
            self.config
                .process_registry()
                .set_process_wait_with_authority(&process_id, wait, &park_authority)
                .await?;
        }
        result
    }

    /// Admit claimable non-terminal processes to this worker's execution
    /// scheduler and return what this call admitted.
    ///
    /// **This is an admission call, not a completion call.** It reads one intake
    /// page, hands those rows to the worker-scoped dispatcher, and returns; the
    /// dispatcher claims, runs, terminalizes, and pages onward in the
    /// background. A returned [`ProcessAdmissionReport`] therefore says which
    /// rows *entered* execution, never that any of them finished. Faults that
    /// strand an admitted row afterwards — a failed claim, read, terminal write,
    /// lease release, or runtime rebuild — and a worklist scan that stopped
    /// short are reported as [`ProcessWorkerFault`]s on the wired
    /// [`ProcessEventSink`](crate::runtime::ProcessEventSink); `Err` is reserved
    /// for this call's own failures (its parent-end pass, its trigger-delivery
    /// reconcile, or its own intake read).
    ///
    /// Waiting for a terminal outcome is the engine seam's job, not this one:
    /// use [`ProcessWorkSubstrate::await_process_terminal`](crate::ProcessWorkSubstrate::await_process_terminal).
    ///
    /// The report covers the rows this call took intake responsibility for: its
    /// own page. When a scan is already in flight this call records a rescan for
    /// it and reports nothing, and the continuation pages of a pass are admitted
    /// by the dispatcher after the starting call has returned.
    ///
    /// This is the sole native executor for every process start: live tool and
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
    pub async fn drive_pending_processes(&self) -> Result<ProcessAdmissionReport, PluginError> {
        self.redrive_missing_opener_parent_end_rows().await?;
        self.drive_pending_parent_end_plans().await?;
        // Absorbing its report keeps the outer call from reporting its own just-admitted rows
        // as somebody else's `Busy` when the scan below sees them already scheduled.
        let nested = self.reconcile_trigger_deliveries().await?;
        let available = std::num::NonZeroUsize::new(
            self.execution_scheduler
                .slots
                .available_slots(super::WorkerSlotKind::Process),
        )
        .unwrap_or(std::num::NonZeroUsize::MIN)
        .min(self.config.native_substrate.worker_sweep.intake_page);
        let (fetch_initial_page, should_start_dispatcher) = {
            let mut state = self.execution_scheduler.state.lock_recover();
            let fetch_initial_page = match state.extra.begin_pass() {
                ProcessPassBegin::FetchInitialPage => Some(available),
                ProcessPassBegin::Coalesced => None,
            };
            let should_start_dispatcher = state.claim_dispatcher();
            (fetch_initial_page, should_start_dispatcher)
        };
        // Coalesced until this call proves otherwise: an empty report from a
        // call that never read the worklist must not read as "nothing pending".
        let mut report = ProcessAdmissionReport {
            intake: ProcessAdmissionIntake::Coalesced,
            ..ProcessAdmissionReport::default()
        };
        if let Some(nested) = nested {
            report.absorb(nested);
        }
        if let Some(limit) = fetch_initial_page {
            let page = match self
                .config
                .process_registry()
                .list_non_terminal_page(limit, None)
                .await
            {
                Ok(page) => page,
                Err(error) => {
                    let mut state = self.execution_scheduler.state.lock_recover();
                    // A recorded rescan is consumed into the ready restart
                    // `scan_failed` schedules — that restart *is* the rescan.
                    // The repair runs whether or not this pass owned the
                    // dispatcher claim: a dispatcher already running picks up
                    // the ready restart on its next loop.
                    let restart_dispatcher = state.extra.scan_failed() && should_start_dispatcher;
                    if should_start_dispatcher && !restart_dispatcher {
                        state.release_dispatcher();
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
            report.absorb(self.install_worklist_page(page));
        }
        self.execution_scheduler.changed.notify_one();
        if should_start_dispatcher {
            let worker = self.detached_for_task();
            crate::task::spawn(async move { worker.run_process_execution_dispatcher().await });
        }
        Ok(report)
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
                    let process_id = record.id.clone();
                    // Install the execution budget only at the native worker
                    // boundary, never in the shared process-segment path.
                    let outcome = Box::pin(crate::core_internal::scope_process_execution_permit(
                        Arc::clone(&worker.execution_scheduler.slots),
                        permit,
                        Arc::clone(&worker.execution_scheduler.changed),
                        worker.recover_process(record),
                    ))
                    .await;
                    // The admitting call has long since returned Ok; this is the
                    // only surface left that can tell the host the row did not
                    // reach a terminal outcome.
                    worker.observe_recovery_outcome(&process_id, outcome).await;
                });
            }

            if let Some((limit, continuation)) = self.next_worklist_page_request() {
                match self
                    .fetch_worklist_page_with_retry(limit, continuation.clone())
                    .await
                {
                    Ok(page) => {
                        // Later pages are admitted by this dispatcher, past the
                        // return of the call that started the pass.
                        let _admitted = self.install_worklist_page(page);
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "process worklist scan remains incomplete after retry exhaustion");
                        {
                            let mut state = self.execution_scheduler.state.lock_recover();
                            state.extra.park_for_rescan(continuation);
                            state.release_dispatcher();
                        }
                        dispatcher_guard.disarm();
                        // Pass-scoped: the pass whose scan failed reports it. It
                        // is never folded into a later call's outcome.
                        self.emit_worker_fault(ProcessWorkerFault::WorklistScanIncomplete {
                            error: error.to_string(),
                        })
                        .await;
                        return;
                    }
                }
                continue;
            }

            let idle = {
                let state = self.execution_scheduler.state.lock_recover();
                state.queue_idle() && matches!(state.extra, ProcessWorklistScan::Idle)
            };

            // An idle dispatcher waits instead of ending. Ending was what made
            // the poke the only thing that ever looked at the store again: a
            // failed poke on an otherwise quiet host left its registered row
            // sitting there until some unrelated call happened to poke. The
            // wait races the shutdown token, so the loop still ends with the
            // worker that owns it.
            let rescan = self.config.native_substrate.worker_sweep.rescan_interval;
            let rescan_due = tokio::select! {
                biased;
                () = self.execution_scheduler.shutdown.cancelled() => return,
                _ = self.execution_scheduler.changed.notified() => false,
                () = self.config.runtime_host.clock.sleep(rescan), if idle => true,
            };
            if rescan_due {
                let mut state = self.execution_scheduler.state.lock_recover();
                // A fresh scan from the start of the worklist: a row this
                // dispatcher never saw is exactly the case being recovered.
                state.extra.schedule_idle_pass();
            }
        }
    }

    /// Start any trigger deliveries whose process row was never registered, and
    /// return the admission report of the re-entrant drive that follows, when
    /// one ran. `None` means this reconcile admitted nothing of its own, so the
    /// caller's report keeps whatever intake state the caller established.
    async fn reconcile_trigger_deliveries(
        &self,
    ) -> Result<Option<ProcessAdmissionReport>, PluginError> {
        let candidates = self.config.trigger_store().list_deliveries().await?;
        if candidates.is_empty() {
            return Ok(None);
        }
        let candidate_process_ids = candidates
            .iter()
            .map(|delivery| delivery.process_id.clone())
            .collect::<Vec<_>>();
        let missing_process_ids = self
            .config
            .process_registry()
            .filter_unregistered_process_ids(&candidate_process_ids)
            .await?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let process_work = self.process_wiring();
        // The reconcile admits what it starts in one explicit drive below and
        // hands that drive's report to its caller. A start's own advisory poke
        // would admit the row in a nested drive whose report nobody reads,
        // and this call would then meet its own row as `Busy`; the starts
        // therefore register without poking.
        #[expect(
            clippy::expect_used,
            reason = "the substrate config was validated when the worker was built"
        )]
        let start_wiring = crate::ProcessWorkWiring::new(
            process_work.watched().clone(),
            Arc::new(RegistrationOnlyProcessWork {
                inner: Arc::clone(process_work.port()),
            }),
        )
        .with_work_cadence(self.config.native_substrate.work_cadence.clone())
        .expect("native substrate config was validated when the worker was built");
        let router = crate::TriggerRouter::new(self.config.trigger_store(), start_wiring)
            .with_process_artifacts(
                Arc::clone(&self.config.runtime_host.durability.process_env_store),
                self.config.runtime_host.process_engines.clone(),
            );
        let mut started_any = false;
        for delivery in candidates {
            if missing_process_ids.contains(&delivery.process_id) {
                let Some(scoped_effect_controller) = self
                    .config
                    .runtime_host
                    .control
                    .effect_host
                    .scoped_static(
                        lash_core::AdmittedScope::unpinned(
                            lash_core::runtime::trigger_delivery_reconcile_scope(
                                &delivery.process_id,
                            ),
                        )
                        .map_err(|err| PluginError::Session(err.to_string()))?,
                    )
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
                        Arc::clone(self.config.process_registry()),
                        &scoped_effect_controller,
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
        if started_any {
            return Ok(Some(
                process_work
                    .port()
                    .admit_pending_processes("trigger_delivery_reconcile")
                    .await?,
            ));
        }
        Ok(None)
    }

    /// Terminalize one of this host's started OwnerBound rows as
    /// `Abandoned{OwnerDrain}` under a freshly claimed drain lease. Returns
    /// `Ok(())` for an acknowledged terminal write and the attempt outcome —
    /// contention, absence, peer settlement, lease loss, or backend failure —
    /// for anything else.
    async fn drain_one_owner_bound(
        &self,
        process_id: &ProcessId,
        owner: crate::LeaseOwnerIdentity,
    ) -> Result<(), ProcessRecoveryAttemptOutcome> {
        let (lease, _current) = self.claim_live_row_for_recovery(process_id).await?;
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
    ///
    /// Returns the typed outcome of the attempt rather than swallowing it: the
    /// dispatcher reports the fault-worthy ones on the worker's fault surface.
    async fn recover_process(&self, record: ProcessRecord) -> ProcessRecoveryOutcome {
        let process_id = record.id.clone();
        // ExternallyOwned: lash never executes the row. The only recovery action
        // is reconciling a pending Abandon Request; there is no owner lease to
        // wait out.
        if record.disposition == RecoveryContract::ExternallyOwned {
            if record.abandon_request.is_some() {
                return self.reconcile_externally_owned_abandon(&process_id).await;
            }
            return ProcessRecoveryOutcome::LeftToOwner;
        }

        let (lease, record) = match self.claim_live_row_for_recovery(&process_id).await {
            Ok(claimed) => claimed,
            Err(disposition) => return ProcessRecoveryOutcome::Deferred(disposition),
        };
        if record.disposition == RecoveryContract::Rerunnable
            && !record
                .wait
                .as_ref()
                .is_some_and(crate::WaitState::is_parked)
            && let (Some(max_attempts), Some(started)) =
                (record.max_attempts, record.first_started.as_deref())
            && started.attempt >= max_attempts
        {
            return ProcessRecoveryOutcome::from_completion(
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
        }

        match record.disposition {
            // Rerunnable: claim, (re-)run, complete — exactly today's behavior.
            RecoveryContract::Rerunnable => Box::pin(self.run_and_complete(record, lease)).await,
            RecoveryContract::OwnerBound if record.first_started.is_some() => {
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
                    None
                };
                match evidence {
                    Some(evidence) => ProcessRecoveryOutcome::from_completion(
                        self.complete_and_release(
                            &lease,
                            &process_id,
                            ProcessAwaitOutput::Abandoned {
                                evidence: Box::new(evidence),
                                control: None,
                            },
                        )
                        .await,
                    ),
                    None => {
                        self.release_or_outcome(&lease, ProcessRecoveryOutcome::LeftToOwner)
                            .await
                    }
                }
            }
            // OwnerBound, never started: first execution is not re-execution, so
            // any worker may run it; the runner records first_started first.
            RecoveryContract::OwnerBound => Box::pin(self.run_and_complete(record, lease)).await,
            // Filtered above; releasing keeps the lease honest if reached.
            RecoveryContract::ExternallyOwned => {
                self.release_or_outcome(&lease, ProcessRecoveryOutcome::LeftToOwner)
                    .await
            }
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
    async fn reconcile_externally_owned_abandon(
        &self,
        process_id: &ProcessId,
    ) -> ProcessRecoveryOutcome {
        let (lease, _current) = match self.claim_live_row_for_recovery(process_id).await {
            Ok(claimed) => claimed,
            Err(disposition) => return ProcessRecoveryOutcome::Deferred(disposition),
        };
        let evidence = AbandonEvidence {
            writer: AbandonWriter::ReconciledRequest,
            // Externally-owned work has no lash execution owner to name.
            owner: None,
            epoch_ms: self.now_ms(),
        };
        ProcessRecoveryOutcome::from_completion(
            self.complete_and_release(
                &lease,
                process_id,
                ProcessAwaitOutput::Abandoned {
                    evidence: Box::new(evidence),
                    control: None,
                },
            )
            .await,
        )
    }

    /// (Re-)run a claimed row under its renewed lease and write the terminal
    /// outcome, the same live-owner-is-single-writer path used before ADR 0019.
    async fn run_and_complete(
        &self,
        record: ProcessRecord,
        lease: ProcessLease,
    ) -> ProcessRecoveryOutcome {
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
                Ok(crate::ProcessRunOutcome::Terminal { output }) => {
                    return self.finish_terminal_run(&lease, &process_id, output).await;
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
                Err(RecoverFailure::LeaseLost(_error)) => {
                    return ProcessRecoveryOutcome::Deferred(
                        ProcessRecoveryAttemptOutcome::LeaseLost {
                            operation: ProcessRecoveryOperation::RenewLease,
                        },
                    );
                }
                Err(RecoverFailure::BackendError(error)) => {
                    // The typed backend event was emitted at the failing
                    // operation. Token-fenced release makes the row claimable
                    // for a healthy retry without risking a successor's lease.
                    let _ = self.release_or_log(&lease).await;
                    return ProcessRecoveryOutcome::Deferred(error.into_public());
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
                    let _ = self.release_or_log(&lease).await;
                    return ProcessRecoveryOutcome::RunFailed(err);
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
        let requires_cancelled_session_turn = matches!(
            registration.input.as_ref(),
            ProcessInput::SessionTurn { .. }
        );
        self.ensure_stable_process_id(&registration)
            .map_err(RecoverFailure::Run)?;
        if registration.disposition == RecoveryContract::ExternallyOwned {
            return Err(RecoverFailure::Run(PluginError::Session(format!(
                "process `{}` is externally-owned and must not be executed by lash",
                registration.id
            ))));
        }
        let current = match self.read_for_recovery(&process_id).await {
            RecoveryReadDisposition::Found(current) => *current,
            RecoveryReadDisposition::Absent => {
                return Err(RecoverFailure::Run(
                    crate::runtime::registry_transitions::unknown_process(&process_id),
                ));
            }
            RecoveryReadDisposition::BackendError(error) => {
                return Err(RecoverFailure::BackendError(error));
            }
        };
        let cancellation = CancellationToken::new();
        if requires_cancelled_session_turn
            && self.cancellation_was_already_requested(&process_id).await?
        {
            cancellation.cancel();
        }
        let mut cancel_watcher = {
            let process_work = self.process_wiring();
            let process_id = process_id.clone();
            let cancellation = cancellation.clone();
            #[cfg(test)]
            let ready = self.config.cancel_watcher_ready.clone();
            crate::task::spawn(async move {
                let wait = process_work.event_awaiter().await_event(
                    &process_id,
                    "process.cancel_requested",
                    0,
                );
                tokio::pin!(wait);
                #[cfg(test)]
                if let Some(ready) = ready {
                    assert!(
                        futures_util::poll!(&mut wait).is_pending(),
                        "cancel watcher parks"
                    );
                    ready.notify_one();
                }
                match wait.await {
                    Ok(_) => {
                        cancellation.cancel();
                        std::future::pending::<Result<(), PluginError>>().await
                    }
                    Err(err) if requires_cancelled_session_turn => Err(err),
                    Err(err) => {
                        tracing::warn!(
                            process_id = %process_id,
                            error = %err,
                            "process cancel watcher stopped before observing cancellation",
                        );
                        std::future::pending::<Result<(), PluginError>>().await
                    }
                }
            })
        };
        let scoped_effect_controller = self
            .config
            .runtime_host
            .control
            .effect_host
            .scoped_static(crate::AdmittedScope::process(
                crate::ProcessRef::from_record(&current),
            ))
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
            SegmentAdmissionOwner::Worker,
        );
        tokio::pin!(pending);
        loop {
            tokio::select! {
                biased;
                watcher = &mut cancel_watcher => {
                    return match watcher {
                        Ok(Err(error)) => Err(RecoverFailure::BackendError(
                            self.recovery_backend_error(
                                &process_id,
                                ProcessRecoveryOperation::ReadProcess,
                                error,
                            ),
                        )),
                        Ok(Ok(())) => unreachable!("successful cancel watcher remains pending"),
                        Err(error) => Err(RecoverFailure::Run(PluginError::Session(format!(
                            "process `{process_id}` cancel watcher task failed: {error}"
                        )))),
                    };
                }
                outcome = &mut pending => {
                    cancel_watcher.abort();
                    // A committed cancellation outranks a runner success: if
                    // the runner settled before observing the cancel signal,
                    // the recorded terminal is still `Cancelled`. The child
                    // session and its committed turn stay retained either way.
                    let outcome = if requires_cancelled_session_turn
                        && self.cancellation_was_already_requested(&process_id).await?
                        && runner_outcome_requires_cancel_fence(&outcome)
                    {
                        Ok(crate::ProcessRunOutcome::Terminal {
                            output: Box::new(crate::ProcessAwaitOutput::from_tool_output(
                                crate::ToolCallOutput::cancelled(
                                    crate::ToolCancellation::runtime(format!(
                                        "process `{process_id}` was cancelled"
                                    )),
                                ),
                            )),
                        })
                    } else {
                        outcome
                    };
                    return outcome.map_err(RecoverFailure::Run);
                }
                _ = self.config.runtime_host.clock.sleep(self.lease_timings().renew_interval()) => {
                    match self
                        .config
                        .process_registry()
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

    async fn cancellation_was_already_requested(
        &self,
        process_id: &ProcessId,
    ) -> Result<bool, RecoverFailure> {
        self.config
            .process_registry()
            .get_process(process_id)
            .await
            .map_err(|error| {
                RecoverFailure::BackendError(self.recovery_backend_error(
                    process_id,
                    ProcessRecoveryOperation::ReadProcess,
                    error,
                ))
            })
            .and_then(|record| {
                record
                    .map(|record| record.cancel_request.is_some())
                    .ok_or_else(|| {
                        RecoverFailure::Run(crate::runtime::registry_transitions::unknown_process(
                            process_id,
                        ))
                    })
            })
    }

    pub async fn request_process_cancel(
        &self,
        process_ref: &crate::ProcessRef,
        request: &crate::CancelRequest,
    ) -> Result<(), PluginError> {
        self.config
            .process_registry()
            .append_event_ref(
                process_ref,
                crate::ProcessEventAppendRequest::cancel_requested(process_ref, request),
            )
            .await
            .map(|_| ())
    }

    /// Ask the child turn a `SessionTurn` process drives to stop now, as a
    /// durable request on the turn's cancellation gate (FIG-3673).
    ///
    /// A process cancel reaches its child turn through this request whether
    /// or not the process is running anywhere: the turn honours it where it
    /// honours any request, and a redrive of the turn observes it through its
    /// recorded peeks. The request is idempotent under one id per process
    /// incarnation. A process that is not a `SessionTurn`, or whose child
    /// session id is not recorded, has no addressable turn and is left to its
    /// recorded waits and peeks.
    pub async fn request_session_turn_child_stop(
        &self,
        record: &crate::ProcessRecord,
        request: &crate::CancelRequest,
    ) -> Result<(), PluginError> {
        let ProcessInput::SessionTurn { create_request, .. } = record.input.as_ref() else {
            return Ok(());
        };
        let Some(session_id) = create_request.session_id.clone() else {
            return Ok(());
        };
        let turn_request = crate::TurnCancelRequest::new(
            crate::TurnAddress::new(session_id, crate::TurnId::from(record.id.as_str())),
            format!("process-cancel:{}:{}", record.id, record.incarnation),
            Some(request.requester.clone()),
        );
        crate::TurnWorkDriver::for_catalog(
            Arc::clone(&self.config.runtime_host.control.effect_host),
            self.config.session_store_factory(),
        )
        .request_cancel(turn_request)
        .await
        .map(|_| ())
        .map_err(PluginError::Runtime)
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
        let env = crate::runtime::load_process_execution_env(
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
        session_id: SessionId,
        policy: crate::SessionPolicy,
        plugin_options: crate::PluginOptions,
        source_label: &str,
    ) -> Result<LashRuntime, PluginError> {
        let attachment_manifest_store = self
            .config
            .session_store_factory()
            .create_store(&crate::SessionStoreCreateRequest {
                pending_observer_intents: Vec::new(),
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
        // A process execution runtime is reconstruction-only: it runs on the
        // storeless path, keeps its session state in memory for the run and
        // persists none of it, so it never aliases a parent-bound catalog's
        // runtime state. Attachment intents still go to the catalog store so
        // the process owner of every blob stays durable.
        let process_work = self.process_wiring();
        let builder = EmbeddedRuntimeBuilder::new(
            self.config.runtime_host.clone(),
            self.config.lease_owner.clone(),
        )
        .with_session_id(session_id.to_string())
        .with_plugin_host(self.config.plugin_host.as_ref().clone())
        .with_policy(policy)
        .with_plugin_options(plugin_options)
        .with_process_work(process_work)
        .with_attachment_manifest_store(attachment_manifest_store)
        .with_queued_work(Arc::clone(&self.config.queued_work));
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

fn runner_outcome_requires_cancel_fence(
    outcome: &Result<crate::ProcessRunOutcome, PluginError>,
) -> bool {
    matches!(
        outcome,
        Ok(outcome)
            if outcome
                .terminal_output()
                .and_then(crate::ProcessAwaitOutput::terminal_status)
                != Some(crate::ProcessStatus::Cancelled)
    )
}

#[async_trait::async_trait]
impl super::native_substrate::NativeProcessAdmissionDriver for DurableProcessWorker {
    fn native_work_cadence(&self) -> crate::WorkCadencePolicy {
        self.config().native_substrate.work_cadence.clone()
    }

    async fn drive_pending_processes(&self) -> Result<ProcessAdmissionReport, PluginError> {
        DurableProcessWorker::drive_pending_processes(self).await
    }
}

/// The process port a trigger-delivery reconcile starts rows through: it
/// registers them and leaves admission to the reconcile's own explicit drive,
/// whose report reaches the caller. Waiting on a started row is the worker's
/// port as usual.
struct RegistrationOnlyProcessWork {
    inner: Arc<dyn crate::ProcessWorkSubstrate>,
}

#[async_trait::async_trait]
impl crate::ProcessWorkSubstrate for RegistrationOnlyProcessWork {
    async fn admit_pending_processes(
        &self,
        _reason: &str,
    ) -> Result<ProcessAdmissionReport, PluginError> {
        Ok(ProcessAdmissionReport {
            intake: ProcessAdmissionIntake::Coalesced,
            ..ProcessAdmissionReport::default()
        })
    }

    async fn await_process_terminal(
        &self,
        process_ref: &crate::ProcessRef,
    ) -> Result<crate::ProcessTerminalWait, PluginError> {
        self.inner.await_process_terminal(process_ref).await
    }
}

#[cfg(test)]
mod permit_tests;
#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod test_backend;

/// The replay refusal a process run ended on, when it is one that parks
/// (FIG-3586): the body could not replay its journal and dispatched nothing.
fn parking_refusal(error: &PluginError) -> Option<&crate::RuntimeEffectControllerError> {
    match error {
        PluginError::RuntimeEffectController(refusal) if refusal.code.parks_turn() => Some(refusal),
        _ => None,
    }
}

/// Who recorded a segment's start before the worker runs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SegmentAdmissionOwner {
    /// The worker's own lease-fenced start CAS admits the attempt.
    Worker,
    /// A durable substrate admitted the segment in its own journal and wrote
    /// the start record; the worker only reads it.
    Substrate,
}
