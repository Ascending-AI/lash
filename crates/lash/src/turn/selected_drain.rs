//! Facade shapes for an exact, host-selected queued-work drain.
//!
//! These mirror the core outcome and refusal vocabulary so a host never has to
//! reach into `lash_core` to interpret a selected drain.

use lash_core::facade_support::{
    SelectedQueuedWorkBatchSatisfaction as CoreSelectedQueuedWorkBatchSatisfaction,
    SelectedQueuedWorkDrainRefusalCause as CoreSelectedQueuedWorkDrainRefusalCause,
};

use crate::error::SelectedQueuedWorkDrainRefusalCause;
use crate::turn::{SelectedQueuedWorkBatchSatisfaction, SelectedQueuedWorkDrainOutcome};

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
