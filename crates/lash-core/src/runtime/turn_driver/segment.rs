//! A turn's segment boundary (FIG-4739): the quiet point at which a physical
//! turn ends so its logical run goes on in a new invocation.
//!
//! A boundary is taken only where a new machine can pick the run up from
//! committed history alone: when the machine asks for a model call after the
//! turn has already made one. Nothing is in flight there, the messages are a
//! prompt, and the continuation the commit owes starts from them with no
//! input of its own. A boundary is never proactive: it is taken because the
//! invocation's journal reached its budget, or because the build it runs on
//! asked the turn to hand over, and both answers are recorded facts, so a
//! replay ends the turn where its first execution did.

use super::*;

/// What a turn took at the segment boundary it ended at.
#[derive(Clone, Debug)]
pub(in crate::runtime) struct BoundaryTaken {
    /// The protocol iterations the run has spent through this turn, which
    /// the owed continuation records.
    pub(in crate::runtime) iterations: u64,
    /// The code cell the boundary stopped inside, which the continuation
    /// issues again.
    pub(in crate::runtime) cell: Option<crate::store::SuspendedCell>,
}

/// What a turn carries for its run's segment boundaries.
#[derive(Debug, Default)]
pub(in crate::runtime) struct TurnSegment {
    /// Whether this turn may end at a boundary. A turn that runs under no
    /// drive has nothing to recover its continuation, and a turn carrying
    /// work an earlier turn withheld for the run's follow-on must reach the
    /// commit that hands that work on.
    pub(in crate::runtime) allowed: bool,
    /// The protocol iterations earlier physical turns of the run spent: the
    /// run's turn budget counts on from them.
    pub(in crate::runtime) iterations_spent: u64,
    /// How many model calls this physical turn has asked for.
    model_calls: usize,
    /// The code cell the run's earlier turn stopped inside at its boundary:
    /// this turn's machine issues it again in place of its first model call.
    pub(in crate::runtime) resume: Option<crate::store::SuspendedCell>,
    /// Set when the turn ended at a boundary.
    pub(in crate::runtime) taken: Option<BoundaryTaken>,
}

impl TurnSegment {
    pub(in crate::runtime) fn new(
        allowed: bool,
        continuation: Option<&crate::store::RunContinuation>,
    ) -> Self {
        Self {
            allowed,
            iterations_spent: continuation.map_or(0, |owed| owed.protocol_iterations),
            model_calls: 0,
            resume: continuation.and_then(|owed| owed.cell.clone()),
            taken: None,
        }
    }

    /// The iterations the run has spent once this turn ends at `iteration`.
    fn spent_through(&self, iteration: usize, run_offset: usize) -> u64 {
        self.iterations_spent
            .saturating_add(iteration.saturating_sub(run_offset) as u64)
    }

    /// The turn budget the run has left for this physical turn.
    pub(super) fn remaining_budget(&self, budget: crate::TurnBudget) -> crate::TurnBudget {
        match budget.max_turns() {
            Some(max_turns) => crate::TurnBudget::bounded(
                max_turns
                    .saturating_sub(usize::try_from(self.iterations_spent).unwrap_or(usize::MAX))
                    .max(1),
            ),
            None => budget,
        }
    }
}

