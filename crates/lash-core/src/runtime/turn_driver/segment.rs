//! A turn's segment boundary (FIG-4739): the quiet point at which a physical
//! turn ends so its logical run goes on in a new invocation.
//!
//! A boundary is taken before a later model call, after every tool dispatch
//! has settled and the Run owns its pending completions, or when a drain's
//! frozen Run refuses a tool round's admission (FIG-5075). The successor picks
//! up committed history and the owed waiting phase without repeating the
//! model call or dispatch: it admits a refused round from the calls the model
//! call recorded, and awaits an admitted one. A boundary is never proactive:
//! it is taken because the invocation's journal reached its budget, or
//! because the build it runs on asked the turn to hand over, and both answers
//! are recorded facts, so a replay ends the turn where its first execution
//! did.

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
    pub(in crate::runtime) opener: crate::store::RunOpenerState,
    pub(in crate::runtime) tools: Option<serde_json::Value>,
}

/// What a turn carries for its run's segment boundaries.
#[derive(Debug, Default)]
pub(in crate::runtime) struct TurnSegment {
    /// Whether this turn may end at a boundary. A turn that runs under no
    /// shift has nothing to recover its continuation, and a turn carrying
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
    resume_tools: Option<serde_json::Value>,
    /// Set when the turn ended at a boundary.
    pub(in crate::runtime) taken: Option<Box<BoundaryTaken>>,
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
            resume_tools: continuation.and_then(|owed| owed.tools.clone()),
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
    /// Ends the turn at the segment boundary a code cell stopped at inside
    /// itself: the wait it was parked on was handed to the run's successor
    /// segment, and the cell captured itself there. The machine is still
    /// waiting on the cell; the owed continuation records that work, and the
    /// successor's machine waits on it again.
    pub(super) async fn end_inside_cell(
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
        if let Some(owner) = &self.tool_run_owner {
            owner
                .capture_tool_run(crate::BoundaryReason::HandOver)
                .await
                .map_err(crate::RuntimeEffectControllerError::into_runtime_error)?;
        }
        let opener = self
            .opener_state
            .boundary_snapshot()
            .map_err(crate::RuntimeEffectControllerError::into_runtime_error)?;
        self.segment.taken = Some(Box::new(BoundaryTaken {
            iterations: self
                .segment
                .spent_through(machine.protocol_iteration(), run_offset),
            cell: Some(cell),
            opener,
            tools: None,
        }));
        machine.finish_with_outcome(TurnOutcome::SegmentBoundary {
            reason: crate::BoundaryReason::HandOver,
        });
        Ok(())
    }

    /// Resumes the code cell or tool round the Run's earlier turn suspended.
    pub(super) fn resume_suspended_cell(
        &mut self,
        machine: &mut TurnMachine,
    ) -> Result<(), RuntimeError> {
        if let Some(tools) = self.segment.resume_tools.take() {
            // A round the predecessor's Run refused at admission owes its
            // calls, and this turn admits them; an admitted round owes its
            // settled dispatch, and this turn awaits it (FIG-5075).
            let (calls, settled, expansion) = serde_json::from_value(tools).map_err(|error| {
                RuntimeError::new(
                    RuntimeErrorCode::ExecutionStateCaptureFailed,
                    error.to_string(),
                )
            })?;
            self.segment.model_calls = 1;
            machine.resume_with(crate::sansio::PendingWork::WaitingForToolResults {
                calls,
                settled,
                expansion,
            });
        }
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
        Ok(())
    }

    /// Ends the turn at a segment boundary while it waits on a tool round,
    /// owing the round to the successor: its unadmitted calls, or its settled
    /// dispatch state.
    pub(super) async fn end_waiting_for_tool_results(
        &mut self,
        machine: &mut TurnMachine,
        run_offset: usize,
        reason: crate::BoundaryReason,
    ) -> Result<(), RuntimeError> {
        let round = machine.waiting_tool_round().ok_or_else(|| {
            RuntimeError::new(
                RuntimeErrorCode::ExecutionStateCaptureFailed,
                "a tool round handed over while the turn waited on none",
            )
        })?;
        let tools = serde_json::to_value(round).map_err(|error| {
            RuntimeError::new(
                RuntimeErrorCode::ExecutionStateCaptureFailed,
                error.to_string(),
            )
        })?;
        if let Some(owner) = &self.tool_run_owner {
            owner
                .capture_tool_run(reason)
                .await
                .map_err(crate::RuntimeEffectControllerError::into_runtime_error)?;
        }
        self.segment.taken = Some(Box::new(BoundaryTaken {
            iterations: self
                .segment
                .spent_through(machine.protocol_iteration(), run_offset),
            cell: None,
            tools: Some(tools),
            opener: self
                .opener_state
                .boundary_snapshot()
                .map_err(crate::RuntimeEffectControllerError::into_runtime_error)?,
        }));
        machine.finish_with_outcome(TurnOutcome::SegmentBoundary { reason });
        Ok(())
    }
}
