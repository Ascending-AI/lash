//! The crash-matrix seam controllers: recording and crash-injecting
//! doubles that wrap the fixture's real controller and forward
//! every group operation to it, so durable effect-group state stays in the
//! substrate under test.

use std::sync::Arc;

use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;

use super::{
    CrashPlacement, EffectOperation, ErrorReturnPlacement, SeamControl, TurnSeamOperation,
    turn_control_resolution_operation,
};
use crate::{
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeEffectEnvelope,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
};

/// The crash matrix's effect seam: an [`EffectLayer`](crate::testing::EffectLayer)
/// that records the turn's effect, group and turn-control operations on its
/// [`SeamControl`], counts external executions, and crashes or fails them where
/// the control is armed. Every group operation lands on the controller it
/// layers, so durable effect-group state stays in the substrate under test.
///
/// [`SeamLayer::over`] layers a controller a law holds;
/// [`SeamLayer::over_scoped`] layers the controller a tier's turn runner lends,
/// which on Restate is borrowed from the turn's handler.
#[derive(Clone)]
pub(crate) struct SeamLayer {
    pub(super) control: SeamControl,
    pub(super) executions: Arc<std::sync::atomic::AtomicUsize>,
    /// The layered controller's journal fault injector, when it is a
    /// journaled controller exposing one (FIG-3524).
    pub(super) journal_faults:
        Option<lash_core::facade_support::effect_replay_driver::EffectJournalFaults>,
}

impl SeamLayer {
    /// `inner` behind this seam.
    pub(crate) fn over(
        self,
        inner: Arc<dyn RuntimeEffectController>,
    ) -> Arc<dyn RuntimeEffectController> {
        crate::testing::LayeredEffectHost::layer_controller(inner, Arc::new(self))
    }

    /// The controller a turn runner lent, behind this seam for as long as the
    /// runner's borrow lives.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the runner lent a validated scope"
    )]
    pub(crate) fn over_scoped<'run>(
        self,
        scoped: crate::ScopedEffectController<'run>,
    ) -> crate::ScopedEffectController<'run> {
        crate::testing::LayeredEffectHost::layer_scoped(scoped, Arc::new(self))
            .expect("layer the lent controller behind the crash seam")
    }

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

/// The refusal a controller answers once the turn's session is deleted under
/// it (FIG-3630): the store's `SessionDeleted`, carrying its cause.
fn session_retirement_refusal(envelope: &RuntimeEffectEnvelope) -> RuntimeEffectControllerError {
    let session_id = envelope
        .invocation
        .execution_scope()
        .session_id()
        .cloned()
        .unwrap_or_else(|| panic!("the scripted tool attempt runs under a session scope"));
    crate::StoreError::SessionDeleted { session_id }.into()
}

