use super::*;

impl<'run> ScopedEffectController<'run> {
    /// This controller bound to another admitted scope.
    ///
    /// The admitted process ref is deliberately *not* carried over: it names
    /// the incarnation of the scope being left, and the new scope is a
    /// different opener (ADR 0099 §1). The caller passes the complete admitted
    /// pair — the process runner's rebind onto the incarnation its authority
    /// CAS admitted (`crates/lash-core/src/runtime/session_manager/process_runners/runner.rs`)
    /// works through this, as does a managed turn narrowing a session scope to
    /// its own turn scope.
    pub fn rescope(
        &self,
        admitted: AdmittedScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        match &self.controller {
            ScopedEffectControllerInner::Borrowed(controller) => {
                ScopedEffectController::borrowed(*controller, admitted)
            }
            ScopedEffectControllerInner::Shared(controller) => {
                ScopedEffectController::shared(Arc::clone(controller), admitted)
            }
            ScopedEffectControllerInner::Owned(controller) => {
                ScopedEffectController::owned(controller.for_scope(admitted.clone()), admitted)
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
                admitted: self.admitted,
            }),
        }
    }
}
