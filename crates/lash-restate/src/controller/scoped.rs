//! The scope-bound views of the in-handler controller.
//!
//! A session or non-process scope gets its view from
//! [`scoped_effect_controller`](RestateRuntimeEffectController::scoped_effect_controller);
//! a process segment gets one only from
//! [`process_segment_controller`](RestateRuntimeEffectController::process_segment_controller),
//! which requires the proof that the segment's start marker committed
//! (FIG-3588).

use std::sync::Arc;

use lash_core::{ExecutionScope, RuntimeError, RuntimeErrorCode, ScopedEffectController};

use super::{RestateControllerContext, RestateRuntimeEffectController, scope_recording};

impl<'ctx, C> RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    /// The controller bound to `scope`. A non-session scope gets a view that
    /// records runtime-operation effects and every group it opens in the
    /// scope's durable-wait index, so a `WhenQuiescent` retirement of the
    /// scope refuses while they are live (FIG-2499); a session scope's
    /// effects complete under Restate's own journal and need no record.
    ///
    /// A process scope is refused here: a process segment's controller comes
    /// only from [`process_segment_controller`](Self::process_segment_controller),
    /// which requires the proof that the segment's start marker committed.
    pub fn scoped_effect_controller<'run>(
        &'run self,
        admitted: lash_core::AdmittedScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        refuse_bare_process_scope(&admitted)?;
        self.recording_controller(admitted)
    }

    /// [`scoped_effect_controller`](Self::scoped_effect_controller) for a
    /// controller its caller owns: the view keeps the controller alive
    /// instead of borrowing it, so it lives as long as the context does.
    pub fn into_scoped_effect_controller(
        self: Arc<Self>,
        admitted: lash_core::AdmittedScope,
    ) -> Result<ScopedEffectController<'ctx>, RuntimeError> {
        refuse_bare_process_scope(&admitted)?;
        admitted.scope().validate()?;
        ScopedEffectController::owned(
            Arc::new(scope_recording::ScopeRecordingController {
                inner: scope_recording::HandlerController::Owned(self),
                admitted: admitted.clone(),
                run_records: Default::default(),
            }),
            admitted,
        )
    }

    /// The controller for one admitted process segment (FIG-3588).
    ///
    /// `started` is the proof, minted only after the segment's start marker
    /// committed, that this execution may run the segment. No other route
    /// yields a controller bound to a process scope, so a segment cannot
    /// dispatch an effect before its marker.
    /// The workflow also registers a journal pin before lending this view;
    /// process quiescence stays conservative until that segment stops issuing
    /// effects, without index calls around each effect (FIG-4849).
    pub fn process_segment_controller<'run>(
        &'run self,
        started: &crate::SegmentStarted,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        self.recording_controller(started.admitted_scope().clone())
    }

    /// The scope already admitted by the Run that sent a realization request.
    /// This handler is the only route besides a started process segment to a
    /// process-scope controller: it executes only the final's protected intents.
    pub(crate) fn realization_controller<'run>(
        &'run self,
        admitted: lash_core::AdmittedScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        self.recording_controller(admitted)
    }

    /// A process-scope controller for a test that executes effects without a
    /// workflow handler, and so without an admitted segment.
    #[cfg(test)]
    pub(crate) fn process_scope_for_test<'run>(
        &'run self,
        admitted: lash_core::AdmittedScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        self.recording_controller(admitted)
    }

    fn recording_controller<'run>(
        &'run self,
        admitted: lash_core::AdmittedScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        admitted.scope().validate()?;
        ScopedEffectController::owned(
            Arc::new(scope_recording::ScopeRecordingController {
                inner: scope_recording::HandlerController::Borrowed(self),
                admitted: admitted.clone(),
                run_records: Default::default(),
            }),
            admitted,
        )
    }
}

/// A process segment's effects are admitted only by its committed start
/// marker (FIG-3588), never by a bare process scope.
fn refuse_bare_process_scope(admitted: &lash_core::AdmittedScope) -> Result<(), RuntimeError> {
    if matches!(admitted.scope(), ExecutionScope::Process { .. }) {
        return Err(RuntimeError::new(
            RuntimeErrorCode::ExecutionScopeAdmissionRefused,
            format!(
                "a process segment's effects are admitted only by its committed start \
                 marker; use process_segment_controller, not a bare {:?}",
                admitted.scope()
            ),
        ));
    }
    Ok(())
}