#[async_trait::async_trait]
impl crate::testing::EffectLayer for SeamLayer {
    async fn execute_effect(
        &self,
        inner: &dyn RuntimeEffectController,
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
            crate::RuntimeEffectCommand::AcceptTurnInput { .. } => {
                Some((EffectOperation::AcceptTurnInput, true))
            }
            _ => None,
        };
        let Some((operation, counts_external_execution)) = operation else {
            if self.control.armed_error_return()
                == Some(ErrorReturnPlacement::StartGatePeekFinalize)
                && matches!(
                    envelope.command,
                    crate::RuntimeEffectCommand::PeekAwaitEvent { .. }
                )
                && envelope.invocation.effect_id()
                    == lash_core::testing::conformance_support::TurnCancelPeekIdentity::StartGate
                        .causal_identity()
            {
                // FIG-3647: fail the start-gate peek's journal finalize once;
                // the peek crosses the seam so the oracle can place it.
                self.journal_faults
                    .clone()
                    .unwrap_or_else(|| panic!("the start-gate placement requires a journaled controller"))
                    .fail_next(
                        lash_core::facade_support::effect_replay_driver::EffectJournalFaultPoint::Finalize,
                        envelope.invocation.replay_key(),
                    );
                return self
                    .control
                    .around(
                        TurnSeamOperation::Effect(EffectOperation::StartGatePeek),
                        inner.execute_effect(envelope, executor),
                    )
                    .await;
            }
            return inner.execute_effect(envelope, executor).await;
        };
        let operation = TurnSeamOperation::Effect(operation);
        // FIG-3524: an armed error-return makes the first tool attempt that
        // reaches the seam fail once — at the seam itself, or inside its
        // journal claim/finalize/renew — instead of crashing the task.
        if matches!(
            operation,
            TurnSeamOperation::Effect(EffectOperation::ToolAttempt { .. })
        ) && let Some(placement) = self.control.take_tool_attempt_error_return()
        {
            match placement.journal_point() {
                None => {
                    let error = match placement {
                        ErrorReturnPlacement::ToolAttemptSessionRetirement => {
                            session_retirement_refusal(&envelope)
                        }
                        _ => self.injected_store_error(),
                    };
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
                .around(operation, inner.execute_effect(envelope, executor))
                .await;
        }
        let executions = Arc::clone(&self.executions);
        let control = self.control.clone();
        let wrapped_operation = operation.clone();
        let wrapped = RuntimeEffectLocalExecutor::testing(move |envelope| {
            let executions = Arc::clone(&executions);
            async move {
                executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let outcome = executor.execute(envelope).await;
                // A tool marks its own external effect; an acceptance's
                // external effect is the store write its executor just made.
                if wrapped_operation == TurnSeamOperation::Effect(EffectOperation::AcceptTurnInput)
                    && control.matches(
                        &wrapped_operation,
                        CrashPlacement::AfterExternalEffectBeforeOutcome,
                    )
                {
                    control.stop_here().await;
                }
                outcome
            }
        });
        self.control
            .around(operation, inner.execute_effect(envelope, wrapped))
            .await
    }

    async fn open_effect_group(
        &self,
        inner: &dyn RuntimeEffectController,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        let operation = TurnSeamOperation::Effect(EffectOperation::GroupOpen {
            children: group.children().len(),
        });
        self.control
            .around(operation, inner.open_effect_group(group))
            .await
    }

    async fn await_next_settlement(
        &self,
        inner: &dyn RuntimeEffectController,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::TurnCancelWait,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        self.control
            .around_completion(
                TurnSeamOperation::Effect(EffectOperation::GroupSettle),
                inner.await_next_settlement(handle, cancel),
            )
            .await
    }

    async fn close_effect_group(
        &self,
        inner: &dyn RuntimeEffectController,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.control
            .around(
                TurnSeamOperation::Effect(EffectOperation::GroupClose),
                inner.close_effect_group(handle, disposition),
            )
            .await
    }

    async fn resolve_await_event(
        &self,
        inner: &dyn crate::AwaitEventResolver,
        key: &crate::AwaitEventKey,
        resolution: crate::Resolution,
    ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
        match turn_control_resolution_operation(key) {
            Some(operation) => {
                self.control
                    .around(operation, inner.resolve_await_event(key, resolution))
                    .await
            }
            None => inner.resolve_await_event(key, resolution).await,
        }
    }
}

/// The one layered host a runner-driven crash law builds all its runtimes on.
///
/// A runtime installs its tool-child host on the tier's host get-or-init, and
/// that tool-child host holds the effect host it routes group children through
/// weakly. So the first runtime's layered host routes every later runtime's
/// children, and a law that layered the tier's host afresh for each execution
/// would strand them once that first layered host dropped. The law layers the
/// tier's host once, holds it for its whole run, and points the layer at the
/// seam of the execution that runs now ([`LawSeamHost::route_to`]).
#[derive(Clone)]
pub(crate) struct LawSeamHost {
    host: Arc<dyn crate::EffectHost>,
    current: Arc<std::sync::Mutex<Option<SeamLayer>>>,
}

impl LawSeamHost {
    /// `host` layered once for a law's lifetime, routing to no seam yet.
    pub(crate) fn over(host: Arc<dyn crate::EffectHost>) -> Self {
        let current = Arc::new(std::sync::Mutex::new(None));
        let layer = Arc::new(RoutedSeamLayer {
            current: Arc::clone(&current),
        });
        Self {
            host: Arc::new(crate::testing::LayeredEffectHost::new(host, layer)),
            current,
        }
    }

