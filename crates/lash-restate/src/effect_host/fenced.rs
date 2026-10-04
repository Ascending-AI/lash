//! One admitted scope's view of the deployment host (FIG-2499, FIG-3780).

use super::*;

/// One scope's view of the deployment host: forwards everything to the shared
/// controller and refuses effects and groups once the scope is retired.
pub(super) struct FencedRestateController {
    pub(super) controller: Arc<RestateEffectHostController>,
    /// The admitted scope this view serves; the groups it opens record it as
    /// their opener (FIG-3780).
    pub(super) admitted: lash_core::AdmittedScope,
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
            return Err(lash_core::facade_support::scope_status::scope_retired(
                identity.key(),
            ));
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
impl RuntimeEffectController for FencedRestateController {
    fn owns_commit_backpressure(&self) -> bool {
        self.controller.owns_commit_backpressure()
    }

    fn attempt_observation(&self) -> Option<lash_trace::AttemptObservation> {
        self.controller.attempt_observation()
    }

    fn hands_over_turns(&self) -> bool {
        self.controller.hands_over_turns()
    }

    fn wants_segment_boundary(
        &self,
        progress: &lash_core::SegmentProgress,
    ) -> Option<lash_core::BoundaryReason> {
        self.controller.wants_segment_boundary(progress)
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

    async fn read_recorded_journal(
        &self,
        range: &lash_core::RecordedKeyRange,
    ) -> Result<lash_core::RecordedJournal, lash_core::RuntimeEffectControllerError> {
        self.controller.read_recorded_journal(range).await
    }
}
