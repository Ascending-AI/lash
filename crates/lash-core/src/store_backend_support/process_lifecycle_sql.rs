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
pub fn process_status_sql_literal(status: ProcessStatus) -> String {
    format!("'{}'", status.label())
}

/// Quote a process-status list as the body of a SQL `IN (...)` list.
pub fn process_status_sql_literal_list(statuses: &[ProcessStatus]) -> String {
    statuses
        .iter()
        .copied()
        .map(process_status_sql_literal)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The live process statuses spelled as the body of a SQL `IN (...)` list.
pub fn live_process_statuses_sql() -> String {
    let live = ProcessStatus::ALL
        .iter()
        .copied()
        .filter(ProcessStatus::is_live)
        .collect::<Vec<_>>();
    if live.is_empty() {
        // Admit no row rather than interpolating the invalid SQL `IN ()`.
        return "FALSE".to_string();
    }
    process_status_sql_literal_list(&live)
}

/// The retired process statuses spelled as the body of a SQL `IN (...)` list.
pub fn retired_process_statuses_sql() -> String {
    let retired = ProcessStatus::ALL
        .iter()
        .copied()
        .filter(ProcessStatus::is_retired)
        .collect::<Vec<_>>();
    if retired.is_empty() {
        return "FALSE".to_string();
    }
    process_status_sql_literal_list(&retired)
}

/// `<column> IN (<live statuses>)`: the live-worklist predicate.
///
/// `column` is a SQL identifier the caller owns (`status`, `p.status`,
/// `processes.status`); it is never user input.
pub fn live_process_status_predicate_sql(column: &str) -> String {
    format!("{column} IN ({})", live_process_statuses_sql())
}

/// `<column> NOT IN (<live statuses>)`: the retention complement of
/// [`live_process_status_predicate_sql`].
///
/// Spelled as the negation of the live set rather than as the retired list so
/// a status that is neither live nor retired can never be silently exempted
/// from retention; `live_and_retired_statuses_partition_the_vocabulary` keeps
/// the two sets complementary.
pub fn retired_process_status_predicate_sql(column: &str) -> String {
    format!("{column} NOT IN ({})", live_process_statuses_sql())
}

/// Quote one wake-delivery state for interpolation into backend SQL.
pub fn wake_delivery_state_sql_literal(state: WakeDeliveryState) -> String {
    format!("'{}'", state.as_str())
}

/// Quote a wake-delivery state list as the body of a SQL `IN (...)` list.
pub fn wake_delivery_state_sql_literal_list(states: &[WakeDeliveryState]) -> String {
    states
        .iter()
        .copied()
        .map(wake_delivery_state_sql_literal)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The undelivered wake-delivery states spelled as the body of a SQL `IN (...)`
/// list: the deliveries a prune must still account for.
pub fn undelivered_wake_delivery_states_sql() -> String {
    let undelivered = WakeDeliveryState::ALL
        .iter()
        .copied()
        .filter(|state| state.is_undelivered())
        .collect::<Vec<_>>();
    if undelivered.is_empty() {
        return "FALSE".to_string();
    }
    wake_delivery_state_sql_literal_list(&undelivered)
}

/// `<column> IN (<undelivered states>)`.
pub fn undelivered_wake_delivery_state_predicate_sql(column: &str) -> String {
    format!("{column} IN ({})", undelivered_wake_delivery_states_sql())
}

#[cfg(test)]
#[path = "process_lifecycle_sql_tests.rs"]
mod tests;
