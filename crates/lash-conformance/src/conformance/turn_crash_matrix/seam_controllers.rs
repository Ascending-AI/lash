//! The crash-matrix seam controllers: recording and crash-injecting
//! doubles that wrap the fixture's real controller and forward
//! every operation to it, so durable Run state stays in the
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
/// that records native attempts, input acceptance and turn-control operations
/// on its [`SeamControl`], counts external executions, and crashes or fails them where
/// the control is armed. Every operation lands on the controller it
/// layers, so durable Run state stays in the substrate under test.
///
/// [`SeamLayer::over_scoped`] layers the controller a tier's turn runner lends,
/// which on Restate is borrowed from the turn's handler.
#[derive(Clone)]
pub(crate) struct SeamLayer {
    pub(super) control: SeamControl,
    pub(super) session_id: crate::SessionId,
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

#[async_trait::async_trait]
impl crate::testing::EffectLayer for SeamLayer {
    fn start_run_attempt<'run>(
        &'run self,
        inner: &'run dyn RuntimeEffectController,
        name: String,
        step: crate::tool_dispatch::RunAttemptStep<'run>,
    ) -> crate::tool_dispatch::RunAttemptHandle<'run> {
        // This fixture declares one tool. Native attempts cross the Run's
        // independent X registration. A replay registers the attempt too and
        // reads its recorded result back, so the seam records the attempt
        // only where it runs: a refused registration, or the attempt's body.
        let operation = TurnSeamOperation::Effect(EffectOperation::ToolAttempt {
            name: "trace_effect".to_string(),
        });
        if let Some(placement) = self.control.take_tool_attempt_error_return() {
            self.control.record(operation.clone());
            let error = match placement {
                ErrorReturnPlacement::ToolAttempt => self.injected_store_error(),
                ErrorReturnPlacement::ToolAttemptSessionRetirement => {
                    crate::StoreError::SessionDeleted {
                        session_id: self.session_id.clone(),
                    }
                    .into()
                }
            };
            let key_error = error.clone();
            return crate::tool_dispatch::RunAttemptHandle {
                body: Box::pin(std::future::ready(())),
                result: crate::tool_dispatch::RunSelectable {
                    key: Box::pin(async move { Err(key_error) }),
                    value: Box::pin(async move { Err(error) }),
                },
            };
        }
        let control = self.control.clone();
        let boundary = operation.clone();
        let wrapped = Box::pin(async move {
            control.record(boundary.clone());
            if control.matches(&boundary, CrashPlacement::Boundary) {
                control.stop_here().await;
            }
            step.await
        });
        let crate::tool_dispatch::RunAttemptHandle { body, result } =
            inner.start_run_attempt(name, wrapped);
        let control = self.control.clone();
        crate::tool_dispatch::RunAttemptHandle {
            body,
            result: crate::tool_dispatch::RunSelectable {
                key: result.key,
                value: Box::pin(async move {
                    let outcome = result.value.await;
                    // The result is durable here. A cut inside the opaque
                    // body would instead be the unrecorded external window.
                    if control.matches(&operation, CrashPlacement::InsideCall) {
                        control.stop_here().await;
                    }
                    outcome
                }),
            },
        }
    }

    async fn execute_effect(
        &self,
        inner: &dyn RuntimeEffectController,
        envelope: RuntimeEffectEnvelope,
        executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        if !matches!(
            envelope.command,
            crate::RuntimeEffectCommand::AcceptTurnInput { .. }
        ) {
            return inner.execute_effect(envelope, executor).await;
        }
        let operation = TurnSeamOperation::Effect(EffectOperation::AcceptTurnInput);
        let executions = Arc::clone(&self.executions);
        let control = self.control.clone();
        let wrapped_operation = operation.clone();
        let wrapped = RuntimeEffectLocalExecutor::testing(move |envelope| {
            let executions = Arc::clone(&executions);
            async move {
                executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let outcome = executor.execute(envelope).await;
                if control.matches(
                    &wrapped_operation,
                    CrashPlacement::AfterExternalEffectBeforeOutcome,
                ) {
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
/// The law layers the tier's host once, holds it for its whole run, and points the layer at the
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
        seam.execute_effect(inner, envelope, executor).await
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
