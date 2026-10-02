//! The queued-work drain: the shift entry a drain identity names, and why a
//! drain ran no turn.

use super::*;

/// Why an automatic queued-turn drain executed no turn.
///
/// An automatic drain names no batch ids, so there is nothing for a host to
/// inspect afterwards: this reason is the whole account of the empty drain.
/// Reading one variant as another is how queued work gets abandoned — a drain
/// that never reached its input is retryable, while an exhausted queue is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EmptyQueuedDrainReason {
    /// Another execution holds the session execution lane, so this drain never
    /// looked at the queue. Nothing was consumed and the work is retryable.
    ExecutionLaneBusy,
    /// The session has no durable store, so no queue exists to drain.
    NoDurableQueue,
    /// The queue was reachable and the admission state machine refused it.
    AdmissionRefused(crate::AdmissionRefusal),
}

impl EmptyQueuedDrainReason {
    /// The stable snake_case spelling, for host logs and metrics labels.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExecutionLaneBusy => "execution_lane_busy",
            Self::NoDurableQueue => "no_durable_queue",
            Self::AdmissionRefused(refusal) => refusal.as_str(),
        }
    }
}

/// One automatic queued-turn drain: the turn it ran, or why it ran none.
#[derive(Clone, Debug)]
pub enum QueuedTurnDrain<T> {
    /// The drain admitted a run and ran its turn.
    Ran(T),
    /// The drain ran no turn, for the named reason.
    Empty(EmptyQueuedDrainReason),
}

impl<T> QueuedTurnDrain<T> {
    /// The turn this drain ran, discarding the empty reason.
    pub fn ran(self) -> Option<T> {
        match self {
            Self::Ran(turn) => Some(turn),
            Self::Empty(_) => None,
        }
    }

    /// Transforms the turn, preserving the empty reason.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> QueuedTurnDrain<U> {
        match self {
            Self::Ran(turn) => QueuedTurnDrain::Ran(f(turn)),
            Self::Empty(reason) => QueuedTurnDrain::Empty(reason),
        }
    }

    #[track_caller]
    pub fn expect(self, message: &str) -> T {
        match self {
            Self::Ran(turn) => turn,
            Self::Empty(reason) => {
                panic!("{message}: queued drain ran no turn ({})", reason.as_str())
            }
        }
    }
}

/// What a drain answers when the seal refused the run it admitted. A
/// superseded admission lost the lane to the shift that sealed first, which
/// is retryable. A lost execution is not: another execution of the run
/// sealed it, and this one must never run it, so the drain stops as it does
/// on [`ShiftStop::SubstrateLost`](crate::engine::ShiftStop::SubstrateLost).
fn refused_run_drain<T>(
    request: &crate::engine::ShiftRequestId,
    run: &crate::TurnId,
    refusal: crate::engine::SealRefusal,
) -> Result<QueuedTurnDrain<T>, RuntimeError> {
    match refusal {
        crate::engine::SealRefusal::Superseded { .. } => Ok(QueuedTurnDrain::Empty(
            EmptyQueuedDrainReason::ExecutionLaneBusy,
        )),
        crate::engine::SealRefusal::ExecutionLost => Err(RuntimeError::new(
            RuntimeErrorCode::QueuedWork,
            format!(
                "queued drain `{}` stopped: run `{run}` was sealed by another execution, \
                 whose history this one cannot read",
                request.as_str()
            ),
        )),
    }
}

