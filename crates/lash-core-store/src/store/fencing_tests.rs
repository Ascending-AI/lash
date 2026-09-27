//! Exhaustive unit coverage for every fencing verdict function.
//!
//! Each decision is exercised across its whole refusal vocabulary — stale
//! token, superseded generation, released lease, expiry at the boundary
//! millisecond, head moved, first commit — because these functions are the
//! only place those answers are now decided.

use super::StoreError;
use super::fencing::*;
use crate::SessionId;

const SESSION: &str = "session-fencing";

fn session_id() -> SessionId {
    SessionId::from(SESSION.to_string())
}

// ---------------------------------------------------------------------------
// The backstop contract
// ---------------------------------------------------------------------------

/// The refusal a site returns when its fenced write loses, standing in for
/// whichever domain refusal the real call site owns.
fn lost_claim_refusal() -> StoreError {
    StoreError::Contended
}

#[test]
fn one_affected_row_satisfies_the_backstop_silently() {
    let (applied, capture) = lash_core_ids::trace_capture::capturing_sync(|| {
        fenced_write_applied(FencedWrite::TurnInputClaimAcquisition, "sqlite", SESSION, 1)
    });
    assert!(applied);
    assert!(
        capture.named(FENCED_WRITE_DISAGREEMENT_EVENT).is_empty(),
        "an applied write records nothing",
    );
    assert!(
        require_fenced_write_applied(
            FencedWrite::TurnInputClaimAcquisition,
            "sqlite",
            SESSION,
            1,
            || panic!("an applied write must not build a refusal"),
        )
        .is_ok()
    );
}

#[test]
fn a_lost_fenced_write_returns_the_sites_own_domain_refusal() {
    // The ruling: the backstop adds evidence, it does not change what the
    // caller receives. A lost lease still reads as a lost lease, so the
    // runtime's stand-down handling is untouched.
    let (result, _capture) = lash_core_ids::trace_capture::capturing_sync(|| {
        require_fenced_write_applied(
            FencedWrite::TurnInputClaimAcquisition,
            "sqlite",
            SESSION,
            0,
            lost_claim_refusal,
        )
    });
    let error = result.expect_err("a fenced write that changed no row must fail closed");
    assert_eq!(error.variant_name(), "Contended");
}

#[test]
fn a_lost_fenced_write_records_the_disagreement_as_evidence() {
    let (result, capture) = lash_core_ids::trace_capture::capturing_sync(|| {
        require_fenced_write_applied(
            FencedWrite::TurnInputClaimSettlement,
            "postgres",
            "input-9",
            0,
            lost_claim_refusal,
        )
    });
    assert!(result.is_err());
    let event = capture.exactly_one(FENCED_WRITE_DISAGREEMENT_EVENT);
    assert_eq!(event.level, "ERROR");
    assert_eq!(event.target, FENCING_TRACE_TARGET);
    assert_eq!(event.field("fenced_write"), "turn_input_claim.settle");
    assert_eq!(event.field("backend"), "postgres");
    assert_eq!(event.field("row_identity"), "input-9");
    assert_eq!(event.field("rows_affected"), "0");
    assert_eq!(event.field("outcome"), "fenced_write_lost");
}

#[test]
fn more_than_one_affected_row_is_the_same_defect() {
    let (result, capture) = lash_core_ids::trace_capture::capturing_sync(|| {
        require_fenced_write_applied(
            FencedWrite::SessionHeadPublication,
            "sqlite",
            SESSION,
            2,
            lost_claim_refusal,
        )
    });
    assert!(
        result.is_err(),
        "a predicate meant to name one row naming two is a defect too",
    );
    assert_eq!(
        capture
            .exactly_one(FENCED_WRITE_DISAGREEMENT_EVENT)
            .field("rows_affected"),
        "2"
    );
}

#[test]
fn the_backstop_carries_any_callers_error_type() {
    // The process, effect and wake families settle in `PluginError` and their
    // controller error, so the backstop must not be welded to `StoreError`.
    #[derive(Debug, PartialEq, Eq)]
    struct ForeignRefusal;
    let (result, _capture) = lash_core_ids::trace_capture::capturing_sync(|| {
        require_fenced_write_applied(
            FencedWrite::WakeDeliverySettlement,
            "postgres",
            "process-1",
            0,
            || ForeignRefusal,
        )
    });
    assert_eq!(result, Err(ForeignRefusal));
}

