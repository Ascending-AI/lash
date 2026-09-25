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
