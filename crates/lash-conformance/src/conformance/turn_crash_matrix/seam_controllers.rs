//! The crash-matrix seam controllers: recording, crash-injecting, and
//! store-authority doubles that wrap the fixture's real controller and forward
//! every group operation to it, so durable effect-group state stays in the
//! substrate under test.

use std::sync::Arc;

use lash_sansio::SessionId;

use super::{EffectOperation, SeamControl, TurnSeamOperation, turn_control_resolution_operation};
use crate::{
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
};

pub(crate) struct SeamEffectController {
    pub(super) inner: Arc<dyn RuntimeEffectController>,
    pub(super) control: SeamControl,
    pub(super) executions: Arc<std::sync::atomic::AtomicUsize>,
    /// The wrapped controller's journal fault injector, when it is a
    /// journaled controller exposing one (FIG-3524).
    pub(super) journal_faults:
        Option<lash_core::facade_support::effect_replay_driver::EffectJournalFaults>,
}

impl SeamEffectController {
    /// The typed store error an armed `ToolAttempt` error-return substitutes
    /// for the real `execute_effect` call: the journal's own `Store`
    /// vocabulary where the controller exposes one, the generic runtime-store
    /// code where it does not.
    fn injected_store_error(&self) -> RuntimeEffectControllerError {
        let code = self
            .journal_faults
            .as_ref()
            .map_or(crate::RuntimeErrorCode::RuntimeStore, |faults| {
                faults.store_code()
            });
        RuntimeEffectControllerError::new(code, "injected store error at the tool-attempt seam")
    }
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for SeamEffectController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.await_event_authority_binding_id()
    }

    async fn prepare_completion_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<crate::CompletionKeyPreparation, crate::RuntimeError> {
        self.inner
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
    ) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &crate::AwaitEventKey,
        resolution: crate::Resolution,
    ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
        match turn_control_resolution_operation(key) {
            Some(operation) => {
                self.control
                    .around(operation, self.inner.resolve_await_event(key, resolution))
                    .await
            }
            None => self.inner.resolve_await_event(key, resolution).await,
        }
    }

    async fn peek_await_event(
        &self,
        key: &crate::AwaitEventKey,
    ) -> Result<Option<crate::Resolution>, crate::RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &crate::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<crate::Resolution, crate::RuntimeError> {
        self.inner.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for SeamEffectController {
    fn effect_journaling(&self) -> crate::EffectJournaling {
        self.inner.effect_journaling()
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let operation = match &envelope.command {
            crate::RuntimeEffectCommand::ToolAttempt { call, .. } => Some((
                if envelope.group.is_some() {
                    EffectOperation::GroupChild {
                        name: call.tool_name.clone(),
                    }
                } else {
                    EffectOperation::ToolAttempt {
                        name: call.tool_name.clone(),
                    }
                },
                true,
            )),
            _ => None,
        };
        let Some((operation, counts_external_execution)) = operation else {
            return self.inner.execute_effect(envelope, executor).await;
        };
        let operation = TurnSeamOperation::Effect(operation);
        // FIG-3524: an armed error-return makes the tool-attempt seam fail
        // once — at the seam itself, or inside its journal claim/finalize/
        // renew — instead of crashing the task.
        if let Some(placement) = self.control.armed_error_return()
            && matches!(
                operation,
                TurnSeamOperation::Effect(EffectOperation::ToolAttempt { .. })
            )
        {
            match placement.journal_point() {
                None => {
                    let error = self.injected_store_error();
                    return self
                        .control
                        .around(operation, async move { Err(error) })
                        .await;
                }
                Some(point) => {
                    let faults = self
                        .journal_faults
                        .clone()
                        .unwrap_or_else(|| panic!("{placement:?} requires a journaled controller"));
                    faults.fail_next(point, envelope.invocation.replay_key());
                }
            }
        }
        if !counts_external_execution {
            return self
                .control
                .around(operation, self.inner.execute_effect(envelope, executor))
                .await;
        }
        let executions = Arc::clone(&self.executions);
        let wrapped = RuntimeEffectLocalExecutor::testing(move |envelope| {
            let executions = Arc::clone(&executions);
            async move {
                executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                executor.execute(envelope).await
            }
        });
        self.control
            .around(operation, self.inner.execute_effect(envelope, wrapped))
            .await
    }

    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        let operation = TurnSeamOperation::Effect(EffectOperation::GroupOpen {
            children: group.children().len(),
        });
        self.control
            .around(operation, self.inner.open_effect_group(group))
            .await
    }

    fn register_group_executors(
        &self,
        executors: Arc<dyn lash_core::GroupExecutors>,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner.register_group_executors(executors)
    }

    fn native_effect_groups_substrate(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        self.inner.native_effect_groups_substrate()
    }

    /// The group child's bound controller is the substrate's, but its seam
    /// traffic is still the turn's: a child's tool attempts cross this seam
    /// on every tier, not only where the host happens to build the child's
    /// controller over it.
    fn group_child_scoped_controller(
        &self,
        admitted: crate::AdmittedScope,
        binding: crate::GroupChildBinding,
    ) -> Result<Option<crate::ScopedEffectController<'static>>, crate::RuntimeError> {
        let Some(bound) = self
            .inner
            .group_child_scoped_controller(admitted, binding)?
        else {
            return Ok(None);
        };
        let Some(controller) = bound.owned_controller() else {
            return Ok(Some(bound));
        };
        let seam: Arc<dyn RuntimeEffectController> = Arc::new(SeamEffectController {
            inner: controller,
            control: self.control.clone(),
            executions: Arc::clone(&self.executions),
            journal_faults: self.journal_faults.clone(),
        });
        crate::ScopedEffectController::shared(seam, bound.admitted_scope().clone()).map(Some)
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        self.control
            .around_completion(
                TurnSeamOperation::Effect(EffectOperation::GroupSettle),
                self.inner.await_next_settlement(handle, cancel),
            )
            .await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.control
            .around(
                TurnSeamOperation::Effect(EffectOperation::GroupClose),
                self.inner.close_effect_group(handle, disposition),
            )
            .await
    }

    // A rank read is a view on a durable fact, not a turn-seam operation — the
    // golden trace names lifecycle operations, so the read forwards without
    // `around` (FIG-3411 part 2).
    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<
        Option<lash_core::runtime::effect::RankedGroupSettlement>,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner.read_group_settlement(group_key, rank).await
    }

    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner.commit_group_child_final(commit).await
    }

    async fn group_child_drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, lash_core::RuntimeEffectControllerError> {
        self.inner
            .group_child_drain_blocked(group_key, commit_seq)
            .await
    }
}