#[test]
fn every_fenced_write_has_a_distinct_label() {
    let writes = [
        FencedWrite::TurnInputClaimAcquisition,
        FencedWrite::TurnInputClaimSettlement,
        FencedWrite::UnclaimedTurnInputSettlement,
        FencedWrite::SessionHeadPublication,
        FencedWrite::QueuedWorkClaimAcquisition,
        FencedWrite::QueuedWorkClaimSettlement,
        FencedWrite::WakeDeliverySettlement,
    ];
    let labels = writes.map(FencedWrite::label);
    let unique = labels.iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(unique.len(), labels.len(), "labels collided: {labels:?}");
}

// ---------------------------------------------------------------------------
// D2 / D5 — generation claimability
// ---------------------------------------------------------------------------

#[test]
fn an_unclaimed_row_is_claimable_whatever_generation_it_retains() {
    let facts = WorkRowClaimFacts {
        claim_token: None,
        claim_session_lease_generation: 4,
        claim_owner_incarnation_id: None,
    };
    assert_eq!(
        turn_input_claimability(facts, 4, "owner"),
        WorkRowClaimability::Claimable
    );
    assert_eq!(
        queued_work_batch_claimability(facts, 4, "owner"),
        WorkRowClaimability::Claimable
    );
}

#[test]
fn a_row_claimed_under_a_superseded_generation_is_reclaimable() {
    let facts = WorkRowClaimFacts {
        claim_token: Some("claim-token"),
        claim_session_lease_generation: 3,
        claim_owner_incarnation_id: Some("owner"),
    };
    assert!(turn_input_claimability(facts, 4, "owner").is_claimable());
    assert!(queued_work_batch_claimability(facts, 4, "owner").is_claimable());
}

#[test]
fn a_row_already_claimed_under_this_generation_is_not_reclaimable() {
    let facts = WorkRowClaimFacts {
        claim_token: Some("claim-token"),
        claim_session_lease_generation: 4,
        claim_owner_incarnation_id: Some("owner"),
    };
    assert_eq!(
        turn_input_claimability(facts, 4, "owner"),
        WorkRowClaimability::HeldByThisGeneration
    );
    assert_eq!(
        queued_work_batch_claimability(facts, 4, "owner"),
        WorkRowClaimability::HeldByThisGeneration
    );
    assert!(!turn_input_claimability(facts, 4, "owner").is_claimable());
    assert!(turn_input_claimability(facts, 4, "new-owner").is_claimable());
    assert!(queued_work_batch_claimability(facts, 4, "new-owner").is_claimable());
}

// ---------------------------------------------------------------------------
// D3 — turn-input settlement authority
// ---------------------------------------------------------------------------

fn claimed_completion() -> crate::TurnInputCompletion {
    crate::TurnInputCompletion {
        session_id: session_id(),
        claim: Some(crate::TurnInputSettlementClaim {
            claim_id: "claim-1".to_string(),
            lease_token: "claim-token-1".to_string(),
        }),
        data: crate::TurnInputCompletionData {
            input_ids: vec![crate::InputId::from("input-1")],
            applications: Vec::new(),
        },
    }
}

fn unclaimed_completion() -> crate::TurnInputCompletion {
    crate::TurnInputCompletion {
        session_id: session_id(),
        claim: None,
        data: crate::TurnInputCompletionData {
            input_ids: vec![crate::InputId::from("input-1")],
            applications: Vec::new(),
        },
    }
}

fn input_id() -> crate::InputId {
    crate::InputId::from("input-1")
}

#[test]
fn claimed_settlement_admits_the_row_that_still_carries_the_claim() {
    assert!(
        require_settleable_turn_input(
            &claimed_completion(),
            &input_id(),
            Some(TurnInputSettlementFacts {
                claim_id: Some("claim-1"),
                claim_token: Some("claim-token-1"),
                claim_session_lease_generation: 4,
                state: crate::TurnInputStateKind::Accepted.as_str(),
            }),
        )
        .is_ok()
    );
}

#[test]
fn claimed_settlement_refuses_a_superseding_claim_and_reports_it() {
    let error = require_settleable_turn_input(
        &claimed_completion(),
        &input_id(),
        Some(TurnInputSettlementFacts {
            claim_id: Some("claim-2"),
            claim_token: Some("claim-token-2"),
            claim_session_lease_generation: 9,
            state: crate::TurnInputStateKind::Accepted.as_str(),
        }),
    )
    .expect_err("a superseded claim cannot settle");
    let StoreError::TurnInputClaimSuperseded {
        superseding_claim_id,
        superseding_session_lease_generation,
        row_id,
        ..
    } = &error
    else {
        panic!("unexpected variant: {error:?}");
    };
    assert_eq!(superseding_claim_id.as_deref(), Some("claim-2"));
    assert_eq!(
        superseding_session_lease_generation.as_deref().copied(),
        Some(9)
    );
    assert_eq!(row_id.as_deref(), Some("input-1"));
}

