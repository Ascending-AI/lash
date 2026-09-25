//! Deployment-level Restate effect host.
//!
//! One responsibility: give a long-lived Lash core a durable await-event
//! boundary when no Restate handler context is in scope. Real effect execution
//! needs a handler, so this host resolves, peeks, awaits, durably cancels, and
//! revokes waits through the ingress and fails loudly for anything else instead
//! of falling back to native execution.

use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use std::sync::{Arc, OnceLock};

use lash_core::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, CompletionKeyPreparation,
    EffectGroupHandle, EffectHost, ExecutionScope, GroupExecutors, GroupSettlement, LoserPolicy,
    Resolution, ResolveOutcome, RuntimeEffectCommand, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectGroup,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError, RuntimeErrorCode,
    ScopedEffectController, ToolIntentOutcomeSink, ToolIntentPreparation,
    facade_support::{RuntimeAwaitEventOptions, ToolChildHost},
};

use crate::durable_wait::{
    RestateDurableWaitAddress, RestateDurableWaitResolveRequest, RestateDurableWaitResolveResponse,
    RestateTurnCancelClosureParticipantRequest, durable_wait_index_key_for_scope,
    durable_wait_index_object_key, restate_await_event_key_for_authority,
    restate_await_event_key_is_valid, restate_await_event_key_is_valid_for_authority,
    restate_durable_wait_request, restate_unknown_or_revoked,
};
use crate::effect_group::{
    EffectGroupCloseDisposition, EffectGroupCloseRequest, EffectGroupCloseResponse,
    EffectGroupDispatchRequest, EffectGroupOpenRequest, EffectGroupOpenResponse,
    EffectGroupPayloadGetResponse, EffectGroupProbeResponse, EffectGroupReadRankRequest,
    EffectGroupReadRankResponse, EffectGroupSettlementTerminal, EffectGroupShape,
    EffectGroupWaitResolution, decode_wait_resolution, drained_wait_lifted, drained_wait_request,
    group_shape_error, payload_key, rank_wait_request, ready_wait_request, settlement_from_payload,
};
use crate::{LashService, RestateAuthorityId, RestateConnection, RestateIngressClient};

/// Deployment-level Restate effect host for long-lived Lash cores.
///
/// Restate's real effect execution requires a handler context, so this host is
/// intentionally a durable boundary, not an executor. HTTP/API code should
/// enter a Restate workflow/object first and then pass
/// [`RestateRuntimeEffectController::scoped_effect_controller`](crate::RestateRuntimeEffectController::scoped_effect_controller)
/// into Lash. If a caller tries to execute through this deployment host
/// directly, it fails loudly instead of falling back to native execution.
#[derive(Clone)]
pub struct RestateEffectHost {
    controller: Arc<RestateEffectHostController>,
    /// This host's one tool-child wiring (ADR 0099 §2), shared across clones.
    ///
    /// Beside the controller's `group_executors` rather than inside it,
    /// because the two answer different questions: that cell holds whatever
    /// resolver was registered, and this one holds the live-opener registry a
    /// turn or process must register its opener in. A get-or-init, so a host
    /// backing several runtimes hands them all the same registry — two
    /// registries on one host would mean a turn registering in one while the
    /// resolver read the other.
    tool_children: Arc<OnceLock<Arc<ToolChildHost>>>,
    turn_attach: Arc<crate::turn::RestateTurnAttach>,
    turn_control_binding_id: Arc<str>,
}

impl RestateEffectHost {
    pub fn new(connection: impl Into<RestateConnection>, authority_id: RestateAuthorityId) -> Self {
        let connection = connection.into();
        let turn_control_binding_id: Arc<str> = Arc::from(authority_id.binding_id());
        let turn_attach_authority_id = authority_id.clone();
        Self {
            controller: Arc::new(RestateEffectHostController {
                await_event_ingress: RestateAwaitEventIngress {
                    ingress: RestateIngressClient::new(connection.clone()),
                },
                authority_id,
                registrations: std::sync::Mutex::new(None),
                group_executors: OnceLock::new(),
            }),
            tool_children: Arc::new(OnceLock::new()),
            turn_attach: Arc::new(crate::turn::RestateTurnAttach::new(
                connection,
                turn_attach_authority_id,
            )),
            turn_control_binding_id,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(connection: impl Into<RestateConnection>) -> Self {
        Self::new(
            connection,
            RestateAuthorityId::new("lash-restate-tests").expect("valid test authority"),
        )
    }

    pub(crate) fn turn_attach_handle(&self) -> Arc<crate::turn::RestateTurnAttach> {
        Arc::clone(&self.turn_attach)
    }

    /// The deployment's durable-authority identity: what the endpoint binds a
    /// tool child's handler-scoped controller with, and what await-event keys
    /// and cancellation bindings derive from.
    pub fn authority_id(&self) -> &RestateAuthorityId {
        &self.controller.authority_id
    }

    /// Register this host's envelope→executor resolver, once.
    ///
    /// One host has one answer to "what code runs this journaled grouped
    /// child", so a second registration of a *different* resolver is refused
    /// and re-registering the resolver already held is a no-op. Until a
    /// resolver is registered the endpoint's dispatch routes nothing it is
    /// asked to execute — the lazy handle [`group_executors`] hands out reads
    /// the same cell at call time.
    ///
    /// [`group_executors`]: RestateEffectHost::group_executors
    pub fn register_group_executors(
        &self,
        executors: Arc<dyn GroupExecutors>,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.controller.register_group_executors(executors)
    }

    /// This host's resolver as the endpoint sees it: a handle that reads the
    /// registration cell at call time, so services built before wiring — or a
    /// tool-child host installed later by `install_tool_child_host` — resolve
    /// through the one answer the host holds. `None` from `executor_for`
    /// while nothing is registered is the routing fact "not mine", not a
    /// failure.
    pub fn group_executors(&self) -> Arc<dyn GroupExecutors> {
        Arc::new(RestateHostGroupExecutors {
            controller: Arc::clone(&self.controller),
        })
    }
}

/// The deployment host's registered resolver, read at call time.
///
/// The endpoint's `EffectGroupDispatch` holds this from construction: it must
/// not capture a resolver snapshot, because the one registration a
/// `ToolChildHost` install performs can land after the services were built.
struct RestateHostGroupExecutors {
    controller: Arc<RestateEffectHostController>,
}

impl GroupExecutors for RestateHostGroupExecutors {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        self.controller
            .group_executors
            .get()?
            .executor_for(envelope)
    }

    fn routes(&self, envelope: &RuntimeEffectEnvelope) -> bool {
        self.controller
            .group_executors
            .get()
            .is_some_and(|executors| executors.routes(envelope))
    }

    /// The registered resolver's routing, read at call time like every
    /// other answer here; with nothing registered there is no host stack to
    /// route through.
    fn route_handler_child_controller<'run>(
        &self,
        controller: ScopedEffectController<'run>,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        match self.controller.group_executors.get() {
            Some(executors) => executors.route_handler_child_controller(controller),
            None => Ok(controller),
        }
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for RestateEffectHost {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(self.controller.authority_id.binding_id().to_string())
    }

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        if !may_defer {
            return Ok(CompletionKeyPreparation::NotNeeded);
        }
        self.await_event_key(scope, wait)
            .await
            .map(CompletionKeyPreparation::Issued)
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.controller.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.controller.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.controller.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.controller
            .await_await_event(key, cancel, deadline)
            .await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.controller
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.controller
            .cancel_await_events_for_session(session_id)
            .await
    }

    async fn retire_await_events_for_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.controller.retire_await_events_for_scope(scope).await
    }

    async fn retire_await_events_for_scope_if_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.controller
            .retire_await_events_for_scope_if_quiescent(scope)
            .await
    }

    async fn reinstate_await_event_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.controller.reinstate_await_event_scope(scope).await
    }

    async fn await_event_scope_is_retired(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.controller.await_event_scope_is_retired(scope).await
    }
}

