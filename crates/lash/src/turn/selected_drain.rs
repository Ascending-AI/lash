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

/// Mirrors a core empty-drain reason into the facade vocabulary.
pub(super) fn empty_drain_reason(
    reason: lash_core::facade_support::EmptyQueuedDrainReason,
) -> crate::turn::EmptyQueuedDrainReason {
    match reason {
        lash_core::facade_support::EmptyQueuedDrainReason::ExecutionLaneBusy => {
            crate::turn::EmptyQueuedDrainReason::ExecutionLaneBusy
        }
        lash_core::facade_support::EmptyQueuedDrainReason::NoDurableQueue => {
            crate::turn::EmptyQueuedDrainReason::NoDurableQueue
        }
        lash_core::facade_support::EmptyQueuedDrainReason::ClaimRefused(refusal) => {
            crate::turn::EmptyQueuedDrainReason::ClaimRefused(refusal)
        }
    }
}

/// Mirrors a core automatic-drain result into the facade vocabulary.
pub(super) fn queued_turn_drain<T>(
    drain: lash_core::facade_support::QueuedTurnDrain<T>,
) -> crate::turn::QueuedTurnDrain<T> {
    match drain {
        lash_core::facade_support::QueuedTurnDrain::Ran(turn) => {
            crate::turn::QueuedTurnDrain::Ran(turn)
        }
        lash_core::facade_support::QueuedTurnDrain::Empty(reason) => {
            crate::turn::QueuedTurnDrain::Empty(empty_drain_reason(reason))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::QueuedWorkClaimRefusal;

    /// Every empty-drain reason the core can report must survive the crossing
    /// into the facade unchanged. A reason that mutates here is the same bug as
    /// a reason that is never computed: the host decides on the wrong fact.
    ///
    /// [`QueuedWorkClaimRefusal::ClaimRaceLost`] is produced by a SQL backend
    /// that loses a claimed row to another writer between reading candidates and
    /// writing the claim. That race is not force-reproducible without a
    /// production test hook, so it is pinned here, at the seam it crosses.
    #[test]
    fn every_empty_drain_reason_crosses_the_facade_unchanged() {
        use crate::turn::EmptyQueuedDrainReason as Facade;
        use lash_core::facade_support::EmptyQueuedDrainReason as Core;

        let cases = [
            (Core::ExecutionLaneBusy, Facade::ExecutionLaneBusy),
            (Core::NoDurableQueue, Facade::NoDurableQueue),
            (
                Core::ClaimRefused(QueuedWorkClaimRefusal::ZeroLimit),
                Facade::ClaimRefused(QueuedWorkClaimRefusal::ZeroLimit),
            ),
            (
                Core::ClaimRefused(QueuedWorkClaimRefusal::Empty),
                Facade::ClaimRefused(QueuedWorkClaimRefusal::Empty),
            ),
            (
                Core::ClaimRefused(QueuedWorkClaimRefusal::NotYetAvailable),
                Facade::ClaimRefused(QueuedWorkClaimRefusal::NotYetAvailable),
            ),
            (
                Core::ClaimRefused(QueuedWorkClaimRefusal::CommandAtHead),
                Facade::ClaimRefused(QueuedWorkClaimRefusal::CommandAtHead),
            ),
            (
                Core::ClaimRefused(QueuedWorkClaimRefusal::DeliveryBoundaryBlocked),
                Facade::ClaimRefused(QueuedWorkClaimRefusal::DeliveryBoundaryBlocked),
            ),
            (
                Core::ClaimRefused(QueuedWorkClaimRefusal::HeadWithheld),
                Facade::ClaimRefused(QueuedWorkClaimRefusal::HeadWithheld),
            ),
            (
                Core::ClaimRefused(QueuedWorkClaimRefusal::ClaimRaceLost),
                Facade::ClaimRefused(QueuedWorkClaimRefusal::ClaimRaceLost),
            ),
        ];
        for (core, expected) in cases {
            assert_eq!(empty_drain_reason(core), expected);
            let crossed =
                queued_turn_drain::<()>(lash_core::facade_support::QueuedTurnDrain::Empty(core));
            assert!(
                matches!(crossed, crate::turn::QueuedTurnDrain::Empty(reason) if reason == expected),
                "empty drain reason changed crossing the facade: {core:?}"
            );
        }
        assert_eq!(
            queued_turn_drain(lash_core::facade_support::QueuedTurnDrain::Ran(7)).ran(),
            Some(7)
        );
    }
}
