//! Exhaustive unit coverage for every fencing verdict function.
//!
//! Each decision is exercised across its whole refusal vocabulary — stale
//! token, superseded generation, released lease, expiry at the boundary
//! millisecond, head moved, first commit — because these functions are the
//! only place those answers are now decided.

use super::StoreError;
use super::fencing::*;
use super::session_execution_lease::{
    LeaseOwnerIdentity, SessionExecutionLeaseAuthority, SessionExecutionLeaseFenceFacts,
    SessionExecutionLeaseRow, require_current_session_execution_lease,
};
use crate::SessionId;

const SESSION: &str = "session-fencing";
const FRESHNESS: &str = "unit_test_snapshot";

fn session_id() -> SessionId {
    SessionId::from(SESSION.to_string())
}

fn owner() -> LeaseOwnerIdentity {
    LeaseOwnerIdentity::opaque("worker-a", "boot-1")
}

fn authority() -> SessionExecutionLeaseAuthority {
    SessionExecutionLeaseAuthority {
        session_id: session_id(),
        owner: owner(),
        executor_id: "executor-1".to_string(),
        lease_token: "token-1".to_string(),
        fencing_token: 7,
    }
}

/// A held lease row as a locked read returns it.
fn row(
    owner: &LeaseOwnerIdentity,
    executor_id: &str,
    lease_token: &str,
    fencing_token: u64,
    expires_at_ms: u64,
) -> SessionExecutionLeaseRow {
    SessionExecutionLeaseRow {
        owner: Some(owner.clone()),
        executor_id: Some(executor_id.to_string()),
        lease_token: Some(lease_token.to_string()),
        fencing_token,
        claimed_at_ms: 500,
        lease_term_ms: 30_000,
        expires_at_ms,
    }
}

/// A released row: the generation is retained, every identity column is NULL.
fn released_row(fencing_token: u64) -> SessionExecutionLeaseRow {
    SessionExecutionLeaseRow {
        owner: None,
        executor_id: None,
        lease_token: None,
        fencing_token,
        claimed_at_ms: 0,
        lease_term_ms: 0,
        expires_at_ms: 0,
    }
}

/// The same held row borrowed as the execution fence's fact view.
fn facts<'a>(
    owner: &'a LeaseOwnerIdentity,
    executor_id: &'a str,
    lease_token: &'a str,
    fencing_token: u64,
    expires_at_epoch_ms: u64,
) -> SessionExecutionLeaseFenceFacts<'a> {
    SessionExecutionLeaseFenceFacts {
        owner: Some(owner),
        executor_id: Some(executor_id),
        lease_token: Some(lease_token),
        fencing_token,
        expires_at_epoch_ms,
    }
}

// ---------------------------------------------------------------------------
// The backstop contract
// ---------------------------------------------------------------------------

/// The refusal a site returns when its fenced write loses, standing in for
/// whichever domain refusal the real call site owns.
fn lost_lease_refusal() -> StoreError {
    StoreError::SessionExecutionLeaseRenewalRefused {
        session_id: session_id(),
    }
}

#[test]
fn one_affected_row_satisfies_the_backstop_silently() {
    let (applied, capture) = lash_core_ids::trace_capture::capturing_sync(|| {
        fenced_write_applied(
            FencedWrite::SessionExecutionLeaseRenewal,
            "sqlite",
            SESSION,
            1,
        )
    });
    assert!(applied);
    assert!(
        capture.named(FENCED_WRITE_DISAGREEMENT_EVENT).is_empty(),
        "an applied write records nothing",
    );
    assert!(
        require_fenced_write_applied(
            FencedWrite::SessionExecutionLeaseRenewal,
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
            FencedWrite::SessionExecutionLeaseRenewal,
            "sqlite",
            SESSION,
            0,
            lost_lease_refusal,
        )
    });
    let error = result.expect_err("a fenced write that changed no row must fail closed");
    assert_eq!(error.variant_name(), "SessionExecutionLeaseRenewalRefused");
}

