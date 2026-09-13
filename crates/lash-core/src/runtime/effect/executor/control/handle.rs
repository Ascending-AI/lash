use super::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SegmentProgress {
    pub effects_executed: u64,
    pub journaled_bytes_estimate: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryReason {
    JournalBudget,
    DurationCap,
}

/// Runtime-internal handle for effect-controller references carried through
/// per-turn execution contexts.
#[derive(Clone)]
pub(crate) enum RuntimeEffectControllerHandle<'run> {
    Borrowed(ScopedEffectController<'run>),
    #[cfg(any(test, feature = "testing"))]
    Shared {
        controller: Arc<dyn RuntimeEffectController>,
        scope: ExecutionScope,
    },
}

impl<'run> RuntimeEffectControllerHandle<'run> {
    pub(crate) fn borrowed(scoped: ScopedEffectController<'run>) -> Self {
        Self::Borrowed(scoped)
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn shared(controller: Arc<dyn RuntimeEffectController>) -> Self {
        Self::Shared {
            controller,
            scope: ExecutionScope::runtime_operation("test-runtime-effect-controller"),
        }
    }

    pub(crate) fn controller(&self) -> &dyn RuntimeEffectController {
        match self {
            Self::Borrowed(scoped) => scoped.controller(),
            #[cfg(any(test, feature = "testing"))]
            Self::Shared { controller, .. } => controller.as_ref(),
        }
    }

    pub(crate) fn scoped(&self) -> ScopedEffectController<'_> {
        match self {
            Self::Borrowed(scoped) => scoped.clone(),
            #[cfg(any(test, feature = "testing"))]
            Self::Shared { controller, scope } => {
                ScopedEffectController::shared(Arc::clone(controller), scope.clone())
                    .expect("runtime effect controller handle carries a valid scope")
            }
        }
    }

    pub(crate) fn clone_scoped(&self) -> RuntimeEffectControllerHandle<'run> {
        self.clone()
    }

    pub(crate) fn to_static(&self) -> Option<RuntimeEffectControllerHandle<'static>> {
        match self {
            Self::Borrowed(scoped) => scoped
                .to_static()
                .map(RuntimeEffectControllerHandle::Borrowed),
            #[cfg(any(test, feature = "testing"))]
            Self::Shared { controller, scope } => Some(RuntimeEffectControllerHandle::Shared {
                controller: Arc::clone(controller),
                scope: scope.clone(),
            }),
        }
    }
}
