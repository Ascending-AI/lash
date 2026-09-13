//! One scope's view of the in-handler controller.
//!
//! Forwards every call to the handler controller and records executing
//! effects and opened groups of a non-session scope in the scope's
//! `LashDurableWaitIndex` object, so the scope's quiescence proof counts them.

use lash_sansio::SessionId;
use std::sync::Arc;

use lash_core::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, CompletionKeyPreparation,
    EffectGroupHandle, ExecutionScope, GroupSettlement, LoserPolicy, QueuedLaneAcquisition,
    QueuedLaneProbe, Resolution, ResolveOutcome, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectFailureDisposition,
    RuntimeEffectGroup, RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeError,
    TurnControlParticipation,
};
use restate_sdk::errors::TerminalError;

use super::{RestateControllerContext, RestateRuntimeEffectController};
use crate::durable_wait::durable_wait_index_key_for_scope;

/// One scope's view of the in-handler controller: forwards everything, and
/// records executing effects and opened groups of a non-session scope in the
/// scope's `LashDurableWaitIndex` object so its quiescence proof counts them.
pub(super) struct ScopeRecordingController<'run, 'ctx, C> {
    pub(super) inner: &'run RestateRuntimeEffectController<'ctx, C>,
    pub(super) scope: ExecutionScope,
}

impl<'run, 'ctx, C> ScopeRecordingController<'run, 'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    /// The scope's index key when its effects are recorded: non-session
    /// scopes only.
    fn index_key(&self) -> Option<String> {
        self.scope
            .session_id()
            .is_none()
            .then(|| durable_wait_index_key_for_scope(&self.scope))
    }

    fn scope_retired(&self) -> RuntimeEffectControllerError {
        match self.scope.journal_identity() {
            Ok(identity) => {
                lash_core::facade_support::effect_replay_driver::scope_retired(identity.key())
            }
            Err(error) => RuntimeEffectControllerError::from(error),
        }
    }

    fn record_error(operation: &str, error: TerminalError) -> RuntimeEffectControllerError {
        RuntimeEffectControllerError::from(RuntimeError::new(
            lash_core::RuntimeErrorCode::RestateEffectController,
            format!("LashDurableWaitIndex/{operation} failed: {error}"),
        ))
    }
}

impl<'run, 'ctx, C> lash_core::ScopeBoundController for ScopeRecordingController<'run, 'ctx, C>
where
    C: RestateControllerContext<'ctx>,
    'ctx: 'run,
{
    fn for_scope<'a>(&self, scope: ExecutionScope) -> Arc<dyn lash_core::ScopeBoundController + 'a>
    where
        Self: 'a,
    {
        Arc::new(ScopeRecordingController {
            inner: self.inner,
            scope,
        })
    }
}

#[async_trait::async_trait]
impl<'run, 'ctx, C> AwaitEventResolver for ScopeRecordingController<'run, 'ctx, C>
where
    C: RestateControllerContext<'ctx>,
    'ctx: 'run,
{
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.await_event_authority_binding_id()
    }

    async fn acquire_queued_lane(
        &self,
        lane: Arc<dyn QueuedLaneProbe>,
        cancel: tokio_util::sync::CancellationToken,
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
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.inner.await_await_event(key, cancel, deadline).await
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
impl<'run, 'ctx, C> RuntimeEffectController for ScopeRecordingController<'run, 'ctx, C>
where
    C: RestateControllerContext<'ctx>,
    'ctx: 'run,
{
    fn owns_commit_backpressure(&self) -> bool {
        self.inner.owns_commit_backpressure()
    }

    fn supports_concurrent_effects(&self) -> bool {
        self.inner.supports_concurrent_effects()
    }

    fn supports_effect_groups(&self) -> bool {
        self.inner.supports_effect_groups()
    }

    fn wants_segment_boundary(
        &self,
        progress: &lash_core::SegmentProgress,
    ) -> Option<lash_core::BoundaryReason> {
        self.inner.wants_segment_boundary(progress)
    }

    async fn runtime_effect_failure_disposition(
        &self,
        code: lash_core::RuntimeErrorCode,
    ) -> Result<RuntimeEffectFailureDisposition, RuntimeError> {
        self.inner.runtime_effect_failure_disposition(code).await
    }

    async fn turn_control_participation(&self) -> Result<TurnControlParticipation, RuntimeError> {
        self.inner.turn_control_participation().await
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if envelope.invocation.execution_scope() != &self.scope {
            return Err(RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::RuntimeEffectScopeMismatch,
                format!(
                    "runtime effect address scope {:?} does not match admitted controller scope {:?}",
                    envelope.invocation.execution_scope(),
                    self.scope
                ),
            ));
        }
        let Some(index_key) = self.index_key() else {
            return self.inner.execute_effect(envelope, local_executor).await;
        };
        let replay_key = envelope.stable_hash()?;
        // Recorded before the effect runs and cleared after, both as durable
        // index calls, so the scope's quiescence proof sees the effect for
        // exactly as long as it executes. The record doubles as the scope
        // fence inside a handler: a revoked index admits nothing.
        if !self
            .inner
            .context
            .scope_effect_begin(index_key.clone(), replay_key.clone())
            .await
            .map_err(|error| Self::record_error("begin_effect", error))?
        {
            return Err(self.scope_retired());
        }
        let outcome = self.inner.execute_effect(envelope, local_executor).await;
        self.inner
            .context
            .scope_effect_end(index_key, replay_key)
            .await
            .map_err(|error| Self::record_error("end_effect", error))?;
        outcome
    }

    async fn open_effect_group(
        &self,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        group.validate_execution_scope(&self.scope)?;
        if let Some(index_key) = self.index_key()
            && !self
                .inner
                .context
                .scope_group_record(index_key, group.group_key().to_string())
                .await
                .map_err(|error| Self::record_error("record_group", error))?
        {
            return Err(self.scope_retired());
        }
        self.inner.open_effect_group(group).await
    }

    async fn await_next_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        self.inner.await_next_settlement(handle, cancel).await
    }

    async fn close_effect_group(
        &self,
        handle: EffectGroupHandle,
        disposition: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.inner.close_effect_group(handle, disposition).await
    }
}
