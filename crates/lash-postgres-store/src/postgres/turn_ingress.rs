//! The PostgreSQL half of the turn-ingress and claim family (FIG-3383).
//!
//! `lash-store-sql`'s `turn_ingress` module owns the column lists and every
//! statement whose text both backends issue verbatim; this module owns the
//! statements that genuinely fork and renders both sets once, at startup.

use std::sync::LazyLock;

use lash_core_execution::store_backend_support as vocabulary;
use lash_store_sql::turn_ingress::{
    TurnIngressStatements, cancel_requests::CancelRequestStatements,
    pending_inputs::PendingInputStatements, queued_batches::QueuedBatchStatements,
    run_specs::RunSpecStatements, tool_intent_submissions::ToolIntentSubmissionStatements,
};
use lash_store_sql::{Dialect, Vocabulary, VocabularyTerm};

// This module is reached through a crate-root `#[path]`, so its children name
// their own files too.
#[path = "turn_ingress/family.rs"]
mod family;
#[path = "turn_ingress/pending_inputs.rs"]
mod pending_inputs;
#[path = "turn_ingress/queued_work.rs"]
mod queued_work;
#[path = "turn_ingress/tool_intents.rs"]
mod tool_intents;

pub(crate) use family::TurnIngressPostgresStatements;
pub(crate) use pending_inputs::PendingInputPostgresStatements;
pub(crate) use queued_work::QueuedBatchPostgresStatements;
pub(crate) use tool_intents::ToolIntentSubmissionPostgresStatements;

/// The `pending_turn_inputs.state` partitions this family's statements name.
///
/// Every expansion is generated from `lash_core_execution::runtime::TurnInputStateKind`, so adding
/// a state is one edit in `lash-core-store` rather than one per statement. The
/// term names are the domain's, and SQLite registers the same ten.
pub(crate) const TURN_INPUT_LIFECYCLE: Vocabulary = Vocabulary::new(&[
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
        "released_turn_input_state",
        vocabulary::released_turn_input_state_sql,
    ),
    VocabularyTerm::new(
        "terminal_turn_input_state",
        vocabulary::terminal_turn_input_state_predicate_sql,
    ),
    VocabularyTerm::new(
        "undelivered_turn_input_state",
        vocabulary::undelivered_turn_input_state_predicate_sql,
    ),
    VocabularyTerm::new("ingress_turn_id", ingress_turn_id_sql),
]);

/// The turn an `active_turn` submitted delivery in `column` addresses, `NULL`
/// for a `next_turn` one: the one JSON read the lifecycle terms need, spelled
/// in this dialect (the SQLite term reads the same field).
fn ingress_turn_id_sql(column: &str) -> String {
    format!("({column}::jsonb ->> 'turn_id')")
}

/// Every turn-ingress statement this store issues.
pub(crate) struct TurnIngressSql {
    /// Cross-table statements both backends issue verbatim.
    pub(crate) family: TurnIngressStatements,
    /// Cross-table statements only PostgreSQL issues.
    pub(crate) family_postgres: TurnIngressPostgresStatements,
    /// `pending_turn_inputs`, shared.
    pub(crate) pending_inputs: PendingInputStatements,
    /// `pending_turn_inputs`, PostgreSQL only.
    pub(crate) pending_inputs_postgres: PendingInputPostgresStatements,
    /// `session_run_specs`, shared.
    pub(crate) run_specs: RunSpecStatements,
    /// `queued_work_batches`, shared.
    pub(crate) queued_batches: QueuedBatchStatements,
    /// `queued_work_batches`, PostgreSQL only.
    pub(crate) queued_batches_postgres: QueuedBatchPostgresStatements,
    /// `turn_cancel_requests`, shared.
    pub(crate) cancel_requests: CancelRequestStatements,

    /// `tool_intent_submissions`, shared.
    pub(crate) tool_intents: ToolIntentSubmissionStatements,
    /// `tool_intent_submissions`, PostgreSQL only.
    pub(crate) tool_intents_postgres: ToolIntentSubmissionPostgresStatements,
}

static TURN_INGRESS_SQL: LazyLock<TurnIngressSql> = LazyLock::new(|| {
    let dialect = Dialect::postgres().with_vocabulary(TURN_INPUT_LIFECYCLE);
    TurnIngressSql {
        family: TurnIngressStatements::render(dialect),
        family_postgres: TurnIngressPostgresStatements::render(dialect),
        pending_inputs: PendingInputStatements::render(dialect),
        pending_inputs_postgres: PendingInputPostgresStatements::render(dialect),
        run_specs: RunSpecStatements::render(dialect),
        queued_batches: QueuedBatchStatements::render(dialect),
        queued_batches_postgres: QueuedBatchPostgresStatements::render(dialect),
        cancel_requests: CancelRequestStatements::render(dialect),

        tool_intents: ToolIntentSubmissionStatements::render(dialect),
        tool_intents_postgres: ToolIntentSubmissionPostgresStatements::render(dialect),
    }
});

/// This store's turn-ingress statements, rendered once at first use.
pub(crate) fn turn_ingress_sql() -> &'static TurnIngressSql {
    &TURN_INGRESS_SQL
}

#[cfg(test)]
#[path = "turn_ingress/tests.rs"]
mod tests;
