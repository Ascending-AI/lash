use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use super::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, BoundaryReason,
    CompletionKeyPreparation, EffectGroupHandle, EffectHost, EffectJournalRetirement,
    ExecutionScope, GroupSettlement, InlineRuntimeEffectController, LoserPolicy, Resolution,
    ResolveOutcome, RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectFailureDisposition, RuntimeEffectGroup, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, ScopedEffectController, SegmentProgress, TurnControlBinding,
    TurnControlParticipation,
};
use crate::RuntimeError;
use crate::runtime::effect::executor::control::facade_ops::ScopedEffectControllerFacadeOps;

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
    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        if !may_defer {
            return Ok(CompletionKeyPreparation::NotNeeded);
        }
        if self
            .allow_process_lifetime_completion_keys
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return self
                .await_event_key(scope, wait)
                .await
                .map(CompletionKeyPreparation::Issued);
        }
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
        ScopedEffectController::shared(Arc::new(self.clone()), scope)
    }

    fn scoped_static(
        &self,
        scope: ExecutionScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        Ok(Some(ScopedEffectController::shared(
            Arc::new(self.clone()),
            scope,
        )?))
    }

    async fn turn_control_binding<'a>(
        &'a self,
        scoped: &'a ScopedEffectController<'_>,
    ) -> Result<TurnControlBinding<'a>, RuntimeError> {
        match scoped.controller().turn_control_participation().await? {
            TurnControlParticipation::Local => Ok(TurnControlBinding::HostOwned {
                resolver: self,
                peek: self.scoped(scoped.execution_scope().clone())?,
            }),
            TurnControlParticipation::DurableJournaled => Ok(TurnControlBinding::RunScoped {
                resolver: scoped.controller(),
                durable_cancel_after_llm: true,
            }),
        }
    }

    async fn retire_effect_journal(
        &self,
        _retirement: EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        Ok(0)
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for InlineEffectHost {
    fn wants_segment_boundary(&self, progress: &SegmentProgress) -> Option<BoundaryReason> {
        self.controller.wants_segment_boundary(progress)
    }

    fn supports_concurrent_effects(&self) -> bool {
        self.controller.supports_concurrent_effects()
    }

    async fn runtime_effect_failure_disposition(
        &self,
        code: crate::RuntimeErrorCode,
    ) -> Result<RuntimeEffectFailureDisposition, RuntimeError> {
        self.controller
            .runtime_effect_failure_disposition(code)
            .await
    }

    async fn turn_control_participation(&self) -> Result<TurnControlParticipation, RuntimeError> {
        self.controller.turn_control_participation().await
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

    // The group methods below must stay forwarded, never trait-defaulted:
    // supports_effect_groups() would otherwise advertise a capability the
    // defaults refuse.
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
        disposition: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.controller
            .close_effect_group(handle, disposition)
            .await
    }
}
