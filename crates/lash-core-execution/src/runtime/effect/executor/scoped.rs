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
    /// rescope onto a turn or any other unpinned scope. The one production
    /// rescope — a process-origin child session turn narrowing to the child
    /// session's own turn scope
    /// (`crates/lash-core/src/runtime/session_manager/session_init.rs`) —
    /// starts from a turn scope, which a process pin cannot match.
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

    /// The borrowed controller bound to `admitted` by its own engine
    /// ([`RuntimeEffectController::scoped_for`]): how an in-process drive
    /// keeps a step under another scope on the handler controller a host
    /// lent it. `None` when the controller is not borrowed, or its engine
    /// binds no scope of its own. Like [`rescope`](Self::rescope), it never
    /// repins a process incarnation.
    pub fn engine_scoped(
        &self,
        admitted: AdmittedScope,
    ) -> Option<Result<ScopedEffectController<'run>, RuntimeError>> {
        let ScopedEffectControllerInner::Borrowed(controller) = &self.controller else {
            return None;
        };
        if let Some(target) = admitted.process_ref()
            && self.admitted.process_ref() != Some(target)
        {
            return Some(Err(RuntimeError::new(
                crate::RuntimeErrorCode::ExecutionScopeAdmissionRefused,
                format!(
                    "cannot bind {existing} onto process incarnation {target}: a scoped controller carries its admission and is never repinned",
                    existing = self.admitted.scope().id(),
                ),
            )));
        }
        controller.scoped_for(admitted)
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
            }),
        }
    }
}