#[async_trait::async_trait]
impl EffectHost for RestateEffectHost {
    fn turn_control_binding_id(&self) -> String {
        self.turn_control_binding_id.to_string()
    }

    async fn list_outstanding_await_event_keys(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<AwaitEventKey>, RuntimeError> {
        if session_id.trim().is_empty() {
            return Err(RuntimeError::new(
                RuntimeErrorCode::InvalidAwaitEventSessionId,
                "await-event session id must be non-empty",
            ));
        }
        let keys: Vec<AwaitEventKey> = self
            .controller
            .await_event_ingress
            .ingress
            .call_object_empty_json(LashService::DurableWaitIndex, session_id, "outstanding")
            .await
            .map_err(|err| {
                RuntimeError::new(
                    RuntimeErrorCode::EngineAwaitEventPeek,
                    format!("failed to list outstanding Restate await-events: {err}"),
                )
            })?;
        Ok(outstanding_owned_by_session(session_id, keys))
    }

    fn turn_attach(&self) -> Option<Arc<dyn lash_core::facade_support::TurnAttach>> {
        Some(self.turn_attach.clone())
    }

    fn await_event_resolver(&self) -> &dyn lash_core::AwaitEventResolver {
        self
    }

    /// The deployment host binds one fence-checking controller per scope:
    /// a retired process or runtime-operation scope refuses every effect and
    /// group with `effect_scope_retired` before Restate is asked to run it,
    /// the same admission refusal the durable-journal hosts make at claim time.
    fn scoped<'run>(
        &'run self,
        admitted: lash_core::AdmittedScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        admitted.scope().validate()?;
        ScopedEffectController::shared(self.fenced_controller(admitted.clone()), admitted)
    }

    fn scoped_static(
        &self,
        admitted: lash_core::AdmittedScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        admitted.scope().validate()?;
        Ok(Some(ScopedEffectController::shared(
            self.fenced_controller(admitted.clone()),
            admitted,
        )?))
    }

    fn install_tool_child_host(&self, candidate: Arc<ToolChildHost>) -> Option<Arc<ToolChildHost>> {
        let installed = self.tool_children.get_or_init(|| candidate);
        // A resolver already registered by something else wins, and this host
        // then routes no tool children: one host has one answer to what runs a
        // grouped child, and quietly replacing that answer would make it depend
        // on which runtime was built last.
        self.register_group_executors(Arc::clone(installed) as Arc<dyn GroupExecutors>)
            .ok()?;
        Some(Arc::clone(installed))
    }

    async fn prepare_tool_intent(
        &self,
        _sink: &dyn ToolIntentOutcomeSink,
        _identity: &lash_core::ToolIntentIdentity,
        _intent: lash_core::ToolIntent,
    ) -> Result<ToolIntentPreparation, RuntimeError> {
        Ok(ToolIntentPreparation::ControllerOwned)
    }

    async fn record_tool_intent_outcome(
        &self,
        sink: &dyn ToolIntentOutcomeSink,
        identity: &lash_core::ToolIntentIdentity,
        submitted: lash_core::ToolIntent,
        outcome: lash_core::ToolIntentExecutionOutcome,
    ) -> Result<(), RuntimeError> {
        sink.retain_in_journal(identity, submitted, outcome).await
    }

    /// Restate owns invocation-journal retention natively, so no Lash-side
    /// replay ledger is deleted here and the count is always 0. The promise
    /// half is real: a scope-exact retirement revokes every durable wait the
    /// scope owns and fences later mints, resolves, peeks, awaits, effects,
    /// and groups under it through the scope's `LashDurableWaitIndex` object,
    /// which survives restarts and redeploys. Session retirements stay a
    /// no-op: session promises are revoked through the session lever the
    /// host already calls. A [`lash_core::EffectRetirementGate::WhenQuiescent`] request
    /// is refused with `effect_scope_not_quiescent` while a durable wait under
    /// the scope is unresolved: the only Lash-owned rows under a scope are its
    /// promises (effects in flight complete under Restate's own journal), and
    /// the scope's index object proves and revokes in one serialized step.
    async fn retire_effect_journal(
        &self,
        retirement: lash_core::EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        let Some(scope) = retirement.retired_scope() else {
            return Ok(0);
        };
        if retirement.gate() == Some(lash_core::EffectRetirementGate::WhenQuiescent) {
            if !self
                .controller
                .retire_await_events_for_scope_if_quiescent(&scope)
                .await?
            {
                let identity = scope.journal_identity()?;
                return Err(
                    lash_core::facade_support::effect_replay_driver::scope_not_quiescent(
                        identity.key(),
                    ),
                );
            }
            return Ok(0);
        }
        self.controller
            .retire_await_events_for_scope(&scope)
            .await?;
        Ok(0)
    }

    async fn reinstate_effect_scope(&self, scope: &ExecutionScope) -> Result<(), RuntimeError> {
        self.controller.reinstate_await_event_scope(scope).await
    }

