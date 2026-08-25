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
    // The bound is a bound: an absent host opinion resolves to it, and only an
    // explicit opt-out removes it.
    let default_attempts = lash::NoProgressBudget::default().max_attempts();
    let documented_default = Some(lash::NoProgressBudget::DEFAULT_MAX_ATTEMPTS);
    assert_eq!(default_attempts, documented_default);
    assert_eq!(lash::NoProgressBudget::Unbounded.max_attempts(), None);
    assert!(lash::NoProgressBudget::bounded(12).is_exhausted_by(12));
    assert!(!lash::NoProgressBudget::bounded(12).is_exhausted_by(11));
    assert!(!lash::NoProgressBudget::Unbounded.is_exhausted_by(10_000));

    let lash::NoProgressBudget::Bounded(bound) = lash::NoProgressBudget::bounded(7) else {
        panic!("a bounded budget carries its bound");
    };
    assert_eq!(bound.get(), 7);

    // The workbench's own policy, resolved the way the runtime resolves it.
    let spec = lash::SessionSpec::new()
        .turn_budget(lash::TurnBudget::bounded(WORKBENCH_MAX_TURNS))
        .no_progress_budget(lash::NoProgressBudget::bounded(
            WORKBENCH_MAX_NO_PROGRESS_ATTEMPTS,
        ));
    let expected_workbench_bound =
        Some(lash::NoProgressBudget::bounded(WORKBENCH_MAX_NO_PROGRESS_ATTEMPTS));
    assert_eq!(spec.no_progress_budget, expected_workbench_bound);

    let policy =
        spec.resolve_against(&lash::runtime::SessionPolicy::new(lash::TurnBudget::Unbounded));
    assert_eq!(policy.turn_budget.max_turns(), Some(WORKBENCH_MAX_TURNS));
    let resolved_attempts = policy.no_progress_budget.max_attempts();
    assert_eq!(resolved_attempts, Some(WORKBENCH_MAX_NO_PROGRESS_ATTEMPTS));
    assert!(
        resolved_attempts < policy.turn_budget.max_turns(),
        "a stall bound at or above the turn budget can never fire"
    );

    // An explicit opt-out survives resolution, so a deployment that wants the
    // old behaviour can still ask for it in as many words.
    let opted_out = lash::SessionSpec::new()
        .no_progress_budget(lash::NoProgressBudget::Unbounded)
        .resolve_against(&policy);
    let opted_out_budget = opted_out.no_progress_budget;
    assert_eq!(opted_out_budget, lash::NoProgressBudget::Unbounded);
}
