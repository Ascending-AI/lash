//! [`LayeredEffectHost`]: a recording or fault layer over any effect host
//! (FIG-3580).
//!
//! A test that has to observe or perturb the effect boundary wraps a real
//! deployment's host instead of hand-writing a controller that re-implements
//! half of one. The host lends exactly the controllers its inner host lends —
//! plain, static and group-child-bound — each wrapped with one
//! [`EffectLayer`]. Every group operation, await-event registry read and
//! journal lever lands on the inner host's substrate, so a group opened through
//! a layered controller is arbitrated by the state the inner host wrote, and a
//! bound child's admissions are fenced exactly where the inner host fences
//! them. A layer sees the seam operations and may observe, delay, fail or crash
//! them; it owns no state the substrate answers from.

use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use super::{
    AdmittedScope, AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason,
    CompletionKeyPreparation, EffectGroupChildCommitOutcome, EffectGroupHandle, EffectHost,
    EffectJournalRetirement, EffectJournaling, ExecutionScope, GroupChildBinding,
    GroupChildFinalCommit, GroupExecutors, GroupSettlement, LoserPolicy, QueuedLaneAcquisition,
    QueuedLaneProbe, RankedGroupSettlement, Resolution, ResolveOutcome, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectGroup,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, ScopedEffectController, SegmentProgress,
    StoreEffectGroupClosing, ToolChildHost, ToolIntentOutcomeSink, ToolIntentPreparation,
    TurnControlAuthorityOwner,
};
use crate::{RuntimeError, RuntimeErrorCode, SessionId};

/// The seam operations a [`LayeredEffectHost`] routes through its layer.
///
/// Each hook receives the inner host's controller (or resolver) for the call
/// and defaults to forwarding to it untouched, so a layer implements only what
/// it observes or perturbs. A hook may run code before or after the inner
/// call, replace the local executor it hands down, return an error in place of
/// the inner call, or park forever to model a crash; it must not answer a
/// group operation from state of its own, because the inner substrate is the
/// only arbiter of a group.
#[async_trait::async_trait]
pub trait EffectLayer: Send + Sync + 'static {
    async fn execute_effect(
        &self,
        inner: &dyn RuntimeEffectController,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        inner.execute_effect(envelope, local_executor).await
    }

    async fn open_effect_group(
        &self,
        inner: &dyn RuntimeEffectController,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        inner.open_effect_group(group).await
    }

    async fn await_next_settlement(
        &self,
        inner: &dyn RuntimeEffectController,
        handle: &mut EffectGroupHandle,
        cancel: CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        inner.await_next_settlement(handle, cancel).await
    }

    async fn close_effect_group(
        &self,
        inner: &dyn RuntimeEffectController,
        handle: EffectGroupHandle,
        disposition: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        inner.close_effect_group(handle, disposition).await
    }

    async fn resolve_await_event(
        &self,
        inner: &dyn AwaitEventResolver,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        inner: &dyn AwaitEventResolver,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        inner: &dyn AwaitEventResolver,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        inner.await_await_event(key, cancel, deadline).await
    }
}

/// An [`EffectHost`] that delegates to `inner` and wraps each scoped
/// controller `inner` lends with `layer`.
///
/// Two methods are this host's own so promises cross the layer:
/// `await_event_resolver` answers with this host, and `turn_control_binding`
/// is the trait's composition over this host's resolver and scoped
/// controllers. Everything else is `inner`'s.
///
/// Testing only. `inner` must lend owned controllers (every store-backed host
/// does); a controller borrowed from `inner` cannot outlive the call that
/// lent it, so it cannot be layered, and the host refuses to scope rather than
/// lend an unlayered one.
pub struct LayeredEffectHost {
    inner: Arc<dyn EffectHost>,
    layer: Arc<dyn EffectLayer>,
}

impl LayeredEffectHost {
    pub fn new(inner: Arc<dyn EffectHost>, layer: Arc<dyn EffectLayer>) -> Self {
        Self { inner, layer }
    }

    /// The host this one layers.
    pub fn inner(&self) -> &Arc<dyn EffectHost> {
        &self.inner
    }

