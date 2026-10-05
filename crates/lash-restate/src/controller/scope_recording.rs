//! One scope's view of the in-handler controller.
//!
//! Forwards every call to the handler controller and records executing
//! effects of runtime operations in the scope's index.
//! Process effects are protected by their segment's journal pin instead.

use lash_sansio::SessionId;
use std::sync::Arc;

use lash_core::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, CompletionKeyPreparation,
    ExecutionScope, Resolution, ResolveOutcome, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, RuntimeError,
};
use restate_sdk::errors::TerminalError;

use super::{RestateControllerContext, RestateRuntimeEffectController};
use crate::durable_wait::durable_wait_index_key_for_scope;

/// One scope's view of the in-handler controller: forwards everything, and
/// records runtime-operation effects in the scope's index.
/// A process segment pins its index once until it can issue no more effects.
pub(super) struct ScopeRecordingController<'run, 'ctx, C> {
    pub(super) inner: HandlerController<'run, 'ctx, C>,
    /// The admitted scope this controller serves: the claim address its
    /// effects fence on and, for a process, the incarnation it runs under.
    pub(super) admitted: lash_core::AdmittedScope,
    pub(super) run_records: lash_core::facade_support::RunRecordObserver,
}

/// The handler controller a scope view forwards to: borrowed from the
/// handler that built it, or owned by the view's holder when nothing outlives
/// the view to lend it from.
pub(super) enum HandlerController<'run, 'ctx, C> {
    Borrowed(&'run RestateRuntimeEffectController<'ctx, C>),
    Owned(Arc<RestateRuntimeEffectController<'ctx, C>>),
}

impl<'ctx, C> Clone for HandlerController<'_, 'ctx, C> {
    fn clone(&self) -> Self {
        match self {
            Self::Borrowed(controller) => Self::Borrowed(controller),
            Self::Owned(controller) => Self::Owned(Arc::clone(controller)),
        }
    }
}

impl<'ctx, C> std::ops::Deref for HandlerController<'_, 'ctx, C> {
    type Target = RestateRuntimeEffectController<'ctx, C>;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Borrowed(controller) => controller,
            Self::Owned(controller) => controller,
        }
    }
}

impl<'run, 'ctx, C> ScopeRecordingController<'run, 'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    /// The scope's index key when its effects are recorded: non-session
    /// scopes only.
    fn index_key(&self) -> Option<String> {
        self.admitted
            .scope()
            .session_id()
            .is_none()
            .then(|| durable_wait_index_key_for_scope(self.admitted.scope()))
    }

    fn scope_retired(&self) -> RuntimeEffectControllerError {
        match self.admitted.scope().journal_identity() {
            Ok(identity) => lash_core::facade_support::scope_status::scope_retired(identity.key()),
            Err(error) => RuntimeEffectControllerError::from(error),
        }
    }

    fn record_error(operation: &str, error: TerminalError) -> RuntimeEffectControllerError {
        crate::wire::typed_terminal(error.message()).unwrap_or_else(|| {
            RuntimeEffectControllerError::from(RuntimeError::new(
                lash_core::RuntimeErrorCode::EngineEffectController,
                format!("LashDurableWaitIndex/{operation} failed: {error}"),
            ))
        })
    }
}