#[test]
fn a_lost_fenced_write_records_the_disagreement_as_evidence() {
    let (result, capture) = lash_core_ids::trace_capture::capturing_sync(|| {
        require_fenced_write_applied(
            FencedWrite::TurnInputClaimSettlement,
            "postgres",
            "input-9",
            0,
            lost_lease_refusal,
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
            lost_lease_refusal,
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
            FencedWrite::ProcessLeaseRelease,
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
        FencedWrite::SessionExecutionLeaseRenewal,
        FencedWrite::SessionExecutionLeaseRelease,
        FencedWrite::TurnInputClaimAcquisition,
        FencedWrite::TurnInputClaimSettlement,
        FencedWrite::UnclaimedTurnInputSettlement,
        FencedWrite::SessionHeadPublication,
        FencedWrite::QueuedWorkClaimAcquisition,
        FencedWrite::QueuedWorkClaimSettlement,
        FencedWrite::EffectReplayLeaseFinalize,
        FencedWrite::EffectReplayLeaseRenewal,
        FencedWrite::ProcessLeaseRenewal,
        FencedWrite::ProcessLeaseRelease,
        FencedWrite::WakeDeliverySettlement,
    ];
    let labels = writes.map(FencedWrite::label);
    let unique = labels.iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(unique.len(), labels.len(), "labels collided: {labels:?}");
}

#[test]
fn time_authorities_are_named_distinctly() {
    assert_eq!(
        FenceTimeAuthority::DatabaseTransaction.label(),
        "database_transaction_clock"
    );
    assert_eq!(
        FenceTimeAuthority::EmbeddedHost.label(),
        "embedded_host_clock"
    );
}

// ---------------------------------------------------------------------------
// D1 — session-execution lease
// ---------------------------------------------------------------------------

#[test]
fn renewal_admits_the_current_holder_before_expiry() {
    let owner = owner();
    let current = row(&owner, "executor-1", "token-1", 7, 1_001);
    let authorized = require_renewable_session_execution_lease(
        Some(&current),
        &authority(),
        1_000,
        FenceTimeAuthority::EmbeddedHost,
        FRESHNESS,
    )
    .expect("the current holder may renew before expiry");
    // The authorized row is handed back so the caller reaches its retained
    // generation without a second option unwrap.
    assert_eq!(authorized.fencing_token, 7);
    assert_eq!(authorized.claimed_at_ms, 500);
}

#[test]
fn renewal_refuses_an_absent_row_as_expired() {
    let error = require_renewable_session_execution_lease(
        None,
        &authority(),
        1_000,
        FenceTimeAuthority::EmbeddedHost,
        FRESHNESS,
    )
    .expect_err("an absent lease row cannot be renewed");
    assert_eq!(error.variant_name(), "SessionExecutionLeaseExpired");
}

#[test]
fn renewal_refuses_a_released_row() {
    let error = require_renewable_session_execution_lease(
        Some(&released_row(7)),
        &authority(),
        1_000,
        FenceTimeAuthority::EmbeddedHost,
        FRESHNESS,
    )
    .expect_err("a released lease row cannot be renewed");
    assert_eq!(error.variant_name(), "SessionExecutionLeaseRenewalRefused");
}

#[test]
fn renewal_refuses_a_stale_lease_token() {
    let owner = owner();
    let error = require_renewable_session_execution_lease(
        Some(&row(&owner, "executor-1", "token-2", 7, 9_999)),
        &authority(),
        1_000,
        FenceTimeAuthority::EmbeddedHost,
        FRESHNESS,
    )
    .expect_err("a rotated lease token cannot renew");
    assert_eq!(error.variant_name(), "SessionExecutionLeaseRenewalRefused");
}

#[test]
fn renewal_refuses_a_different_incarnation_and_a_different_executor() {
    let other_incarnation = LeaseOwnerIdentity::opaque("worker-a", "boot-2");
    let owner = owner();
    for (current, case) in [
        (
            row(&other_incarnation, "executor-1", "token-1", 7, 9_999),
            "incarnation",
        ),
        (row(&owner, "executor-2", "token-1", 7, 9_999), "executor"),
    ] {
        let error = require_renewable_session_execution_lease(
            Some(&current),
            &authority(),
            1_000,
            FenceTimeAuthority::EmbeddedHost,
            FRESHNESS,
        )
        .unwrap_err();
        assert_eq!(
            error.variant_name(),
            "SessionExecutionLeaseRenewalRefused",
            "{case} mismatch must refuse renewal",
        );
    }
}

#[test]
fn renewal_expiry_is_decided_at_the_boundary_millisecond() {
    let owner = owner();
    // Expiry is strictly-after: the lease is dead at exactly `expires_at`.
    let at_boundary = require_renewable_session_execution_lease(
        Some(&row(&owner, "executor-1", "token-1", 7, 1_000)),
        &authority(),
        1_000,
        FenceTimeAuthority::DatabaseTransaction,
        FRESHNESS,
    )
    .expect_err("a lease expiring exactly now is expired");
    assert_eq!(at_boundary.variant_name(), "SessionExecutionLeaseExpired");
    assert!(
        require_renewable_session_execution_lease(
            Some(&row(&owner, "executor-1", "token-1", 7, 1_001)),
            &authority(),
            1_000,
            FenceTimeAuthority::DatabaseTransaction,
            FRESHNESS,
        )
        .is_ok(),
        "one millisecond of life is still life",
    );
}

#[test]
fn renewal_ignores_the_fencing_generation_by_contract() {
    // Renewal fences on owner, executor and lease token. The generation is
    // commit authority, not lock-lifecycle authority (CONTEXT: Session
    // Execution Lease Authority), and renewal never rotates it.
    let owner = owner();
    assert!(
        require_renewable_session_execution_lease(
            Some(&row(&owner, "executor-1", "token-1", 999, 9_999)),
            &authority(),
            1_000,
            FenceTimeAuthority::EmbeddedHost,
            FRESHNESS,
        )
        .is_ok()
    );
    // The execution fence, which guards writes, does consult it.
    let fence_error = require_current_session_execution_lease(
        &session_id(),
        Some(facts(&owner, "executor-1", "token-1", 999, 9_999)),
        &authority(),
        1_000,
    )
    .expect_err("a superseded generation cannot fence execution writes");
    assert_eq!(fence_error.variant_name(), "SessionExecutionLeaseExpired");
}

#[test]
fn release_admits_the_current_holder_even_after_expiry() {
    let owner = owner();
    assert!(
        require_releasable_session_execution_lease(
            Some(&row(&owner, "executor-1", "token-1", 7, 0)),
            &authority(),
            FRESHNESS,
        )
        .is_ok(),
        "a lapsed holder may still hand the lane back",
    );
}

#[test]
fn release_refuses_an_absent_a_released_and_a_stale_token_row() {
    let owner = owner();
    let released = released_row(7);
    let rotated = row(&owner, "executor-1", "token-2", 7, 9_999);
    for current in [None, Some(&released), Some(&rotated)] {
        let error = require_releasable_session_execution_lease(current, &authority(), FRESHNESS)
            .expect_err("release must refuse a row that does not name the holder");
        assert_eq!(error.variant_name(), "SessionExecutionLeaseReleaseRefused");
    }
}

// ---------------------------------------------------------------------------
// D2 / D5 — generation claimability
// ---------------------------------------------------------------------------

#[test]
fn an_unclaimed_row_is_claimable_whatever_generation_it_retains() {
    let facts = WorkRowClaimFacts {
        claim_token: None,
        claim_session_lease_generation: 4,
    };
    assert_eq!(
        turn_input_claimability(facts, 4),
        WorkRowClaimability::Claimable
    );
    assert_eq!(
        queued_work_batch_claimability(facts, 4),
        WorkRowClaimability::Claimable
    );
}

#[test]
fn a_row_claimed_under_a_superseded_generation_is_reclaimable() {
    let facts = WorkRowClaimFacts {
        claim_token: Some("claim-token"),
        claim_session_lease_generation: 3,
    };
    assert!(turn_input_claimability(facts, 4).is_claimable());
    assert!(queued_work_batch_claimability(facts, 4).is_claimable());
}

#[test]
fn a_row_already_claimed_under_this_generation_is_not_reclaimable() {
    let facts = WorkRowClaimFacts {
        claim_token: Some("claim-token"),
        claim_session_lease_generation: 4,
    };
    assert_eq!(
        turn_input_claimability(facts, 4),
        WorkRowClaimability::HeldByThisGeneration
    );
    assert_eq!(
        queued_work_batch_claimability(facts, 4),
        WorkRowClaimability::HeldByThisGeneration
    );
    assert!(!turn_input_claimability(facts, 4).is_claimable());
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
// D6 — effect-replay lease
// ---------------------------------------------------------------------------

fn effect_facts<'a>(
    envelope_hash: &'a str,
    lease_owner_id: Option<&'a str>,
    lease_token: Option<&'a str>,
    status: &'a str,
    lease_expires_at_ms: u64,
) -> EffectReplayLeaseFacts<'a> {
    EffectReplayLeaseFacts {
        envelope_hash,
        lease_owner_id,
        lease_token,
        status,
        lease_expires_at_ms,
    }
}

fn effect_authority() -> EffectReplayLeaseAuthority<'static> {
    EffectReplayLeaseAuthority {
        envelope_hash: "hash-1",
        owner_id: "owner-1",
        lease_token: "lease-1",
    }
}

