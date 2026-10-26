//! The SQLite half of the turn-ingress and admission family (FIG-3383).
//!
//! `lash-store-sql`'s `turn_ingress` module owns the column lists and every
//! statement whose text both backends issue verbatim; this module owns the
//! statements that genuinely fork and renders both sets once, at startup.
//!
//! Two renderings, because the family is reached through two different
//! addressings:
//!
//! * the session catalog's own database, [`turn_ingress_sql`], which is where
//!   nine of the ten tables live;
//! * the process registry's database, [`tool_intent_sql`], a one-connection
//!   family addressed unqualified the way it always has been;

use std::sync::LazyLock;

use lash_core_store::store_backend_support as vocabulary;
use lash_store_sql::turn_ingress::{
    TurnIngressStatements, cancel_requests::CancelRequestStatements,
    forsaken_inputs::ForsakenInputStatements, pending_inputs::PendingInputStatements,
    queued_batches::QueuedBatchStatements, run_specs::RunSpecStatements,
    tool_intent_submissions::ToolIntentSubmissionStatements,
};
use lash_store_sql::{Dialect, Vocabulary, VocabularyTerm};

mod family;
mod pending_inputs;
mod queued_work;
mod tool_intents;

pub(crate) use family::TurnIngressSqliteStatements;
pub(crate) use pending_inputs::PendingInputSqliteStatements;
pub(crate) use queued_work::QueuedBatchSqliteStatements;
pub(crate) use tool_intents::ToolIntentSubmissionSqliteStatements;

/// The `pending_turn_inputs.state` partitions this family's statements name.
///
/// Every expansion is generated from
/// [`TurnInputStateKind`](lash_core_store::turn_input_vocabulary::TurnInputStateKind), so adding a state is
/// one edit in `lash-core-store` rather than one per statement. The term names
/// are the domain's, and PostgreSQL registers the same ten.
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
/// in this dialect (the PostgreSQL term reads the same field).
fn ingress_turn_id_sql(column: &str) -> String {
    format!("json_extract({column}, '$.turn_id')")
}

/// Every turn-ingress statement the session catalog issues.
pub(crate) struct TurnIngressSql {
    /// Cross-table statements both backends issue verbatim.
    pub(crate) family: TurnIngressStatements,
    /// Cross-table statements only SQLite issues.
    pub(crate) family_sqlite: TurnIngressSqliteStatements,
    /// `pending_turn_inputs`, shared.
    pub(crate) pending_inputs: PendingInputStatements,
    /// `pending_turn_inputs`, SQLite only.
    pub(crate) pending_inputs_sqlite: PendingInputSqliteStatements,
    /// `session_run_specs`, shared.
    pub(crate) run_specs: RunSpecStatements,
    /// `forsaken_turn_inputs`, shared.
    pub(crate) forsaken_inputs: ForsakenInputStatements,
    /// `queued_work_batches`, shared.
    pub(crate) queued_batches: QueuedBatchStatements,
    /// `queued_work_batches`, SQLite only.
    pub(crate) queued_batches_sqlite: QueuedBatchSqliteStatements,
    /// `turn_cancel_requests`, shared.
    pub(crate) cancel_requests: CancelRequestStatements,
}

impl TurnIngressSql {
    fn render() -> Self {
        let dialect = crate::schema_layout::MAIN.with_vocabulary(TURN_INPUT_LIFECYCLE);
        Self {
            family: TurnIngressStatements::render(dialect),
            family_sqlite: TurnIngressSqliteStatements::render(dialect),
            pending_inputs: PendingInputStatements::render(dialect),
            pending_inputs_sqlite: PendingInputSqliteStatements::render(dialect),
            run_specs: RunSpecStatements::render(dialect),
            forsaken_inputs: ForsakenInputStatements::render(dialect),
            queued_batches: QueuedBatchStatements::render(dialect),
            queued_batches_sqlite: QueuedBatchSqliteStatements::render(dialect),
            cancel_requests: CancelRequestStatements::render(dialect),
        }
    }
}

static TURN_INGRESS_SQL: LazyLock<TurnIngressSql> = LazyLock::new(TurnIngressSql::render);

/// The session catalog's turn-ingress statements, rendered once at first use.
pub(crate) fn turn_ingress_sql() -> &'static TurnIngressSql {
    &TURN_INGRESS_SQL
}

/// The `tool_intent_submissions` statements the process registry issues.
pub(crate) struct ToolIntentSql {
    /// Shared across both backends.
    pub(crate) shared: ToolIntentSubmissionStatements,
    /// SQLite only.
    pub(crate) sqlite: ToolIntentSubmissionSqliteStatements,
}

static TOOL_INTENT_SQL: LazyLock<ToolIntentSql> = LazyLock::new(|| {
    // The process registry is one database on one connection, addressed the
    // way it always has been: unqualified (ADR 0098 §5).
    let dialect = Dialect::sqlite_unqualified();
    ToolIntentSql {
        shared: ToolIntentSubmissionStatements::render(dialect),
        sqlite: ToolIntentSubmissionSqliteStatements::render(dialect),
    }
});

/// The process registry's tool-intent statements, rendered once at first use.
pub(crate) fn tool_intent_sql() -> &'static ToolIntentSql {
    &TOOL_INTENT_SQL
}

/// Drain one page of settled host tool-intent payloads of `session` (of
/// every session when `None`) whose owning run's revision its session no
/// longer retains, answering how many were drained (FIG-6612). Reads
/// already answer them as expired; this releases their bytes.
pub(crate) fn release_settled_tool_intent_payloads(
    tx: &rusqlite::Connection,
    session: Option<&lash_kernel_value::identity::SessionId>,
) -> rusqlite::Result<usize> {
    crate::conn::cached_execute(
        tx,
        tool_intent_sql().shared.release_settled_payloads.sql(),
        rusqlite::params![
            session.map(lash_kernel_value::identity::SessionId::as_str),
            i64::from(lash_store_sql::session::revisions::MATERIAL_RELEASE_BATCH),
        ],
    )
}

#[cfg(test)]
#[path = "turn_ingress/tests.rs"]
mod tests;
