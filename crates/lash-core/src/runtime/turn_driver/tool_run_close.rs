//! A turn's end for the tool Run it opened (ADR 0099 §7).
//!
//! Every final exit of a turn's effect loop — success, failure and
//! cancellation — closes the turn's logical Run and drains its accepted
//! finals with the turn's own execution context before the turn's outcome
//! commits. Worker loss is not an exit: a dead worker never reaches this
//! code, and the redriven turn closes the same Run when *it* exits. A
//! physical segment boundary transfers the logical Run to its continuation;
//! it stays live. An abort is not an exit either: a turn that aborts on a
//! live fault or a park records nothing and is redriven, so its Run stays
//! live for the redrive exactly as a dead worker's does.
//!
//! The end runs while the logical Run owner is still live, because closing
//! drains protected work the owner's issued attempts still hold.

use super::*;

impl<'run> RuntimeTurnDriver<'run> {
    /// The opener's end ahead of the turn's terminal checkpoint: close,
    /// finalize and incorporate the tool Run the turn opened, so what the
    /// losers' settlements carry — checkpoint messages, possession, usage —
    /// is delivered and committed by that checkpoint, before the turn's
    /// outcome (§7 step 2 precedes the outcome commit). Messages it delivers
    /// reopen the turn exactly as any checkpoint message at completion does.
    pub(super) async fn finish_tool_run_before_completion(&self) -> Result<(), RuntimeError> {
        self.close_turn_tool_run().await.map(drop)
    }

    async fn close_turn_tool_run(&self) -> Result<(), RuntimeError> {
        if !self.opener_state.holds_tool_run() {
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
                    format!("the turn's Run could not be closed: {error}"),
                )
            })?;
        context
            .close_tool_run()
            .await
            .map_err(crate::RuntimeEffectControllerError::into_runtime_error)?;
        Ok(())
    }
}