#[test]
fn effect_replay_lease_is_current_only_when_every_fact_agrees() {
    assert_eq!(
        effect_replay_lease_verdict(
            Some(effect_facts(
                "hash-1",
                Some("owner-1"),
                Some("lease-1"),
                EFFECT_REPLAY_IN_PROGRESS_STATUS,
                1_001,
            )),
            effect_authority(),
            1_000,
        ),
        EffectReplayLeaseVerdict::Current
    );
}

#[test]
fn effect_replay_lease_names_each_refusal_distinctly() {
    let cases: [(Option<EffectReplayLeaseFacts<'_>>, EffectReplayLeaseVerdict); 5] = [
        (None, EffectReplayLeaseVerdict::Absent),
        (
            Some(effect_facts(
                "hash-other",
                Some("owner-1"),
                Some("lease-1"),
                EFFECT_REPLAY_IN_PROGRESS_STATUS,
                9_999,
            )),
            EffectReplayLeaseVerdict::EnvelopeMismatch,
        ),
        (
            Some(effect_facts(
                "hash-1",
                Some("owner-1"),
                Some("lease-2"),
                EFFECT_REPLAY_IN_PROGRESS_STATUS,
                9_999,
            )),
            EffectReplayLeaseVerdict::Superseded,
        ),
        (
            Some(effect_facts(
                "hash-1",
                Some("owner-1"),
                Some("lease-1"),
                "succeeded",
                9_999,
            )),
            EffectReplayLeaseVerdict::NotInProgress,
        ),
        (
            Some(effect_facts(
                "hash-1",
                Some("owner-1"),
                Some("lease-1"),
                EFFECT_REPLAY_IN_PROGRESS_STATUS,
                1_000,
            )),
            EffectReplayLeaseVerdict::Expired,
        ),
    ];
    for (observed, expected) in cases {
        let verdict = effect_replay_lease_verdict(observed, effect_authority(), 1_000);
        assert_eq!(verdict, expected, "observed {observed:?}");
        assert!(!verdict.is_current());
    }
}

#[test]
fn a_released_effect_replay_lease_row_is_superseded_not_current() {
    assert_eq!(
        effect_replay_lease_verdict(
            Some(effect_facts(
                "hash-1",
                None,
                None,
                EFFECT_REPLAY_IN_PROGRESS_STATUS,
                9_999
            )),
            effect_authority(),
            1_000,
        ),
        EffectReplayLeaseVerdict::Superseded
    );
}

#[test]
fn effect_replay_expiry_is_decided_at_the_boundary_millisecond() {
    for (expires_at, expected) in [
        (999_u64, EffectReplayLeaseVerdict::Expired),
        (1_000, EffectReplayLeaseVerdict::Expired),
        (1_001, EffectReplayLeaseVerdict::Current),
    ] {
        assert_eq!(
            effect_replay_lease_verdict(
                Some(effect_facts(
                    "hash-1",
                    Some("owner-1"),
                    Some("lease-1"),
                    EFFECT_REPLAY_IN_PROGRESS_STATUS,
                    expires_at,
                )),
                effect_authority(),
                1_000,
            ),
            expected,
            "expires_at {expires_at}",
        );
    }
}

// ---------------------------------------------------------------------------
// D7 — process lease
// ---------------------------------------------------------------------------

fn process_facts<'a>(
    lease_owner_id: Option<&'a str>,
    lease_token: Option<&'a str>,
    lease_fencing_token: u64,
    lease_expires_at_ms: u64,
) -> ProcessLeaseFacts<'a> {
    ProcessLeaseFacts {
        lease_owner_id,
        lease_token,
        lease_fencing_token,
        lease_expires_at_ms,
    }
}

