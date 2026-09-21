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
    /// This controller, bound to `admitted` instead; it lives as long as this one does.
    fn for_scope<'a>(&self, admitted: AdmittedScope) -> Arc<dyn ScopeBoundController + 'a>
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

/// Scoped low-level controller plus the admitted execution scope it is
/// serving.
#[derive(Clone)]
pub struct ScopedEffectController<'run> {
    pub(in crate::runtime::effect::executor) controller: ScopedEffectControllerInner<'run>,
    /// The scope this controller serves plus, when it is a process, the
    /// store-minted incarnation it was admitted under.
    ///
    /// [`ExecutionScope::Process`] carries the reusable process *name* and
    /// nothing else, which ADR 0099 §1 names as the gap: the opener that owns
    /// durable work is the name bound to one incarnation, and "a retired or
    /// mismatched incarnation is refused, never rebound to the current process
    /// carrying the same name". Keeping the pair in one [`AdmittedScope`]
    /// means a process-scoped controller always carries its incarnation —
    /// the half-admitted shape is unconstructible, so any execution running
    /// under this controller can name its opener
    /// ([`Self::admitted_process`]).
    pub(in crate::runtime::effect::executor) admitted: AdmittedScope,
}

impl<'run> ScopedEffectController<'run> {
    /// Returns the execution scope this controller has admitted.
    pub fn execution_scope(&self) -> &ExecutionScope {
        self.admitted.scope()
    }

    /// Returns the admitted scope this controller serves: the execution scope
    /// plus, when it is a process, the incarnation it was admitted under.
    pub fn admitted_scope(&self) -> &AdmittedScope {
        &self.admitted
    }

    /// The process incarnation this controller's scope was admitted under.
    ///
    /// `None` for every non-process scope. A `Process` scope always carries
    /// `Some`: [`AdmittedScope`] makes the unpinned shape unconstructible, so
    /// `None` here is "not a process", never "not yet bound" (ADR 0099 §1).
    pub fn admitted_process(&self) -> Option<&crate::ProcessRef> {
        self.admitted.process_ref()
    }

    /// Validates the admitted scope and binds a borrowed controller for
    /// effect-host implementors; invalid or empty scope identities are
    /// rejected before execution.
    pub fn borrowed(
        controller: &'run dyn RuntimeEffectController,
        admitted: AdmittedScope,
    ) -> Result<Self, RuntimeError> {
        admitted.scope().validate()?;
        Ok(Self {
            controller: ScopedEffectControllerInner::Borrowed(controller),
            admitted,
        })
    }

    /// Validates the admitted scope and binds an owned controller for
    /// effect-host implementors that must move the scoped host across an
    /// asynchronous boundary.
    pub fn shared(
        controller: Arc<dyn RuntimeEffectController>,
        admitted: AdmittedScope,
    ) -> Result<Self, RuntimeError> {
        admitted.scope().validate()?;
        Ok(Self {
            controller: ScopedEffectControllerInner::Shared(controller),
            admitted,
        })
    }

