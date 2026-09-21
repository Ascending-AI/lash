use super::*;

impl<'run> ScopedEffectController<'run> {
    /// This controller bound to another scope.
    ///
    /// The admitted process ref is deliberately *not* carried over: it names
    /// the incarnation of the scope being left, and the new scope is a
    /// different opener (ADR 0099 §1). The one production rescope — a managed
    /// turn narrowing a session scope to its own turn scope
    /// (`crates/lash-core/src/runtime/session_manager/turns.rs`) — never starts
    /// from a process scope, which it passes through untouched.
    pub fn rescope(
        &self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        match &self.controller {
            ScopedEffectControllerInner::Borrowed(controller) => {
                ScopedEffectController::borrowed(*controller, scope)
            }
            ScopedEffectControllerInner::Shared(controller) => {
                ScopedEffectController::shared(Arc::clone(controller), scope)
            }
            ScopedEffectControllerInner::Owned(controller) => {
                ScopedEffectController::owned(controller.for_scope(scope.clone()), scope)
            }
        }
    }

    pub fn into_static(self) -> Result<ScopedEffectController<'static>, Self> {
        match self.controller {
            ScopedEffectControllerInner::Borrowed(_) | ScopedEffectControllerInner::Owned(_) => {
                Err(self)
            }
            ScopedEffectControllerInner::Shared(controller) => Ok(ScopedEffectController {
                controller: ScopedEffectControllerInner::Shared(controller),
                scope: self.scope,
                admitted_process: self.admitted_process,
            }),
        }
    }
}