    fn bind_process_registry(&self, binding: lash_core::ProcessRegistryBinding) {
        *self
            .controller
            .registrations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(binding.registrations);
    }

    async fn register_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.controller
            .register_turn_cancel_closure_participant(participant_id, scope)
            .await
    }

    async fn release_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.controller
            .release_turn_cancel_closure_participant(participant_id, scope)
            .await
    }
}

fn outstanding_owned_by_session(
    session_id: &SessionId,
    keys: Vec<AwaitEventKey>,
) -> Vec<AwaitEventKey> {
    keys.into_iter()
        .filter(|key| key.scope.session_id() == Some(session_id))
        .collect()
}

impl RestateEffectHost {
    fn fenced_controller(
        &self,
        admitted: lash_core::AdmittedScope,
    ) -> Arc<dyn RuntimeEffectController> {
        Arc::new(FencedRestateController {
            controller: self.controller.clone(),
            admitted,
        })
    }
}

/// One scope's view of the deployment host: forwards everything to the shared
/// controller and refuses effects and groups once the scope is retired.
struct FencedRestateController {
    controller: Arc<RestateEffectHostController>,
    /// The admitted scope this view serves; the groups it opens record it as
    /// their opener (FIG-3780).
    admitted: lash_core::AdmittedScope,
}

impl FencedRestateController {
    async fn refuse_if_retired(&self) -> Result<(), RuntimeEffectControllerError> {
        if self.admitted.scope().session_id().is_some() {
            return Ok(());
        }
        if self
            .controller
            .await_event_scope_is_retired(self.admitted.scope())
            .await?
        {
            let identity = self.admitted.scope().journal_identity()?;
            return Err(
                lash_core::facade_support::effect_replay_driver::scope_retired(identity.key()),
            );
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for FencedRestateController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(self.controller.authority_id.binding_id().to_string())
    }

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        self.controller
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.controller.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.controller.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.controller.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.controller
            .await_await_event(key, cancel, deadline)
            .await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.controller
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.controller
            .cancel_await_events_for_session(session_id)
            .await
    }

    async fn retire_await_events_for_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.controller.retire_await_events_for_scope(scope).await
    }

    async fn retire_await_events_for_scope_if_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.controller
            .retire_await_events_for_scope_if_quiescent(scope)
            .await
    }

    async fn reinstate_await_event_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.controller.reinstate_await_event_scope(scope).await
    }

    async fn await_event_scope_is_retired(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.controller.await_event_scope_is_retired(scope).await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for FencedRestateController {
    fn owns_commit_backpressure(&self) -> bool {
        self.controller.owns_commit_backpressure()
    }

    fn wants_segment_boundary(
        &self,
        progress: &lash_core::SegmentProgress,
    ) -> Option<lash_core::BoundaryReason> {
        self.controller.wants_segment_boundary(progress)
    }

    fn register_group_executors(
        &self,
        executors: Arc<dyn GroupExecutors>,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.controller.register_group_executors(executors)
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        envelope
            .invocation
            .validate_execution_scope(self.admitted.scope())?;
        self.refuse_if_retired().await?;
        self.controller
            .execute_effect(envelope, local_executor)
            .await
    }

    async fn open_effect_group(
        &self,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        group.validate_execution_scope(self.admitted.scope())?;
        self.refuse_if_retired().await?;
        // The group is a live child of this scope until its index reports
        // every child settled: recorded in the scope's index so a
        // `WhenQuiescent` retirement counts it (FIG-2499).
        if self.admitted.scope().session_id().is_none()
            && !self
                .controller
                .record_scope_group(self.admitted.scope(), group.group_key())
                .await?
        {
            let identity = self.admitted.scope().journal_identity()?;
            return Err(
                lash_core::facade_support::effect_replay_driver::scope_retired(identity.key()),
            );
        }
        self.controller
            .open_effect_group_opened_by(group, &self.admitted)
            .await
    }

    async fn await_next_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        cancel: lash_core::TurnCancelWait,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        self.controller.await_next_settlement(handle, cancel).await
    }
    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<Option<lash_core::RankedGroupSettlement>, lash_core::RuntimeEffectControllerError>
    {
        self.controller.read_group_settlement(group_key, rank).await
    }

    async fn close_effect_group(
        &self,
        handle: EffectGroupHandle,
        disposition: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.controller
            .close_effect_group(handle, disposition)
            .await
    }

    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        RuntimeEffectControllerError,
    > {
        self.controller.commit_group_child_final(commit).await
    }

    async fn await_group_child_drain_admission(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.controller
            .await_group_child_drain_admission(group_key, commit_seq)
            .await
    }

    async fn read_recorded_journal(
        &self,
        range: &lash_core::RecordedKeyRange,
    ) -> Result<lash_core::RecordedJournal, lash_core::RuntimeEffectControllerError> {
        self.controller.read_recorded_journal(range).await
    }
}

mod ingress;
mod turn_stop;
use ingress::*;
struct RestateEffectHostController {
    await_event_ingress: RestateAwaitEventIngress,
    authority_id: RestateAuthorityId,
    /// The bound process registry's registration truth (ADR 0049): a process
    /// scope's index says `revoked` only as a cache of the registry's fence,
    /// so a revoked index on a registered process is stale and is reinstated
    /// on first use.
    registrations: std::sync::Mutex<Option<Arc<dyn lash_core::ProcessRegistrationProbe>>>,
    /// This host's one answer to "what code runs a journaled grouped child".
    ///
    /// Registered once — by `install_tool_child_host` or by an embedder
    /// keeping its own resolver — and read by the endpoint's dispatch through
    /// the lazy handle [`RestateHostGroupExecutors`], so the open, a redriven
    /// child and preflight all consult the same cell.
    group_executors: OnceLock<Arc<dyn GroupExecutors>>,
}

fn ingress_group_error(
    operation: &str,
    error: crate::RestateHttpError,
) -> RuntimeEffectControllerError {
    if let Some(refusal) = ingress_protocol_refusal(&error) {
        return refusal;
    }
    let service_unregistered = error.is_service_unregistered();
    let message = format!("Restate effect-group operation {operation} failed: {error}");
    if service_unregistered {
        RuntimeEffectControllerError::new(RuntimeErrorCode::EngineServiceUnregistered, message)
    } else {
        group_shape_error(message)
    }
}

