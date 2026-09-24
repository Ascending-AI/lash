//! A turn's end for the effect groups it opened (ADR 0099 §7; FIG-3397).
//!
//! Every final exit of a turn's effect loop — success, failure and
//! cancellation — closes the groups the turn still holds and finalizes them
//! with the turn's own execution context before the turn's outcome and
//! accounting commit. Worker loss is not an exit: a dead worker never reaches
//! this code, and the redriven turn closes the same groups when *it* exits.
//! An abort is not an exit either: a turn that aborts on a live fault or a
//! park records nothing and is redriven, so its groups stay live for the
//! redrive exactly as a dead worker's do.
//!
//! The end runs while the turn is still registered as a live opener, because
//! finalization's first step may have to run a child no process is running,
//! and a child resolves its executor through its opener's live registration.

use super::*;

impl<'run> RuntimeTurnDriver<'run> {
    /// Recover the losers of groups this turn's opener accepted before its
    /// worker died (ADR 0099 W5): a resumed turn serves its completed cells
    /// from the journal, so no cell reopens those groups, and their accepted
    /// children would otherwise wait for the turn's end to cancel them. A dead
    /// worker is not a closed opener. Building the context registers the turn
    /// as a live opener first, which is what lets a recovered child resolve
    /// its runner. Best-effort: a failure is traced, and the turn's end still
    /// closes every live group under its scope.
    pub(super) async fn recover_opener_groups(&self, event_tx: &mpsc::Sender<RuntimeStreamEvent>) {
        // Without a closing seam there is no journal of live groups to read:
        // the tier answers recovery itself (Restate) or holds nothing that
        // outlives its process.
        if self
            .host
            .core
            .control
            .effect_host
            .effect_group_closing()
            .is_none()
        {
            return;
        }
        let (session_event_tx, session_event_rx) = mpsc::channel::<SessionStreamEvent>(1);
        drop(session_event_rx);
        let context = match self.execution_context(
            session_event_tx,
            event_tx,
            Arc::new(crate::ChronologicalProjection::default()),
        ) {
            Ok(context) => context,
            Err(error) => {
                tracing::warn!(
                    session_id = %self.session_id,
                    turn_id = %self.turn_id,
                    error = %error,
                    "a resumed turn could not build the context its group recovery needs",
                );
                return;
            }
        };
        if let Err(error) = context.recover_opener_groups().await {
            tracing::warn!(
                session_id = %self.session_id,
                turn_id = %self.turn_id,
                error = %error,
                "recovering a resumed turn's live effect groups failed",
            );
        }
    }

    /// The opener's end ahead of the turn's terminal checkpoint: close,
    /// finalize and incorporate every group the turn formed, so what the
    /// losers' settlements carry — checkpoint messages, possession, usage —
    /// is delivered and committed by that checkpoint, before the turn's
    /// outcome (§7 step 2 precedes the outcome commit). Messages it delivers
    /// reopen the turn exactly as any checkpoint message at completion does.
    pub(super) async fn finish_opener_groups_before_completion(
        &self,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
    ) -> Result<(), RuntimeError> {
        self.close_turn_groups(event_tx).await.map(drop)
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
    /// A loop that aborts — a live fault or a park, the causes that record
    /// nothing (FIG-3575) — closes nothing. Closing under `Cancel` would
    /// cancel-decide a child whose run the fault interrupted, and the redrive
    /// would then serve that child's cancellation as its recorded outcome: the
    /// live fault turned into an outcome after all. The groups stay live, and
    /// the redrive's `recover_opener_groups` or its reopen runs them on.
    pub(super) async fn end_opener_groups(
        &self,
        result: Result<(crate::MessageSequence, usize), RuntimeError>,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
    ) -> Result<(crate::MessageSequence, usize), RuntimeError> {
        if let Err(error) = &result
            && error.turn_failure_cause().aborts_invocation()
        {
            return result;
        }
        match self.close_turn_groups(event_tx).await {
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

    async fn close_turn_groups(
        &self,
        event_tx: &mpsc::Sender<RuntimeStreamEvent>,
    ) -> Result<(), RuntimeError> {
        // Nothing to close: no cursor held, and no closing seam whose journal
        // could name a group this turn no longer holds.
        if !self.opener_state.holds_groups()
            && self
                .host
                .core
                .control
                .effect_host
                .effect_group_closing()
                .is_none()
        {
            return Ok(());
        }
        let (session_event_tx, session_event_rx) = mpsc::channel::<SessionStreamEvent>(1);
        drop(session_event_rx);
        let context = self
            .execution_context(
                session_event_tx,
                event_tx,
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
