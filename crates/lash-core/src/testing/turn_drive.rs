//! A test's turn, driven the way the engine drives one.
//!
//! A host never drives a turn: it sends an input, and the engine's session
//! drive admits it and runs its root (FIG-3600, FIG-3837). A kernel test that
//! holds its runtime by `&mut` does both halves in its own task, on the
//! controller of a handler it opened on the Restate server double: it accepts
//! the input as a host's send does, then runs the drive's recorded
//! `AdmitDrive` steps and each admitted root through
//! [`run_admitted_root_reporting`](crate::drive::run_admitted_root_reporting),
//! the calls the engine's own handlers make, until the root that drove the
//! input has run.
//!
//! A root's live fault is the engine's to retry (FIG-3897), so it comes back
//! as the attempt's error and nothing tears the root down: a test that wants
//! the retry drives the same turn again.
//!
//! A store-less runtime has no durable ingress to accept into and no drive to
//! admit from, so the kernel runs its input directly, as it does for any
//! store-less runtime.

use crate::engine::{AdmitVerdict, DriveLoop, DriveRequest, DriveRequestId};
use crate::runtime::{LashRuntime, TurnOptions};
use crate::{AgentFrameRun, AssembledTurn, RuntimeError, RuntimeErrorCode, TurnId, TurnInput};
use crate::{EmptyQueuedDrainReason, QueuedTurnDrain};

