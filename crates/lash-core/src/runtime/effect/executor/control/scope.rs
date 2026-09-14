//! `ScopedEffectController`: a controller paired with the execution scope it
//! serves, plus the `ScopeBoundController` re-scoping seam.
//!
//! Split out of `control.rs` verbatim to keep every file in this module under
//! the production file-size budget; no item, signature or path changed.

use super::*;

/// A controller built for one scope that can build itself for another: what
/// an engine-side controller that must know the scope of every effect it runs
/// hands to [`ScopedEffectController::owned`], so the runtime's rescoping (a
/// turn under a process, a child under its parent) keeps the scope exact.
pub trait ScopeBoundController: RuntimeEffectController {
    /// This controller, bound to `scope` instead; it lives as long as this one does.
    fn for_scope<'a>(&self, scope: ExecutionScope) -> Arc<dyn ScopeBoundController + 'a>
    where
        Self: 'a;
}

pub(in crate::runtime::effect::executor) enum ScopedEffectControllerInner<'run> {
    Borrowed(&'run dyn RuntimeEffectController),
    Shared(Arc<dyn RuntimeEffectController>),
    /// A controller built for this scope alone and living no longer than the
    /// borrow it wraps: an engine-side controller that records the scope's
    /// executing effects under the scope (FIG-2499).
    Owned(Arc<dyn ScopeBoundController + 'run>),
}

impl Clone for ScopedEffectControllerInner<'_> {
    fn clone(&self) -> Self {
        match self {
            Self::Borrowed(controller) => Self::Borrowed(*controller),
            Self::Shared(controller) => Self::Shared(Arc::clone(controller)),
            Self::Owned(controller) => Self::Owned(Arc::clone(controller)),
        }
    }
}

/// Scoped low-level controller plus the semantic execution scope it is serving.
#[derive(Clone)]
pub struct ScopedEffectController<'run> {
    pub(in crate::runtime::effect::executor) controller: ScopedEffectControllerInner<'run>,
    pub(in crate::runtime::effect::executor) scope: ExecutionScope,
}

impl<'run> ScopedEffectController<'run> {
    /// Returns the execution scope this controller has admitted.
    pub fn execution_scope(&self) -> &ExecutionScope {
        &self.scope
    }

    /// Validates a scope and binds a borrowed controller for effect-host implementors; invalid or
    /// empty scope identities are rejected before execution.
    pub fn borrowed(
        controller: &'run dyn RuntimeEffectController,
        scope: ExecutionScope,
    ) -> Result<Self, RuntimeError> {
        scope.validate()?;
        Ok(Self {
            controller: ScopedEffectControllerInner::Borrowed(controller),
            scope,
        })
    }

    /// Validates a scope and binds an owned controller for effect-host implementors that must move
    /// the scoped host across an asynchronous boundary.
    pub fn shared(
        controller: Arc<dyn RuntimeEffectController>,
        scope: ExecutionScope,
    ) -> Result<Self, RuntimeError> {
        scope.validate()?;
        Ok(Self {
            controller: ScopedEffectControllerInner::Shared(controller),
            scope,
        })
    }

    /// Validates a scope and binds a controller built for that scope and bounded by the borrow it
    /// wraps, for effect-host implementors whose engine-side controller must know the scope of
    /// every effect it runs.
    pub fn owned(
        controller: Arc<dyn ScopeBoundController + 'run>,
        scope: ExecutionScope,
    ) -> Result<Self, RuntimeError> {
        scope.validate()?;
        Ok(Self {
            controller: ScopedEffectControllerInner::Owned(controller),
            scope,
        })
    }

    /// Exposes controller to effect-host implementors while scoping and journaling durable effects.
    pub fn controller(&self) -> &dyn RuntimeEffectController {
        match &self.controller {
            ScopedEffectControllerInner::Borrowed(controller) => *controller,
            ScopedEffectControllerInner::Shared(controller) => controller.as_ref(),
            ScopedEffectControllerInner::Owned(controller) => controller.as_ref(),
        }
    }

    fn validate_envelope_scope(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Result<(), RuntimeEffectControllerError> {
        envelope.invocation.validate_execution_scope(&self.scope)
    }

    /// Executes an effect only after proving that its address belongs to this
    /// controller's admitted scope.
    pub async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.validate_envelope_scope(&envelope)?;
        self.controller()
            .execute_effect(envelope, local_executor)
            .await
    }

