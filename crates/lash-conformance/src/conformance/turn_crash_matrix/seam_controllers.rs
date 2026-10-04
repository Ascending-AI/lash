//! The crash-matrix seam controllers: recording and crash-injecting
//! doubles that wrap the fixture's real controller and forward
//! every group operation to it, so durable effect-group state stays in the
//! substrate under test.

use std::sync::Arc;

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
/// [`SeamLayer::over_scoped`] layers the controller a tier's turn runner lends,
/// which on Restate is borrowed from the turn's handler.
#[derive(Clone)]
pub(crate) struct SeamLayer {
    pub(super) control: SeamControl,
    pub(super) executions: Arc<std::sync::atomic::AtomicUsize>,
}

impl SeamLayer {
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

    fn injected_store_error(&self) -> RuntimeEffectControllerError {
        RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::RuntimeStore,
            "injected store error at the tool-attempt seam",
        )
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
                EffectOperation::ToolAttempt {
                    name: call.tool_name.clone(),
                },
                true,
            )),
            crate::RuntimeEffectCommand::AcceptTurnInput { .. } => {
                Some((EffectOperation::AcceptTurnInput, true))
            }
            _ => None,
        };
        let Some((operation, counts_external_execution)) = operation else {
            return inner.execute_effect(envelope, executor).await;
        };
        let operation = TurnSeamOperation::Effect(operation);
        // FIG-3524: an armed error-return makes the first tool attempt that
        // reaches the seam fail once, at the seam itself, instead of
        // crashing the task.
        if matches!(
            operation,
            TurnSeamOperation::Effect(EffectOperation::ToolAttempt { .. })
        ) && let Some(placement) = self.control.take_tool_attempt_error_return()
        {
            let error = match placement {
                ErrorReturnPlacement::ToolAttemptSessionRetirement => {
                    session_retirement_refusal(&envelope)
                }
                ErrorReturnPlacement::ToolAttempt => self.injected_store_error(),
            };
            return self
                .control
                .around(operation, async move { Err(error) })
                .await;
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
