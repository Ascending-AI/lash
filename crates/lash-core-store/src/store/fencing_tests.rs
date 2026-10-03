//! Fenced-write backstop refusals and diagnostic evidence.

use super::StoreError;
use super::fencing::*;
const SESSION: &str = "session-fencing";

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
