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
    ScopedEffectController, facade_support::RuntimeAwaitEventOptions,
};

use crate::durable_wait::{
    RestateDurableWaitAddress, RestateDurableWaitResolveRequest, RestateDurableWaitResolveResponse,
    RestateDurableWaitRunRequest, RestateTurnCancelClosureParticipantRequest, WaitObserver,
    durable_wait_index_key_for_scope, durable_wait_index_object_key, observe_durable_wait,
    restate_await_event_key_for_authority, restate_await_event_key_is_valid,
    restate_await_event_key_is_valid_for_authority, restate_durable_wait_request,
    restate_unknown_or_revoked,
};
use crate::effect_group::{
    EffectGroupCloseOutcome, EffectGroupCloseRequest, EffectGroupCloseResponse,
    EffectGroupDispatchRequest, EffectGroupOpenRequest, EffectGroupOpenResponse,
    EffectGroupPayloadGetResponse, EffectGroupProbeResponse, EffectGroupReadRankRequest,
    EffectGroupReadRankResponse, EffectGroupSettlementTerminal, EffectGroupShape,
    await_group_notice_via_ingress, group_shape_error, payload_key, settlement_from_payload,
};
use crate::effect_group::{EffectGroupNotice, EffectGroupNotification};
use crate::{LashService, RestateAuthorityId, RestateConnection, RestateIngressClient};

mod journal_verdict;
use crate::effect_group::ingress_group_error;
pub use journal_verdict::RestateJournalAuthority;

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
    turn_attach: Arc<crate::turn::RestateTurnAttach>,
    turn_control_binding_id: Arc<str>,
    /// What [`EffectHost::journal_replay`] reads, bound once by the engine
    /// that owns the store set. Unbound, every live journal answers
    /// `MayReplay`: the cleanup executor waits rather than sever what a
    /// replay may still read.
    journal_authority: Arc<OnceLock<RestateJournalAuthority>>,
}

impl RestateEffectHost {
    /// The host of a deployment in the default namespace, reaching Restate
    /// through `connection` under `authority_id`, of the build whose drain
    /// generation is `build_generation` (FIG-3795, FIG-4454): a group this
    /// host opens dispatches on that build's lane, whose endpoint the
    /// deployment binds — the engine's
    /// [`build_generation`](crate::RestateEngine::build_generation), which
    /// is the core's (`LashCore::build_generation`): it exists once the
    /// core's plugins are registered (FIG-4744).
    pub fn new(
        connection: impl Into<RestateConnection>,
        authority_id: RestateAuthorityId,
        build_generation: lash_core::engine::BuildGeneration,
    ) -> Self {
        Self::in_namespace(
            connection,
            authority_id,
            build_generation,
            crate::RestateNamespace::default(),
        )
    }

    /// A host outside any deployment, in the default namespace: a tool or an
    /// operator process that mints, resolves, awaits and peeks await events
    /// under `authority_id`. It runs no build of a core, so it has no
    /// generation and serves no lane: a group opened through it is refused,
    /// typed, before anything is dispatched.
    pub fn outside_deployment(
        connection: impl Into<RestateConnection>,
        authority_id: RestateAuthorityId,
    ) -> Self {
        Self::on_generation(
            connection,
            authority_id,
            lash_core::engine::EngineGeneration::unbound(),
            crate::RestateNamespace::default(),
        )
    }

    /// [`new`](Self::new) for a deployment in `namespace` (FIG-3898): every
    /// lash service this host calls is that namespace's.
    pub fn in_namespace(
        connection: impl Into<RestateConnection>,
        authority_id: RestateAuthorityId,
        build_generation: lash_core::engine::BuildGeneration,
        namespace: crate::RestateNamespace,
    ) -> Self {
        Self::on_generation(
            connection,
            authority_id,
            lash_core::engine::EngineGeneration::fixed(build_generation),
            namespace,
        )
    }

    /// The engine's own host: it opens groups on the lane of the generation
    /// the engine's core binds into `generation`.
    pub(crate) fn on_generation(
        connection: impl Into<RestateConnection>,
        authority_id: RestateAuthorityId,
        generation: lash_core::engine::EngineGeneration,
        namespace: crate::RestateNamespace,
    ) -> Self {
        let connection = connection.into();
        let turn_control_binding_id: Arc<str> = Arc::from(authority_id.binding_id());
        let turn_attach_authority_id = authority_id.clone();
        Self {
            controller: Arc::new(RestateEffectHostController {
                await_event_ingress: RestateAwaitEventIngress {
                    ingress: RestateIngressClient::new(connection.clone()),
                    namespace: namespace.clone(),
                },
                authority_id,
                generation,
                registrations: std::sync::Mutex::new(None),
                group_executors: OnceLock::new(),
                wait_receipts: OnceLock::new(),
            }),
            turn_attach: Arc::new(crate::turn::RestateTurnAttach::in_namespace(
                connection,
                turn_attach_authority_id,
                namespace,
            )),
            turn_control_binding_id,
            journal_authority: Arc::new(OnceLock::new()),
        }
    }

