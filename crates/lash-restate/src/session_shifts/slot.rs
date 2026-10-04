//! Deployment-local ownership and options for installed session shifts.

use super::*;
use std::sync::{Mutex, Weak};

// ---------------------------------------------------------------------------
// The `SessionShifts` slot
// ---------------------------------------------------------------------------

/// The core's [`SessionShifts`] as a deployment's session handlers see it.
///
/// The endpoint binds `LashSession` and `LashTurn` before any core over the
/// backend exists, so they read the `SessionShifts` from this slot when a shift runs.
/// The core fills it through the engine's
/// [`install_session_shifts`](SessionWorkEngine::install_session_shifts), a
/// get-or-init: while a core keeps its installation, one engine has one
/// answer to what runs its shifts, whichever core was built first.
///
/// The slot holds the installation **weakly**. The `SessionShifts` belongs to the
/// core, which owns the backend this slot lives in; a strong reference back
/// would make the three a cycle no drop ever breaks. An install wraps the
/// `SessionShifts` in an installation, and the core keeps the installation the
/// install returns for as long as it serves shifts. A shift holds the `SessionShifts`
/// it runs on, never the installation, so a shift still in flight when its
/// core is dropped runs to its end on that core's `SessionShifts` without keeping the
/// install live: a core built meanwhile installs its own `SessionShifts` (FIG-4017).
/// A shift that runs while no live installation is held (before the core is
/// built, or after it was dropped) fails its attempt retryably, naming the
/// empty slot, and a later install serves it. Clones share one slot.
#[derive(Clone, Default)]
pub struct RestateSessionShiftsSlot {
    installation: Arc<Mutex<Option<Weak<InstalledSessionShifts>>>>,
    /// The effect budget of a run's invocation, when the engine's
    /// configuration set one
    /// ([`RestateConfig::with_run_effect_budget`](crate::RestateConfig::with_run_effect_budget)).
    run_effect_budget: Option<u64>,
    drain_marks: Option<Arc<dyn lash_core::store::generation_drain::GenerationDrainStore>>,
}

/// A `SessionShifts` as a [`RestateSessionShiftsSlot`] installed it: what its core
/// keeps, and whose life decides whether the install is live. It answers
/// every call with the `SessionShifts` it wraps.
struct InstalledSessionShifts {
    shifts: Arc<dyn SessionShifts>,
}

#[async_trait::async_trait]
impl SessionShifts for InstalledSessionShifts {
    fn owns_reconciliation(&self) -> bool {
        self.shifts.owns_reconciliation()
    }

    fn runs_on(&self, shifts: &dyn SessionShifts) -> bool {
        self.shifts.runs_on(shifts)
    }

    async fn reconcile(
        &self,
        cursor: &lash_core::engine::ReconcileCursor,
        page: std::num::NonZeroUsize,
    ) -> Result<lash_core::engine::ReconcileCursor, lash_core::StoreError> {
        self.shifts.reconcile(cursor, page).await
    }

    fn hold_shift(&self, session: &SessionId) -> lash_core::engine::ShiftHold {
        self.shifts.hold_shift(session)
    }

    async fn admit(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        request: &ShiftRequest,
        admitting_generation: &lash_core::engine::BuildGeneration,
        ordinal: u32,
        draining: Option<&BuildGeneration>,
    ) -> Result<AdmitVerdict, ShiftAbort> {
        self.shifts
            .admit(controller, request, admitting_generation, ordinal, draining)
            .await
    }

    async fn execute_run(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        admitted: Admitted,
    ) -> RunEnd {
        self.shifts.execute_run(controller, admitted).await
    }

    async fn close_run(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        session: &SessionId,
        run: &lash_core::TurnId,
    ) -> Result<(), ShiftAbort> {
        self.shifts.close_run(controller, session, run).await
    }
}

impl RestateSessionShiftsSlot {
    /// An empty slot.
    pub fn new() -> Self {
        Self::default()
    }

    /// This slot, whose runs' invocations run under `budget` effects.
    pub(crate) fn with_run_effect_budget(mut self, budget: Option<u64>) -> Self {
        self.run_effect_budget = budget;
        self
    }

    pub(crate) fn with_generation_drain(
        mut self,
        marks: Arc<dyn lash_core::store::generation_drain::GenerationDrainStore>,
    ) -> Self {
        self.drain_marks = Some(marks);
        self
    }

    /// The options a run's controller journals under.
    pub(super) fn run_options(&self) -> crate::RestateEffectControllerOptions {
        let options = crate::RestateEffectControllerOptions::default()
            .with_generation_drain(self.drain_marks.clone());
        match self.run_effect_budget {
            Some(budget) => options.segment_effect_budget(budget),
            None => options,
        }
    }

    /// Install `shifts` unless a live installation is held already; returns
    /// the installation the slot now serves, which the caller keeps alive for
    /// as long as it serves shifts.
    pub fn install(&self, shifts: Arc<dyn SessionShifts>) -> Arc<dyn SessionShifts> {
        self.install_new(shifts).0
    }

    /// [`Self::install`], also answering whether the slot took `shifts` —
    /// false when a live installation was already held and is kept.
    pub(super) fn install_new(
        &self,
        shifts: Arc<dyn SessionShifts>,
    ) -> (Arc<dyn SessionShifts>, bool) {
        let mut slot = self
            .installation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(live) = slot.as_ref().and_then(Weak::upgrade) {
            return (live, false);
        }
        let installation = Arc::new(InstalledSessionShifts { shifts });
        *slot = Some(Arc::downgrade(&installation));
        (installation, true)
    }

    /// The installed `SessionShifts`, if its installation is still held. The `SessionShifts`
    /// returned does not keep the installation live.
    pub fn installed(&self) -> Option<Arc<dyn SessionShifts>> {
        self.installation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|installation| Arc::clone(&installation.shifts))
    }

    /// The installed `SessionShifts`, or the retryable failure of a shift that ran
    /// while none was installed.
    pub(super) fn shifts_for(&self, handler: &str) -> Result<Arc<dyn SessionShifts>, HandlerError> {
        self.installed().ok_or_else(|| {
            HandlerError::from(std::io::Error::other(format!(
                "{handler}: no SessionShifts is installed on this deployment; \
                 a core over this backend installs it when it is built"
            )))
        })
    }
}

impl std::fmt::Debug for RestateSessionShiftsSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestateSessionShiftsSlot")
            .field("installed", &self.installed().is_some())
            .finish()
    }
}
