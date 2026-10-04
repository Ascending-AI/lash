//! A test's turn, executed the way the engine executes one.
//!
//! A host never executes a turn: it sends an input, and the engine's session
//! shift admits it and executes its run (FIG-3600, FIG-3837). A kernel test that
//! holds its runtime by `&mut` does both halves in its own task, on the
//! controller of a handler it opened on the Restate server double: it accepts
//! the input as a host's send does, then runs the shift's recorded
//! `AdmitShift` steps and each admitted run through
//! [`execute_admitted_run_reporting`](crate::shift::execute_admitted_run_reporting),
//! the calls the engine's own handlers make, until the run that drove the
//! input has run.
//!
//! A run's live fault is the engine's to retry (FIG-3897), so it comes back
//! as the attempt's error and nothing tears the run down: a test that wants
//! the retry executes the same turn again.
//!
//! A store-less runtime has no durable ingress to accept into and no shift to
//! admit from, so the kernel runs its input directly, as it does for any
//! store-less runtime.

use crate::engine::{AdmitVerdict, ShiftLoop, ShiftRequest, ShiftRequestId};
use crate::runtime::{LashRuntime, TurnOptions};
use crate::{AgentFrameRun, AssembledTurn, RuntimeError, RuntimeErrorCode, TurnId, TurnInput};
use crate::{EmptyQueuedDrainReason, QueuedTurnDrain};