    fn layered(
        layer: &Arc<dyn EffectLayer>,
        scoped: &ScopedEffectController<'_>,
    ) -> Result<ScopedEffectController<'static>, RuntimeError> {
        let inner = scoped.owned_controller().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                "a LayeredEffectHost layers only a host that lends owned scoped controllers",
            )
        })?;
        ScopedEffectController::shared(
            Arc::new(LayeredController {
                inner,
                layer: Arc::clone(layer),
            }),
            scoped.admitted_scope().clone(),
        )
    }

    fn layered_option(
        &self,
        scoped: Option<ScopedEffectController<'static>>,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        scoped
            .map(|scoped| Self::layered(&self.layer, &scoped))
            .transpose()
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for LayeredEffectHost {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.await_event_authority_binding_id()
    }

    async fn acquire_queued_lane(
        &self,
        lane: Arc<dyn QueuedLaneProbe>,
        cancel: CancellationToken,
    ) -> Result<QueuedLaneAcquisition, RuntimeError> {
        self.inner.acquire_queued_lane(lane, cancel).await
    }

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        self.inner
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.layer
            .resolve_await_event(self.inner.await_event_resolver(), key, resolution)
            .await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.layer
            .peek_await_event(self.inner.await_event_resolver(), key)
            .await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.layer
            .await_await_event(self.inner.await_event_resolver(), key, cancel, deadline)
            .await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }

    async fn retire_await_events_for_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.inner.retire_await_events_for_scope(scope).await
    }

    async fn retire_await_events_for_scope_if_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.inner
            .retire_await_events_for_scope_if_quiescent(scope)
            .await
    }

    async fn reinstate_await_event_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.inner.reinstate_await_event_scope(scope).await
    }

    async fn await_event_scope_is_retired(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.inner.await_event_scope_is_retired(scope).await
    }
}

#[async_trait::async_trait]
impl EffectHost for LayeredEffectHost {
    fn turn_control_binding_id(&self) -> String {
        self.inner.turn_control_binding_id()
    }

    fn turn_control_authority_owner(&self) -> TurnControlAuthorityOwner {
        self.inner.turn_control_authority_owner()
    }

    async fn list_outstanding_await_event_keys(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<AwaitEventKey>, RuntimeError> {
        self.inner
            .list_outstanding_await_event_keys(session_id)
            .await
    }

    fn turn_attach(&self) -> Option<Arc<dyn crate::TurnAttach>> {
        self.inner.turn_attach()
    }

    fn scoped<'run>(
        &'run self,
        admitted: AdmittedScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        Self::layered(&self.layer, &self.inner.scoped(admitted)?)
    }

    fn scoped_static(
        &self,
        admitted: AdmittedScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        self.layered_option(self.inner.scoped_static(admitted)?)
    }

    fn scoped_for_group_child(
        &self,
        admitted: AdmittedScope,
        binding: GroupChildBinding,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        self.layered_option(self.inner.scoped_for_group_child(admitted, binding)?)
    }

    fn effect_group_closing(&self) -> Option<Arc<dyn StoreEffectGroupClosing>> {
        self.inner.effect_group_closing()
    }

    fn install_tool_child_host(&self, candidate: Arc<ToolChildHost>) -> Option<Arc<ToolChildHost>> {
        self.inner.install_tool_child_host(candidate)
    }

    /// The layered host itself, so a turn-control binding composed from this
    /// host resolves and peeks its promises through the layer. The trait's
    /// own `turn_control_binding` is kept for the same reason: it builds the
    /// binding from this host's resolver and scoped controllers.
    fn await_event_resolver(&self) -> &dyn AwaitEventResolver {
        self
    }

    async fn prepare_tool_intent(
        &self,
        sink: &dyn ToolIntentOutcomeSink,
        identity: &crate::ToolIntentIdentity,
        intent: crate::ToolIntent,
    ) -> Result<ToolIntentPreparation, RuntimeError> {
        self.inner.prepare_tool_intent(sink, identity, intent).await
    }

    async fn record_tool_intent_outcome(
        &self,
        sink: &dyn ToolIntentOutcomeSink,
        identity: &crate::ToolIntentIdentity,
        submitted: crate::ToolIntent,
        outcome: crate::ToolIntentExecutionOutcome,
    ) -> Result<(), RuntimeError> {
        self.inner
            .record_tool_intent_outcome(sink, identity, submitted, outcome)
            .await
    }

    async fn retire_effect_journal(
        &self,
        retirement: EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        self.inner.retire_effect_journal(retirement).await
    }

    async fn pending_artifact_owner_retirements(
        &self,
    ) -> Result<Vec<ExecutionScope>, RuntimeError> {
        self.inner.pending_artifact_owner_retirements().await
    }