/// The typed effect-group protocol refusal an index handler answered an
/// ingress call with, recovered from the terminal error's message in the
/// response body.
pub(crate) fn ingress_protocol_refusal(
    error: &crate::RestateHttpError,
) -> Option<RuntimeEffectControllerError> {
    let crate::RestateHttpError::Status { body, .. } = error else {
        return None;
    };
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()?
        .get("message")?
        .as_str()?
        .to_owned();
    crate::effect_group::protocol_refusal_in(&message)
}

#[async_trait::async_trait]
impl AwaitEventResolver for RestateEffectHostController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some(self.authority_id.binding_id().to_string())
    }

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        if !may_defer {
            return Ok(CompletionKeyPreparation::NotNeeded);
        }
        self.await_event_key(scope, wait)
            .await
            .map(CompletionKeyPreparation::Issued)
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        scope.validate()?;
        if !self.scope_admits_mint(scope).await? {
            return Err(restate_unknown_or_revoked());
        }
        restate_await_event_key_for_authority(&self.authority_id, scope, wait)
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        if !restate_await_event_key_is_valid_for_authority(&self.authority_id, key) {
            return Ok(ResolveOutcome::UnknownOrRevoked);
        }
        resolve_restate_await_event_via_ingress(&self.await_event_ingress, key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        let ingress = &self.await_event_ingress;
        self.ensure_key_access(key).await?;
        let workflow_key = RestateDurableWaitAddress::for_key(key).workflow_key;
        ingress
            .ingress
            .call_workflow_empty::<Option<Resolution>>(
                LashService::DurableWaitWorkflow.name(),
                &workflow_key,
                "peek",
            )
            .await
            .map_err(|err| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::EngineAwaitEventPeek,
                    err.to_string(),
                )
            })
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        let ingress = &self.await_event_ingress;
        self.ensure_key_access(key).await?;
        let attach = turn_cancel_watch_attachment(key);
        await_restate_await_event_via_ingress(ingress, key, cancel, deadline, attach.as_deref())
            .await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        let ingress = &self.await_event_ingress;
        update_restate_session_waits_via_ingress(ingress, session_id, true).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        let ingress = &self.await_event_ingress;
        update_restate_session_waits_via_ingress(ingress, session_id, false).await
    }

    /// Revoke every durable wait of a non-session `scope` and fence the
    /// scope's `LashDurableWaitIndex` object, durably: the promise half of
    /// scope retirement on Restate. A session scope is refused as everywhere.
    async fn retire_await_events_for_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        scope.validate()?;
        if scope.session_id().is_some() {
            return Err(restate_scope_not_retirable(scope));
        }
        retire_restate_scope_via_ingress(&self.await_event_ingress, scope, false).await
    }

    /// Revoke and fence the scope's index only if no durable wait under it is
    /// unresolved; the index object decides both in one serialized step.
    async fn retire_await_events_for_scope_if_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        scope.validate()?;
        if scope.session_id().is_some() {
            return Err(restate_scope_not_retirable(scope));
        }
        let index_key = durable_wait_index_key_for_scope(scope);
        self.await_event_ingress
            .ingress
            .call_object_json::<_, bool>(
                LashService::DurableWaitIndex.name(),
                &index_key,
                "revoke_all_if_quiescent",
                &(),
            )
            .await
            .map_err(|err| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::EngineAwaitEventSessionUpdate,
                    err.to_string(),
                )
            })
    }

    async fn reinstate_await_event_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        scope.validate()?;
        if scope.session_id().is_some() {
            return Err(restate_scope_not_retirable(scope));
        }
        update_restate_scope_waits_via_ingress(&self.await_event_ingress, scope, "reinstate").await
    }

    async fn await_event_scope_is_retired(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        if scope.session_id().is_some() {
            return Ok(false);
        }
        let index_key = durable_wait_index_key_for_scope(scope);
        if !restate_index_is_revoked_via_ingress(&self.await_event_ingress, &index_key).await? {
            return Ok(false);
        }
        // The durable store fence is the truth for a process scope: the index
        // flag is its cache, and a registration that committed while no host
        // was bound (or whose post-commit reinstate was lost) leaves the
        // cache stale. Repair it here, on first use (ADR 0049).
        if let ExecutionScope::Process { process_id } = scope
            && self.process_is_registered(process_id).await?
        {
            update_restate_scope_waits_via_ingress(&self.await_event_ingress, scope, "reinstate")
                .await?;
            return Ok(false);
        }
        Ok(true)
    }
}

impl RestateEffectHostController {
    async fn register_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        scope.validate()?;
        if scope.session_id().is_some() {
            return Ok(());
        }
        let index_key = durable_wait_index_key_for_scope(scope);
        let admitted = self
            .await_event_ingress
            .ingress
            .call_object_json::<_, bool>(
                LashService::DurableWaitIndex.name(),
                &index_key,
                "register_closure_participant",
                &RestateTurnCancelClosureParticipantRequest {
                    participant_id: participant_id.to_string(),
                },
            )
            .await
            .map_err(|error| {
                RuntimeError::new(
                    RuntimeErrorCode::EngineAwaitEventSessionUpdate,
                    error.to_string(),
                )
            })?;
        if admitted {
            Ok(())
        } else {
            Err(RuntimeError::new(
                RuntimeErrorCode::EffectScopeRetired,
                format!(
                    "effect scope `{}` is retired",
                    scope.journal_identity()?.key()
                ),
            ))
        }
    }

    async fn release_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        scope.validate()?;
        if scope.session_id().is_some() {
            return Ok(());
        }
        let index_key = durable_wait_index_key_for_scope(scope);
        self.await_event_ingress
            .ingress
            .call_object_json::<_, ()>(
                LashService::DurableWaitIndex.name(),
                &index_key,
                "release_closure_participant",
                &RestateTurnCancelClosureParticipantRequest {
                    participant_id: participant_id.to_string(),
                },
            )
            .await
            .map_err(|error| {
                RuntimeError::new(
                    RuntimeErrorCode::EngineAwaitEventSessionUpdate,
                    error.to_string(),
                )
            })
    }

    /// Whether `scope` still admits a mint: a session scope until its index
    /// is revoked, a non-session scope until it is retired (read-through
    /// above, since retirement is a non-session concept).
    async fn scope_admits_mint(&self, scope: &ExecutionScope) -> Result<bool, RuntimeError> {
        if scope.session_id().is_some() {
            let index_key = durable_wait_index_key_for_scope(scope);
            return Ok(!restate_index_is_revoked_via_ingress(
                &self.await_event_ingress,
                &index_key,
            )
            .await?);
        }
        Ok(!self.await_event_scope_is_retired(scope).await?)
    }

    /// The key's fence: a session key through its session index, a
    /// non-session key through the scope read-through above.
    async fn ensure_key_access(&self, key: &AwaitEventKey) -> Result<(), RuntimeError> {
        if !restate_await_event_key_is_valid_for_authority(&self.authority_id, key) {
            return Err(restate_unknown_or_revoked());
        }
        if key.scope.session_id().is_some() {
            return ensure_restate_key_access_via_ingress(&self.await_event_ingress, key).await;
        }
        if self.await_event_scope_is_retired(&key.scope).await? {
            return Err(restate_unknown_or_revoked());
        }
        Ok(())
    }

    async fn process_is_registered(&self, process_id: &ProcessId) -> Result<bool, RuntimeError> {
        let probe = self
            .registrations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(probe) = probe else {
            return Ok(false);
        };
        probe
            .process_is_registered(process_id)
            .await
            .map_err(|error| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::EngineAwaitEventRevocationRead,
                    format!("process registry read-through failed: {error}"),
                )
            })
    }

    async fn record_scope_group(
        &self,
        scope: &ExecutionScope,
        group_key: &str,
    ) -> Result<bool, RuntimeError> {
        let index_key = durable_wait_index_key_for_scope(scope);
        self.await_event_ingress
            .ingress
            .call_object_json::<_, bool>(
                LashService::DurableWaitIndex.name(),
                &index_key,
                "record_group",
                &crate::durable_wait::RestateDurableWaitGroupRequest {
                    group_key: group_key.to_string(),
                },
            )
            .await
            .map_err(|err| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::EngineAwaitEventSessionUpdate,
                    err.to_string(),
                )
            })
    }
}

