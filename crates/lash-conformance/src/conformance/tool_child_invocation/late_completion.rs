//! L03/L07: a Run-cancelled source refuses revival by late external writes.
use super::*;
pub async fn a_late_completion_after_a_cancel_decision_is_refused(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    super::deferred_commit::deferred_run(fixture, prefix, true, super::deferred_commit::Hold::None)
        .await;
}
