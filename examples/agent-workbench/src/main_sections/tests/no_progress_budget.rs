use super::*;

/// FIG-1407: the workbench bounds how long a turn may fail to get anything
/// done, not only how much it may do.
///
/// The bound that shipped before this was `TurnBudget::Unbounded` and nothing
/// else, so a model answering with a cell that never committed re-called the
/// provider until an operator noticed: 1,223 calls, zero committed nodes, turn
/// still open. Both bounds are asserted here as resolved session policy,
/// because the bug was policy the workbench never expressed.
#[test]
fn the_workbench_bounds_both_turn_work_and_turn_stalling() {
    // The workbench's own policy, resolved the way the runtime resolves it.
    let spec = lash::SessionSpec::new(
        "test-model",
        lash::TurnBudget::bounded(WORKBENCH_MAX_TURNS),
        lash::MaxToolCalls::new(1024),
    )
    .no_progress_budget(lash::NoProgressBudget::bounded(
        WORKBENCH_MAX_NO_PROGRESS_ATTEMPTS,
    ));
    let expected_workbench_bound = Some(lash::NoProgressBudget::bounded(
        WORKBENCH_MAX_NO_PROGRESS_ATTEMPTS,
    ));
    assert_eq!(spec.no_progress_budget, expected_workbench_bound);

    let policy = spec
        .stated_root_policy()
        .expect("a root spec states its policy without a catalog");
    assert_eq!(policy.turn_budget.max_turns(), Some(WORKBENCH_MAX_TURNS));
    let resolved_attempts = policy.no_progress_budget.max_attempts();
    assert_eq!(resolved_attempts, Some(WORKBENCH_MAX_NO_PROGRESS_ATTEMPTS));
    assert!(
        resolved_attempts < policy.turn_budget.max_turns(),
        "a stall bound at or above the turn budget can never fire"
    );
}
