use crate::ProcessId;
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

mod park;

use crate::RuntimeHostConfig;
use crate::{
    DeploymentStore, PluginError, PluginFactory, PluginHost, PluginStack, ProcessExecutionContext,
    ProcessInput, ProcessRecord, ProcessRegistration, ProcessRegistry,
};
use lash_core::core_internal::{ProcessRuntimeContext, ProcessRuntimePorts};
use lash_core_execution::runtime::effect::ProcessRunner;

/// Deployment-local configuration for rebuilding durable process executions.
///
/// Process rows intentionally carry only portable process input and provenance.
/// Workers provide plugins, providers, stores, secrets, and host capabilities
/// for the deployment that owns those rows.
#[derive(Clone)]
pub struct DurableProcessWorkerConfig {
    pub plugin_host: Arc<PluginHost>,
    /// The host config and its one backend, which supplies the session
    /// catalog and trigger store this worker reaches (ADR 0102, D2).
    pub runtime_host: RuntimeHostConfig,
    /// The config a session-turn process's session is created with where
    /// its recorded create request states none. A host names it: there is
    /// no implicit default (FIG-4376).
    pub session_policy: crate::SessionPolicy,
    /// Pacing of the registry waits a process run makes through this
    /// worker's process work.
    pub work_cadence: crate::WorkCadencePolicy,
    process_work: crate::ProcessWorkWiring,
    queued_work: Arc<dyn crate::SessionWorkEngine>,
    pub turn_phase_probe_slot: crate::runtime::RuntimeTurnPhaseProbeSlot,
    /// The host owner identity a process runtime runs under.
    pub lease_owner: crate::LeaseOwnerIdentity,
}

impl DurableProcessWorkerConfig {
    pub fn new(
        plugin_host: Arc<PluginHost>,
        runtime_host: RuntimeHostConfig,
        process_work: crate::ProcessWorkWiring,
        queued_work: Arc<dyn crate::SessionWorkEngine>,
        lease_owner: crate::LeaseOwnerIdentity,
        session_policy: crate::SessionPolicy,
    ) -> Self {
        Self {
            plugin_host,
            runtime_host,
            session_policy,
            work_cadence: crate::WorkCadencePolicy::default(),
            process_work,
            queued_work,
            turn_phase_probe_slot: crate::runtime::RuntimeTurnPhaseProbeSlot::default(),
            lease_owner,
        }
    }

    /// The backend's session catalog, which every session this worker
    /// creates, opens or reconstructs goes through.
    pub fn session_store_factory(&self) -> Arc<dyn DeploymentStore> {
        self.runtime_host.session_store_factory()
    }

    /// The backend's trigger store.
    pub fn trigger_store(&self) -> Arc<dyn crate::TriggerStore> {
        self.runtime_host.trigger_store()
    }

    pub fn process_registry(&self) -> &Arc<dyn ProcessRegistry> {
        self.process_work.registry()
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
        process_work: crate::ProcessWorkWiring,
        queued_work: Arc<dyn crate::SessionWorkEngine>,
        lease_owner: crate::LeaseOwnerIdentity,
        session_policy: crate::SessionPolicy,
    ) -> Self {
        Self::new(
            Arc::new(PluginHost::new(plugin_factories.into_iter().collect())),
            runtime_host,
            process_work,
            queued_work,
            lease_owner,
            session_policy,
        )
    }

    pub fn from_plugin_stack(
        plugin_stack: PluginStack,
        runtime_host: RuntimeHostConfig,
        process_work: crate::ProcessWorkWiring,
        queued_work: Arc<dyn crate::SessionWorkEngine>,
        lease_owner: crate::LeaseOwnerIdentity,
        session_policy: crate::SessionPolicy,
    ) -> Self {
        Self::from_plugin_factories(
            plugin_stack.into_factories(),
            runtime_host,
            process_work,
            queued_work,
            lease_owner,
            session_policy,
        )
    }
}

/// Runs the segments of durable processes a substrate admitted: the engine's
/// process workflow hands each segment to [`Self::run_process_segment_with_scoped_effect_controller`].
#[derive(Clone)]
pub struct DurableProcessWorker {
    config: Arc<DurableProcessWorkerConfig>,
}

