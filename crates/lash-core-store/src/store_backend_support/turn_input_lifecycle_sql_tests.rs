//! What each turn-input lifecycle predicate spells, and that the partitions
//! stay derived from the enum rather than from a list written here.

use super::*;

#[test]
fn each_predicate_spells_the_partition_it_names() {
    assert_eq!(
        accepted_turn_input_state_predicate_sql("state"),
        "state IN ('accepted')"
    );
    assert_eq!(
        pending_active_turn_input_state_predicate_sql("state"),
        "state IN ('pending_active')"
    );
    assert_eq!(
        cancelled_turn_input_state_predicate_sql("state"),
        "state IN ('cancelled')"
    );
    assert_eq!(
        deferred_next_turn_turn_input_state_predicate_sql("state"),
        "state IN ('deferred_next_turn')"
    );
    assert_eq!(
        active_turn_input_state_predicate_sql("state"),
        "state IN ('pending_active', 'accepted')"
    );
    assert_eq!(
        undelivered_turn_input_state_predicate_sql("state"),
        "state IN ('pending_active', 'deferred_next_turn')"
    );
    assert_eq!(
        terminal_turn_input_state_predicate_sql("state"),
        "state IN ('cancelled', 'completed')"
    );
    assert_eq!(
        nonterminal_turn_input_state_predicate_sql("state"),
        "state NOT IN ('cancelled', 'completed')"
    );
}

#[test]
fn a_predicate_carries_whatever_column_reference_the_statement_gave_it() {
    // A vocabulary token's column may be plain or qualified, and the backends
    // use both: an unqualified `state` in a single-table filter and a
    // table-qualified one inside a join.
    assert_eq!(
        active_turn_input_state_predicate_sql("pending_turn_inputs.state"),
        "pending_turn_inputs.state IN ('pending_active', 'accepted')"
    );
    assert_eq!(
        nonterminal_turn_input_state_predicate_sql("input.state"),
        "input.state NOT IN ('cancelled', 'completed')"
    );
}

#[test]
fn the_terminal_partition_is_derived_from_the_enum_not_from_a_list_here() {
    // The SQL backstop and the Rust verdict answer the same question, so every
    // state must land on the same side of both. This is the proof the two
    // cannot drift when a variant is added.
    let spelled = terminal_turn_input_state_predicate_sql("state");
    for state in TurnInputStateKind::ALL {
        let quoted = super::super::state_sql_literal(*state);
        assert_eq!(
            spelled.contains(&quoted),
            state.is_terminal(),
            "`{}` is {} the SQL terminal set but {} terminal in the enum",
            state.as_str(),
            if spelled.contains(&quoted) {
                "in"
            } else {
                "not in"
            },
            if state.is_terminal() { "is" } else { "is not" },
        );
        assert_eq!(
            crate::store_backend_support::unclaimed_turn_input_is_settleable(state.as_str()),
            !state.is_terminal(),
        );
    }
}
