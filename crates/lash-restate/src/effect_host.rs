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
    EffectHost, ExecutionScope, Resolution, ResolveOutcome, RuntimeEffectCommand,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
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
use crate::{LashService, RestateAuthorityId, RestateConnection, RestateIngressClient};

mod journal_verdict;
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
    /// through `connection` under `authority_id`.
    pub fn new(connection: impl Into<RestateConnection>, authority_id: RestateAuthorityId) -> Self {
        Self::in_namespace(connection, authority_id, crate::RestateNamespace::default())
    }

    /// A host outside any deployment, in the default namespace, for resolving
    /// await events under `authority_id`.
    pub fn outside_deployment(
        connection: impl Into<RestateConnection>,
        authority_id: RestateAuthorityId,
    ) -> Self {
        Self::in_deployment_namespace(connection, authority_id, crate::RestateNamespace::default())
    }

    /// [`new`](Self::new) for a deployment in `namespace` (FIG-3898): every
    /// lash service this host calls is that namespace's.
    pub fn in_namespace(
        connection: impl Into<RestateConnection>,
        authority_id: RestateAuthorityId,
        namespace: crate::RestateNamespace,
    ) -> Self {
        Self::in_deployment_namespace(connection, authority_id, namespace)
    }

    /// Construct the ingress host in its deployment namespace.
    pub(crate) fn in_deployment_namespace(
        connection: impl Into<RestateConnection>,
        authority_id: RestateAuthorityId,
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
                registrations: std::sync::Mutex::new(None),
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
use ingress::*;
struct RestateEffectHostController {
    await_event_ingress: RestateAwaitEventIngress,
    authority_id: RestateAuthorityId,
    /// The bound process registry's registration truth (ADR 0049): a process
    /// scope's index says `revoked` only as a cache of the registry's fence,
    /// so a revoked index on a registered process is stale and is reinstated
    /// on first use.
    registrations: std::sync::Mutex<Option<Arc<dyn lash_core::ProcessRegistrationProbe>>>,
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
}

#[async_trait::async_trait]
impl RuntimeEffectController for RestateEffectHostController {
    fn owns_commit_backpressure(&self) -> bool {
        true
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