    /// Validates the admitted scope and binds a controller built for that
    /// scope and bounded by the borrow it wraps, for effect-host implementors
    /// whose engine-side controller must know the scope of every effect it
    /// runs.
    pub fn owned(
        controller: Arc<dyn ScopeBoundController + 'run>,
        admitted: AdmittedScope,
    ) -> Result<Self, RuntimeError> {
        admitted.scope().validate()?;
        Ok(Self {
            controller: ScopedEffectControllerInner::Owned(controller),
            admitted,
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
        envelope
            .invocation
            .validate_execution_scope(self.admitted.scope())
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
        self.admitted.scope().id()
    }

    /// Exposes turn id to effect-host implementors while scoping and journaling durable effects.
    /// Returns `None` when no turn id is present.
    pub fn turn_id(&self) -> Option<&TurnId> {
        self.admitted.scope().turn_id()
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
        TurnCancelWait::observing(cancellation, self.admitted.scope().clone())
    }

    pub fn to_static(&self) -> Option<ScopedEffectController<'static>> {
        let ScopedEffectControllerInner::Shared(controller) = &self.controller else {
            return None;
        };
        Some(ScopedEffectController {
            controller: ScopedEffectControllerInner::Shared(Arc::clone(controller)),
            admitted: self.admitted.clone(),
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

pub mod facade_ops {
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
            self.admitted.scope()
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
                    EffectTaskController::scoped(controller, self.admitted.clone())?;
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

#[cfg(test)]
mod admitted_scope_tests {
    use super::*;
    use crate::{AdmittedScopeError, ProcessIncarnation, ProcessRef};

    fn process_ref(name: &str, incarnation: u64) -> ProcessRef {
        ProcessRef::new(
            name,
            ProcessIncarnation::from_registration_sequence(incarnation),
        )
    }

    fn shared_controller() -> Arc<dyn RuntimeEffectController> {
        Arc::new(crate::NativeRuntimeEffectController::default())
    }

    /// A process scope alone cannot name its opener: the incarnation the
    /// admission authority bound is part of the controller's construction, so
    /// the half-admitted shape has no constructor to build (ADR 0099 §1).
    #[test]
    fn a_process_scoped_controller_carries_the_admitted_incarnation() {
        let scoped = ScopedEffectController::shared(
            shared_controller(),
            AdmittedScope::process(process_ref("worker", 4)),
        )
        .expect("process scope");

        assert_eq!(scoped.admitted_process(), Some(&process_ref("worker", 4)));
        assert_eq!(scoped.execution_scope(), &ExecutionScope::process("worker"));
    }

    /// The checked constructor refuses the two ways to lie about a process:
    /// no incarnation at all, and an incarnation of another process. Both are
    /// `AdmittedScope` construction errors — they never reach the controller.
    #[test]
    fn a_process_scope_must_name_its_own_incarnation() {
        assert!(matches!(
            AdmittedScope::new(ExecutionScope::process("worker"), None),
            Err(AdmittedScopeError::ProcessIncarnationMissing { .. })
        ));
        assert!(matches!(
            AdmittedScope::new(
                ExecutionScope::process("worker"),
                Some(process_ref("indexer", 1)),
            ),
            Err(AdmittedScopeError::ProcessPinMismatch { .. })
        ));
    }

    /// A turn scope has an opener of its own and never borrows a process's.
    #[test]
    fn a_turn_scope_refuses_an_admitted_incarnation() {
        assert!(matches!(
            AdmittedScope::new(
                ExecutionScope::turn("session-1", "turn-1"),
                Some(process_ref("worker", 1)),
            ),
            Err(AdmittedScopeError::NonProcessScopePinned { .. })
        ));
    }

    /// The managed child turn a process backs is spawned onto its own task, so
    /// the binding has to survive the `'static` conversion that spawn requires.
    #[test]
    fn the_admitted_incarnation_survives_the_static_conversion() {
        let scoped = ScopedEffectController::shared(
            shared_controller(),
            AdmittedScope::process(process_ref("worker", 9)),
        )
        .expect("process scope");

        assert_eq!(
            scoped
                .to_static()
                .expect("a shared controller is static")
                .admitted_process(),
            Some(&process_ref("worker", 9))
        );
        assert_eq!(
            scoped
                .into_static()
                .map_err(|_| "a shared controller is static")
                .expect("static")
                .admitted_process(),
            Some(&process_ref("worker", 9))
        );
    }

    /// A rescope is a different opener, so it never inherits the incarnation of
    /// the scope it left.
    #[test]
    fn a_rescope_drops_the_admitted_incarnation() {
        let scoped = ScopedEffectController::shared(
            shared_controller(),
            AdmittedScope::process(process_ref("worker", 2)),
        )
        .expect("process scope");

        let rescoped = scoped
            .rescope(AdmittedScope::turn("session-1", "turn-1"))
            .expect("rescope onto a turn");

        assert!(rescoped.admitted_process().is_none());
    }

    /// The one production rescope onto another process — the process runner
    /// rebinding its controller onto the incarnation the authority CAS
    /// admitted — swaps the pin rather than carrying the stale read over.
    #[test]
    fn a_rescope_onto_another_incarnation_rebinds_the_pin() {
        let scoped = ScopedEffectController::shared(
            shared_controller(),
            AdmittedScope::process(process_ref("worker", 2)),
        )
        .expect("process scope");

        let rescoped = scoped
            .rescope(AdmittedScope::process(process_ref("worker", 7)))
            .expect("rescope onto the admitted incarnation");

        assert_eq!(rescoped.admitted_process(), Some(&process_ref("worker", 7)));
        assert_eq!(
            rescoped.execution_scope(),
            &ExecutionScope::process("worker")
        );
    }
}