/// A test's turn on a runtime it holds, executed by the engine's own calls on
/// the controller in its [`TurnOptions`]. See the module docs.
#[async_trait::async_trait]
pub trait TestTurnExecution {
    /// Accept `input` and execute it to its run's terminal, answering every
    /// physical turn the run ran (a frame switch's follow-on turns
    /// included).
    ///
    /// The turn id is `input`'s trace id, else the scope id of the
    /// controller in `opts`; it is the accepted row's source key, so it
    /// names the run that executes the row. The shift runs whatever the
    /// session admits ahead of it first, a follow-on the head owes included.
    async fn execute_turn_frames(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AgentFrameRun, RuntimeError>;

    /// [`execute_turn_frames`](Self::execute_turn_frames), answering the run's
    /// terminal physical turn.
    async fn execute_turn(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AssembledTurn, RuntimeError>;

    /// Run `input` through the kernel's in-process turn entry, the one a
    /// child session's turn takes inside its parent's execution: a journaled
    /// acceptance step (ADR 0069 §6), then the session shift body up to the
    /// run that drove the accepted row, answering its terminal physical
    /// turn. The acceptance laws exercise it.
    async fn execute_child_session_turn(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AssembledTurn, RuntimeError>;

    /// Execute the session without sending anything: the first recorded
    /// admission of `request`, and the run it admits run to its terminal
    /// (the follow-on the head owes, an unfinished queued run, or the next
    /// pending input). Answers the run's physical turns, or `None` when
    /// admission admitted no run or the run ran no turn here.
    async fn execute_next_run(
        &mut self,
        request: &str,
        opts: TurnOptions<'_>,
    ) -> Result<Option<AgentFrameRun>, RuntimeError>;

    /// Run one queue run through recorded engine admission for tests that
    /// formerly called the direct queued drain. The command lane applies
    /// first as runs of its own that execution no turn (ADR 0101 §4), so the
    /// drain answers the first run admitted on turn-lane work.
    async fn execute_one_admitted_queued_run(
        &mut self,
        opts: TurnOptions<'_>,
    ) -> Result<QueuedTurnDrain<AssembledTurn>, RuntimeError>;
}

#[async_trait::async_trait]
impl TestTurnExecution for LashRuntime {
    async fn execute_one_admitted_queued_run(
        &mut self,
        opts: TurnOptions<'_>,
    ) -> Result<QueuedTurnDrain<AssembledTurn>, RuntimeError> {
        let controller = opts.scoped_effect_controller();
        let request = ShiftRequest {
            session: self.state().session_id.clone(),
            request: ShiftRequestId::new(opts.execution_scope_id()),
            intended_lane: None,
        };
        let mut ordinal = 0_u32;
        let mut rules = ShiftLoop::new();
        loop {
            let AdmitVerdict::Admit(admitted) =
                crate::shift::admit_shift(self, &controller, &request, ordinal, None)
                    .await
                    .map_err(crate::engine::ShiftAbort::into_error)?
            else {
                return Ok(QueuedTurnDrain::Empty(
                    EmptyQueuedDrainReason::AdmissionRefused(crate::AdmissionRefusal::Empty),
                ));
            };
            let work = admitted.work().clone();
            let sinks = crate::shift::ShiftSinks {
                events: opts.events_or_noop(),
                turn_events: opts.turn_events_or_noop(),
                local_stop: opts.local_stop().clone(),
                settled: &crate::runtime::shift::NoopRunSettledSink,
            };
            let report =
                crate::shift::execute_admitted_run_reporting(self, &controller, admitted, sinks)
                    .await
                    .map_err(crate::engine::ShiftAbort::into_error)?;
            if matches!(
                work,
                crate::engine::AdmittedWork::Commands { .. }
                    | crate::engine::AdmittedWork::Operation { .. }
            ) && rules.after(&work, &report.outcome).is_none()
            {
                ordinal += 1;
                continue;
            }
            return Ok(match report.run.and_then(AgentFrameRun::into_final_turn) {
                Some(turn) => QueuedTurnDrain::Ran(turn),
                None => QueuedTurnDrain::Empty(EmptyQueuedDrainReason::AdmissionRefused(
                    crate::AdmissionRefusal::Empty,
                )),
            });
        }
    }

    async fn execute_turn_frames(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AgentFrameRun, RuntimeError> {
        if self.services.store.is_none() {
            return self.stream_turn_with_agent_frames(input, opts).await;
        }
        let controller = opts.scoped_effect_controller();
        let turn_id = input
            .trace_turn_id
            .clone()
            .unwrap_or_else(|| TurnId::fixture(opts.execution_scope_id()));
        let accepted = self
            .enqueue_turn_input(
                input,
                crate::TurnInputIngress::next_turn(),
                Some(turn_id.as_str().to_owned()),
            )
            .await?;
        let acceptance = crate::TurnInputAcceptanceReceipt::from(&accepted);
        let aborted = |error: RuntimeError| error.with_turn_input_acceptance(acceptance.clone());
        let request = ShiftRequest {
            session: self.state().session_id.clone(),
            request: ShiftRequestId::new(format!("turn:{turn_id}")),
            intended_lane: None,
        };
        let sinks = crate::shift::ShiftSinks {
            events: opts.events_or_noop(),
            turn_events: opts.turn_events_or_noop(),
            local_stop: opts.local_stop().clone(),
            settled: &crate::runtime::shift::NoopRunSettledSink,
        };
        let mut rules = ShiftLoop::new();
        let mut ordinal = 0_u32;
        loop {
            let verdict = crate::shift::admit_shift(self, &controller, &request, ordinal, None)
                .await
                .map_err(|abort| aborted(abort.into_error()))?;
            let admitted = match verdict {
                AdmitVerdict::Admit(admitted) => admitted,
                AdmitVerdict::Parked(park) => {
                    return Err(aborted(RuntimeError::new(
                        RuntimeErrorCode::SessionRunPending,
                        format!(
                            "accepted turn input `{}` waits behind parked run `{}` (park {})",
                            accepted.input_id, park.run, park.park
                        ),
                    )));
                }
                stop => {
                    return Err(aborted(RuntimeError::new(
                        RuntimeErrorCode::AcceptedTurnInputCeded,
                        format!(
                            "accepted turn input `{}` was not executed: admission answered \
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
                        "accepted turn input `{}` was not executed: the shift stopped {stop:?}",
                        accepted.input_id
                    ),
                )));
            }
            let work = admitted.work().clone();
            let report = crate::shift::execute_admitted_run_reporting(
                self,
                &controller,
                admitted,
                sinks.clone(),
            )
            .await
            .map_err(|abort| aborted(abort.into_error()))?;
            if report.executed_inputs.contains(&accepted.input_id) {
                let mut run = report.run.ok_or_else(|| {
                    aborted(RuntimeError::new(
                        RuntimeErrorCode::AcceptedTurnInputCeded,
                        format!(
                            "the run that drove accepted turn input `{}` ran no turn here",
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
                        "accepted turn input `{}` was not executed: the shift stopped {stop:?}",
                        accepted.input_id
                    ),
                )));
            }
        }
    }

    async fn execute_child_session_turn(
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

    async fn execute_next_run(
        &mut self,
        request: &str,
        opts: TurnOptions<'_>,
    ) -> Result<Option<AgentFrameRun>, RuntimeError> {
        let controller = opts.scoped_effect_controller();
        let request = ShiftRequest {
            session: self.state().session_id.clone(),
            request: ShiftRequestId::new(request),
            intended_lane: None,
        };
        let AdmitVerdict::Admit(admitted) =
            crate::shift::admit_shift(self, &controller, &request, 0, None)
                .await
                .map_err(crate::engine::ShiftAbort::into_error)?
        else {
            return Ok(None);
        };
        let sinks = crate::shift::ShiftSinks {
            events: opts.events_or_noop(),
            turn_events: opts.turn_events_or_noop(),
            local_stop: opts.local_stop().clone(),
            settled: &crate::runtime::shift::NoopRunSettledSink,
        };
        let report =
            crate::shift::execute_admitted_run_reporting(self, &controller, admitted, sinks)
                .await
                .map_err(crate::engine::ShiftAbort::into_error)?;
        Ok(report.run)
    }

    async fn execute_turn(
        &mut self,
        input: TurnInput,
        opts: TurnOptions<'_>,
    ) -> Result<AssembledTurn, RuntimeError> {
        let run = self.execute_turn_frames(input, opts).await?;
        run.into_final_turn().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::AcceptedTurnInputCeded,
                "a executed run assembled no physical turn",
            )
        })
    }
}