    /// Exposes scope id to effect-host implementors while scoping and journaling durable effects.
    pub fn scope_id(&self) -> &str {
        self.scope.id()
    }

    /// Exposes turn id to effect-host implementors while scoping and journaling durable effects.
    /// Returns `None` when no turn id is present.
    pub fn turn_id(&self) -> Option<&TurnId> {
        self.scope.turn_id()
    }

    /// The complete turn-cancel trio for a wait built directly against this
    /// scope, for the wait sites that have no `RuntimeExecutionContext` to ask.
    /// The trio it yields is always observing: a scope alone cannot say whether
    /// the enclosing execution opted out.
    ///
    /// That is exact for the turn-driver and test-dispatch sites that call this
    /// producer: both only run work that observes turn cancellation. Process
    /// bodies instead build one unobserved trio at their execution boundary and
    /// carry it through retry sleeps and deferred-tool awaits whole.
    pub(crate) fn turn_cancel_wait(&self, cancellation: CancellationToken) -> TurnCancelWait {
        TurnCancelWait::observing(cancellation, self.scope.clone())
    }

    pub(crate) fn to_static(&self) -> Option<ScopedEffectController<'static>> {
        let ScopedEffectControllerInner::Shared(controller) = &self.controller else {
            return None;
        };
        Some(ScopedEffectController {
            controller: ScopedEffectControllerInner::Shared(Arc::clone(controller)),
            scope: self.scope.clone(),
        })
    }

    pub fn owned_controller(&self) -> Option<Arc<dyn RuntimeEffectController>> {
        match &self.controller {
            ScopedEffectControllerInner::Shared(controller) => Some(Arc::clone(controller)),
            ScopedEffectControllerInner::Borrowed(_) | ScopedEffectControllerInner::Owned(_) => {
                None
            }
        }
    }
}

pub(crate) mod facade_ops {
    use super::*;

    /// Facade-internal operations for [`ScopedEffectController`].
    ///
    /// This is not integrator surface, carries no stability promise, and exists
    /// only for the `lash` facade. See [ADR 0051](https://github.com/Ascending-AI/lash/blob/main/docs/adr/0051-the-facade-is-the-host-api-core-is-integrator-seams.md).
    #[async_trait::async_trait]
    pub trait ScopedEffectControllerFacadeOps {
        fn execution_scope(&self) -> &ExecutionScope;

        /// Executes one facade-owned process effect while making this controller
        /// available to the local process command itself. Borrowed controllers
        /// are proxied across the process task boundary; shared controllers can
        /// be passed through directly.
        async fn execute_process_effect(
            &self,
            envelope: RuntimeEffectEnvelope,
            local_executor: RuntimeEffectLocalExecutor<'static>,
        ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError>;
    }

    #[async_trait::async_trait]
    impl ScopedEffectControllerFacadeOps for ScopedEffectController<'_> {
        fn execution_scope(&self) -> &ExecutionScope {
            &self.scope
        }

        #[expect(
            clippy::expect_used,
            reason = "the effect-task proxy owns the controller it lends"
        )]
        async fn execute_process_effect(
            &self,
            envelope: RuntimeEffectEnvelope,
            local_executor: RuntimeEffectLocalExecutor<'static>,
        ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
            self.validate_envelope_scope(&envelope)?;
            let controller = self.controller();
            let (owned_controller, task_requests) = if let Some(owned) = self.owned_controller() {
                (owned, None)
            } else {
                let (proxy, requests) =
                    EffectTaskController::scoped(controller, self.execution_scope().clone())?;
                (
                    proxy
                        .owned_controller()
                        .expect("effect-task proxy owns its controller"),
                    Some(requests),
                )
            };
            let local_executor = local_executor.with_process_effect_controller(owned_controller);
            if let Some(task_requests) = task_requests {
                drive_effect_controller_task(
                    controller,
                    self.execution_scope().clone(),
                    envelope,
                    local_executor,
                    task_requests,
                )
                .await
            } else {
                self.execute_effect(envelope, local_executor).await
            }
        }
    }
}
