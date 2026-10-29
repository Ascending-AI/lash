//! The turn's iteration reduction, independent of protocol history.

use super::{DriverAction, DriverContextView, SessionHistoryRecord, TurnProtocol};

/// Whether the protocol's completed unit committed successful execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IterationProgress {
    Productive,
    Unproductive,
}

/// Core's admission decision after one nonterminal model iteration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContinueDecision {
    AfterWork,
    TurnLimit,
    NoProgressLimit { consecutive: usize },
}

/// The result of the reduction, consumed once by the turn machine.
pub struct IterationContinuation {
    pub(super) next_iteration: usize,
    pub(super) consecutive_unproductive: usize,
    pub(super) decision: ContinueDecision,
}

impl IterationContinuation {
    pub(super) fn finish(
        iteration: usize,
        run_offset: usize,
        consecutive: usize,
        progress: IterationProgress,
        turns: lash_protocol_types::TurnBudget,
        no_progress: lash_protocol_types::NoProgressBudget,
    ) -> Self {
        let next_iteration = iteration + 1;
        let consecutive_unproductive = match progress {
            IterationProgress::Productive => 0,
            IterationProgress::Unproductive => consecutive.saturating_add(1),
        };
        let decision = if turns
            .max_turns()
            .is_some_and(|limit| next_iteration.saturating_sub(run_offset) >= limit)
        {
            ContinueDecision::TurnLimit
        } else if no_progress.is_exhausted_by(consecutive_unproductive) {
            ContinueDecision::NoProgressLimit {
                consecutive: consecutive_unproductive,
            }
        } else {
            ContinueDecision::AfterWork
        };
        Self {
            next_iteration,
            consecutive_unproductive,
            decision,
        }
    }
}

impl<M: TurnProtocol> DriverContextView<'_, M> {
    /// Finish a nonterminal iteration. The protocol renders feedback only after
    /// core admits continuation or selects a no-progress diagnostic. Turn-limit
    /// exhaustion is silent and never calls the renderer.
    pub fn finish_iteration(
        &self,
        progress: IterationProgress,
        feedback: impl FnOnce(ContinueDecision) -> Vec<SessionHistoryRecord<M::Event>>,
    ) -> DriverAction<M> {
        let continuation = IterationContinuation::finish(
            self.protocol_iteration,
            self.protocol_run_offset,
            self.consecutive_unproductive,
            progress,
            self.config.turn_budget,
            self.config.no_progress_budget,
        );
        let feedback = match continuation.decision {
            ContinueDecision::TurnLimit => Vec::new(),
            decision => feedback(decision),
        };
        DriverAction::FinishIteration {
            continuation,
            feedback,
        }
    }
}
