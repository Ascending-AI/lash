use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use super::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason, EffectGroupHandle,
    EffectHost, EffectJournalRetirement, ExecutionScope, GroupSettlement,
    InlineRuntimeEffectController, LoserDisposition, Resolution, ResolveOutcome,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectGroup, RuntimeEffectLocalExecutor, RuntimeEffectOutcome, ScopedEffectController,
    SegmentProgress,
};
use crate::RuntimeError;

/// In-process deployment effect host.
#[derive(Clone)]
pub struct InlineEffectHost {
    controller: Arc<dyn RuntimeEffectController>,
    allow_process_lifetime_completion_keys: Arc<std::sync::atomic::AtomicBool>,
}

impl InlineEffectHost {
    pub fn new(controller: Arc<dyn RuntimeEffectController>) -> Self {
        Self {
            controller,
            allow_process_lifetime_completion_keys: Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            )),
        }
    }

    /// Explicitly accept that externally routed completion keys die with this
    /// process. Intended only for deliberately single-process embeddings.
    pub fn allow_process_lifetime_completion_keys(self) -> Self {
        self.allow_process_lifetime_completion_keys
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self
    }
}

impl Default for InlineEffectHost {
    fn default() -> Self {
        Self::new(Arc::new(InlineRuntimeEffectController::default()))
    }
}

#[async_trait::async_trait]
impl AwaitEventResolver for InlineEffectHost {
    fn replay_ownership(&self) -> crate::EffectReplayOwnership {
        self.controller.replay_ownership()
    }

    fn durable_workflow_controller(&self) -> bool {
        self.controller.durable_workflow_controller()
    }

    fn journal_addressing(&self) -> crate::EffectJournalAddressing {
        self.controller.journal_addressing()
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        self.controller.allows_process_lifetime_completion_keys()
            || self
                .allow_process_lifetime_completion_keys
                .load(std::sync::atomic::Ordering::Relaxed)
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
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.controller
            .await_await_event(key, cancel, deadline)
            .await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.controller
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.controller
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl EffectHost for InlineEffectHost {
    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        ScopedEffectController::shared(
            Arc::new(InlineHostScopedController {
                controller: Arc::clone(&self.controller),
                allow_process_lifetime_completion_keys: Arc::clone(
                    &self.allow_process_lifetime_completion_keys,
                ),
            }),
            scope,
        )
    }

    fn scoped_static(
        &self,
        scope: ExecutionScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        Ok(Some(ScopedEffectController::shared(
            Arc::new(InlineHostScopedController {
                controller: Arc::clone(&self.controller),
                allow_process_lifetime_completion_keys: Arc::clone(
                    &self.allow_process_lifetime_completion_keys,
                ),
            }),
            scope,
        )?))
    }

    async fn retire_effect_journal(
        &self,
        _retirement: EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        Ok(0)
    }
}

#[derive(Clone)]
struct InlineHostScopedController {
    controller: Arc<dyn RuntimeEffectController>,
    allow_process_lifetime_completion_keys: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl AwaitEventResolver for InlineHostScopedController {
    fn replay_ownership(&self) -> crate::EffectReplayOwnership {
        self.controller.replay_ownership()
    }

    fn durable_workflow_controller(&self) -> bool {
        self.controller.durable_workflow_controller()
    }

    fn journal_addressing(&self) -> crate::EffectJournalAddressing {
        self.controller.journal_addressing()
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        self.controller.allows_process_lifetime_completion_keys()
            || self
                .allow_process_lifetime_completion_keys
                .load(std::sync::atomic::Ordering::Relaxed)
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
        cancel: CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        self.controller
            .await_await_event(key, cancel, deadline)
            .await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.controller
            .revoke_await_events_for_session(session_id)
            .await
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.controller
            .cancel_await_events_for_session(session_id)
            .await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for InlineHostScopedController {
    fn wants_segment_boundary(&self, progress: &SegmentProgress) -> Option<BoundaryReason> {
        self.controller.wants_segment_boundary(progress)
    }

    fn supports_concurrent_effects(&self) -> bool {
        self.controller.supports_concurrent_effects()
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.controller
            .execute_effect(envelope, local_executor)
            .await
    }

    // The four group methods are forwarded rather than defaulted. A scoped view
    // that answered the capability flag from the wrapped controller while
    // leaving the methods on their fail-closed defaults would report a host that
    // supports groups and then refuse every group opened through the only path
    // production reaches it by — `EffectHost::scoped`.

    fn supports_effect_groups(&self) -> bool {
        self.controller.supports_effect_groups()
    }

    async fn open_effect_group(
        &self,
        group: RuntimeEffectGroup,
    ) -> Result<EffectGroupHandle, RuntimeEffectControllerError> {
        self.controller.open_effect_group(group).await
    }

    async fn await_next_settlement(
        &self,
        handle: &mut EffectGroupHandle,
        cancel: CancellationToken,
    ) -> Result<GroupSettlement, RuntimeEffectControllerError> {
        self.controller.await_next_settlement(handle, cancel).await
    }

    async fn close_effect_group(
        &self,
        handle: EffectGroupHandle,
        disposition: LoserDisposition,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.controller
            .close_effect_group(handle, disposition)
            .await
    }
}