impl RestateEffectHostController {
    /// Opens `group` on behalf of `opener`, the admitted scope of the view the
    /// group is opened through: the shape records it, and every child the
    /// dispatcher runs is admitted from it (FIG-3780).
    pub(crate) async fn open_effect_group_opened_by(
        &self,
        group: RuntimeEffectGroup,
        opener: &lash_core::AdmittedScope,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        group.validate_execution_scope(opener.scope())?;
        let ingress = &self.await_event_ingress.ingress;
        let group_key = group.group_key().to_string();
        let handle = EffectGroupHandle::new(&group);
        let shape = EffectGroupShape::from_group(&group, opener)?;
        let probe = ingress
            .call_object_empty_json::<EffectGroupProbeResponse>(
                LashService::EffectGroupIndex,
                &group_key,
                "probe",
            )
            .await
            .map_err(|error| ingress_group_error("EffectGroupIndex/probe", error))?;
        // Opener liveness is this process's to judge; the endpoint's is routing.
        let local = self.group_executors.get().and_then(|executors| {
            (group.children().iter()).position(|child| executors.executor_for(child).is_none())
        });
        if matches!(probe, EffectGroupProbeResponse::Absent)
            && let Some(position) = match local {
                found @ Some(_) => found,
                None => ingress
                    .call_workflow_json::<_, Option<usize>>(
                        LashService::EffectGroupDispatch.name(),
                        &group_key,
                        "preflight",
                        &group.children(),
                    )
                    .await
                    .map_err(|error| ingress_group_error("EffectGroupDispatch/preflight", error))?,
            }
        {
            let replay_key = shape.replay_keys.get(position).ok_or_else(|| {
                group_shape_error(format!(
                    "effect group {group_key} preflight named child {position}, outside the {} children its shape carries",
                    shape.replay_keys.len()
                ))
            })?;
            return Err(group_shape_error(format!(
                "effect group {group_key} child {position} ({replay_key}) has no registered executor; refusing before group state is created"
            )));
        }
        let content_checked = group.reopen() == lash_core::GroupReopen::RetainedContent;
        let opened = ingress
            .call_object_json::<_, EffectGroupOpenResponse>(
                LashService::EffectGroupIndex.name(),
                &group_key,
                "open",
                &EffectGroupOpenRequest {
                    shape: shape.clone(),
                    content_checked,
                },
            )
            .await
            .map_err(|error| ingress_group_error("EffectGroupIndex/open", error))?;
        match opened {
            EffectGroupOpenResponse::OpenedFresh | EffectGroupOpenResponse::ReopenedPreparing => {
                ingress
                    .send_workflow_json(
                        LashService::EffectGroupDispatch.name(),
                        &group_key,
                        "run",
                        &EffectGroupDispatchRequest {
                            group_key: group_key.clone(),
                        },
                    )
                    .await
                    .map_err(|error| ingress_group_error("EffectGroupDispatch/run", error))?;
                let request = ready_wait_request(&shape.wait_scope, &group_key)?;
                let address = RestateDurableWaitAddress::for_key(&request.key);
                let resolution = ingress
                    .call_workflow_json::<_, Resolution>(
                        LashService::DurableWaitWorkflow.name(),
                        &address.workflow_key,
                        "await_resolution",
                        &request,
                    )
                    .await
                    .map_err(|error| {
                        ingress_group_error(
                            "LashDurableWaitWorkflow/await_resolution(READY)",
                            error,
                        )
                    })?;
                match decode_wait_resolution(resolution)? {
                    EffectGroupWaitResolution::Ready => Ok(handle),
                    EffectGroupWaitResolution::Refused { reason } => Err(group_shape_error(
                        format!("effect group {group_key} routing was refused: {reason:?}"),
                    )),
                    EffectGroupWaitResolution::Retired => Err(group_shape_error(format!(
                        "effect group {group_key} was retired before it became ready"
                    ))),
                    other => Err(group_shape_error(format!(
                        "effect group {group_key} READY wait resolved as {other:?}"
                    ))),
                }
            }
            EffectGroupOpenResponse::ReopenedReady => Ok(handle),
            EffectGroupOpenResponse::ReopenedClosed { effective } => match effective {
                EffectGroupCloseDisposition::Refused { reason } => Err(group_shape_error(format!(
                    "effect group {group_key} routing was refused: {reason:?}"
                ))),
                EffectGroupCloseDisposition::RunToCompletion
                | EffectGroupCloseDisposition::Cancel => Ok(handle),
            },
            EffectGroupOpenResponse::Retired => Err(group_shape_error(format!(
                "effect group {group_key} is retired"
            ))),
            EffectGroupOpenResponse::ShapeMismatch if content_checked => Err(
                crate::effect_group::content_checked_shape_mismatch(&group_key),
            ),
            EffectGroupOpenResponse::ShapeMismatch => Err(group_shape_error(format!(
                "effect group {group_key} was reopened with a different durable shape"
            ))),
            // The engine-neutral divergence, exactly as a recorded run whose
            // envelope drifted reports it: the turn parks.
            EffectGroupOpenResponse::ContentMismatch { position } => {
                Err(crate::effect_group::content_mismatch(&group_key, position))
            }
        }
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for RestateEffectHostController {
    fn owns_commit_backpressure(&self) -> bool {
        true
    }

    /// Register this host's envelope→executor resolver, once.
    ///
    /// One host has one answer to "what code runs this journaled grouped
    /// child", so this is set once and then read by the endpoint's dispatch
    /// through [`RestateHostGroupExecutors`]. A second registration of a
    /// *different* resolver is refused rather than allowed to win: two
    /// resolvers on one deployment means two answers for one child, and which
    /// one a given path got would depend on when it asked. Re-registering the
    /// resolver already held is a no-op, so a host handed out repeatedly need
    /// not track whether it has been wired yet.
    ///
    /// [`OnceLock::set`] is the arbiter rather than a preceding `get`: a
    /// get-then-set pair leaves a window in which two threads both read `None`,
    /// both write, and the loser is told `Ok` while its resolver was dropped on
    /// the floor — the exact drift this refusal exists to prevent. `set` decides,
    /// and its `Err` hands back the rejected resolver so the same-resolver case
    /// stays a no-op.
    fn register_group_executors(
        &self,
        executors: Arc<dyn GroupExecutors>,
    ) -> Result<(), RuntimeEffectControllerError> {
        let Err(rejected) = self.group_executors.set(executors) else {
            return Ok(());
        };
        // A rejected `set` means the cell is already initialized; an absent
        // held resolver is unreachable, so it takes the conflicting-resolver
        // answer rather than a panic.
        match self.group_executors.get() {
            Some(held) if Arc::ptr_eq(held, &rejected) => Ok(()),
            _ => Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectGroupShape,
                "this effect host already has a different registered group \
                 executor resolver; one host has one answer to what runs a \
                 journaled grouped child, and a second answer would make which \
                 one a path got depend on when it asked",
            )),
        }
    }