    pub(crate) fn bind_wait_receipts(
        &self,
        store: Arc<dyn lash_core::store::WaitReceiptStore>,
        clock: Arc<dyn lash_core::Clock>,
    ) {
        let _ = self.controller.wait_receipts.set((store, clock));
    }

    /// Bind the reads [`EffectHost::journal_replay`] answers from, once; a
    /// later binding is ignored, like the host's other get-or-init cells.
    pub(crate) fn bind_journal_authority(&self, authority: RestateJournalAuthority) {
        let _ = self.journal_authority.set(authority);
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(connection: impl Into<RestateConnection>) -> Self {
        Self::new(
            connection,
            RestateAuthorityId::new("lash-restate-tests").expect("valid test authority"),
            crate::tests::test_build_generation(),
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

    /// The namespace this host's deployment names its services in
    /// (FIG-3898).
    pub fn namespace(&self) -> &crate::RestateNamespace {
        &self.controller.await_event_ingress.namespace
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
    /// registration cell at call time, so services built before wiring resolve
    /// through the one answer the host holds. `None` from `executor_for`
    /// while nothing is registered is the routing fact "not mine", not a
    /// failure.
    pub fn group_executors(&self) -> Arc<dyn GroupExecutors> {
        Arc::new(RestateHostGroupExecutors {
            controller: Arc::clone(&self.controller),
        })
    }
}

mod group_executors;
use group_executors::RestateHostGroupExecutors;

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

    async fn publish_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<Option<ResolveOutcome>, RuntimeError> {
        self.controller.publish_await_event(key, resolution).await
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
    ) -> Result<Resolution, RuntimeError> {
        self.controller.await_await_event(key, cancel).await
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

    async fn retire_closed_run_waits(
        &self,
        session_id: &SessionId,
        run: &lash_core::TurnId,
        committed_turn: Option<&lash_core::TurnId>,
    ) -> Result<(), RuntimeError> {
        let await_event_ingress = &self.controller.await_event_ingress;
        await_event_ingress
            .ingress
            .call_lash_object::<_, ()>(
                &await_event_ingress.service(LashService::DurableWaitRegistry),
                session_id,
                "retire_run",
                &RestateDurableWaitRunRequest {
                    session_id: session_id.clone(),
                    run: run.clone(),
                    committed_turn: committed_turn.cloned(),
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

    async fn list_outstanding_await_event_keys(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<AwaitEventKey>, RuntimeError> {
        let keys: Vec<AwaitEventKey> = self
            .controller
            .await_event_ingress
            .ingress
            .call_lash_object(
                &self
                    .controller
                    .await_event_ingress
                    .namespace
                    .stable(LashService::DurableWaitRegistry)
                    .name(),
                session_id,
                "outstanding",
                &(),
            )
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

    /// Restate owns invocation-journal retention natively, so no Lash-side
    /// replay ledger is deleted here and the count is always 0. The promise
    /// half is real: a scope-exact retirement revokes every durable wait the
    /// scope owns and fences later mints, resolves, peeks, awaits, effects,
    /// and groups under it through the scope's `LashDurableWaitRegistry` object,
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
                    lash_core::facade_support::scope_status::scope_not_quiescent(identity.key()),
                );
            }
            return Ok(0);
        }
        self.controller
            .retire_await_events_for_scope(&scope)
            .await?;
        Ok(0)
    }

    /// Restate's verdict on one journal (ADR 0113 §2.5); see
    /// [`journal_verdict`].
    async fn journal_replay(
        &self,
        journal: &lash_sansio::EffectJournalIdentity,
    ) -> Result<lash_core::JournalReplay, RuntimeError> {
        journal_verdict::journal_replay(self, journal).await
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

mod fenced;
use fenced::FencedRestateController;

mod ingress;
mod turn_stop;
use ingress::*;
struct RestateEffectHostController {
    await_event_ingress: RestateAwaitEventIngress,
    authority_id: RestateAuthorityId,
    /// The drain generation of the build this host runs: the
    /// `EffectGroupDispatch` lane the groups it opens dispatch on (FIG-3795,
    /// FIG-4454).
    generation: lash_core::engine::EngineGeneration,
    /// The bound process registry's registration truth (ADR 0049): a process
    /// scope's index says `revoked` only as a cache of the registry's fence,
    /// so a revoked index on a registered process is stale and is reinstated
    /// on first use.
    registrations: std::sync::Mutex<Option<Arc<dyn lash_core::ProcessRegistrationProbe>>>,
    /// This host's one answer to "what code runs a journaled grouped child".
    ///
    /// Registered once by an embedder and read by the endpoint's dispatch through
    /// the lazy handle [`RestateHostGroupExecutors`], so the open, a redriven
    /// child and preflight all consult the same cell.
    group_executors: OnceLock<Arc<dyn GroupExecutors>>,
    wait_receipts: OnceLock<(
        Arc<dyn lash_core::store::WaitReceiptStore>,
        Arc<dyn lash_core::Clock>,
    )>,
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
            .call_lash_workflow::<_, Option<Resolution>>(
                &self
                    .await_event_ingress
                    .service(LashService::DurableWaitWorkflow),
                &workflow_key,
                "peek",
                &(),
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
    ) -> Result<Resolution, RuntimeError> {
        let ingress = &self.await_event_ingress;
        self.ensure_key_access(key).await?;
        await_restate_await_event_via_ingress(ingress, key, cancel, IngressAwait::of_key(key)).await
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
    /// scope's `LashDurableWaitRegistry` object, durably: the promise half of
    /// scope retirement on Restate. A session scope is refused as everywhere.
    async fn retire_await_events_for_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        scope.validate()?;
        if scope.session_id().is_some() {
            return Err(restate_scope_not_retirable(scope));
        }
        retire_restate_scope_via_ingress(&self.await_event_ingress, scope, false).await?;
        self.retire_observation_receipts(scope).await
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
        let retired = self
            .await_event_ingress
            .ingress
            .call_lash_object::<_, bool>(
                &self
                    .await_event_ingress
                    .service(LashService::DurableWaitRegistry),
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
            })?;
        if retired {
            self.retire_observation_receipts(scope).await?;
        }
        Ok(retired)
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
            .call_lash_object::<_, bool>(
                &self
                    .await_event_ingress
                    .service(LashService::DurableWaitRegistry),
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
            .call_lash_object::<_, ()>(
                &self
                    .await_event_ingress
                    .service(LashService::DurableWaitRegistry),
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
            .call_lash_object::<_, bool>(
                &self
                    .await_event_ingress
                    .service(LashService::DurableWaitRegistry),
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
        let (shape, membership) = EffectGroupShape::from_group(&group, opener)?;
        // A replay-leg handler can suspend at the very next await. Pin its
        // local tool contexts before probing the index so a child dispatched
        // during that suspension can still resolve its executor.
        if let Some(executors) = self.group_executors.get() {
            executors.pin_group(&group);
        }
        // The group dispatches on this host's build's lane (FIG-3795): its
        // children run on the build that opened it.
        let dispatch_lane = self.await_event_ingress.namespace.generation(
            LashService::EffectGroupDispatch,
            self.generation
                .get()
                .map_err(lash_core::RuntimeError::from)?
                .clone(),
        );
        let probe = ingress
            .call_lash_object::<_, EffectGroupProbeResponse>(
                &self
                    .await_event_ingress
                    .namespace
                    .stable(LashService::EffectGroupState)
                    .name(),
                &group_key,
                "probe",
                &(),
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
                    .call_lash_workflow::<_, Option<usize>>(
                        &dispatch_lane.name(),
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
            if let Some(executors) = self.group_executors.get() {
                executors.release_group(&group_key);
            }
            return Err(group_shape_error(format!(
                "effect group {group_key} child {position} ({replay_key}) has no registered executor; refusing before group state is created"
            )));
        }
        let content_checked = group.reopen() == lash_core::GroupReopen::RetainedContent;
        // The dispatch route is data (FIG-3795 S10): declared at open,
        // retained by the index, and the submit goes to the route the open
        // response reports — a reopen's retained route wins over this one.
        let dispatch_route = dispatch_lane.name().into_owned();
        let opened = ingress
            .call_lash_object::<_, EffectGroupOpenResponse>(
                &self
                    .await_event_ingress
                    .service(LashService::EffectGroupState),
                &group_key,
                "open",
                &EffectGroupOpenRequest {
                    shape: shape.clone(),
                    membership,
                    dispatch_route,
                    content_checked,
                },
            )
            .await
            .map_err(|error| ingress_group_error("EffectGroupIndex/open", error))?;
        match opened {
            EffectGroupOpenResponse::OpenedFresh { dispatch_route }
            | EffectGroupOpenResponse::ReopenedPreparing { dispatch_route } => {
                ingress
                    .send_lash_workflow(
                        &dispatch_route,
                        &group_key,
                        "run",
                        &EffectGroupDispatchRequest {
                            group_key: group_key.clone(),
                        },
                    )
                    .await
                    .map_err(|error| ingress_group_error("EffectGroupDispatch/run", error))?;
                // The group index's own readiness notice (FIG-4344).
                let notification = await_group_notice_via_ingress(
                    ingress,
                    &self
                        .await_event_ingress
                        .service(LashService::EffectGroupState),
                    &group_key,
                    &EffectGroupNotice::Ready,
                )
                .await
                .map_err(|error| {
                    ingress_group_error("EffectGroupIndex/await_notice(Ready)", error)
                })?;
                match notification {
                    EffectGroupNotification::Ready => Ok(handle),
                    EffectGroupNotification::Refused { reason } => Err(group_shape_error(format!(
                        "effect group {group_key} routing was refused: {reason:?}"
                    ))),
                    EffectGroupNotification::Retired => Err(group_shape_error(format!(
                        "effect group {group_key} was retired before it became ready"
                    ))),
                    other => Err(group_shape_error(format!(
                        "effect group {group_key} READY wait resolved as {other:?}"
                    ))),
                }
            }
            EffectGroupOpenResponse::ReopenedReady => Ok(handle),
            EffectGroupOpenResponse::ReopenedClosed { effective } => match effective {
                EffectGroupCloseOutcome::Refused { reason } => {
                    if let Some(executors) = self.group_executors.get() {
                        executors.release_group(&group_key);
                    }
                    Err(group_shape_error(format!(
                        "effect group {group_key} routing was refused: {reason:?}"
                    )))
                }
                EffectGroupCloseOutcome::RunToCompletion => Ok(handle),
                EffectGroupCloseOutcome::Cancel => {
                    if let Some(executors) = self.group_executors.get() {
                        executors.release_group(&group_key);
                    }
                    Ok(handle)
                }
            },
            EffectGroupOpenResponse::Retired => {
                if let Some(executors) = self.group_executors.get() {
                    executors.release_group(&group_key);
                }
                Err(group_shape_error(format!(
                    "effect group {group_key} is retired"
                )))
            }
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

    /// The host's own controller admits a group under the group's own scope;
    /// a scope's view opens with its admission.
    async fn open_effect_group(
        &self,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        let opener = lash_core::AdmittedScope::new(group.invocation().execution_scope().clone());
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
            .call_lash_object::<_, EffectGroupReadRankResponse>(
                &self
                    .await_event_ingress
                    .service(LashService::EffectGroupState),
                handle.group_key(),
                "read_rank",
                &EffectGroupReadRankRequest {
                    rank,
                    for_caller: true,
                    run: false,
                },
            )
            .await
            .map_err(|error| ingress_group_error("EffectGroupIndex/read_rank", error))?;
        if matches!(read, EffectGroupReadRankResponse::NotSettled) {
            // The group index's own rank notice (FIG-4344).
            let index_service = self
                .await_event_ingress
                .service(LashService::EffectGroupState);
            let notice = EffectGroupNotice::Rank { rank };
            let wait = await_group_notice_via_ingress(
                ingress,
                &index_service,
                handle.group_key(),
                &notice,
            );
            tokio::pin!(wait);
            // Unjournaled here: the turn's gate is raced over ingress (FIG-3672 P9).
            let notification = tokio::select! {
                result = &mut wait => Some(result.map_err(|error| ingress_group_error(
                    "EffectGroupIndex/await_notice(Rank)", error
                ))?),
                _ = cancel.cancellation().cancelled() => None, // The waiter's own stop: no journal here.
                stop = self.turn_stop(cancel.observed_scope()) => {
                    stop?;
                    None
                }
            };
            let Some(notification) = notification else {
                return Err(RuntimeEffectControllerError::new(
                    RuntimeErrorCode::RuntimeEffectGroupAwaitCancelled,
                    format!(
                        "awaiting effect group {} rank {rank} was cancelled",
                        handle.group_key()
                    ),
                ));
            };
            match notification {
                EffectGroupNotification::Rank => {}
                EffectGroupNotification::Retired => {
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
                .call_lash_object::<_, EffectGroupReadRankResponse>(
                    &self
                        .await_event_ingress
                        .service(LashService::EffectGroupState),
                    handle.group_key(),
                    "read_rank",
                    &EffectGroupReadRankRequest {
                        rank,
                        for_caller: true,
                        run: false,
                    },
                )
                .await
                .map_err(|error| ingress_group_error("EffectGroupIndex/read_rank", error))?;
        }
        let record = match read {
            EffectGroupReadRankResponse::Settled { settlement, .. } => settlement,
            EffectGroupReadRankResponse::SettledRun { .. } => {
                return Err(group_shape_error(format!(
                    "effect group {} answered a one-rank read of rank {rank} with a run",
                    handle.group_key()
                )));
            }
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
                .call_lash_object::<_, EffectGroupPayloadGetResponse>(
                    &self
                        .await_event_ingress
                        .namespace
                        .stable(LashService::EffectGroupPayload)
                        .name(),
                    &payload_key(handle.group_key(), record.position),
                    "get",
                    &(),
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
        let settlement = settlement_from_payload(rank, record, payload)?;
        handle.advance()?;
        if handle.is_exhausted()
            && let Some(executors) = self.group_executors.get()
        {
            executors.release_group(handle.group_key());
        }
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
            .call_lash_object::<_, EffectGroupReadRankResponse>(
                &self
                    .await_event_ingress
                    .service(LashService::EffectGroupState),
                group_key,
                "read_rank",
                &EffectGroupReadRankRequest {
                    rank,
                    for_caller: false,
                    run: false,
                },
            )
            .await
            .map_err(|error| ingress_group_error("EffectGroupIndex/read_rank", error))?;
        let (record, child_replay_key) = match read {
            EffectGroupReadRankResponse::Settled {
                settlement,
                child_replay_key,
            } => (settlement, child_replay_key),
            EffectGroupReadRankResponse::SettledRun { .. } => {
                return Err(group_shape_error(format!(
                    "effect group {group_key} answered a one-rank read of rank {rank} with a run"
                )));
            }
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
                .call_lash_object::<_, EffectGroupPayloadGetResponse>(
                    &self
                        .await_event_ingress
                        .namespace
                        .stable(LashService::EffectGroupPayload)
                        .name(),
                    &payload_key(group_key, record.position),
                    "get",
                    &(),
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
        let settlement = settlement_from_payload(rank, record, payload)?;
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
            .call_lash_object::<_, EffectGroupCloseResponse>(
                &self
                    .await_event_ingress
                    .service(LashService::EffectGroupState),
                &group_key,
                "close",
                &EffectGroupCloseRequest { disposition },
            )
            .await
            .map_err(|error| ingress_group_error("EffectGroupIndex/close", error))?;
        match response {
            EffectGroupCloseResponse::Closed | EffectGroupCloseResponse::AlreadyClosed => {
                if disposition == LoserPolicy::Cancel
                    && let Some(executors) = self.group_executors.get()
                {
                    executors.release_group(&group_key);
                }
                Ok(())
            }
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

    async fn await_group_child_drain_admission(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<(), RuntimeEffectControllerError> {
        // The §5 barrier on the group index's own notice, over ingress
        // (FIG-4344): the index answers once no committed sibling ranked
        // below `rank` still owes its seat, or retirement releases the wait.
        let notification = await_group_notice_via_ingress(
            &self.await_event_ingress.ingress,
            &self
                .await_event_ingress
                .service(LashService::EffectGroupState),
            group_key,
            &EffectGroupNotice::Drained { rank },
        )
        .await
        .map_err(|error| ingress_group_error("EffectGroupIndex/await_notice(Drained)", error))?;
        match notification {
            EffectGroupNotification::Drained
            | EffectGroupNotification::Retired
            | EffectGroupNotification::Absent => Ok(()),
            other => Err(group_shape_error(format!(
                "effect group {group_key} barrier at rank {rank} answered {other:?}"
            ))),
        }
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
            let RuntimeAwaitEventOptions { cancellation, .. } =
                local_executor.into_await_event_options()?;
            let resolution = await_restate_await_event_via_ingress(
                ingress,
                key,
                cancellation,
                IngressAwait::Effect(&effect_replay_key),
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
mod tests;

impl RestateEffectHostController {
    async fn retire_observation_receipts(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        if let Some((store, clock)) = self.wait_receipts.get() {
            let owner = serde_json::to_string(scope).map_err(|e| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::EngineEffectController,
                    e.to_string(),
                )
            })?;
            store
                .retire_observation_receipts(&owner, clock.timestamp_ms())
                .await
                .map_err(|error| RuntimeEffectControllerError::from(error).into_runtime_error())?;
        }
        Ok(())
    }
}
