use super::*;

pub(super) fn settle_claim_before_successor_reclaim() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::SettleClaimBeforeSuccessorReclaim,
        // FIG-641 / ADR 0029: supersession is reclaim-mediated by design.
        // All three backends currently accept this pre-reclaim settlement;
        // this differential demonstrates agreement, not a conformance law.
        operations: vec![
            StoreOperation::EnqueueNextTurnInput,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "first-owner",
            },
            StoreOperation::ClaimNextTurnInput {
                lease: LeaseSlot::First,
            },
            StoreOperation::ReleaseSessionLease {
                lease: LeaseSlot::First,
            },
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::Successor,
                owner: "successor-owner",
            },
            StoreOperation::CommitStaleTurnInputClaim {
                expected_head_revision: 0,
            },
        ],
    }
}

pub(super) fn queued_work_claim_and_abandon() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::QueuedWorkClaimAndAbandon,
        operations: vec![
            StoreOperation::EnqueueClaimableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "queued-work-owner",
            },
            StoreOperation::ClaimQueuedWork {
                lease: LeaseSlot::First,
            },
            StoreOperation::AbandonQueuedWorkClaim,
        ],
    }
}

pub(super) fn same_generation_exact_claim_deferral() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::SameGenerationExactClaimDeferral,
        // FIG-1065: an exact re-claim of a batch this generation already
        // holds must defer identically on every backend — no new claim,
        // no durable mutation.
        operations: vec![
            StoreOperation::EnqueueClaimableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "queued-work-owner",
            },
            StoreOperation::ClaimQueuedWork {
                lease: LeaseSlot::First,
            },
            StoreOperation::ClaimHeldBatchById {
                lease: LeaseSlot::First,
            },
        ],
    }
}

pub(super) fn queued_work_claim_superseded_after_reclaim() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::QueuedWorkClaimSupersededAfterReclaim,
        // FIG-1065: once the successor generation reclaims the row, the
        // stale claim's settlement must be refused identically on every
        // backend with no durable mutation.
        operations: vec![
            StoreOperation::EnqueueClaimableQueuedWork,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "queued-work-owner",
            },
            StoreOperation::ClaimQueuedWork {
                lease: LeaseSlot::First,
            },
            StoreOperation::RetainStaleClaims,
            StoreOperation::ReleaseSessionLease {
                lease: LeaseSlot::First,
            },
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::Successor,
                owner: "successor-owner",
            },
            StoreOperation::ClaimQueuedWork {
                lease: LeaseSlot::Successor,
            },
            StoreOperation::CommitStaleQueuedWorkClaim {
                expected_head_revision: 0,
            },
        ],
    }
}

pub(super) fn turn_input_claim_superseded_after_reclaim() -> GeneratedCase {
    GeneratedCase {
        name: CaseName::TurnInputClaimSupersededAfterReclaim,
        // FIG-1065: the turn-input counterpart — a stale claim settling
        // after the successor reclaimed the row is refused identically.
        operations: vec![
            StoreOperation::EnqueueNextTurnInput,
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::First,
                owner: "first-owner",
            },
            StoreOperation::ClaimNextTurnInput {
                lease: LeaseSlot::First,
            },
            StoreOperation::RetainStaleClaims,
            StoreOperation::ReleaseSessionLease {
                lease: LeaseSlot::First,
            },
            StoreOperation::AcquireSessionLease {
                slot: LeaseSlot::Successor,
                owner: "successor-owner",
            },
            StoreOperation::ClaimNextTurnInput {
                lease: LeaseSlot::Successor,
            },
            StoreOperation::CommitStaleTurnInputClaim {
                expected_head_revision: 0,
            },
        ],
    }
}