#[derive(Clone)]
pub(crate) struct CrashAfterCheckpointExecutionController {
    pub(super) inner: Arc<dyn RuntimeEffectController>,
}

/// Persistent Native journals ordinary effects in the backend controller but
/// keeps the three reserved turn-control promises in the session store.
#[derive(Clone)]
pub(crate) struct StoreOwnedTurnControlController {
    pub(super) inner: Arc<dyn RuntimeEffectController>,
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for StoreOwnedTurnControlController {
    async fn prepare_completion_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<crate::CompletionKeyPreparation, crate::RuntimeError> {
        self.inner
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
    ) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &crate::AwaitEventKey,
        resolution: crate::Resolution,
    ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &crate::AwaitEventKey,
    ) -> Result<Option<crate::Resolution>, crate::RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &crate::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<crate::Resolution, crate::RuntimeError> {
        self.inner.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for StoreOwnedTurnControlController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.inner.execute_effect(envelope, executor).await
    }

    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        self.inner.open_effect_group(group).await
    }

    fn register_group_executors(
        &self,
        executors: Arc<dyn lash_core::GroupExecutors>,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner.register_group_executors(executors)
    }

    fn native_effect_groups_substrate(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        self.inner.native_effect_groups_substrate()
    }

    fn group_child_scoped_controller(
        &self,
        admitted: crate::AdmittedScope,
        binding: crate::GroupChildBinding,
    ) -> Result<Option<crate::ScopedEffectController<'static>>, crate::RuntimeError> {
        self.inner.group_child_scoped_controller(admitted, binding)
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        self.inner.await_next_settlement(handle, cancel).await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner.close_effect_group(handle, disposition).await
    }

    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<
        Option<lash_core::runtime::effect::RankedGroupSettlement>,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner.read_group_settlement(group_key, rank).await
    }

    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner.commit_group_child_final(commit).await
    }

    async fn group_child_drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, lash_core::RuntimeEffectControllerError> {
        self.inner
            .group_child_drain_blocked(group_key, commit_seq)
            .await
    }
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for CrashAfterCheckpointExecutionController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.await_event_authority_binding_id()
    }

    async fn prepare_completion_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<crate::CompletionKeyPreparation, crate::RuntimeError> {
        self.inner
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }

    async fn await_event_key(
        &self,
        scope: &crate::ExecutionScope,
        wait: crate::AwaitEventWaitIdentity,
    ) -> Result<crate::AwaitEventKey, crate::RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &crate::AwaitEventKey,
        resolution: crate::Resolution,
    ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &crate::AwaitEventKey,
    ) -> Result<Option<crate::Resolution>, crate::RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &crate::AwaitEventKey,
        cancel: tokio_util::sync::CancellationToken,
        deadline: Option<std::time::Instant>,
    ) -> Result<crate::Resolution, crate::RuntimeError> {
        self.inner.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), crate::RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for CrashAfterCheckpointExecutionController {
    fn effect_journaling(&self) -> crate::EffectJournaling {
        self.inner.effect_journaling()
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if !matches!(
            &envelope.command,
            crate::RuntimeEffectCommand::Checkpoint {
                // The recorded AfterWork outcome supplies predecessor
                // authority. Crashing after BeforeCompletion executes forces
                // recovery to reclaim and journal its replacement authority.
                checkpoint: crate::CheckpointKind::BeforeCompletion,
            }
        ) {
            return self.inner.execute_effect(envelope, executor).await;
        }
        let crash_after_execution =
            RuntimeEffectLocalExecutor::testing(move |envelope| async move {
                let _outcome = executor.execute(envelope).await;
                std::process::exit(86);
            });
        self.inner
            .execute_effect(envelope, crash_after_execution)
            .await
    }

    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        self.inner.open_effect_group(group).await
    }

    fn register_group_executors(
        &self,
        executors: Arc<dyn lash_core::GroupExecutors>,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner.register_group_executors(executors)
    }

    fn native_effect_groups_substrate(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        self.inner.native_effect_groups_substrate()
    }

    fn group_child_scoped_controller(
        &self,
        admitted: crate::AdmittedScope,
        binding: crate::GroupChildBinding,
    ) -> Result<Option<crate::ScopedEffectController<'static>>, crate::RuntimeError> {
        self.inner.group_child_scoped_controller(admitted, binding)
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        self.inner.await_next_settlement(handle, cancel).await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner.close_effect_group(handle, disposition).await
    }

    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<
        Option<lash_core::runtime::effect::RankedGroupSettlement>,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner.read_group_settlement(group_key, rank).await
    }

    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner.commit_group_child_final(commit).await
    }

    async fn group_child_drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, lash_core::RuntimeEffectControllerError> {
        self.inner
            .group_child_drain_blocked(group_key, commit_seq)
            .await
    }
}
