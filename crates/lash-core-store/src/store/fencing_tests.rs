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
