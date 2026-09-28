//! The queued-work drain: the drive entry a drain identity names, and why a
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
    /// The queue was reachable and the claim state machine refused it.
    ClaimRefused(crate::QueuedWorkClaimRefusal),
}

impl EmptyQueuedDrainReason {
    /// The stable snake_case spelling, for host logs and metrics labels.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExecutionLaneBusy => "execution_lane_busy",
            Self::NoDurableQueue => "no_durable_queue",
            Self::ClaimRefused(refusal) => refusal.as_str(),
        }
    }
}

/// One automatic queued-turn drain: the turn it ran, or why it ran none.
#[derive(Clone, Debug)]
pub enum QueuedTurnDrain<T> {
    /// The drain admitted a root and ran its turn.
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

impl LashRuntime {
    /// Drain the session's next work through the session drive (FIG-3600):
    /// one drive, named by the drain's identity, run until its first root has
    /// run, answered in the automatic drain's contract.
    ///
    /// The drain's identity is its drive request, so a redrive of the same
    /// drain replays that drive's recorded admissions and the root it
    /// admitted replays at the head it was admitted on (FIG-3748). A drain
    /// with no identity is refused. Queued work and idle next-turn input run
    /// as the roots the drive admits for them.
    pub async fn drive_next_queued_root<'a>(
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
        // The drain's identity names its drive: a redrive of the drain must
        // name the same one, so an anonymous drain has none to replay.
        let identity = opts.source.identity().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::QueuedWork,
                "the drive entry needs a drain identity to name its drive request",
            )
        })?;
        let bound = opts.bind(identity)?;
        let controller = bound.scoped_effect_controller();
        let request = crate::engine::DriveRequest {
            session: self.state.session_id.clone(),
            request: crate::engine::DriveRequestId::new(controller.scope_id()),
            build_generation: self.host.core.backend().build_generation().clone(),
        };
        let sinks = crate::runtime::drive::DriveSinks {
            events: bound.events_or_noop(),
            turn_events: bound.turn_events_or_noop(),
            local_stop: bound.local_stop().clone(),
            settled: &crate::runtime::drive::NoopRootSettledSink,
        };
        let drive = Box::pin(self.drive_until(
            &controller,
            &request,
            &sinks,
            None,
            crate::runtime::drive::DriveLimits {
                follow_on: crate::runtime::drive::FollowOnRecovery::Recover,
                max_roots: None,
            },
            // The command lane applies first and runs no turn: the drain
            // answers the first root that took turn-lane work.
            |run| !matches!(run.outcome, crate::engine::RootOutcome::Applied { .. }),
        ))
        .await;
        let crate::runtime::drive::DriveRun { outcome, runs, .. } = match drive {
            Ok(drive) => drive,
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
        if let Some(run) = runs
            .into_iter()
            .find(|run| !matches!(run.outcome, crate::engine::RootOutcome::Applied { .. }))
        {
            return Ok(match (run.outcome, run.run, run.empty_drain) {
                (_, Some(run), _) => match run.into_final_turn() {
                    Some(turn) => QueuedTurnDrain::Ran(turn),
                    None => QueuedTurnDrain::Empty(EmptyQueuedDrainReason::ClaimRefused(
                        crate::QueuedWorkClaimRefusal::ClaimRaceLost,
                    )),
                },
                (_, None, Some(reason)) => QueuedTurnDrain::Empty(reason),
                // Another drive sealed the session first: it holds the lane.
                (crate::engine::RootOutcome::Refused { .. }, None, _) => {
                    QueuedTurnDrain::Empty(EmptyQueuedDrainReason::ExecutionLaneBusy)
                }
                // The root's rows were answered by another driver.
                (_, None, _) => QueuedTurnDrain::Empty(EmptyQueuedDrainReason::ClaimRefused(
                    crate::QueuedWorkClaimRefusal::ClaimRaceLost,
                )),
            });
        }
        match outcome.stop {
            crate::engine::DriveStop::Idle => Ok(QueuedTurnDrain::Empty(
                EmptyQueuedDrainReason::ClaimRefused(self.idle_drain_refusal().await?),
            )),
            crate::engine::DriveStop::Parked(park) => Err(RuntimeError::new(
                RuntimeErrorCode::QueuedRunPending,
                format!(
                    "queued work on session `{}` waits behind parked root `{}` (park {}); it \
                     is driven once that park is resolved",
                    self.state.session_id, park.root, park.park
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

    /// Why an idle drive ran nothing: no work at all, or work that appeared
    /// after admission read the queue and is therefore another claim's to
    /// take.
    async fn idle_drain_refusal(&self) -> Result<crate::QueuedWorkClaimRefusal, RuntimeError> {
        let Some(store) = self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
        else {
            return Ok(crate::QueuedWorkClaimRefusal::Empty);
        };
        let pending = store
            .list_pending_queued_work(&self.state.session_id)
            .await
            .map_err(super::runtime_error_from_store_commit)?;
        Ok(if pending.is_empty() {
            crate::QueuedWorkClaimRefusal::Empty
        } else {
            crate::QueuedWorkClaimRefusal::ClaimRaceLost
        })
    }
}