    async fn complete_artifact_owner_retirement(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.inner.complete_artifact_owner_retirement(scope).await
    }

    async fn reinstate_effect_scope(&self, scope: &ExecutionScope) -> Result<(), RuntimeError> {
        self.inner.reinstate_effect_scope(scope).await
    }

    fn bind_process_registry(&self, binding: crate::ProcessRegistryBinding) {
        self.inner.bind_process_registry(binding);
    }

    async fn register_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.inner
            .register_turn_cancel_closure_participant(participant_id, scope)
            .await
    }

    async fn release_turn_cancel_closure_participant(
        &self,
        participant_id: &str,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.inner
            .release_turn_cancel_closure_participant(participant_id, scope)
            .await
    }
}

/// One scoped controller of the inner host, with the layer in front of its
/// seam operations and everything else forwarded.
struct LayeredController {
    inner: Arc<dyn RuntimeEffectController>,
    layer: Arc<dyn EffectLayer>,
}

#[async_trait::async_trait]
impl AwaitEventResolver for LayeredController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.await_event_authority_binding_id()
    }

    async fn acquire_queued_lane(
        &self,
        lane: Arc<dyn QueuedLaneProbe>,
        cancel: CancellationToken,
    ) -> Result<QueuedLaneAcquisition, RuntimeError> {
        self.inner.acquire_queued_lane(lane, cancel).await
    }

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        self.inner
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.layer
            .resolve_await_event(self.inner.as_ref(), key, resolution)
            .await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.layer.peek_await_event(self.inner.as_ref(), key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.layer
            .await_await_event(self.inner.as_ref(), key, cancel, deadline)
            .await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }

    async fn retire_await_events_for_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.inner.retire_await_events_for_scope(scope).await
    }

    async fn retire_await_events_for_scope_if_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.inner
            .retire_await_events_for_scope_if_quiescent(scope)
            .await
    }

    async fn reinstate_await_event_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        self.inner.reinstate_await_event_scope(scope).await
    }

    async fn await_event_scope_is_retired(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.inner.await_event_scope_is_retired(scope).await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for LayeredController {
    fn owns_commit_backpressure(&self) -> bool {
        self.inner.owns_commit_backpressure()
    }

    fn effect_journaling(&self) -> EffectJournaling {
        self.inner.effect_journaling()
    }

    fn wants_segment_boundary(&self, progress: &SegmentProgress) -> Option<BoundaryReason> {
        self.inner.wants_segment_boundary(progress)
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.layer
            .execute_effect(self.inner.as_ref(), envelope, local_executor)
            .await
    }

    async fn open_effect_group(
        &self,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        self.layer
            .open_effect_group(self.inner.as_ref(), group)
            .await
    }

    fn register_group_executors(
        &self,
        executors: Arc<dyn GroupExecutors>,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.inner.register_group_executors(executors)
    }

    fn native_effect_groups_substrate(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        self.inner.native_effect_groups_substrate()
    }

    /// The inner substrate mints the bound child, and the layer still sees its
    /// traffic: a child's effects cross the same seam its parent's do.
    fn group_child_scoped_controller(
        &self,
        admitted: AdmittedScope,
        binding: GroupChildBinding,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        self.inner
            .group_child_scoped_controller(admitted, binding)?
            .map(|scoped| LayeredEffectHost::layered(&self.layer, &scoped))
            .transpose()
    }

    async fn await_next_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        cancel: CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        self.layer
            .await_next_settlement(self.inner.as_ref(), handle, cancel)
            .await
    }

    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<Option<RankedGroupSettlement>, RuntimeEffectControllerError> {
        self.inner.read_group_settlement(group_key, rank).await
    }

    async fn close_effect_group(
        &self,
        handle: EffectGroupHandle,
        disposition: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.layer
            .close_effect_group(self.inner.as_ref(), handle, disposition)
            .await
    }

    async fn commit_group_child_final(
        &self,
        commit: GroupChildFinalCommit,
    ) -> Result<EffectGroupChildCommitOutcome, RuntimeEffectControllerError> {
        self.inner.commit_group_child_final(commit).await
    }

    async fn await_group_child_drain_admission(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.inner
            .await_group_child_drain_admission(group_key, commit_seq)
            .await
    }

    async fn read_recorded_journal(
        &self,
        range: &crate::RecordedKeyRange,
    ) -> Result<crate::RecordedJournal, crate::RuntimeEffectControllerError> {
        self.inner.read_recorded_journal(range).await
    }
}
