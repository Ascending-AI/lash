use super::*;

impl<'run> ScopedEffectController<'run> {
    /// This controller bound to another admitted scope.
    ///
    /// A rescope changes the claim address, never the admitted process: the
    /// only process target this accepts is the process the controller was
    /// already admitted for, so a non-process controller cannot rescope into
    /// a process controller and a process controller cannot rescope onto
    /// another process. A process controller may rescope onto a turn or any
    /// other non-process scope. The one production
    /// rescope — a process-origin child session turn narrowing to the child
    /// session's own turn scope
    /// (`crates/lash-core/src/runtime/session_manager/session_init.rs`) —
    /// starts from a turn scope, which a process pin cannot match.
    pub fn rescope(
        &self,
        admitted: AdmittedScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        if let Some(target) = admitted.process_id()
            && self.admitted.process_id() != Some(target)
        {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::ExecutionScopeAdmissionRefused,
                format!(
                    "cannot rescope {existing} onto process {target}: a scoped controller carries its admission and is never rebound",
                    existing = self.admitted.scope().id(),
                ),
            ));
        }
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

    /// Whether this controller was built for its scope and can build itself
    /// for another ([`ScopeBoundController`]): a [`rescope`](Self::rescope)
    /// of it keeps every effect under the new scope. A borrowed or shared
    /// controller keeps the scope its engine bound it to, so an effect under
    /// another scope needs a controller of its own.
    pub fn is_scope_bound(&self) -> bool {
        matches!(self.controller, ScopedEffectControllerInner::Owned(_))
    }

    pub fn into_static(self) -> Result<ScopedEffectController<'static>, Self> {
        match self.controller {
            ScopedEffectControllerInner::Borrowed(_) | ScopedEffectControllerInner::Owned(_) => {
                Err(self)
            }
            ScopedEffectControllerInner::Shared(controller) => Ok(ScopedEffectController {
                controller: ScopedEffectControllerInner::Shared(controller),
                admitted: self.admitted,
                journal_guard: self.journal_guard,
                keyless_starts: self.keyless_starts,
            }),
        }
    }
}