impl<'run, 'ctx, C> lash_core::ScopeBoundController for ScopeRecordingController<'run, 'ctx, C>
where
    C: RestateControllerContext<'ctx>,
    'ctx: 'run,
{
    fn for_scope<'a>(
        &self,
        admitted: lash_core::AdmittedScope,
    ) -> Arc<dyn lash_core::ScopeBoundController + 'a>
    where
        Self: 'a,
    {
        Arc::new(ScopeRecordingController {
            inner: self.inner.clone(),
            admitted,
            run_records: Default::default(),
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

    async fn publish_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<Option<ResolveOutcome>, RuntimeError> {
        self.inner.publish_await_event(key, resolution).await
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
    ) -> Result<Resolution, RuntimeError> {
        self.inner.await_await_event(key, cancel).await
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

    fn attempt_observation(&self) -> Option<lash_trace::AttemptObservation> {
        self.inner.attempt_observation()
    }

    fn hands_over_turns(&self) -> bool {
        self.inner.hands_over_turns()
    }

    fn wants_segment_boundary(
        &self,
        progress: &lash_core::SegmentProgress,
    ) -> Option<lash_core::BoundaryReason> {
        self.inner.wants_segment_boundary(progress)
    }

    async fn observe_process_cancel(
        &self,
        lent_stop: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, RuntimeEffectControllerError> {
        self.inner.observe_process_cancel(lent_stop).await
    }

    async fn record_process_drive_step(
        &self,
        name: String,
        step: lash_core::ProcessDriveStep<'_>,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.inner.record_process_drive_step(name, step).await
    }

    fn run_record_observer(&self) -> Option<&lash_core::facade_support::RunRecordObserver> {
        Some(&self.run_records)
    }

    async fn record_run_schedule(
        &self,
        name: String,
        step: lash_core::RunRecordStep<'_>,
    ) -> Result<lash_core::tool_run::RunJournalEntry, RuntimeEffectControllerError> {
        self.run_records
            .record_schedule(&*self.inner, name, step)
            .await
    }

    fn start_run_attempt<'step>(
        &'step self,
        name: String,
        step: lash_core::tool_dispatch::RunAttemptStep<'step>,
    ) -> lash_core::tool_dispatch::RunAttemptHandle<'step> {
        self.inner.start_run_attempt(name, step)
    }

    fn start_run_prepare<'step>(
        &'step self,
        name: String,
        step: lash_core::tool_dispatch::RunStartPrepareStep<'step>,
    ) -> lash_core::tool_dispatch::RunStepHandle<'step, lash_core::tool_dispatch::RunStartPrepared>
    {
        self.inner.start_run_prepare(name, step)
    }

    async fn issue_run_realization<'step>(
        &'step self,
        request: lash_core::tool_dispatch::RealizationRequest,
    ) -> Result<lash_core::tool_dispatch::IssuedRealization<'step>, RuntimeEffectControllerError>
    {
        self.inner.issue_run_realization(request).await
    }

    /// Attach to previously issued protected work without sending or executing it again.
    async fn attach_run_realization<'step>(
        &'step self,
        invocation_id: String,
    ) -> Result<
        lash_core::tool_dispatch::RunSelectable<
            'step,
            lash_core::tool_dispatch::RealizationReceipt,
        >,
        RuntimeEffectControllerError,
    > {
        self.inner.attach_run_realization(invocation_id).await
    }

    fn start_run_retry(&self, backoff_ms: u64) -> lash_core::tool_dispatch::RunRetryTimer<'_> {
        self.inner.start_run_retry(backoff_ms)
    }

    async fn arm_run_source(
        &self,
        descriptor: lash_core::tool_run::SourceDescriptor,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.inner.arm_run_source(descriptor).await
    }
    async fn attach_run_process_terminal(
        &self,
        descriptor: lash_core::tool_run::SourceDescriptor,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.inner.attach_run_process_terminal(descriptor).await
    }
    async fn cancel_run_source(
        &self,
        descriptor: lash_core::tool_run::SourceDescriptor,
    ) -> Result<lash_core::tool_run::SourceSeal, RuntimeEffectControllerError> {
        self.inner.cancel_run_source(descriptor).await
    }
    async fn peek_run_cut(
        &self,
    ) -> Result<Option<lash_core::BoundaryReason>, RuntimeEffectControllerError> {
        self.inner.peek_run_cut().await
    }

    async fn await_run_sources(
        &self,
        subscriptions: Vec<lash_core::tool_run::SourceSubscription>,
        cancel: lash_core::TurnCancelWait,
    ) -> Result<(usize, lash_core::tool_run::SourceSeal), RuntimeEffectControllerError> {
        self.inner.await_run_sources(subscriptions, cancel).await
    }

    async fn record_run_record(
        &self,
        name: String,
        step: lash_core::RunRecordStep<'_>,
    ) -> Result<lash_core::tool_run::RunJournalEntry, RuntimeEffectControllerError> {
        self.run_records.record(&*self.inner, name, step).await
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if envelope.invocation.execution_scope() != self.admitted.scope() {
            return Err(RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::RuntimeEffectScopeMismatch,
                format!(
                    "runtime effect address scope {:?} does not match admitted controller scope {:?}",
                    envelope.invocation.execution_scope(),
                    self.admitted.scope()
                ),
            ));
        }
        if matches!(self.admitted.scope(), ExecutionScope::Process { .. }) {
            return self.inner.execute_effect(envelope, local_executor).await;
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
            .scope_effect_begin(&self.inner.namespace, index_key.clone(), replay_key.clone())
            .await
            .map_err(|error| Self::record_error("begin_effect", error))?
        {
            return Err(self.scope_retired());
        }
        let outcome = self.inner.execute_effect(envelope, local_executor).await;
        self.inner
            .context
            .scope_effect_end(&self.inner.namespace, index_key, replay_key)
            .await
            .map_err(|error| Self::record_error("end_effect", error))?;
        outcome
    }

    async fn read_recorded_journal(
        &self,
        range: &lash_core::RecordedKeyRange,
    ) -> Result<lash_core::RecordedJournal, lash_core::RuntimeEffectControllerError> {
        self.inner.read_recorded_journal(range).await
    }
}
