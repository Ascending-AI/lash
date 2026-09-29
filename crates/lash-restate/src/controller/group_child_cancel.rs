//! The cancel fact of the effect-group child a controller drives (ADR 0105
//! §4, FIG-3904).
//!
//! Group dispatch binds a child's controller to the child's cancel fact. A
//! wait of the child that observes no turn races the fact as a journaled arm,
//! the child's step boundaries peek it as a journaled call, and a recorded
//! step body watches it live. The engine's own cancellation of the child's
//! invocation, which the index requests right after it decides the child's
//! cancel, is that same decided cancel wherever it surfaces.

use super::*;
use crate::durable_wait::RestateDurableWaitAwaitRequest;

impl<'ctx, C> RestateRuntimeEffectController<'ctx, C> {
    /// Bind this controller to the effect-group child it drives, whose cancel
    /// fact is `cancel`. Only group dispatch sets it.
    pub(crate) fn with_group_child_cancel(
        mut self,
        cancel: crate::effect_group::GroupChildCancel,
    ) -> Self {
        self.options.group_child_cancel = Some(Arc::new(cancel));
        self
    }

    /// Whether `terminal` is the engine's cancellation of the group child this
    /// controller drives, surfacing at one of the child's awaits. The engine
    /// journals its signal, so a replay meets it at the same await.
    pub(super) fn is_group_child_engine_cancel(&self, terminal: &TerminalError) -> bool {
        self.options.group_child_cancel.is_some() && terminal.code() == 409
    }

    /// The cancel fact a wait of this controller races as a journaled arm: a
    /// bound group child's, for a wait that observes no turn and belongs to
    /// no process segment.
    pub(super) fn group_child_wait_race(
        &self,
        turn_cancel: Option<&RestateDurableWaitAwaitRequest>,
    ) -> Option<RestateDurableWaitAwaitRequest> {
        match (
            turn_cancel,
            self.options.process_cancel,
            &self.options.group_child_cancel,
        ) {
            (None, context::ProcessCancelRace::NotRaced, Some(child_cancel)) => {
                Some(child_cancel.await_request())
            }
            _ => None,
        }
    }

    pub(super) fn group_child_cancel_watch(
        &self,
    ) -> Option<Arc<dyn lash_core::GroupChildCancelWatch>> {
        self.options
            .group_child_cancel
            .as_ref()
            .map(|child_cancel| child_cancel.watch())
    }
}

impl<'ctx, C> RestateRuntimeEffectController<'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    /// A journaled peek of the bound child's cancel fact; `false` for a
    /// controller that drives no group child.
    pub(super) async fn peek_group_child_cancel(
        &self,
    ) -> Result<bool, RuntimeEffectControllerError> {
        let Some(child_cancel) = &self.options.group_child_cancel else {
            return Ok(false);
        };
        let (address, replay_key) = child_cancel.peek_target();
        match self
            .context
            .peek_event(&self.namespace, address, replay_key)
            .await
        {
            Ok(peeked) => Ok(peeked.is_some_and(crate::effect_group::group_child_cancel_verdict)),
            Err(err) if self.is_group_child_engine_cancel(&err) => Ok(true),
            Err(err) => Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::EngineAwaitEventPeek,
                err.to_string(),
            )),
        }
    }

    /// A process command's failure, as the bound group child meets it. The
    /// engine's cancellation of the child's invocation fails the command's
    /// journaled step with a 409 the command reports as a controller fault;
    /// the child's journaled cancel fact, peeked as its step boundaries peek
    /// it, tells that decided cancel apart from any other fault, which is
    /// returned unchanged. A replay meets the same failure and the same peek.
    pub(super) async fn group_child_process_failure(
        &self,
        error: RuntimeEffectControllerError,
    ) -> RuntimeEffectControllerError {
        if self.options.group_child_cancel.is_none()
            || error.code != RuntimeErrorCode::EngineEffectController
        {
            return error;
        }
        match self.peek_group_child_cancel().await {
            Ok(true) => group_child_cancelled(),
            Ok(false) | Err(_) => error,
        }
    }

    /// A timer of this controller. One that observes no turn races the bound
    /// child's cancel fact; any other races as the turn-cancel race answers
    /// it. The outer `Err` is the child's typed cancel, whichever way it
    /// surfaced; the inner result is the turn-cancel race's.
    pub(super) async fn sleep_raced(
        &self,
        invocation: &RuntimeEffectInvocation,
        duration_ms: u64,
        turn_cancel: Option<RestateDurableWaitAwaitRequest>,
    ) -> Result<Result<RestateTurnCancelRaceOutcome<()>, TerminalError>, RuntimeEffectControllerError>
    {
        let duration = Duration::from_millis(duration_ms);
        let Some(cancel) = self.group_child_wait_race(turn_cancel.as_ref()) else {
            let raced = self
                .context
                .sleep_or_turn_cancel(
                    &self.namespace,
                    duration,
                    turn_cancel,
                    self.options.process_cancel,
                )
                .await;
            return match raced {
                Err(err) if self.is_group_child_engine_cancel(&err) => Err(group_child_cancelled()),
                raced => Ok(raced),
            };
        };
        match self
            .context
            .sleep_or_group_child_cancel(&self.namespace, duration, cancel)
            .await
        {
            Ok(Some(())) => Ok(Ok(RestateTurnCancelRaceOutcome::Completed(()))),
            Ok(None) => {
                self.emit_trace(Some(invocation), || {
                    lash_trace::TraceEvent::DurableTimerResolved {
                        duration_ms,
                        status: lash_trace::TraceDurableTimerStatus::Cancelled,
                    }
                });
                Err(group_child_cancelled())
            }
            Err(err) if self.is_group_child_engine_cancel(&err) => Err(group_child_cancelled()),
            Err(err) => Ok(Err(err)),
        }
    }

    /// An await of the bound child raced against `cancel`. The index that
    /// decided the cancel released the event wait it lost to (FIG-3630).
    pub(super) async fn await_event_under_group_child_cancel(
        &self,
        invocation: &RuntimeEffectInvocation,
        request: RestateDurableWaitAwaitRequest,
        replay_key: String,
        cancel: RestateDurableWaitAwaitRequest,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        let raced = self
            .context
            .await_event_or_group_child_cancel(&self.namespace, request, replay_key, cancel)
            .await;
        let status = match &raced {
            Ok(Some(resolution)) => resolution_trace_label(resolution),
            Ok(None) => lash_trace::TraceDurableWaitResolution::Cancelled,
            Err(err) if self.is_group_child_engine_cancel(err) => {
                lash_trace::TraceDurableWaitResolution::Cancelled
            }
            Err(_) => lash_trace::TraceDurableWaitResolution::Failed,
        };
        self.emit_trace(Some(invocation), || {
            lash_trace::TraceEvent::DurableWaitResolved {
                wait_kind: "await_event".to_string(),
                resolution: status,
            }
        });
        match raced {
            Ok(Some(resolution)) => Ok(RuntimeEffectOutcome::AwaitEvent { resolution }),
            Ok(None) => Err(group_child_cancelled()),
            Err(err) if self.is_group_child_engine_cancel(&err) => Err(group_child_cancelled()),
            Err(err) => Err(RuntimeEffectControllerError::new(
                RuntimeErrorCode::EngineEffectController,
                err.to_string(),
            )),
        }
    }
}

/// A group child whose step lost to the child's cancel: the typed end of the
/// child, which its dispatch settles `Cancelled`.
pub(super) fn group_child_cancelled() -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        RuntimeErrorCode::RuntimeEffectGroupChildCancelled,
        "an effect-group child's step lost to the child's cancel",
    )
}
