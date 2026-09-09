use super::*;

impl<'run> ScopedEffectController<'run> {
    pub(crate) fn rescope(
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

    pub(crate) fn into_static(self) -> Result<ScopedEffectController<'static>, Self> {
        match self.controller {
            ScopedEffectControllerInner::Borrowed(_) | ScopedEffectControllerInner::Owned(_) => {
                Err(self)
            }
            ScopedEffectControllerInner::Shared(controller) => Ok(ScopedEffectController {
                controller: ScopedEffectControllerInner::Shared(controller),
                scope: self.scope,
            }),
        }
    }
}

impl<'run> RuntimeEffectControllerHandle<'run> {
    pub(crate) fn scoped_for(
        &self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        match self {
            Self::Borrowed(scoped) => scoped.rescope(scope),
            #[cfg(any(test, feature = "testing"))]
            Self::Shared { controller, .. } => {
                ScopedEffectController::shared(Arc::clone(controller), scope)
            }
        }
    }
}