    /// The host's own controller has no process admission, so a group opened
    /// on it is admitted unpinned; a scope's view opens with its admission.
    async fn open_effect_group(
        &self,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        let opener =
            lash_core::AdmittedScope::unpinned(group.invocation().execution_scope().clone())
                .map_err(|error| {
                    RuntimeEffectControllerError::new(
                        RuntimeErrorCode::ExecutionScopeAdmissionRefused,
                        format!(
                            "effect group {} has no admitted opener: {error}",
                            group.group_key()
                        ),
                    )
                })?;
        self.open_effect_group_opened_by(group, &opener).await
    }

    async fn await_next_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        cancel: lash_core::TurnCancelWait,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        if handle.is_exhausted() {
            return Err(group_shape_error(format!(
                "effect group {} has no settlement after its {} children",
                handle.group_key(),
                handle.children()
            )));
        }
        let ingress = &self.await_event_ingress.ingress;
        let rank = u64::try_from(handle.consumed() + 1).map_err(|error| {
            group_shape_error(format!("effect group rank does not fit u64: {error}"))
        })?;
        let mut read = ingress
            .call_object_json::<_, EffectGroupReadRankResponse>(
                LashService::EffectGroupIndex.name(),
                handle.group_key(),
                "read_rank",
                &EffectGroupReadRankRequest {
                    rank,
                    for_caller: true,
                },
            )
            .await
            .map_err(|error| ingress_group_error("EffectGroupIndex/read_rank", error))?;
        if matches!(read, EffectGroupReadRankResponse::NotSettled) {
            let scope = ExecutionScope::runtime_operation(handle.group_key());
            let request = rank_wait_request(&scope, handle.group_key(), rank)?;
            let address = RestateDurableWaitAddress::for_key(&request.key);
            let wait = ingress.call_workflow_json::<_, Resolution>(
                LashService::DurableWaitWorkflow.name(),
                &address.workflow_key,
                "await_resolution",
                &request,
            );
            tokio::pin!(wait);
            // Unjournaled here: the turn's gate is raced over ingress (FIG-3672 P9).
            let resolution = tokio::select! {
                result = &mut wait => Some(result.map_err(|error| ingress_group_error(
                    "LashDurableWaitWorkflow/await_resolution(RANK)", error
                ))?),
                _ = cancel.cancellation().cancelled() => None, // The waiter's own stop: no journal here.
                stop = self.turn_stop(cancel.observed_scope()) => {
                    stop?;
                    None
                }
            };
            let Some(resolution) = resolution else {
                return Err(RuntimeEffectControllerError::new(
                    RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled,
                    format!(
                        "awaiting effect group {} rank {rank} was cancelled",
                        handle.group_key()
                    ),
                ));
            };
            match decode_wait_resolution(resolution)? {
                EffectGroupWaitResolution::Rank => {}
                EffectGroupWaitResolution::Retired => {
                    return Err(group_shape_error(format!(
                        "effect group {} was retired while awaiting rank {rank}",
                        handle.group_key()
                    )));
                }
                other => {
                    return Err(group_shape_error(format!(
                        "effect group {} rank {rank} wait resolved as {other:?}",
                        handle.group_key()
                    )));
                }
            }
            read = ingress
                .call_object_json::<_, EffectGroupReadRankResponse>(
                    LashService::EffectGroupIndex.name(),
                    handle.group_key(),
                    "read_rank",
                    &EffectGroupReadRankRequest {
                        rank,
                        for_caller: true,
                    },
                )
                .await
                .map_err(|error| ingress_group_error("EffectGroupIndex/read_rank", error))?;
        }
        let record = match read {
            EffectGroupReadRankResponse::Settled { settlement, .. } => settlement,
            EffectGroupReadRankResponse::NotSettled => {
                return Err(group_shape_error(format!(
                    "effect group {} rank {rank} remained unsettled after its notification",
                    handle.group_key()
                )));
            }
            EffectGroupReadRankResponse::Closed => {
                return Err(group_shape_error(format!(
                    "effect group {} is closed to this caller",
                    handle.group_key()
                )));
            }
            EffectGroupReadRankResponse::UnknownGroup => {
                return Err(group_shape_error(format!(
                    "effect group {} is unknown",
                    handle.group_key()
                )));
            }
            EffectGroupReadRankResponse::Retired => {
                return Err(group_shape_error(format!(
                    "effect group {} is retired",
                    handle.group_key()
                )));
            }
        };
        let payload = if matches!(
            record.terminal,
            EffectGroupSettlementTerminal::StoredPayload
        ) {
            match ingress
                .call_object_empty_json::<EffectGroupPayloadGetResponse>(
                    LashService::EffectGroupPayload,
                    &payload_key(handle.group_key(), record.position),
                    "get",
                )
                .await
                .map_err(|error| ingress_group_error("EffectGroupPayload/get", error))?
            {
                EffectGroupPayloadGetResponse::Stored { bytes } => Some(bytes),
                EffectGroupPayloadGetResponse::Missing => {
                    return Err(group_shape_error(format!(
                        "effect group {} rank {rank} refers to a missing payload",
                        handle.group_key()
                    )));
                }
                EffectGroupPayloadGetResponse::Retired => {
                    return Err(group_shape_error(format!(
                        "effect group {} payload was retired",
                        handle.group_key()
                    )));
                }
            }
        } else {
            None
        };
        let settlement = settlement_from_payload(record, payload)?;
        handle.advance()?;
        Ok(settlement)
    }

    /// The cursorless rank read the §6 incorporation record needs: the same
    /// index `read_rank` + payload `get` pair as `await_next_settlement`,
    /// minus the cursor and the durable wait.
    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<Option<lash_core::RankedGroupSettlement>, RuntimeEffectControllerError> {
        let ingress = &self.await_event_ingress.ingress;
        let read = ingress
            .call_object_json::<_, EffectGroupReadRankResponse>(
                LashService::EffectGroupIndex.name(),
                group_key,
                "read_rank",
                &EffectGroupReadRankRequest {
                    rank,
                    for_caller: false,
                },
            )
            .await
            .map_err(|error| ingress_group_error("EffectGroupIndex/read_rank", error))?;
        let (record, child_replay_key) = match read {
            EffectGroupReadRankResponse::Settled {
                settlement,
                child_replay_key,
            } => (settlement, child_replay_key),
            EffectGroupReadRankResponse::NotSettled | EffectGroupReadRankResponse::Closed => {
                return Ok(None);
            }
            EffectGroupReadRankResponse::UnknownGroup => {
                return Err(group_shape_error(format!(
                    "effect group {group_key} is unknown"
                )));
            }
            EffectGroupReadRankResponse::Retired => {
                return Err(group_shape_error(format!(
                    "effect group {group_key} is retired"
                )));
            }
        };
        let payload = if matches!(
            record.terminal,
            EffectGroupSettlementTerminal::StoredPayload
        ) {
            match ingress
                .call_object_empty_json::<EffectGroupPayloadGetResponse>(
                    LashService::EffectGroupPayload,
                    &payload_key(group_key, record.position),
                    "get",
                )
                .await
                .map_err(|error| ingress_group_error("EffectGroupPayload/get", error))?
            {
                EffectGroupPayloadGetResponse::Stored { bytes } => Some(bytes),
                EffectGroupPayloadGetResponse::Missing => {
                    return Err(group_shape_error(format!(
                        "effect group {group_key} rank {rank} refers to a missing payload"
                    )));
                }
                EffectGroupPayloadGetResponse::Retired => {
                    return Err(group_shape_error(format!(
                        "effect group {group_key} payload was retired"
                    )));
                }
            }
        } else {
            None
        };
        let settlement = settlement_from_payload(record, payload)?;
        Ok(Some(lash_core::RankedGroupSettlement {
            sequence: settlement.sequence,
            child_replay_key,
            outcome: settlement.outcome,
        }))
    }

    async fn close_effect_group(
        &self,
        handle: EffectGroupHandle,
        disposition: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        let group_key = handle.group_key().to_string();
        let response = self
            .await_event_ingress
            .ingress
            .call_object_json::<_, EffectGroupCloseResponse>(
                LashService::EffectGroupIndex.name(),
                &group_key,
                "close",
                &EffectGroupCloseRequest { disposition },
            )
            .await
            .map_err(|error| ingress_group_error("EffectGroupIndex/close", error))?;
        match response {
            EffectGroupCloseResponse::Closed | EffectGroupCloseResponse::AlreadyClosed => Ok(()),
            EffectGroupCloseResponse::WidenRefused => Err(group_shape_error(format!(
                "effect group {group_key} close attempted to widen its declared loser disposition"
            ))),
            EffectGroupCloseResponse::NotReady => Err(group_shape_error(format!(
                "effect group {group_key} cannot close before registration"
            ))),
            EffectGroupCloseResponse::UnknownGroup => Err(group_shape_error(format!(
                "effect group {group_key} is unknown"
            ))),
            EffectGroupCloseResponse::Retired => Err(group_shape_error(format!(
                "effect group {group_key} is retired"
            ))),
        }
    }

    /// The §4 boundary over ingress — the same route the ctx-based
    /// controller takes, with the durable membership record resolving which
    /// group's index owns this replay key. The serialized index handler is
    /// the linearization point; `drain_input` is deliberately not retained
    /// on this tier because the committed-but-unseated index state plus the
    /// dispatch workflow's redrive is the resumable publication obligation.
    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        RuntimeEffectControllerError,
    > {
        use lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome as Outcome;
        let scope = ExecutionScope::from_journal_key(&commit.scope_id).ok_or_else(|| {
            group_shape_error(format!(
                "group-child commit scope id `{}` does not decode to an execution scope",
                commit.scope_id
            ))
        })?;
        let ingress = &self.await_event_ingress.ingress;
        let index_key = durable_wait_index_key_for_scope(&scope);
        let membership: Option<String> = ingress
            .call_object_json::<_, Option<String>>(
                LashService::DurableWaitIndex.name(),
                &index_key,
                "group_child_membership",
                &crate::durable_wait::RestateDurableWaitGroupChildMembershipRequest {
                    replay_key: commit.replay_key.clone(),
                },
            )
            .await
            .map_err(|error| {
                ingress_group_error("LashDurableWaitIndex/group_child_membership", error)
            })?;
        let Some(group_key) = membership else {
            return Ok(Outcome::Ungrouped);
        };
        let response = ingress
            .call_object_json::<_, crate::effect_group::EffectGroupCommitChildResponse>(
                LashService::EffectGroupIndex.name(),
                &group_key,
                "commit_child",
                &crate::effect_group::EffectGroupCommitChildRequest {
                    replay_key: commit.replay_key.clone(),
                },
            )
            .await
            .map_err(|error| ingress_group_error("EffectGroupIndex/commit_child", error))?;
        Ok(match response {
            crate::effect_group::EffectGroupCommitChildResponse::Committed {
                commit_seq, ..
            } => Outcome::Committed {
                group_key,
                commit_seq,
            },
            crate::effect_group::EffectGroupCommitChildResponse::AlreadyCommitted {
                commit_seq,
                ..
            } => Outcome::AlreadyCommitted {
                group_key,
                commit_seq,
                drain_input: None,
            },
            crate::effect_group::EffectGroupCommitChildResponse::CancelDecided { rank } => {
                Outcome::CancelDecided {
                    group_key,
                    commit_seq: rank,
                }
            }
            crate::effect_group::EffectGroupCommitChildResponse::UnknownChild => {
                return Err(group_shape_error(format!(
                    "effect group {group_key} membership names replay key `{}` but its \
                     index holds no such child; the two durable records disagree",
                    commit.replay_key
                )));
            }
            crate::effect_group::EffectGroupCommitChildResponse::UnknownGroup
            | crate::effect_group::EffectGroupCommitChildResponse::Retired => {
                return Err(group_shape_error(format!(
                    "effect group {group_key} carries membership for replay key `{}` but \
                     its index is gone or retired; the two durable records disagree",
                    commit.replay_key
                )));
            }
        })
    }

    async fn await_group_child_drain_admission(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<(), RuntimeEffectControllerError> {
        // The §5 barrier on the engine's own wake, over ingress: the index
        // names the lower-commit siblings still owed a seat, and the drain
        // parks on each one's durable drained wake instead of polling.
        let ingress = &self.await_event_ingress.ingress;
        let (wait_scope, positions) = match ingress
            .call_object_json::<_, crate::effect_group::EffectGroupDrainBlockersResponse>(
                LashService::EffectGroupIndex.name(),
                group_key,
                "drain_blockers",
                &crate::effect_group::EffectGroupDrainBlockersRequest { commit_seq },
            )
            .await
            .map_err(|error| ingress_group_error("EffectGroupIndex/drain_blockers", error))?
        {
            crate::effect_group::EffectGroupDrainBlockersResponse::Admitted => return Ok(()),
            crate::effect_group::EffectGroupDrainBlockersResponse::Blocked {
                wait_scope,
                positions,
            } => (wait_scope, positions),
        };
        for position in positions {
            let request = drained_wait_request(&wait_scope, group_key, position)?;
            let address = RestateDurableWaitAddress::for_key(&request.key);
            let resolution = ingress
                .call_workflow_json::<_, Resolution>(
                    LashService::DurableWaitWorkflow.name(),
                    &address.workflow_key,
                    "await_resolution",
                    &request,
                )
                .await
                .map_err(|error| {
                    ingress_group_error("LashDurableWaitWorkflow/await_resolution(DRAINED)", error)
                })?;
            drained_wait_lifted(group_key, position, resolution)?;
        }
        Ok(())
    }

    /// Positional, as the in-handler controller answers: this controller's
    /// effects are journaled entries of a Restate invocation (FIG-3586).
    async fn read_recorded_journal(
        &self,
        _range: &lash_core::RecordedKeyRange,
    ) -> Result<lash_core::RecordedJournal, RuntimeEffectControllerError> {
        Ok(lash_core::RecordedJournal::Positional)
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let effect_replay_key = envelope.stable_hash()?;
        if let RuntimeEffectCommand::AwaitEvent { key } = &envelope.command {
            if !restate_await_event_key_is_valid_for_authority(&self.authority_id, key) {
                return Err(RuntimeEffectControllerError::from(
                    restate_unknown_or_revoked(),
                ));
            }
            let ingress = &self.await_event_ingress;
            let RuntimeAwaitEventOptions {
                cancellation,
                deadline,
                ..
            } = local_executor.into_await_event_options()?;
            let resolution = await_restate_await_event_via_ingress(
                ingress,
                key,
                cancellation,
                deadline,
                Some(&effect_replay_key),
            )
            .await
            .map_err(RuntimeEffectControllerError::from)?;
            return Ok(RuntimeEffectOutcome::AwaitEvent { resolution });
        }
        Err(RuntimeEffectControllerError::new(
            RuntimeErrorCode::EngineEffectHostRequiresHandlerScope,
            format!(
                "effect `{}` must enter a Restate handler and use RestateRuntimeEffectController::scoped_effect_controller",
                envelope.invocation.effect_id()
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable_wait::restate_await_event_key;

    fn service_call_error(status: u16) -> crate::RestateHttpError {
        crate::RestateHttpError::Status {
            operation: "Restate object call",
            url: "https://restate.invalid/EffectGroupIndex/group/probe".to_string(),
            status,
            body: "not found".to_string(),
        }
    }

    #[test]
    fn effect_group_ingress_404_is_restate_service_unregistered() {
        let error = ingress_group_error("EffectGroupIndex/probe", service_call_error(404));

        assert_eq!(error.code, RuntimeErrorCode::EngineServiceUnregistered);
        assert!(error.message.contains("EffectGroupIndex/probe"));
    }

    #[test]
    fn effect_group_ingress_non_registration_failure_stays_a_shape_error() {
        let error = ingress_group_error("EffectGroupIndex/probe", service_call_error(503));

        assert_eq!(error.code, RuntimeErrorCode::RuntimeEffectGroupShape);
    }

    #[test]
    fn session_administrative_read_rejects_non_session_scope_aliases() {
        for scope in [
            ExecutionScope::process("alias-process"),
            ExecutionScope::runtime_operation("alias-operation"),
        ] {
            let alias = SessionId::from(durable_wait_index_key_for_scope(&scope));
            let key = restate_await_event_key(
                &scope,
                AwaitEventWaitIdentity::tool_completion("alias-wait"),
            )
            .expect("derive non-session wait key");

            assert!(
                outstanding_owned_by_session(&alias, vec![key]).is_empty(),
                "a non-session wait indexed at `{alias}` must not be advertised as session-owned"
            );
        }
    }
}
