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
    /// records every effect it executes and every group it opens in the
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
        self.recording_controller(admitted)
    }

    /// The controller for one admitted process segment (FIG-3588).
    ///
    /// `started` is the proof, minted only after the segment's start marker
    /// committed, that this execution may run the segment. No other route
    /// yields a controller bound to a process scope, so a segment cannot
    /// dispatch an effect before its marker.
    pub fn process_segment_controller<'run>(
        &'run self,
        started: &crate::SegmentStarted,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        self.recording_controller(started.admitted_scope().clone())
    }

    /// A process-scope controller for a test that drives effects without a
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
                inner: self,
                admitted: admitted.clone(),
                binding: None,
            }),
            admitted,
        )
    }

    /// The group-child-bound twin of
    /// [`scoped_effect_controller`](Self::scoped_effect_controller) (ADR 0099
    /// §4, FIG-3470): every effect the returned controller serves is admitted
    /// through `EffectGroupIndex/admit_semantic` under `binding`'s recorded
    /// child before its `ctx.run`, so a nested admission minted under a
    /// cancel-decided child refuses at the serialized index rather than
    /// executing under ambient authority.
    pub fn scoped_effect_controller_for_group_child<'run>(
        &'run self,
        admitted: lash_core::AdmittedScope,
        binding: lash_core::GroupChildBinding,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        admitted.scope().validate()?;
        ScopedEffectController::owned(
            Arc::new(scope_recording::ScopeRecordingController {
                inner: self,
                admitted: admitted.clone(),
                binding: Some(binding),
            }),
            admitted,
        )
    }
}
