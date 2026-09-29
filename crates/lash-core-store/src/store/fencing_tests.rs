//! Exhaustive unit coverage for every fencing verdict function.
//!
//! Each decision is exercised across its whole refusal vocabulary — stale
//! token, head moved, first commit — because these functions are the only
//! place those answers are now decided.

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
fn lost_fence_refusal() -> StoreError {
    StoreError::Contended
}

#[test]
fn one_affected_row_satisfies_the_backstop_silently() {
    let (applied, capture) = lash_core_ids::trace_capture::capturing_sync(|| {
        fenced_write_applied(FencedWrite::IngressAdmission, "sqlite", SESSION, 1)
    });
    assert!(applied);
    assert!(
        capture.named(FENCED_WRITE_DISAGREEMENT_EVENT).is_empty(),
        "an applied write records nothing",
    );
    assert!(
        require_fenced_write_applied(
            FencedWrite::IngressAdmission,
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
    // caller receives. A lost fence still reads as a lost fence, so the
    // runtime's stand-down handling is untouched.
    let (result, _capture) = lash_core_ids::trace_capture::capturing_sync(|| {
        require_fenced_write_applied(
            FencedWrite::IngressAdmission,
            "sqlite",
            SESSION,
            0,
            lost_fence_refusal,
        )
    });
    let error = result.expect_err("a fenced write that changed no row must fail closed");
    assert_eq!(error.variant_name(), "Contended");
}

#[test]
fn a_lost_fenced_write_records_the_disagreement_as_evidence() {
    let (result, capture) = lash_core_ids::trace_capture::capturing_sync(|| {
        require_fenced_write_applied(
            FencedWrite::IngressSettlement,
            "postgres",
            "input-9",
            0,
            lost_fence_refusal,
        )
    });
    assert!(result.is_err());
    let event = capture.exactly_one(FENCED_WRITE_DISAGREEMENT_EVENT);
    assert_eq!(event.level, "ERROR");
    assert_eq!(event.target, FENCING_TRACE_TARGET);
    assert_eq!(event.field("fenced_write"), "ingress.settle");
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
            lost_fence_refusal,
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
        FencedWrite::IngressAdmission,
        FencedWrite::IngressSettlement,
        FencedWrite::SessionHeadPublication,
        FencedWrite::WakeDeliverySettlement,
    ];
    let labels = writes.map(FencedWrite::label);
    let unique = labels.iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(unique.len(), labels.len(), "labels collided: {labels:?}");
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