impl RuntimeTurnDriver<'_> {
    /// Ends the turn at a segment boundary when one is wanted at this quiet
    /// point: the machine is about to ask the model again. `true` when the
    /// machine was finished on the boundary and the model call is not made.
    pub(super) async fn end_at_segment_boundary(
        &mut self,
        machine: &mut TurnMachine,
        run_offset: usize,
    ) -> Result<bool, RuntimeError> {
        if !self.segment.allowed {
            return Ok(false);
        }
        self.segment.model_calls += 1;
        if self.segment.model_calls == 1 {
            // The turn has done nothing yet: a boundary here would hand over
            // an identical turn.
            return Ok(false);
        }
        let iteration = machine.protocol_iteration();
        let controller = self.scoped_effect_controller.controller();
        let reason = match controller.wants_segment_boundary(&crate::SegmentProgress {
            effects_executed: self.scoped_effect_controller.effects_executed(),
            journaled_bytes_estimate: None,
        }) {
            Some(reason) => Some(reason),
            None if controller.hands_over_turns() && self.observe_drain_mark(iteration).await? => {
                Some(crate::BoundaryReason::HandOver)
            }
            None => None,
        };
        let Some(reason) = reason else {
            return Ok(false);
        };
        self.segment.taken = Some(BoundaryTaken {
            iterations: self.segment.spent_through(iteration, run_offset),
            cell: None,
        });
        machine.finish_with_outcome(TurnOutcome::SegmentBoundary { reason });
        Ok(true)
    }

    /// Whether the cells this turn runs may hand their durable waits to the
    /// run's successor segment: the turn may end at a boundary, and its
    /// engine moves turns off a draining build.
    pub(super) fn cells_hand_over(&self) -> bool {
        self.segment.allowed
            && self
                .scoped_effect_controller
                .controller()
                .hands_over_turns()
    }

    /// Ends the turn at the segment boundary a code cell stopped at inside
    /// itself: the wait it was parked on was handed to the run's successor
    /// segment, and the cell captured itself there. The machine is still
    /// waiting on the cell; the owed continuation records that work, and the
    /// successor's machine waits on it again.
    pub(super) fn end_inside_cell(
        &mut self,
        machine: &mut TurnMachine,
        run_offset: usize,
    ) -> Result<(), RuntimeError> {
        let Some((language, code, driver_state)) = machine.waiting_exec() else {
            return Err(RuntimeError::new(
                RuntimeErrorCode::ExecutionStateCaptureFailed,
                "a code cell stopped at a segment boundary the turn was not waiting on",
            ));
        };
        if !self.segment.allowed {
            return Err(RuntimeError::new(
                RuntimeErrorCode::ExecutionStateCaptureFailed,
                "a code cell stopped at a segment boundary in a turn that takes none",
            ));
        }
        let cell = crate::store::SuspendedCell {
            language: language.to_owned(),
            code: code.to_owned(),
            driver_plugin_id: driver_state.plugin_id.clone(),
            driver_state: driver_state.payload.clone(),
        };
        self.segment.taken = Some(BoundaryTaken {
            iterations: self
                .segment
                .spent_through(machine.protocol_iteration(), run_offset),
            cell: Some(cell),
        });
        machine.finish_with_outcome(TurnOutcome::SegmentBoundary {
            reason: crate::BoundaryReason::HandOver,
        });
        Ok(())
    }

    /// Starts the machine at the code cell the run's earlier turn stopped
    /// inside, when this turn continues one.
    pub(super) fn resume_suspended_cell(&mut self, machine: &mut TurnMachine) {
        if let Some(cell) = self.segment.resume.take() {
            // The resumed cell is this turn's work: the model call after it
            // is a quiet point like any other.
            self.segment.model_calls = 1;
            machine.resume_with(crate::sansio::PendingWork::Exec {
                language: cell.language,
                code: cell.code,
                driver_state: crate::ProtocolDriverState::new(
                    cell.driver_plugin_id,
                    cell.driver_state,
                ),
            });
        }
    }

    /// The turn's recorded read of its build's drain mark at the quiet point
    /// before model call `iteration`. A turn under no drive root runs on no
    /// build of its own and reads nothing.
    async fn observe_drain_mark(&self, iteration: usize) -> Result<bool, RuntimeError> {
        let Some(generation) = self.drive_generation.clone() else {
            return Ok(false);
        };
        let controller = &self.scoped_effect_controller;
        let step = format!("drain-mark:{}:{iteration}", self.turn_id);
        let invocation = crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(controller.execution_scope().clone(), step.clone())?,
            crate::RuntimeAttribution {
                session_id: Some(self.session_id.clone()),
                turn_id: Some(self.turn_id.clone()),
                turn_index: None,
                protocol_iteration: None,
            },
            step,
        );
        let outcome = controller
            .execute_effect(
                crate::RuntimeEffectEnvelope::new(
                    invocation,
                    crate::RuntimeEffectCommand::ObserveDrainMark {
                        generation: generation.clone(),
                    },
                ),
                lash_core_execution::core_internal::owned_runner_executor(
                    Box::new(ObserveDrainMarkRunner {
                        marks: self.host.core.backend().generation_drain(),
                        generation,
                    }),
                    None,
                ),
            )
            .await
            .map_err(crate::RuntimeEffectControllerError::into_runtime_error)?;
        match outcome {
            crate::RuntimeEffectOutcome::ObserveDrainMark { draining } => Ok(draining),
            other => Err(crate::RuntimeEffectControllerError::wrong_outcome(
                crate::RuntimeEffectKind::ObserveDrainMark,
                other.kind(),
            )
            .into_runtime_error()),
        }
    }
}

/// The first execution of a turn's drain-mark read: the same mark a drive's
/// admission and the recovery leader's hand-over duty read (ADR 0106 §1).
struct ObserveDrainMarkRunner {
    marks: Arc<dyn crate::store::generation_drain::GenerationDrainStore>,
    generation: crate::engine::BuildGeneration,
}

#[async_trait::async_trait]
impl crate::runtime::effect::executor::RuntimeEffectLocalRunner for ObserveDrainMarkRunner {
    async fn execute(
        self: Box<Self>,
        envelope: crate::RuntimeEffectEnvelope,
        _usage_run: Option<crate::UsageRun>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let crate::RuntimeEffectCommand::ObserveDrainMark { generation } = &envelope.command else {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "drain mark reader received another command",
            ));
        };
        if *generation != self.generation {
            return Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch,
                "drain mark reader was bound to another generation",
            ));
        }
        let draining = self
            .marks
            .draining_generations()
            .await
            .map_err(|error| {
                crate::RuntimeEffectControllerError::from(
                    crate::runtime::runtime_error_from_store_commit(error),
                )
                .retryable_uncommitted_derivation()
            })?
            .iter()
            .any(|marked| marked.generation == self.generation);
        Ok(crate::RuntimeEffectOutcome::ObserveDrainMark { draining })
    }
}
