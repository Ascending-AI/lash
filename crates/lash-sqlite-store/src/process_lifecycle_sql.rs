//! The one place this crate spells a process or wake-delivery lifecycle
//! predicate.
//!
//! Every worklist, prune, preflight and change-feed statement below builds its
//! filter from [`lash_core::ProcessStatus`] and
//! [`lash_core::WakeDeliveryState`] through these helpers, so a new variant
//! cannot compile without declaring which side of the live/retired (or
//! delivered/undelivered) partition it falls on. `process_lifecycle_vocabulary`
//! in `lash-sim` fails if a raw lifecycle literal reappears in a statement
//! here.

use lash_core::WakeDeliveryState;

/// `<column> IN ('running', 'waiting')`: the live worklist predicate.
pub(crate) fn live_process_status(column: &str) -> String {
    lash_core::store_backend_support::live_process_status_predicate_sql(column)
}

/// `<column> NOT IN ('running', 'waiting')`: the retention complement.
pub(crate) fn retired_process_status(column: &str) -> String {
    lash_core::store_backend_support::retired_process_status_predicate_sql(column)
}

/// `<column> NOT IN ('completed', 'failed', 'cancelled', 'abandoned')`: rows
/// whose outcome is still open, `caller_departed` included.
pub(crate) fn nonterminal_process_status(column: &str) -> String {
    lash_core::store_backend_support::nonterminal_process_status_predicate_sql(column)
}

/// `<column> IN ('pending', 'enqueuing')`: deliveries still owed to a target.
pub(crate) fn undelivered_wake_delivery_state(column: &str) -> String {
    lash_core::store_backend_support::undelivered_wake_delivery_state_predicate_sql(column)
}

/// One quoted wake-delivery state, for `SET state = …` and `state = …`.
pub(crate) fn wake_delivery_state(state: WakeDeliveryState) -> String {
    lash_core::store_backend_support::wake_delivery_state_sql_literal(state)
}