fn process_authority() -> ProcessLeaseAuthority<'static> {
    ProcessLeaseAuthority {
        lease_token: "lease-1",
        fencing_token: 5,
    }
}

#[test]
fn process_lease_is_current_only_when_token_and_generation_agree() {
    assert_eq!(
        process_lease_verdict(
            Some(process_facts(Some("owner-1"), Some("lease-1"), 5, 1_001)),
            process_authority(),
            1_000,
        ),
        ProcessLeaseVerdict::Current
    );
}

#[test]
fn process_lease_names_each_refusal_distinctly() {
    let cases: [(Option<ProcessLeaseFacts<'_>>, ProcessLeaseVerdict); 5] = [
        (None, ProcessLeaseVerdict::Absent),
        (
            Some(process_facts(None, None, 5, 9_999)),
            ProcessLeaseVerdict::Released,
        ),
        (
            Some(process_facts(Some("owner-1"), Some("lease-2"), 5, 9_999)),
            ProcessLeaseVerdict::Superseded,
        ),
        (
            Some(process_facts(Some("owner-1"), Some("lease-1"), 6, 9_999)),
            ProcessLeaseVerdict::GenerationSuperseded,
        ),
        (
            Some(process_facts(Some("owner-1"), Some("lease-1"), 5, 1_000)),
            ProcessLeaseVerdict::Expired,
        ),
    ];
    for (observed, expected) in cases {
        let verdict = process_lease_verdict(observed, process_authority(), 1_000);
        assert_eq!(verdict, expected, "observed {observed:?}");
        assert!(!verdict.is_current());
        assert_ne!(verdict.label(), ProcessLeaseVerdict::Current.label());
    }
}

#[test]
fn process_lease_release_needs_the_fencing_token() {
    // Both release paths decide through this verdict (FIG-3388), and it
    // refuses a presented lease whose generation the row has moved past — the
    // case a token-only fence would admit.
    assert_eq!(
        process_lease_verdict(
            Some(process_facts(Some("owner-1"), Some("lease-1"), 6, 9_999)),
            process_authority(),
            1_000,
        ),
        ProcessLeaseVerdict::GenerationSuperseded
    );
}

// ---------------------------------------------------------------------------
// D8 — wake delivery
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
