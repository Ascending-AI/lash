use crate::core_internal::await_event_scope_not_retirable;
use crate::queued_lane::{
    CompletionKeyPreparation, QueuedLaneAcquisition, QueuedLaneAttempt, QueuedLaneProbe,
};
use crate::queued_lane_wait;
use crate::{
    AwaitEventKey, AwaitEventWaitIdentity, ExecutionScope, Resolution, ResolveOutcome,
    RuntimeError, SessionId,
};
use std::sync::Arc;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Shared AwaitEvent contract for effect boundaries.
///
/// Both the deployment-level [`EffectHost`] factory and the per-run
/// [`RuntimeEffectController`] resolve AwaitEvents.
#[async_trait::async_trait]
pub trait AwaitEventResolver: Send + Sync {
    /// Stable identity of the durable authority that minted keys accepted by
    /// this resolver. Durable turn-control composition uses this to prevent a
    /// host label from being paired with another owner's controller and keys.
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }

    /// Acquire the authoritative session-execution lane a durable queued drain
    /// needs before it may claim work.
    ///
    /// The default is the one-shot contract every non-re-driven boundary owes:
    /// one attempt, `Busy` reported as `NotAcquired`, the durable row left
    /// pending. A boundary whose *invocation* is re-driven by a durable engine
    /// overrides with [`wait_out_crashed_lane_holder`](Self::wait_out_crashed_lane_holder):
    /// it waits out a crashed-looking holder and otherwise fails with the typed
    /// retryable `RuntimeErrorCode::SessionExecutionLaneBusy` so the engine's
    /// retry policy — not a sleep inside one invocation — paces the next
    /// attempt. Neither path bypasses or forges the holder's lease.
    ///
    /// Arguments are owned so this can be proxied across
    /// `EffectControllerTaskRequest`.
    async fn acquire_queued_lane(
        &self,
        lane: Arc<dyn QueuedLaneProbe>,
        _cancel: CancellationToken,
    ) -> Result<QueuedLaneAcquisition, RuntimeError> {
        match lane.try_acquire().await? {
            QueuedLaneAttempt::Acquired(guard) => Ok(QueuedLaneAcquisition::Acquired(guard)),
            QueuedLaneAttempt::Busy(_) => Ok(QueuedLaneAcquisition::NotAcquired),
        }
    }

    /// Bounded, aliveness-aware wait for engine-re-driven boundaries. Provided
    /// so `lash-restate` adopts the policy without lash-core exporting the
    /// policy types or a second free function.
    async fn wait_out_crashed_lane_holder(
        &self,
        lane: Arc<dyn QueuedLaneProbe>,
        cancel: CancellationToken,
    ) -> Result<QueuedLaneAcquisition, RuntimeError> {
        let mut wait = queued_lane_wait::QueuedLaneWait::default();
        #[cfg(feature = "otel-trace")]
        let mut contention_started: Option<tokio::time::Instant> = None;
        loop {
            let acquisition = match lane.try_acquire().await {
                Ok(acquisition) => acquisition,
                Err(error) => {
                    #[cfg(feature = "otel-trace")]
                    if let Some(started) = contention_started {
                        crate::operational_metrics::record_session_lane_contention_wait(
                            started.elapsed(),
                            "error",
                        );
                    }
                    return Err(error);
                }
            };
            match acquisition {
                QueuedLaneAttempt::Acquired(guard) => {
                    #[cfg(feature = "otel-trace")]
                    if let Some(started) = contention_started {
                        crate::operational_metrics::record_session_lane_contention_wait(
                            started.elapsed(),
                            "acquired",
                        );
                    }
                    return Ok(QueuedLaneAcquisition::Acquired(guard));
                }
                QueuedLaneAttempt::Busy(holder) => {
                    #[cfg(feature = "otel-trace")]
                    let started = *contention_started.get_or_insert_with(tokio::time::Instant::now);
                    let slice_ms = match wait.observe(&holder) {
                        queued_lane_wait::QueuedLaneWaitStep::Wait { slice_ms } => slice_ms,
                        queued_lane_wait::QueuedLaneWaitStep::GiveUp(give_up) => {
                            let waited_ms = wait.waited_ms();
                            #[cfg(feature = "otel-trace")]
                            crate::operational_metrics::record_session_lane_contention_wait(
                                started.elapsed(),
                                "gave_up",
                            );
                            crate::operational_metrics::record_session_lane_give_up(
                                give_up.as_str(),
                            );
                            queued_lane_wait::trace_busy_gave_up(&holder, give_up, waited_ms);
                            return Err(queued_lane_wait::lane_busy_error(
                                &holder, give_up, waited_ms,
                            ));
                        }
                    };
                    queued_lane_wait::trace_busy_wait(&holder, slice_ms, wait.waited_ms());
                    let sleep = lane.pause(std::time::Duration::from_millis(slice_ms));
                    tokio::select! {
                        () = sleep => {}
                        () = cancel.cancelled() => {
                            let give_up = queued_lane_wait::QueuedLaneGiveUp::CancelledWhileWaiting;
                            let waited_ms = wait.waited_ms();
                            #[cfg(feature = "otel-trace")]
                            crate::operational_metrics::record_session_lane_contention_wait(
                                started.elapsed(),
                                "gave_up",
                            );
                            crate::operational_metrics::record_session_lane_give_up(
                                give_up.as_str(),
                            );
                            queued_lane_wait::trace_busy_gave_up(
                                &holder,
                                give_up,
                                waited_ms,
                            );
                            return Err(queued_lane_wait::lane_busy_error(
                                &holder,
                                give_up,
                                waited_ms,
                            ));
                        },
                    }
                }
            }
        }
    }

    async fn prepare_completion_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<CompletionKeyPreparation, RuntimeError> {
        let _ = (scope, wait);
        if may_defer {
            Ok(CompletionKeyPreparation::Unsupported)
        } else {
            Ok(CompletionKeyPreparation::NotNeeded)
        }
    }

    async fn await_event_key(
        &self,
        _scope: &ExecutionScope,
        _wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventUnsupported,
            "this effect boundary does not support await-event keys",
        ))
    }

    async fn resolve_await_event(
        &self,
        _key: &AwaitEventKey,
        _resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        Ok(ResolveOutcome::UnknownOrRevoked)
    }

    /// Read a keyed promise without waiting for or resolving it.
    ///
    /// Turn owners use this as a synchronous start gate before beginning a
    /// new effect. Durable owners must perform that read through their
    /// handler-scoped, replay-aware controller: its result affects subsequent
    /// command order and therefore must replay identically after an owner
    /// crash. An unresolved promise returns `None` and remains open.
    async fn peek_await_event(
        &self,
        _key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventUnsupported,
            "this effect boundary does not support await-event reads",
        ))
    }

    async fn await_await_event(
        &self,
        _key: &AwaitEventKey,
        _cancel: CancellationToken,
        _deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventUnsupported,
            "this effect boundary does not support await-event waits",
        ))
    }

    async fn revoke_await_events_for_session(
        &self,
        _session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventUnsupported,
            "this effect boundary does not support revoking await-event waits",
        ))
    }

    /// Cancel every *outstanding* durable wait for `session_id` without
    /// deleting the session: each waiter receives a terminal
    /// [`Resolution::Cancelled`] instead of hanging, late resolves observe
    /// that terminal, and waits registered afterwards behave normally — in
    /// contrast to [`revoke_await_events_for_session`](Self::revoke_await_events_for_session),
    /// which tombstones the session's waits forever.
    ///
    /// The default errors loudly: an effect boundary that tracks durable waits
    /// must implement this to honor the host lever, and one that cannot must
    /// not silently claim success.
    async fn cancel_await_events_for_session(
        &self,
        _session_id: &SessionId,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventCancelUnsupported,
            "this effect boundary does not support cancelling durable waits",
        ))
    }

    /// Drop every promise of the terminal non-session `scope` and fence the
    /// scope permanently: later mints, resolves, peeks, and waits under it
    /// report `await_event_unknown_or_revoked`, including after a restart on
    /// durable hosts. Session-bearing scopes are refused with
    /// `await_event_scope_not_retirable`; they are revoked as a family through
    /// [`revoke_await_events_for_session`](Self::revoke_await_events_for_session).
    ///
    /// This is the promise half of [`EffectHost::retire_effect_journal`]: a
    /// durable host performs both halves in one transaction there and answers
    /// this lever from the same code path.
    async fn retire_await_events_for_scope(
        &self,
        _scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::new(
            crate::RuntimeErrorCode::AwaitEventUnsupported,
            "this effect boundary does not support retiring await-event scopes",
        ))
    }

    /// [`retire_await_events_for_scope`](Self::retire_await_events_for_scope)
    /// only when no waiter is parked on a promise under `scope`, answering
    /// whether it retired: `Ok(false)` leaves the scope untouched and unfenced.
    /// The proof and the fence must land under one lock. Resolvers that prove
    /// quiescence elsewhere (a durable journal reads its wait rows in the
    /// retirement transaction) retire unconditionally here and answer `true`.
    async fn retire_await_events_for_scope_if_quiescent(
        &self,
        scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        self.retire_await_events_for_scope(scope)
            .await
            .map(|()| true)
    }

    /// Lift the fence
    /// [`retire_await_events_for_scope`](Self::retire_await_events_for_scope)
    /// left on a non-session `scope`, because its owner is registered again.
    /// Resolvers that never fence have nothing to lift and answer `Ok`.
    async fn reinstate_await_event_scope(
        &self,
        scope: &ExecutionScope,
    ) -> Result<(), RuntimeError> {
        if scope.session_id().is_some() {
            return Err(await_event_scope_not_retirable(scope));
        }
        Ok(())
    }

    /// Whether `scope` is fenced by a scope-exact retirement this resolver
    /// holds. Every admission path that runs effects for a scope consults it
    /// before executing, so a retired scope is refused even where no journal
    /// claim exists to refuse it (the in-process host). Resolvers whose
    /// journal already refuses retired scopes at claim time may answer
    /// `false`.
    async fn await_event_scope_is_retired(
        &self,
        _scope: &ExecutionScope,
    ) -> Result<bool, RuntimeError> {
        Ok(false)
    }
}