impl LashRuntime {
    /// Drain the session's next work through the session shift (FIG-3600):
    /// one shift, named by the drain's identity, run until its first run has
    /// run, answered in the automatic drain's contract.
    ///
    /// The drain's identity is its shift request, so a redrive of the same
    /// drain replays that shift's recorded admissions and the run it
    /// admitted replays at the head it was admitted on (FIG-3748). A drain
    /// with no identity is refused. Queued work and idle next-turn input run
    /// as the runs the shift admits for them.
    pub async fn execute_next_queued_run<'a>(
        &mut self,
        opts: impl Into<QueuedTurnOptions<'a>>,
    ) -> Result<QueuedTurnDrain<AssembledTurn>, RuntimeError> {
        let opts = opts.into();
        if self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
            .is_none()
        {
            return Ok(QueuedTurnDrain::Empty(
                EmptyQueuedDrainReason::NoDurableQueue,
            ));
        }
        // The drain's identity names its shift: a redrive of the drain must
        // name the same one, so an anonymous drain has none to replay.
        let identity = opts.source.identity().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::QueuedWork,
                "the shift entry needs a drain identity to name its shift request",
            )
        })?;
        let bound = opts.bind(identity)?;
        let controller = bound.scoped_effect_controller();
        let request = crate::engine::ShiftRequest {
            session: self.state.session_id.clone(),
            request: crate::engine::ShiftRequestId::new(controller.scope_id()),
            intended_lane: None,
        };
        let sinks = crate::runtime::shift::ShiftSinks {
            events: bound.events_or_noop(),
            turn_events: bound.turn_events_or_noop(),
            local_stop: bound.local_stop().clone(),
            settled: &crate::runtime::shift::NoopRunSettledSink,
        };
        let shift = Box::pin(self.work_until(
            &controller,
            &request,
            &sinks,
            None,
            crate::runtime::shift::ShiftLimits {
                follow_on: crate::runtime::shift::FollowOnRecovery::Recover,
                max_runs: None,
                acceptor: false,
            },
            // The command lane applies first and runs no turn: the drain
            // answers the first run that took turn-lane work.
            |executed| !matches!(executed.outcome, crate::engine::RunOutcome::Applied { .. }),
        ))
        .await;
        let crate::runtime::shift::ShiftLoopEnd { outcome, runs, .. } = match shift {
            Ok(shift) => shift,
            Err(abort) => {
                let error = abort.into_error();
                if error.code == RuntimeErrorCode::SessionExecutionLaneBusy {
                    return Ok(QueuedTurnDrain::Empty(
                        EmptyQueuedDrainReason::ExecutionLaneBusy,
                    ));
                }
                return Err(error);
            }
        };
        if let Some(executed) = runs
            .into_iter()
            .find(|executed| !matches!(executed.outcome, crate::engine::RunOutcome::Applied { .. }))
        {
            return Ok(
                match (executed.outcome, executed.run, executed.empty_drain) {
                    (_, Some(executed), _) => match executed.into_final_turn() {
                        Some(turn) => QueuedTurnDrain::Ran(turn),
                        None => QueuedTurnDrain::Empty(EmptyQueuedDrainReason::AdmissionRefused(
                            crate::AdmissionRefusal::AdmissionRaceLost,
                        )),
                    },
                    (_, None, Some(reason)) => QueuedTurnDrain::Empty(reason),
                    (crate::engine::RunOutcome::Refused { run, refusal }, None, _) => {
                        return refused_run_drain(&request.request, &run, refusal);
                    }
                    // The run's rows were answered by another driver.
                    (_, None, _) => {
                        QueuedTurnDrain::Empty(EmptyQueuedDrainReason::AdmissionRefused(
                            crate::AdmissionRefusal::AdmissionRaceLost,
                        ))
                    }
                },
            );
        }
        match outcome.stop {
            crate::engine::ShiftStop::Idle => Ok(QueuedTurnDrain::Empty(
                EmptyQueuedDrainReason::AdmissionRefused(self.idle_drain_refusal().await?),
            )),
            crate::engine::ShiftStop::Parked(park) => Err(RuntimeError::new(
                RuntimeErrorCode::SessionRunPending,
                format!(
                    "queued work on session `{}` waits behind parked run `{}` (park {}); it \
                     is executed once that park is resolved",
                    self.state.session_id, park.run, park.park
                ),
            )),
            stop => Err(RuntimeError::new(
                RuntimeErrorCode::QueuedWork,
                format!(
                    "queued drain `{}` stopped: {stop:?}",
                    request.request.as_str()
                ),
            )),
        }
    }

    /// Why an idle shift ran nothing: no work at all, or work that appeared
    /// after admission read the queue and is therefore another admission's to
    /// take.
    async fn idle_drain_refusal(&self) -> Result<crate::AdmissionRefusal, RuntimeError> {
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return Ok(crate::AdmissionRefusal::Empty);
        };
        let pending = store
            .list_open_queued_work()
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        Ok(if pending.is_empty() {
            crate::AdmissionRefusal::Empty
        } else {
            crate::AdmissionRefusal::AdmissionRaceLost
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{SealRefusal, ShiftRequestId};

    /// F75 (FIG-4648): a seal another admission superseded leaves the lane
    /// busy and the work retryable; a seal another execution of the run took
    /// is a lost execution, never a busy lane.
    #[test]
    fn a_lost_execution_is_never_answered_as_a_busy_lane() {
        let request = ShiftRequestId::new("drain-1");
        let run = crate::TurnId::from("r");
        assert!(matches!(
            refused_run_drain::<()>(&request, &run, SealRefusal::Superseded { epoch: 3 }),
            Ok(QueuedTurnDrain::Empty(
                EmptyQueuedDrainReason::ExecutionLaneBusy
            ))
        ));
        let lost = refused_run_drain::<()>(&request, &run, SealRefusal::ExecutionLost)
            .expect_err("a lost execution stops the drain");
        assert_eq!(lost.code, RuntimeErrorCode::QueuedWork);
    }
}