/// A test's turn on a runtime it holds, driven by the engine's own calls on
/// the controller in its [`TurnOptions`]. See the module docs.
#[async_trait::async_trait]
pub trait TestTurnDrive {
    /// Accept `input` and drive it to its root's terminal, answering every
    /// physical turn the root ran (a frame switch's follow-on turns
    /// included).
    ///
    /// The turn id is `input`'s trace id, else the scope id of the
    /// controller in `opts`; it is the accepted row's source key, so it
    /// names the root that drives the row. The drive runs whatever the
    /// session admits ahead of it first, a follow-on the head owes included.
    async fn drive_turn_frames(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AgentFrameRun, RuntimeError>;

    /// [`drive_turn_frames`](Self::drive_turn_frames), answering the root's
    /// terminal physical turn.
    async fn drive_turn(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AssembledTurn, RuntimeError>;

    /// Run `input` through the kernel's in-process turn entry, the one a
    /// child session's turn takes inside its parent's execution: a journaled
    /// acceptance step (ADR 0069 §6), then the session drive body up to the
    /// root that drove the accepted row, answering its terminal physical
    /// turn. The acceptance laws exercise it.
    async fn drive_child_session_turn(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AssembledTurn, RuntimeError>;

    /// Drive the session without sending anything: the first recorded
    /// admission of `request`, and the root it admits run to its terminal
    /// (the follow-on the head owes, an unfinished queued run, or the next
    /// pending input). Answers the root's physical turns, or `None` when
    /// admission admitted no root or the root ran no turn here.
    async fn drive_next_root(
        &mut self,
        request: &str,
        opts: TurnOptions<'_>,
    ) -> Result<Option<AgentFrameRun>, RuntimeError>;

    /// Run one queue root through recorded engine admission for tests that
    /// formerly called the direct queued drain. The command lane applies
    /// first as roots of its own that run no turn (ADR 0101 §4), so the
    /// drain answers the first root admitted on turn-lane work.
    async fn drive_one_admitted_queued_root(
        &mut self,
        opts: TurnOptions<'_>,
    ) -> Result<QueuedTurnDrain<AssembledTurn>, RuntimeError>;
}

#[async_trait::async_trait]
impl TestTurnDrive for LashRuntime {
    async fn drive_one_admitted_queued_root(
        &mut self,
        opts: TurnOptions<'_>,
    ) -> Result<QueuedTurnDrain<AssembledTurn>, RuntimeError> {
        let controller = opts.scoped_effect_controller();
        let request = DriveRequest {
            session: self.state.session_id.clone(),
            request: DriveRequestId::new(opts.execution_scope_id()),
            build_generation: self.host.core.backend().build_generation().clone(),
        };
        let mut ordinal = 0_u32;
        let mut rules = DriveLoop::new();
        loop {
            let AdmitVerdict::Admit(admitted) =
                crate::drive::admit_drive(self, &controller, &request, ordinal)
                    .await
                    .map_err(crate::engine::DriveAbort::into_error)?
            else {
                return Ok(QueuedTurnDrain::Empty(
                    EmptyQueuedDrainReason::ClaimRefused(crate::QueuedWorkClaimRefusal::Empty),
                ));
            };
            let work = admitted.work().clone();
            let sinks = crate::drive::DriveSinks {
                events: opts.events_or_noop(),
                turn_events: opts.turn_events_or_noop(),
                local_stop: opts.local_stop().clone(),
            };
            let report =
                crate::drive::run_admitted_root_reporting(self, &controller, admitted, sinks)
                    .await
                    .map_err(crate::engine::DriveAbort::into_error)?;
            if matches!(work, crate::engine::AdmittedWork::Commands { .. })
                && rules.after(&work, &report.outcome).is_none()
            {
                ordinal += 1;
                continue;
            }
            return Ok(match report.run.and_then(AgentFrameRun::into_final_turn) {
                Some(turn) => QueuedTurnDrain::Ran(turn),
                None => QueuedTurnDrain::Empty(EmptyQueuedDrainReason::ClaimRefused(
                    crate::QueuedWorkClaimRefusal::Empty,
                )),
            });
        }
    }

    async fn drive_turn_frames(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AgentFrameRun, RuntimeError> {
        if self
            .session
            .as_ref()
            .and_then(|session| session.history_store())
            .is_none()
        {
            return self.stream_turn_with_agent_frames(input, opts).await;
        }
        let controller = opts.scoped_effect_controller();
        let turn_id = input
            .trace_turn_id
            .clone()
            .unwrap_or_else(|| TurnId::from(opts.execution_scope_id()));
        let accepted = self
            .enqueue_turn_input(
                input,
                crate::TurnInputIngress::next_turn(),
                Some(turn_id.as_str().to_owned()),
            )
            .await?;
        let acceptance = crate::TurnInputAcceptanceReceipt::from(&accepted);
        let aborted = |error: RuntimeError| error.with_turn_input_acceptance(acceptance.clone());
        let request = DriveRequest {
            session: self.state.session_id.clone(),
            request: DriveRequestId::new(format!("turn:{turn_id}")),
            build_generation: self.host.core.backend().build_generation().clone(),
        };
        let sinks = crate::drive::DriveSinks {
            events: opts.events_or_noop(),
            turn_events: opts.turn_events_or_noop(),
            local_stop: opts.local_stop().clone(),
        };
        let mut rules = DriveLoop::new();
        let mut ordinal = 0_u32;
        loop {
            let verdict = crate::drive::admit_drive(self, &controller, &request, ordinal)
                .await
                .map_err(|abort| aborted(abort.into_error()))?;
            let admitted = match verdict {
                AdmitVerdict::Admit(admitted) => admitted,
                AdmitVerdict::Parked(park) => {
                    return Err(aborted(RuntimeError::new(
                        RuntimeErrorCode::QueuedRunPending,
                        format!(
                            "accepted turn input `{}` waits behind parked root `{}` (park {})",
                            accepted.input_id, park.root, park.park
                        ),
                    )));
                }
                stop => {
                    return Err(aborted(RuntimeError::new(
                        RuntimeErrorCode::AcceptedTurnInputCeded,
                        format!(
                            "accepted turn input `{}` was not driven: admission answered \
                             {stop:?}",
                            accepted.input_id
                        ),
                    )));
                }
            };
            ordinal += 1;
            if let Err(stop) = rules.before(&admitted) {
                return Err(aborted(RuntimeError::new(
                    RuntimeErrorCode::AcceptedTurnInputCeded,
                    format!(
                        "accepted turn input `{}` was not driven: the drive stopped {stop:?}",
                        accepted.input_id
                    ),
                )));
            }
            let work = admitted.work().clone();
            let report = crate::drive::run_admitted_root_reporting(
                self,
                &controller,
                admitted,
                sinks.clone(),
            )
            .await
            .map_err(|abort| aborted(abort.into_error()))?;
            if report.driven_inputs.contains(&accepted.input_id) {
                let mut run = report.run.ok_or_else(|| {
                    aborted(RuntimeError::new(
                        RuntimeErrorCode::AcceptedTurnInputCeded,
                        format!(
                            "the root that drove accepted turn input `{}` ran no turn here",
                            accepted.input_id
                        ),
                    ))
                })?;
                if let Some(admitted) = run.turns.first_mut() {
                    admitted.turn_input_acceptance = Some(acceptance.clone());
                }
                run.acceptance = Some(acceptance);
                return Ok(run);
            }
            if let Some(stop) = rules.after(&work, &report.outcome) {
                return Err(aborted(RuntimeError::new(
                    RuntimeErrorCode::AcceptedTurnInputCeded,
                    format!(
                        "accepted turn input `{}` was not driven: the drive stopped {stop:?}",
                        accepted.input_id
                    ),
                )));
            }
        }
    }

    async fn drive_child_session_turn(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AssembledTurn, RuntimeError> {
        let run = self.stream_turn_with_agent_frames(input, opts).await?;
        run.into_final_turn().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::EmptyAgentFrameRun,
                "a child session turn assembled no physical turn",
            )
        })
    }

    async fn drive_next_root(
        &mut self,
        request: &str,
        opts: TurnOptions<'_>,
    ) -> Result<Option<AgentFrameRun>, RuntimeError> {
        let controller = opts.scoped_effect_controller();
        let request = DriveRequest {
            session: self.state.session_id.clone(),
            request: DriveRequestId::new(request),
            build_generation: self.host.core.backend().build_generation().clone(),
        };
        let AdmitVerdict::Admit(admitted) =
            crate::drive::admit_drive(self, &controller, &request, 0)
                .await
                .map_err(crate::engine::DriveAbort::into_error)?
        else {
            return Ok(None);
        };
        let sinks = crate::drive::DriveSinks {
            events: opts.events_or_noop(),
            turn_events: opts.turn_events_or_noop(),
            local_stop: opts.local_stop().clone(),
        };
        let report = crate::drive::run_admitted_root_reporting(self, &controller, admitted, sinks)
            .await
            .map_err(crate::engine::DriveAbort::into_error)?;
        Ok(report.run)
    }

    async fn drive_turn(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AssembledTurn, RuntimeError> {
        let run = self.drive_turn_frames(input, opts).await?;
        run.into_final_turn().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::AcceptedTurnInputCeded,
                "a driven root assembled no physical turn",
            )
        })
    }
}
