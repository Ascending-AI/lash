use super::*;

impl<'run> ScopedEffectController<'run> {
    /// This controller bound to another admitted scope.
    ///
    /// A rescope changes the claim address, never the admission: the only
    /// process target this accepts names the same [`ProcessRef`] the
    /// controller was already admitted under, and a non-process controller has
    /// no pin a process target could match, so it cannot rescope into a
    /// process controller. A same-name successor incarnation is refused rather
    /// than rebound — ADR 0099 §1 rules that "a retired or mismatched
    /// incarnation is refused, never rebound to the current process carrying
    /// the same name". Dropping the pin is fine: a process controller may
    /// rescope onto a turn or any other unpinned scope, which is what a
    /// process-scoped child session turn narrowing to its own turn scope does.
    pub fn rescope(
        &self,
        admitted: AdmittedScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        if let Some(target) = admitted.process_ref()
            && self.admitted.process_ref() != Some(target)
        {
            return Err(RuntimeError::new(
                crate::RuntimeErrorCode::ExecutionScopeAdmissionRefused,
                format!(
                    "cannot rescope {existing} onto process incarnation {target}: a scoped controller carries its admission and is never repinned",
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
