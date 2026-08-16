//! Facade shapes for an exact, host-selected queued-work drain.
//!
//! These mirror the core outcome and refusal vocabulary so a host never has to
//! reach into `lash_core` to interpret a selected drain.

use lash_core::facade_support::{
    SelectedQueuedWorkBatchSatisfaction as CoreSelectedQueuedWorkBatchSatisfaction,
    SelectedQueuedWorkDrainRefusalCause as CoreSelectedQueuedWorkDrainRefusalCause,
};

use crate::error::SelectedQueuedWorkDrainRefusalCause;

/// How one distinct requested batch ID satisfied a successful selected drain.
///
/// Missing rows are idempotent success. A present row that cannot join the
/// exact composition causes a pre-execution refusal, not a satisfaction value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SelectedQueuedWorkBatchSatisfaction {
    /// This invocation claimed and executed the durable row.
    ClaimedNow {
        /// Requested durable batch ID.
        batch_id: String,
    },
    /// No durable row remained, so the idempotent request was already done.
    AlreadySatisfied {
        /// Requested durable batch ID.
        batch_id: String,
    },
}

/// Successful result of an exact, host-selected queued-work drain.
///
/// Every distinct requested ID was either executed now or had no remaining
/// durable row. A present ID that cannot join the exact claim returns
/// [`EmbedError::SelectedQueuedWorkDrainRefused`](crate::EmbedError::SelectedQueuedWorkDrainRefused)
/// before selected execution.
#[derive(Clone, Debug)]
pub struct SelectedQueuedWorkDrainOutcome<T> {
    /// Executed turn, absent only for a fully satisfied drain with no selected turn.
    pub turn: Option<T>,
    /// One entry per distinct requested ID, ordered by first occurrence.
    pub satisfied: Vec<SelectedQueuedWorkBatchSatisfaction>,
}

impl<T> SelectedQueuedWorkDrainOutcome<T> {
    /// Reports whether this successful drain settled every requested ID without
    /// executing a selected turn.
    ///
    /// Refusals are errors, so `true` means every distinct ID was satisfied
    /// without a selected turn (or the selection was empty), never unclaimable.
    pub fn settled_without_selected_turn(&self) -> bool {
        self.turn.is_none()
    }

    /// Reports whether this successful drain executed a newly claimed turn.
    ///
    /// `false` has the same fully-satisfied meaning as
    /// [`Self::settled_without_selected_turn`].
    pub fn executed_selected_turn(&self) -> bool {
        self.turn.is_some()
    }

    /// Returns the turn or panics with `message` when the successful drain was
    /// fully satisfied without one.
    #[track_caller]
    pub fn expect(self, message: &str) -> T {
        self.turn.expect(message)
    }
}

/// Mirrors a core selected-drain outcome into the facade vocabulary.
pub(super) fn selected_drain_outcome<T>(
    outcome: lash_core::facade_support::SelectedQueuedWorkDrainOutcome<T>,
) -> SelectedQueuedWorkDrainOutcome<T> {
    SelectedQueuedWorkDrainOutcome {
        turn: outcome.turn,
        satisfied: outcome
            .satisfied
            .into_iter()
            .map(|satisfaction| match satisfaction {
                CoreSelectedQueuedWorkBatchSatisfaction::ClaimedNow { batch_id } => {
                    SelectedQueuedWorkBatchSatisfaction::ClaimedNow { batch_id }
                }
                CoreSelectedQueuedWorkBatchSatisfaction::AlreadySatisfied { batch_id } => {
                    SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied { batch_id }
                }
            })
            .collect(),
    }
}

/// Mirrors a core selected-drain refusal into the facade vocabulary.
pub(super) fn selected_drain_refusal_cause(
    cause: CoreSelectedQueuedWorkDrainRefusalCause,
) -> SelectedQueuedWorkDrainRefusalCause {
    match cause {
        CoreSelectedQueuedWorkDrainRefusalCause::UnclaimableTogether {
            unclaimed_batch_ids,
        } => SelectedQueuedWorkDrainRefusalCause::UnclaimableTogether {
            unclaimed_batch_ids,
        },
        CoreSelectedQueuedWorkDrainRefusalCause::InterruptedBatchRequiresFullComposition {
            required_batch_ids,
        } => SelectedQueuedWorkDrainRefusalCause::InterruptedBatchRequiresFullComposition {
            required_batch_ids,
        },
        CoreSelectedQueuedWorkDrainRefusalCause::ExecutionLaneBusy => {
            SelectedQueuedWorkDrainRefusalCause::ExecutionLaneBusy
        }
        CoreSelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow {
            batch_id,
            batch_enqueue_seq,
            required_context_tokens,
            max_context_tokens,
        } => SelectedQueuedWorkDrainRefusalCause::QueuedItemExceedsContextWindow {
            batch_id,
            batch_enqueue_seq,
            required_context_tokens,
            max_context_tokens,
        },
    }
}
