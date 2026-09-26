//! The PostgreSQL half of the turn-ingress and claim family (FIG-3383).
//!
//! `lash-store-sql`'s `turn_ingress` module owns the column lists and every
//! statement whose text both backends issue verbatim; this module owns the
//! statements that genuinely fork and renders both sets once, at startup.

use lash_store_sql::turn_ingress::queued_runs::QueuedRunStatements;
use std::sync::LazyLock;

use lash_core_execution::store_backend_support as vocabulary;
use lash_store_sql::turn_ingress::{
    TurnIngressStatements, cancel_requests::CancelRequestStatements,
    cancellation_bindings::CancellationBindingStatements,
    closure_authorizations::ClosureAuthorizationStatements, pending_inputs::PendingInputStatements,
    queued_batches::QueuedBatchStatements, queued_items::QueuedItemStatements,
    retired_scopes::RetiredScopeStatements,
    session_execution_leases::SessionExecutionLeaseStatements,
    tool_intent_submissions::ToolIntentSubmissionStatements,
    turn_park_events::TurnParkEventStatements, turn_parks::TurnParkStatements,
};
use lash_store_sql::{Dialect, Vocabulary, VocabularyTerm};

// This module is reached through a crate-root `#[path]`, so its children name
// their own files too.
#[path = "turn_ingress/family.rs"]
mod family;
#[path = "turn_ingress/leases.rs"]
mod leases;
#[path = "turn_ingress/pending_inputs.rs"]
mod pending_inputs;
#[path = "turn_ingress/queued_work.rs"]
mod queued_work;
#[path = "turn_ingress/turn_cancel.rs"]
mod turn_cancel;
#[path = "turn_ingress/turn_parks.rs"]
mod turn_parks;

pub(crate) use family::TurnIngressPostgresStatements;
pub(crate) use leases::SessionExecutionLeasePostgresStatements;
pub(crate) use pending_inputs::PendingInputPostgresStatements;
pub(crate) use queued_work::{QueuedBatchPostgresStatements, QueuedItemPostgresStatements};
pub(crate) use turn_cancel::{
    CancelAffectedInputPostgresStatements, CancelRequestPostgresStatements,
    CancellationBindingPostgresStatements, ClosureAuthorizationPostgresStatements,
    RetiredScopePostgresStatements, ToolIntentSubmissionPostgresStatements,
};
pub(crate) use turn_parks::{TurnParkClockPostgresStatements, TurnParkPostgresStatements};

/// The `pending_turn_inputs.state` partitions this family's statements name.
///
/// Every expansion is generated from `lash_core_execution::runtime::TurnInputStateKind`, so adding
/// a state is one edit in `lash-core-store` rather than one per statement. The
/// term names are the domain's, and SQLite registers the same eight.
const TURN_INPUT_LIFECYCLE: Vocabulary = Vocabulary::new(&[
    VocabularyTerm::new(
        "accepted_turn_input_state",
        vocabulary::accepted_turn_input_state_predicate_sql,
    ),
    VocabularyTerm::new(
        "active_turn_input_state",
        vocabulary::active_turn_input_state_predicate_sql,
    ),
    VocabularyTerm::new(
        "cancelled_turn_input_state",
        vocabulary::cancelled_turn_input_state_predicate_sql,
    ),
    VocabularyTerm::new(
        "deferred_next_turn_turn_input_state",
        vocabulary::deferred_next_turn_turn_input_state_predicate_sql,
    ),
    VocabularyTerm::new(
        "nonterminal_turn_input_state",
        vocabulary::nonterminal_turn_input_state_predicate_sql,
    ),
    VocabularyTerm::new(
        "pending_active_turn_input_state",
        vocabulary::pending_active_turn_input_state_predicate_sql,
    ),
    VocabularyTerm::new(
        "terminal_turn_input_state",
        vocabulary::terminal_turn_input_state_predicate_sql,
    ),
    VocabularyTerm::new(
        "undelivered_turn_input_state",
        vocabulary::undelivered_turn_input_state_predicate_sql,
    ),
]);