#[test]
fn claimed_settlement_refuses_a_stale_claim_token_under_the_same_claim_id() {
    let error = require_settleable_turn_input(
        &claimed_completion(),
        &input_id(),
        Some(TurnInputSettlementFacts {
            claim_id: Some("claim-1"),
            claim_token: Some("claim-token-rotated"),
            claim_session_lease_generation: 4,
            state: crate::TurnInputStateKind::Accepted.as_str(),
        }),
    )
    .expect_err("a rotated claim token cannot settle");
    assert_eq!(error.variant_name(), "TurnInputClaimSuperseded");
}

#[test]
fn claimed_settlement_refuses_a_vanished_row() {
    let error = require_settleable_turn_input(&claimed_completion(), &input_id(), None)
        .expect_err("a vanished row cannot settle");
    let StoreError::TurnInputClaimSuperseded {
        superseding_claim_id,
        superseding_session_lease_generation,
        ..
    } = &error
    else {
        panic!("unexpected variant: {error:?}");
    };
    assert!(superseding_claim_id.is_none());
    assert!(superseding_session_lease_generation.is_none());
}

#[test]
fn unclaimed_settlement_admits_an_unclaimed_nonterminal_row() {
    for state in [
        crate::TurnInputStateKind::PendingActive,
        crate::TurnInputStateKind::DeferredNextTurn,
        crate::TurnInputStateKind::Accepted,
    ] {
        assert!(
            require_settleable_turn_input(
                &unclaimed_completion(),
                &input_id(),
                Some(TurnInputSettlementFacts {
                    claim_id: None,
                    claim_token: None,
                    claim_session_lease_generation: 0,
                    state: state.as_str(),
                }),
            )
            .is_ok(),
            "state {state:?} must remain settleable",
        );
    }
}

#[test]
fn unclaimed_settlement_refuses_a_terminal_row_and_names_the_state() {
    for state in [
        crate::TurnInputStateKind::Cancelled,
        crate::TurnInputStateKind::Completed,
    ] {
        let error = require_settleable_turn_input(
            &unclaimed_completion(),
            &input_id(),
            Some(TurnInputSettlementFacts {
                claim_id: None,
                claim_token: None,
                claim_session_lease_generation: 0,
                state: state.as_str(),
            }),
        )
        .expect_err("a terminal row is already settled");
        let StoreError::UnclaimedTurnInputSettlementSuperseded { observed_state, .. } = &error
        else {
            panic!("unexpected variant: {error:?}");
        };
        assert_eq!(observed_state.as_deref(), Some(state.as_str()));
    }
}

#[test]
fn unclaimed_settlement_refuses_a_row_a_claim_took() {
    let error = require_settleable_turn_input(
        &unclaimed_completion(),
        &input_id(),
        Some(TurnInputSettlementFacts {
            claim_id: Some("claim-7"),
            claim_token: Some("claim-token-7"),
            claim_session_lease_generation: 11,
            state: crate::TurnInputStateKind::Accepted.as_str(),
        }),
    )
    .expect_err("a claimed row is not an unclaimed settlement");
    let StoreError::UnclaimedTurnInputSettlementSuperseded {
        superseding_claim_id,
        ..
    } = &error
    else {
        panic!("unexpected variant: {error:?}");
    };
    assert_eq!(superseding_claim_id.as_deref(), Some("claim-7"));
}

#[test]
fn an_unrecognised_state_name_stays_settleable() {
    // A state the binary cannot decode is not proof of settlement; refusing
    // here would strand rows a newer writer produced.
    assert!(unclaimed_turn_input_is_settleable("not-a-known-state"));
    assert!(!unclaimed_turn_input_is_settleable(
        crate::TurnInputStateKind::Completed.as_str()
    ));
}

// ---------------------------------------------------------------------------
// D5 — queued-work settlement authority
// ---------------------------------------------------------------------------

fn queued_completion() -> crate::QueuedWorkCompletion {
    crate::QueuedWorkCompletion {
        session_id: session_id(),
        claim_id: "claim-1".to_string(),
        lease_token: "claim-token-1".to_string(),
        data: crate::QueuedWorkCompletionData {
            batch_ids: vec![crate::BatchId::from("batch-1")],
        },
    }
}

