//! A turn's end for the effect groups it opened (ADR 0099 §7; FIG-3397).
//!
//! Every final exit of a turn's effect loop — success, failure and
//! cancellation — closes the groups the turn still holds and finalizes them
//! with the turn's own execution context before the turn's outcome and
//! accounting commit. Worker loss is not an exit: a dead worker never reaches
//! this code, and the redriven turn closes the same groups when *it* exits.
//! A physical segment boundary transfers the logical opener to its
//! continuation; its groups stay live. An abort is not an exit either: a turn
//! that aborts on a live fault or a park records nothing and is redriven, so
//! its groups stay live for the redrive exactly as a dead worker's do.
//!
//! The end runs while the logical Run owner is still live, because
//! finalization's first step may have to run a child no process is running,
//! and a child resolves its executor through its opener's live registration.

use super::*;

/// The opener stays live until the commit decides whether this segment
/// continues its Run or becomes a terminal cancellation.
pub(in crate::runtime) struct OpenerForCommit<'run> {
    pub(in crate::runtime) context: Option<crate::RuntimeExecutionContext<'run>>,
    pub(in crate::runtime) messages: crate::tool_dispatch::CheckpointMessageBuffer,
}

impl OpenerForCommit<'_> {
    pub(in crate::runtime) async fn close(self) -> Result<Vec<crate::PluginMessage>, RuntimeError> {
        if let Some(context) = self.context {
            context
                .close_opener_groups()
                .await
                .map_err(crate::RuntimeEffectControllerError::into_runtime_error)?;
        }
        Ok(self.messages.drain())
    }
}

impl<'run> RuntimeTurnDriver<'run> {
    pub(in crate::runtime) fn take_opener_for_commit(
        &self,
    ) -> Result<OpenerForCommit<'run>, RuntimeError> {
        let context = if self.opener_state.holds_groups() || self.opener_state.holds_tool_run() {
            let context = self
                .execution_context_observing(
                    crate::engine::NullObservationSink::arc(),
                    Arc::new(crate::ChronologicalProjection::default()),
                )
                .map_err(|error| {
                    RuntimeError::new(
                        RuntimeErrorCode::ToolCatalogResolutionFailed,
                        error.to_string(),
                    )
                })?;
            Some(context)
        } else {
            None
        };
        Ok(OpenerForCommit {
            context,
            messages: self.checkpoint_messages.clone(),
        })
    }

    /// The opener's end ahead of the turn's terminal checkpoint: close,
    /// finalize and incorporate every group the turn formed, so what the
    /// losers' settlements carry — checkpoint messages, possession, usage —
    /// is delivered and committed by that checkpoint, before the turn's
    /// outcome (§7 step 2 precedes the outcome commit). Messages it delivers
    /// reopen the turn exactly as any checkpoint message at completion does.
    pub(super) async fn finish_opener_groups_before_completion(&self) -> Result<(), RuntimeError> {
        self.close_turn_groups().await.map(drop)
    }

    /// Close, finalize and incorporate every group this turn's opener holds.
    ///
    /// `run` passes its effect loop's result through: a clean loop whose end
    /// fails returns that failure, so the turn does not commit an outcome
    /// whose losers' facts were never incorporated (§7 step 2 precedes the
    /// outcome commit), while an already-failed loop keeps its own error and
    /// the end's failure is only traced — the turn is not committing, and the
    /// recorded `closing` is what its retry resumes from. A turn that reached
    /// its terminal checkpoint already ended its groups there; this pass then
    /// finds nothing left.
    ///
    /// A loop that aborts without a recorded cancellation — a live fault or
    /// a park, the causes that record nothing (FIG-3575) — closes nothing.
    /// Closing under `Cancel` would
    /// cancel-decide a child whose run the fault interrupted, and the redrive
    /// would then serve that child's cancellation as its recorded outcome: the
    /// live fault turned into an outcome after all. The groups stay live, and
    /// the redrive's `recover_opener_groups` or its reopen runs them on.
    pub(super) async fn end_opener_groups(
        &self,
        result: Result<(crate::MessageSequence, usize), RuntimeError>,
    ) -> Result<(crate::MessageSequence, usize), RuntimeError> {
        if result.is_ok() && self.segment.taken.is_some() {
            return result;
        }
        // A recorded cancellation is a logical exit even if its completed
        // cell's response handoff failed. Close while the Run owner is live;
        // the cancellation finisher runs after this borrowed owner returns.
        if let Err(error) = &result
            && error.turn_failure_cause().aborts_invocation()
            && self.turn_cancel.is_none()
        {
            return result;
        }
        match self.close_turn_groups().await {
            Ok(_) => result,
            Err(error) => match result {
                Ok(_) => Err(error),
                Err(original) => {
                    tracing::warn!(
                        session_id = %self.session_id,
                        turn_id = %self.turn_id,
                        error = %error,
                        "closing a failed turn's effect groups failed; `closing` stays recorded for the retry",
                    );
                    Err(original)
                }
            },
        }
    }

    async fn close_turn_groups(&self) -> Result<(), RuntimeError> {
        if !self.opener_state.holds_groups() && !self.opener_state.holds_tool_run() {
            return Ok(());
        }
        // The closing pass's emissions have no host lane: the old code
        // dropped the channel receiver outright.
        let context = self
            .execution_context_observing(
                crate::engine::NullObservationSink::arc(),
                Arc::new(crate::ChronologicalProjection::default()),
            )
            .map_err(|error| {
                RuntimeError::new(
                    RuntimeErrorCode::ToolCatalogResolutionFailed,
                    format!("the turn's effect groups could not be closed: {error}"),
                )
            })?;
        let closed = context
            .close_opener_groups()
            .await
            .map_err(crate::RuntimeEffectControllerError::into_runtime_error)?;
        if !closed.pending.is_empty() {
            tracing::debug!(
                session_id = %self.session_id,
                turn_id = %self.turn_id,
                pending = ?closed.pending,
                "turn end left effect groups closing with obligations owed elsewhere",
            );
        }
        Ok(())
    }
}