/// Every turn-ingress statement this store issues.
pub(crate) struct TurnIngressSql {
    pub(crate) queued_runs: QueuedRunStatements,
    /// Cross-table statements both backends issue verbatim.
    pub(crate) family: TurnIngressStatements,
    /// Cross-table statements only PostgreSQL issues.
    pub(crate) family_postgres: TurnIngressPostgresStatements,
    /// `pending_turn_inputs`, shared.
    pub(crate) pending_inputs: PendingInputStatements,
    /// `pending_turn_inputs`, PostgreSQL only.
    pub(crate) pending_inputs_postgres: PendingInputPostgresStatements,
    /// `queued_work_batches`, shared.
    pub(crate) queued_batches: QueuedBatchStatements,
    /// `queued_work_batches`, PostgreSQL only.
    pub(crate) queued_batches_postgres: QueuedBatchPostgresStatements,
    /// `queued_work_items`, shared.
    pub(crate) queued_items: QueuedItemStatements,
    /// `queued_work_items`, PostgreSQL only.
    pub(crate) queued_items_postgres: QueuedItemPostgresStatements,
    /// `session_execution_leases`, shared.
    pub(crate) leases: SessionExecutionLeaseStatements,
    /// `session_execution_leases`, PostgreSQL only.
    pub(crate) leases_postgres: SessionExecutionLeasePostgresStatements,
    /// `turn_cancel_requests`, shared.
    pub(crate) cancel_requests: CancelRequestStatements,
    /// `turn_cancel_requests`, PostgreSQL only.
    pub(crate) cancel_requests_postgres: CancelRequestPostgresStatements,
    /// `turn_cancel_affected_inputs` statements, all of them PostgreSQL-only.
    pub(crate) cancel_affected_inputs_postgres: CancelAffectedInputPostgresStatements,
    /// `turn_cancellation_bindings`, shared.
    pub(crate) bindings: CancellationBindingStatements,
    /// `turn_cancellation_bindings`, PostgreSQL only.
    pub(crate) bindings_postgres: CancellationBindingPostgresStatements,
    /// `turn_cancel_closure_authorizations`, shared.
    pub(crate) closures: ClosureAuthorizationStatements,
    /// `turn_cancel_closure_authorizations`, PostgreSQL only.
    pub(crate) closures_postgres: ClosureAuthorizationPostgresStatements,
    /// `turn_cancel_retired_scopes`, shared.
    /// `turn_parks`, shared.
    pub(crate) turn_parks: TurnParkStatements,
    /// `turn_parks`, PostgreSQL only.
    pub(crate) turn_parks_postgres: TurnParkPostgresStatements,
    /// `turn_park_clock`, PostgreSQL only.
    pub(crate) turn_park_clock: TurnParkClockPostgresStatements,
    /// `turn_park_events`, shared.
    pub(crate) turn_park_events: TurnParkEventStatements,
    pub(crate) retired_scopes: RetiredScopeStatements,
    /// `turn_cancel_retired_scopes`, PostgreSQL only.
    pub(crate) retired_scopes_postgres: RetiredScopePostgresStatements,
    /// `tool_intent_submissions`, shared.
    pub(crate) tool_intents: ToolIntentSubmissionStatements,
    /// `tool_intent_submissions`, PostgreSQL only.
    pub(crate) tool_intents_postgres: ToolIntentSubmissionPostgresStatements,
}

static TURN_INGRESS_SQL: LazyLock<TurnIngressSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres().with_vocabulary(TURN_INPUT_LIFECYCLE);
    TurnIngressSql {
        queued_runs: QueuedRunStatements::render(dialect),
        family: TurnIngressStatements::render(dialect),
        family_postgres: TurnIngressPostgresStatements::render(dialect),
        pending_inputs: PendingInputStatements::render(dialect),
        pending_inputs_postgres: PendingInputPostgresStatements::render(dialect),
        queued_batches: QueuedBatchStatements::render(dialect),
        queued_batches_postgres: QueuedBatchPostgresStatements::render(dialect),
        queued_items: QueuedItemStatements::render(dialect),
        queued_items_postgres: QueuedItemPostgresStatements::render(dialect),
        leases: SessionExecutionLeaseStatements::render(dialect),
        leases_postgres: SessionExecutionLeasePostgresStatements::render(dialect),
        cancel_requests: CancelRequestStatements::render(dialect),
        cancel_requests_postgres: CancelRequestPostgresStatements::render(dialect),
        cancel_affected_inputs_postgres: CancelAffectedInputPostgresStatements::render(dialect),
        bindings: CancellationBindingStatements::render(dialect),
        bindings_postgres: CancellationBindingPostgresStatements::render(dialect),
        closures: ClosureAuthorizationStatements::render(dialect),
        closures_postgres: ClosureAuthorizationPostgresStatements::render(dialect),
        turn_parks: TurnParkStatements::render(dialect),
        turn_parks_postgres: TurnParkPostgresStatements::render(dialect),
        turn_park_clock: TurnParkClockPostgresStatements::render(dialect),
        turn_park_events: TurnParkEventStatements::render(dialect),
        retired_scopes: RetiredScopeStatements::render(dialect),
        retired_scopes_postgres: RetiredScopePostgresStatements::render(dialect),
        tool_intents: ToolIntentSubmissionStatements::render(dialect),
        tool_intents_postgres: ToolIntentSubmissionPostgresStatements::render(dialect),
    }
});

/// This store's turn-ingress statements, rendered once at first use.
pub(crate) fn turn_ingress_sql() -> &'static TurnIngressSql {
    &TURN_INGRESS_SQL
}

/// The test lease epoch a build may inject into a statement's ready cutoff.
///
/// The claim path samples `transaction_timestamp()` once per transaction and
/// binds it, so only the checkpoint probe — which runs outside a transaction —
/// carries the server clock in its own text. This is how that probe still
/// honours a steered lease clock without a second round trip, and in a
/// production build it is a compile-time `None`.
pub(crate) fn injected_lease_epoch_ms(
    #[cfg(any(test, feature = "testing"))] lease_clock: Option<
        &std::sync::Arc<dyn lash_core_execution::Clock>,
    >,
) -> Option<i64> {
    #[cfg(any(test, feature = "testing"))]
    {
        lease_clock.map(|clock| clock.timestamp_ms() as i64)
    }
    #[cfg(not(any(test, feature = "testing")))]
    {
        None
    }
}

#[cfg(test)]
#[path = "turn_ingress/tests.rs"]
mod tests;