    /// The layered host a law's runtimes run on.
    pub(crate) fn host(&self) -> Arc<dyn crate::EffectHost> {
        Arc::clone(&self.host)
    }

    /// Routes the host's layer to `seam` from now on.
    pub(crate) fn route_to(&self, seam: &SeamLayer) {
        *self.current.lock_recover() = Some(seam.clone());
    }
}

/// The layer of a [`LawSeamHost`]: the seam of the execution that runs now,
/// or a pass-through before any execution routed one.
struct RoutedSeamLayer {
    current: Arc<std::sync::Mutex<Option<SeamLayer>>>,
}

impl RoutedSeamLayer {
    fn seam(&self) -> Option<SeamLayer> {
        self.current.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl crate::testing::EffectLayer for RoutedSeamLayer {
    async fn execute_effect(
        &self,
        inner: &dyn RuntimeEffectController,
        envelope: RuntimeEffectEnvelope,
        executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let Some(seam) = self.seam() else {
            return inner.execute_effect(envelope, executor).await;
        };
        // The host's layer carries a group child's effects, which the engine
        // runs apart from the turn's own execution. When the law's crash
        // kills the process running the turn, the child's execution dies with
        // it: the call is dropped where it stands and the execution ends in a
        // live fault, which the engine retries as it recovers any execution
        // its process lost.
        let control = seam.control.clone();
        tokio::select! {
            biased;
            () = control.process_crash() => Err(RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RuntimeStore,
                "the group child's execution died with the crashed process",
            )),
            outcome = seam.execute_effect(inner, envelope, executor) => outcome,
        }
    }

    async fn open_effect_group(
        &self,
        inner: &dyn RuntimeEffectController,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        match self.seam() {
            Some(seam) => seam.open_effect_group(inner, group).await,
            None => inner.open_effect_group(group).await,
        }
    }

    async fn await_next_settlement(
        &self,
        inner: &dyn RuntimeEffectController,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::TurnCancelWait,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        match self.seam() {
            Some(seam) => seam.await_next_settlement(inner, handle, cancel).await,
            None => inner.await_next_settlement(handle, cancel).await,
        }
    }

    async fn close_effect_group(
        &self,
        inner: &dyn RuntimeEffectController,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        match self.seam() {
            Some(seam) => seam.close_effect_group(inner, handle, disposition).await,
            None => inner.close_effect_group(handle, disposition).await,
        }
    }

    async fn resolve_await_event(
        &self,
        inner: &dyn crate::AwaitEventResolver,
        key: &crate::AwaitEventKey,
        resolution: crate::Resolution,
    ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
        match self.seam() {
            Some(seam) => seam.resolve_await_event(inner, key, resolution).await,
            None => inner.resolve_await_event(key, resolution).await,
        }
    }
}

#[derive(Clone)]
pub(crate) struct CrashAfterCheckpointExecutionController {
    pub(super) inner: Arc<dyn RuntimeEffectController>,
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
    async fn drive_independent_effect_work<'work>(
        &self,
        work: Vec<crate::IndependentEffectWork<'work>>,
    ) {
        self.inner.drive_independent_effect_work(work).await;
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
        cancel: lash_core::TurnCancelWait,
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

    async fn await_group_child_drain_admission(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner
            .await_group_child_drain_admission(group_key, commit_seq)
            .await
    }
}