impl DurableProcessWorker {
    pub fn new(config: DurableProcessWorkerConfig) -> Result<Self, crate::WorkCadenceError> {
        config.work_cadence.validate()?;
        Ok(Self {
            config: Arc::new(config),
        })
    }

    pub fn config(&self) -> &DurableProcessWorkerConfig {
        &self.config
    }

    #[expect(
        clippy::expect_used,
        reason = "the work cadence was validated when the worker was built"
    )]
    fn process_wiring(&self) -> crate::ProcessWorkWiring {
        self.config
            .process_work
            .clone()
            .with_work_cadence(self.config.work_cadence.clone())
            .expect("the work cadence was validated when the worker was built")
    }

    /// The executable generation `registration`'s engine runs it as
    /// (FIG-3571): what the incarnation's start record must name. A durable
    /// substrate that records the start itself, before this worker runs the
    /// segment, stamps it from here so the record matches the one this worker
    /// would write (FIG-3588).
    pub fn executable_generation(
        &self,
        registration: &ProcessRegistration,
    ) -> Option<crate::ExecutableGeneration> {
        match registration.input.as_ref() {
            crate::ProcessInput::Engine { kind, payload } => self
                .config
                .runtime_host
                .process_engines
                .require(kind)
                .ok()
                .and_then(|engine| engine.program_identity(payload)),
            _ => None,
        }
    }

    /// A non-terminal boundary ends the current substrate invocation.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_process_segment_with_scoped_effect_controller(
        &self,
        process_id: ProcessId,
        registration: ProcessRegistration,
        execution_context: ProcessExecutionContext,
        execution_write_authority: crate::ProcessExecutionWriteAuthority,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
        cancellation: CancellationToken,
        handover: Option<crate::SegmentHandover>,
    ) -> Result<crate::ProcessRunOutcome, PluginError> {
        // Externally-owned rows are never executed by lash (ADR 0110): refuse
        // before touching a runtime.
        if registration.input.is_externally_owned() {
            return Err(PluginError::Session(format!(
                "process `{}` is externally-owned and must not be executed by lash",
                process_id
            )));
        }
        // The substrate's handler minted this controller from its own context;
        // it crosses this worker's effect host's stack here, once (FIG-3738).
        let scoped_effect_controller = self
            .config
            .runtime_host
            .backend()
            .effect_host()
            .route_handler_child_controller(scoped_effect_controller)
            .map_err(PluginError::Runtime)?;
        let current = self
            .config
            .process_registry()
            .get_process(&process_id)
            .await?
            .ok_or_else(|| crate::runtime::registry_transitions::unknown_process(&process_id))?;
        self.run_process_segment_from_current(
            process_id,
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
        process_id: ProcessId,
        registration: ProcessRegistration,
        current: ProcessRecord,
        execution_context: ProcessExecutionContext,
        execution_write_authority: crate::ProcessExecutionWriteAuthority,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
        cancellation: CancellationToken,
        handover: Option<crate::SegmentHandover>,
    ) -> Result<crate::ProcessRunOutcome, PluginError> {
        let owner = execution_write_authority.owner_identity();
        let attempt = current.first_started.as_deref().map_or(1, |started| {
            if started.owner.same_incarnation(&owner) {
                started.attempt
            } else {
                started.attempt.saturating_add(1)
            }
        });
        let execution_write_authority = execution_write_authority.bind_attempt(attempt);
        // The start record's executable generation (FIG-3571): the one the
        // incarnation's first attempt ran as, stamped from its engine once and
        // inherited by every later attempt, never re-derived, so an
        // incarnation another build started keeps that build's stamp.
        let current_generation = self.executable_generation(&registration);
        let generation = match current.first_started.as_deref() {
            Some(started) => started.generation.clone(),
            None => current_generation.clone(),
        };
        // A durable substrate admits a segment in its own journal before the
        // worker runs it (FIG-3588): the start record it wrote is read here,
        // never written again. A second write on every redrive would be live
        // registry I/O that answers differently once the process is terminal,
        // and a redrive reaches it after the terminal is stored (FIG-3673).
        // Only a record that holds no start yet is started here.
        let admitted = match current.first_started.is_some() {
            true => current.clone(),
            false => self
                .config
                .process_registry()
                .record_first_started_with_authority(
                    &process_id,
                    crate::ProcessStarted {
                        owner,
                        attempt,
                        started_at_ms: self.now_ms(),
                        build_generation: None,
                        generation,
                    },
                    &execution_write_authority,
                )
                .await?
                .into_record(),
        };
        // The authority CAS above is the admission: the controller must
        // already be admitted for the process it returned (ADR 0099 §1).
        let cas_admission = crate::AdmittedScope::process(admitted.id.clone());
        if scoped_effect_controller.admitted_scope() != &cas_admission {
            return Err(PluginError::Runtime(crate::RuntimeError::new(
                crate::RuntimeErrorCode::ExecutionScopeAdmissionRefused,
                format!(
                    "process worker for `{}` was pinned to {:?} but the admission CAS admitted {:?}",
                    process_id,
                    scoped_effect_controller.admitted_scope(),
                    cas_admission,
                ),
            )));
        }
        // A parked process (FIG-3586) is re-run to find out whether this build
        // can replay its journal. The park stays on the record — listed, with
        // its `since_ms` and `park_id` — while the rerun no longer exempts the
        // process from its attempt budget: a run that refuses again re-parks
        // it, and a run that goes on to fail for any other reason spends its
        // budget as usual (FIG-3659 NOW-B).
        if current.is_refusing_park() {
            self.config
                .process_registry()
                .begin_parked_rerun_with_authority(&process_id, &execution_write_authority)
                .await?;
        }
        let park_authority = execution_write_authority.clone();
        // The generation fence (FIG-3571), before the runtime, the artifact
        // load and the compile: an incarnation started under another
        // executable generation, or before the stamp existed, parks typed with
        // nothing run. The admitted record is the one the start just returned.
        let started_under = admitted
            .first_started
            .as_deref()
            .and_then(|started| started.generation.as_ref());
        if let Err(refusal) =
            crate::ExecutableGenerationRefusal::check(started_under, current_generation)
        {
            let error =
                PluginError::Runtime(crate::RuntimeError::retired_process_generation(refusal));
            self.park_refused_process(&process_id, &error, &park_authority)
                .await?;
            return Err(error);
        }
        let execution_context =
            execution_context.with_execution_write_authority(execution_write_authority);
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
        let turn_phase_probe =
            probe_scope.and_then(|scope| self.config.turn_phase_probe_slot.get_for_scope(scope));
        // The opener is the name bound to the incarnation this run was
        // admitted under, read off the record the authority CAS returned
        // rather than re-read later (ADR 0099 §1).
        let admitted = crate::execution::runtime::effect::AdmittedProcess {
            registration,
            process_id: process_id.clone(),
        };
        let runtime = Box::pin(ProcessRuntimeContext::for_admitted(
            ProcessRuntimePorts {
                host: self.config.runtime_host.clone(),
                plugin_host: Arc::clone(&self.config.plugin_host),
                process_work: self.process_wiring(),
                queued_work: Arc::clone(&self.config.queued_work),
                lease_owner: self.config.lease_owner.clone(),
                turn_phase_probe,
            },
            &admitted,
            self.config.session_policy.clone(),
        ))
        .await
        .map_err(|err| {
            PluginError::Session(format!(
                "failed to build the runtime of process `{process_id}`: {err}"
            ))
        })?;
        let result = runtime
            .run_process(
                admitted,
                execution_context,
                Arc::clone(self.config.process_registry()),
                scoped_effect_controller,
                cancellation,
                handover,
            )
            .await
            .map_err(crate::ProcessInfraError::into_plugin_error);
        if let Err(error) = &result {
            self.park_refused_process(&process_id, error, &park_authority)
                .await?;
        }
        result
    }

    /// Wall-clock epoch ms from the worker's configured clock.
    fn now_ms(&self) -> u64 {
        self.config.runtime_host.clock.timestamp_ms()
    }

    pub async fn request_process_cancel(
        &self,
        process_id: &crate::ProcessId,
        request: &crate::CancelRequest,
    ) -> Result<(), PluginError> {
        self.config
            .process_registry()
            .append_event(
                process_id,
                crate::ProcessEventAppendRequest::cancel_requested(process_id, request),
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
            format!("process-cancel:{}", record.id),
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
}