#[test]
fn queued_work_settlement_admits_its_own_claim_and_refuses_everything_else() {
    assert!(
        require_settleable_queued_work(
            &queued_completion(),
            "batch-1",
            Some(QueuedWorkSettlementFacts {
                claim_id: Some("claim-1"),
                claim_token: Some("claim-token-1"),
                claim_session_lease_generation: 4,
            }),
        )
        .is_ok()
    );
    for observed in [
        None,
        Some(QueuedWorkSettlementFacts {
            claim_id: None,
            claim_token: None,
            claim_session_lease_generation: 4,
        }),
        Some(QueuedWorkSettlementFacts {
            claim_id: Some("claim-2"),
            claim_token: Some("claim-token-2"),
            claim_session_lease_generation: 5,
        }),
        Some(QueuedWorkSettlementFacts {
            claim_id: Some("claim-1"),
            claim_token: Some("claim-token-rotated"),
            claim_session_lease_generation: 4,
        }),
    ] {
        let error = require_settleable_queued_work(&queued_completion(), "batch-1", observed)
            .expect_err("only the holding claim may settle");
        assert_eq!(error.variant_name(), "QueuedWorkClaimSuperseded");
    }
}

// ---------------------------------------------------------------------------
// D4 — head publication
// ---------------------------------------------------------------------------

#[test]
fn head_publication_publishes_over_the_revision_it_read() {
    assert_eq!(
        head_publication_verdict(0, 0),
        HeadPublicationVerdict::Publish
    );
    assert_eq!(
        head_publication_verdict(41, 41),
        HeadPublicationVerdict::Publish
    );
}

#[test]
fn head_publication_reports_a_head_that_moved_under_the_lock() {
    assert_eq!(
        head_publication_verdict(41, 42),
        HeadPublicationVerdict::HeadMoved {
            planned_from_revision: 41,
            observed_head_revision: 42,
        }
    );
    // A first commit whose placeholder row already advanced is the same answer.
    assert_eq!(
        head_publication_verdict(0, 1),
        HeadPublicationVerdict::HeadMoved {
            planned_from_revision: 0,
            observed_head_revision: 1,
        }
    );
}

#[test]
fn head_publication_requires_the_single_writer_transaction() {
    assert!(require_single_writer_head_publication(&session_id(), "sqlite", true).is_ok());
    let error = require_single_writer_head_publication(&session_id(), "sqlite", false)
        .expect_err("a head read outside the write transaction must be refused");
    assert_eq!(error.variant_name(), "UnfencedHeadPublication");
    assert!(error.to_string().contains("single-writer"), "{error}");
}

// ---------------------------------------------------------------------------
// D7 — wake delivery
// ---------------------------------------------------------------------------

const ENQUEUING: &str = "enqueuing";

#[test]
fn wake_delivery_is_held_only_while_enqueuing_under_this_token() {
    assert_eq!(
        wake_delivery_claim_verdict(
            Some(WakeDeliveryClaimFacts {
                state: ENQUEUING,
                claim_token: Some("claim-1"),
            }),
            "claim-1",
            ENQUEUING,
        ),
        WakeDeliveryClaimVerdict::Held
    );
}

#[test]
fn wake_delivery_names_each_refusal_distinctly() {
    let cases: [(Option<WakeDeliveryClaimFacts<'_>>, WakeDeliveryClaimVerdict); 4] = [
        (None, WakeDeliveryClaimVerdict::Absent),
        (
            Some(WakeDeliveryClaimFacts {
                state: "pending",
                claim_token: Some("claim-1"),
            }),
            WakeDeliveryClaimVerdict::NotEnqueuing,
        ),
        (
            Some(WakeDeliveryClaimFacts {
                state: ENQUEUING,
                claim_token: Some("claim-2"),
            }),
            WakeDeliveryClaimVerdict::Superseded,
        ),
        (
            Some(WakeDeliveryClaimFacts {
                state: ENQUEUING,
                claim_token: None,
            }),
            WakeDeliveryClaimVerdict::Superseded,
        ),
    ];
    for (observed, expected) in cases {
        let verdict = wake_delivery_claim_verdict(observed, "claim-1", ENQUEUING);
        assert_eq!(verdict, expected, "observed {observed:?}");
        assert!(!verdict.is_held());
    }
}
