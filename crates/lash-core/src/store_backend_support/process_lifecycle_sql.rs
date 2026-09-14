//! Process and wake-delivery lifecycle predicates, spelled once from the
//! exported vocabulary.
//!
//! Every backend worklist, prune, preflight and change-feed query filters on
//! the same two partitions: the live process rows a worklist keeps, and the
//! retired rows retention may reclaim. Retyping those partitions as SQL string
//! literals is how a new [`ProcessStatus`](crate::ProcessStatus) variant
//! silently drops rows from a worklist while making live rows prunable — the
//! queries compile clean either way. The functions here derive the literal
//! lists from the enums instead, so the SQL and the Rust predicates
//! ([`ProcessStatus::is_live`](crate::ProcessStatus::is_live),
//! [`ProcessStatus::is_retired`](crate::ProcessStatus::is_retired)) cannot
//! drift.
//!
//! Schema `CHECK` vocabularies are deliberately *not* generated here: they are
//! the durable constraint surface owned by
//! [`required_constraints`](super::required_constraints) and FIG-2811.

use crate::{ProcessStatus, WakeDeliveryState};

/// Quote one process status for interpolation into backend SQL.
pub(crate) fn process_status_sql_literal(status: ProcessStatus) -> String {
    format!("'{}'", status.label())
}

/// Quote a process-status list as the body of a SQL `IN (...)` list.
pub(crate) fn process_status_sql_literal_list(statuses: &[ProcessStatus]) -> String {
    statuses
        .iter()
        .copied()
        .map(process_status_sql_literal)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A predicate that admits no row.
///
/// Used only for the unreachable empty-partition case: `IN ()` is not valid
/// SQL in either backend, and a bare `FALSE` is not a predicate the `<column>
/// IN (...)` shape can carry.
const NO_ROW_PREDICATE: &str = "1 = 0";

/// A predicate that admits every row: the complement of [`NO_ROW_PREDICATE`].
const EVERY_ROW_PREDICATE: &str = "1 = 1";

/// The live process statuses spelled as the body of a SQL `IN (...)` list.
///
/// Empty only if no variant is live, which the exhaustive
/// [`ProcessStatus::is_live`] match makes a deliberate choice rather than an
/// oversight; the predicate builders below turn that case into a predicate
/// instead of an invalid `IN ()`.
pub(crate) fn live_process_statuses_sql() -> String {
    let live = ProcessStatus::ALL
        .iter()
        .copied()
        .filter(ProcessStatus::is_live)
        .collect::<Vec<_>>();
    process_status_sql_literal_list(&live)
}

/// `<column> IN (<live statuses>)`: the live-worklist predicate.
///
/// `column` is a SQL identifier the caller owns (`status`, `p.status`,
/// `processes.status`); it is never user input.
pub fn live_process_status_predicate_sql(column: &str) -> String {
    let live = live_process_statuses_sql();
    if live.is_empty() {
        return NO_ROW_PREDICATE.to_string();
    }
    format!("{column} IN ({live})")
}

/// `<column> NOT IN (<live statuses>)`: the retention complement of
/// [`live_process_status_predicate_sql`].
///
/// Spelled as the negation of the live set rather than as the retired list so
/// a status that is neither live nor retired can never be silently exempted
/// from retention; `live_and_retired_statuses_partition_the_vocabulary` keeps
/// the two sets complementary.
pub fn retired_process_status_predicate_sql(column: &str) -> String {
    let live = live_process_statuses_sql();
    if live.is_empty() {
        // Nothing is live, so every row is retired.
        return EVERY_ROW_PREDICATE.to_string();
    }
    format!("{column} NOT IN ({live})")
}

/// The terminal process statuses spelled as the body of a SQL `IN (...)` list.
///
/// Terminal is not the complement of live:
/// [`ProcessStatus::CallerDeparted`](crate::ProcessStatus::CallerDeparted) is
/// neither live nor terminal, so it appears in neither list and a
/// nonterminal predicate must select it.
pub(crate) fn terminal_process_statuses_sql() -> String {
    let terminal = ProcessStatus::ALL
        .iter()
        .copied()
        .filter(|status| status.is_terminal())
        .collect::<Vec<_>>();
    process_status_sql_literal_list(&terminal)
}

/// `<column> NOT IN (<terminal statuses>)`: the rows whose outcome is still
/// open, including `caller_departed`.
///
/// Spelled as the negation of the terminal set for the same reason
/// [`retired_process_status_predicate_sql`] negates the live set: a status
/// that is neither live nor terminal must land on this side of the predicate
/// without an edit here, and `caller_departed` is exactly that status. A
/// pending-cancel row in that state still carries an unanswered request.
pub fn nonterminal_process_status_predicate_sql(column: &str) -> String {
    let terminal = terminal_process_statuses_sql();
    if terminal.is_empty() {
        // Nothing is terminal, so every row is still open.
        return EVERY_ROW_PREDICATE.to_string();
    }
    format!("{column} NOT IN ({terminal})")
}

/// Quote one wake-delivery state for interpolation into backend SQL.
pub fn wake_delivery_state_sql_literal(state: WakeDeliveryState) -> String {
    format!("'{}'", state.as_str())
}

/// Quote a wake-delivery state list as the body of a SQL `IN (...)` list.
pub(crate) fn wake_delivery_state_sql_literal_list(states: &[WakeDeliveryState]) -> String {
    states
        .iter()
        .copied()
        .map(wake_delivery_state_sql_literal)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The undelivered wake-delivery states spelled as the body of a SQL `IN (...)`
/// list: the deliveries a prune must still account for.
pub(crate) fn undelivered_wake_delivery_states_sql() -> String {
    let undelivered = WakeDeliveryState::ALL
        .iter()
        .copied()
        .filter(|state| state.is_undelivered())
        .collect::<Vec<_>>();
    wake_delivery_state_sql_literal_list(&undelivered)
}

/// `<column> IN (<undelivered states>)`.
pub fn undelivered_wake_delivery_state_predicate_sql(column: &str) -> String {
    let undelivered = undelivered_wake_delivery_states_sql();
    if undelivered.is_empty() {
        return NO_ROW_PREDICATE.to_string();
    }
    format!("{column} IN ({undelivered})")
}

#[cfg(test)]
#[path = "process_lifecycle_sql_tests.rs"]
mod tests;
