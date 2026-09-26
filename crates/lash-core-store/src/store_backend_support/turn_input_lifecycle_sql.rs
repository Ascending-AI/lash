//! Turn-input lifecycle predicates, spelled once from the state vocabulary.
//!
//! `pending_turn_inputs.state` is the turn-ingress family's lifecycle column,
//! and every claim scan, settlement backstop, repair sweep and retention prune
//! filters on one of a handful of partitions of it. Retyping those partitions
//! as SQL literals at each call site is how a new
//! [`TurnInputStateKind`](crate::TurnInputStateKind) variant silently stops a
//! claim from seeing a row while letting retention delete it — the queries
//! compile clean either way. The functions here derive the literal lists from
//! the enum, exactly as
//! [`process_lifecycle_sql`](super::process_lifecycle_sql) does for
//! `processes.status`, so the SQL and the Rust predicates
//! ([`TurnInputStateKind::is_terminal`](crate::TurnInputStateKind::is_terminal),
//! [`unclaimed_turn_input_is_settleable`](crate::store_backend_support::unclaimed_turn_input_is_settleable))
//! cannot drift.
//!
//! Each of these is registered as a `{{term(column)}}` vocabulary term by both
//! store backends (ADR 0098 §3), so a neutral statement names the partition and
//! never spells it.
//!
//! Schema `CHECK` vocabularies are deliberately *not* generated here: they are
//! the durable constraint surface owned by
//! [`required_constraints`](super::required_constraints).

use crate::TurnInputStateKind;

/// A predicate that admits no row.
///
/// `IN ()` is not valid SQL in either backend, so an empty partition renders as
/// a predicate rather than as a syntax error.
const NO_ROW_PREDICATE: &str = "1 = 0";

/// A predicate that admits every row: the complement of [`NO_ROW_PREDICATE`].
const EVERY_ROW_PREDICATE: &str = "1 = 1";

fn membership_predicate(column: &str, states: &[TurnInputStateKind]) -> String {
    let list = super::state_sql_literal_list(states);
    if list.is_empty() {
        return NO_ROW_PREDICATE.to_string();
    }
    format!("{column} IN ({list})")
}

/// `<column> = 'accepted'`: the state a claimed active-turn input carries.
///
/// `column` is a SQL identifier the caller owns (`state`,
/// `pending_turn_inputs.state`); it is never user input.
pub fn accepted_turn_input_state_predicate_sql(column: &str) -> String {
    membership_predicate(column, &[TurnInputStateKind::Accepted])
}

/// `<column> = 'pending_active'`: an active-turn input nobody has claimed.
pub fn pending_active_turn_input_state_predicate_sql(column: &str) -> String {
    membership_predicate(column, &[TurnInputStateKind::PendingActive])
}

/// `<column> = 'cancelled'`: an input withdrawn or cancelled before any turn
/// applied it.
pub fn cancelled_turn_input_state_predicate_sql(column: &str) -> String {
    membership_predicate(column, &[TurnInputStateKind::Cancelled])
}

/// `<column> = 'deferred_next_turn'`: an input held for the next turn.
pub fn deferred_next_turn_turn_input_state_predicate_sql(column: &str) -> String {
    membership_predicate(column, &[TurnInputStateKind::DeferredNextTurn])
}

/// `<column> IN ('pending_active', 'accepted')`: the active-turn partition.
///
/// The rows an active-turn claim, an orphan scan and an interrupted-turn repair
/// all range over: an active-turn input is either waiting to be claimed or
/// claimed by a runner that may since have gone.
pub fn active_turn_input_state_predicate_sql(column: &str) -> String {
    membership_predicate(
        column,
        &[
            TurnInputStateKind::PendingActive,
            TurnInputStateKind::Accepted,
        ],
    )
}

/// `<column> IN ('pending_active', 'deferred_next_turn')`: the rows a caller
/// listing a session's queue sees as still open for delivery.
pub fn undelivered_turn_input_state_predicate_sql(column: &str) -> String {
    membership_predicate(
        column,
        &[
            TurnInputStateKind::PendingActive,
            TurnInputStateKind::DeferredNextTurn,
        ],
    )
}

fn terminal_states() -> Vec<TurnInputStateKind> {
    TurnInputStateKind::ALL
        .iter()
        .copied()
        .filter(|state| state.is_terminal())
        .collect()
}

/// `<column> IN (<terminal states>)`: the rows retention may reclaim.
pub fn terminal_turn_input_state_predicate_sql(column: &str) -> String {
    membership_predicate(column, &terminal_states())
}

/// `<column> NOT IN (<terminal states>)`: the SQL backstop of
/// [`unclaimed_turn_input_is_settleable`](crate::store_backend_support::unclaimed_turn_input_is_settleable).
///
/// Spelled as the negation of the terminal set rather than as a list of open
/// states for the same reason
/// [`nonterminal_process_status_predicate_sql`](super::nonterminal_process_status_predicate_sql)
/// negates the terminal set: a state that is neither open nor terminal must
/// land on this side of the predicate without an edit here.
pub fn nonterminal_turn_input_state_predicate_sql(column: &str) -> String {
    let terminal = super::state_sql_literal_list(&terminal_states());
    if terminal.is_empty() {
        // Nothing is terminal, so every row is still open for settlement.
        return EVERY_ROW_PREDICATE.to_string();
    }
    format!("{column} NOT IN ({terminal})")
}

#[cfg(test)]
#[path = "turn_input_lifecycle_sql_tests.rs"]
mod tests;
