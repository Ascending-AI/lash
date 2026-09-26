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
}

/// Runtime-internal handle for effect-controller references carried through
/// per-turn execution contexts.
#[derive(Clone)]
pub enum RuntimeEffectControllerHandle<'run> {
    Borrowed(ScopedEffectController<'run>),
    #[cfg(any(test, feature = "testing"))]
    Shared {
        controller: Arc<dyn RuntimeEffectController>,
        admitted: AdmittedScope,
    },
}

impl<'run> RuntimeEffectControllerHandle<'run> {
    pub fn borrowed(scoped: ScopedEffectController<'run>) -> Self {
        Self::Borrowed(scoped)
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn shared(controller: Arc<dyn RuntimeEffectController>) -> Self {
        Self::Shared {
            controller,
            admitted: AdmittedScope::runtime_operation("test-runtime-effect-controller"),
        }
    }

    pub fn controller(&self) -> &dyn RuntimeEffectController {
        match self {
            Self::Borrowed(scoped) => scoped.controller(),
            #[cfg(any(test, feature = "testing"))]
            Self::Shared { controller, .. } => controller.as_ref(),
        }
    }

    #[cfg_attr(
        any(test, feature = "testing"),
        expect(
            clippy::expect_used,
            reason = "the shared handle was built from a valid admitted scope"
        )
    )]
    pub fn scoped(&self) -> ScopedEffectController<'_> {
        match self {
            Self::Borrowed(scoped) => scoped.clone(),
            #[cfg(any(test, feature = "testing"))]
            Self::Shared {
                controller,
                admitted,
            } => ScopedEffectController::shared(Arc::clone(controller), admitted.clone())
                .expect("runtime effect controller handle carries a valid scope"),
        }
    }

    pub fn clone_scoped(&self) -> RuntimeEffectControllerHandle<'run> {
        self.clone()
    }

    /// This handle serving one replayed language command: every journal
    /// write made through it asks `guard` first (FIG-3586).
    #[cfg_attr(
        any(test, feature = "testing"),
        expect(
            clippy::expect_used,
            reason = "the shared handle was built from a valid admitted scope"
        )
    )]
    pub fn with_journal_guard(
        &self,
        guard: Arc<super::CommandJournalGuard>,
    ) -> RuntimeEffectControllerHandle<'run> {
        match self {
            Self::Borrowed(scoped) => Self::Borrowed(scoped.clone().with_journal_guard(guard)),
            #[cfg(any(test, feature = "testing"))]
            Self::Shared {
                controller,
                admitted,
            } => Self::Borrowed(
                ScopedEffectController::shared(Arc::clone(controller), admitted.clone())
                    .expect("runtime effect controller handle carries a valid scope")
                    .with_journal_guard(guard),
            ),
        }
    }

    pub(crate) fn to_static(&self) -> Option<RuntimeEffectControllerHandle<'static>> {
        match self {
            Self::Borrowed(scoped) => scoped
                .to_static()
                .map(RuntimeEffectControllerHandle::Borrowed),
            #[cfg(any(test, feature = "testing"))]
            Self::Shared {
                controller,
                admitted,
            } => Some(RuntimeEffectControllerHandle::Shared {
                controller: Arc::clone(controller),
                admitted: admitted.clone(),
            }),
        }
    }
}
