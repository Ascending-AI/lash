//! The SQLite half of the turn-ingress and claim family (FIG-3383).
//!
//! `lash-store-sql`'s `turn_ingress` module owns the column lists and every
//! statement whose text both backends issue verbatim; this module owns the
//! statements that genuinely fork and renders both sets once, at startup.
//!
//! Three renderings, because the family is reached through three different
//! addressings:
//!
//! * the session catalog's own database, [`turn_ingress_sql`], which is where
//!   nine of the ten tables live;
//! * the process registry's database, [`tool_intent_sql`], a one-connection
//!   family addressed unqualified the way it always has been;
//! * every schema an effect host has attached,
//!   [`closure_participant_sql`], because "is this cancellation scope still
//!   occupied?" is asked from the journal's connection as well as the
//!   catalog's.

use lash_store_sql::turn_ingress::queued_runs::QueuedRunStatements;
use std::sync::LazyLock;

use lash_core_execution::store_backend_support as vocabulary;
use lash_store_sql::turn_ingress::{
    TurnIngressStatements, cancel_requests::CancelRequestStatements,
    cancellation_bindings::CancellationBindingStatements,
    closure_authorizations::ClosureAuthorizationStatements,
    closure_participants::ClosureParticipantStatements, pending_inputs::PendingInputStatements,
    queued_batches::QueuedBatchStatements, queued_items::QueuedItemStatements,
    retired_scopes::RetiredScopeStatements,
    session_execution_leases::SessionExecutionLeaseStatements,
    tool_intent_submissions::ToolIntentSubmissionStatements,
};
use lash_store_sql::{Dialect, Vocabulary, VocabularyTerm};

use crate::scope_fence::Schema;

mod family;
mod pending_inputs;
mod queued_work;
mod turn_cancel;

pub(crate) use family::TurnIngressSqliteStatements;
pub(crate) use pending_inputs::PendingInputSqliteStatements;
pub(crate) use queued_work::{QueuedBatchSqliteStatements, QueuedItemSqliteStatements};
pub(crate) use turn_cancel::{
    CancelRequestSqliteStatements, CancellationBindingSqliteStatements,
    ClosureAuthorizationSqliteStatements, RetiredScopeSqliteStatements,
    ToolIntentSubmissionSqliteStatements,
};

/// The `pending_turn_inputs.state` partitions this family's statements name.
///
/// Every expansion is generated from
/// [`TurnInputStateKind`](lash_core_execution::runtime::TurnInputStateKind), so adding a state is
/// one edit in `lash-core-store` rather than one per statement. The term names
/// are the domain's, and PostgreSQL registers the same seven.
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

/// Every turn-ingress statement the session catalog issues.
pub(crate) struct TurnIngressSql {
    pub(crate) queued_runs: QueuedRunStatements,
    /// Cross-table statements both backends issue verbatim.
    pub(crate) family: TurnIngressStatements,
    /// Cross-table statements only SQLite issues.
    pub(crate) family_sqlite: TurnIngressSqliteStatements,
    /// `pending_turn_inputs`, shared.
    pub(crate) pending_inputs: PendingInputStatements,
    /// `pending_turn_inputs`, SQLite only.
    pub(crate) pending_inputs_sqlite: PendingInputSqliteStatements,
    /// `queued_work_batches`, shared.
    pub(crate) queued_batches: QueuedBatchStatements,
    /// `queued_work_batches`, SQLite only.
    pub(crate) queued_batches_sqlite: QueuedBatchSqliteStatements,
    /// `queued_work_items`, shared.
    pub(crate) queued_items: QueuedItemStatements,
    /// `queued_work_items`, SQLite only.
    pub(crate) queued_items_sqlite: QueuedItemSqliteStatements,
    /// `session_execution_leases`, shared.
    pub(crate) leases: SessionExecutionLeaseStatements,
    /// `turn_cancel_requests`, shared.
    pub(crate) cancel_requests: CancelRequestStatements,
    /// `turn_cancel_requests`, SQLite only.
    pub(crate) cancel_requests_sqlite: CancelRequestSqliteStatements,
    /// `turn_cancellation_bindings`, shared.
    pub(crate) bindings: CancellationBindingStatements,
    /// `turn_cancellation_bindings`, SQLite only.
    pub(crate) bindings_sqlite: CancellationBindingSqliteStatements,
    /// `turn_cancel_closure_authorizations`, shared.
    pub(crate) closures: ClosureAuthorizationStatements,
    /// `turn_cancel_closure_authorizations`, SQLite only.
    pub(crate) closures_sqlite: ClosureAuthorizationSqliteStatements,
    /// `turn_cancel_retired_scopes`, shared.
    pub(crate) retired_scopes: RetiredScopeStatements,
    /// `turn_cancel_retired_scopes`, SQLite only.
    pub(crate) retired_scopes_sqlite: RetiredScopeSqliteStatements,
}

impl TurnIngressSql {
    fn render() -> Self {
        let dialect = Schema::Main.dialect().with_vocabulary(TURN_INPUT_LIFECYCLE);
        Self {
            queued_runs: QueuedRunStatements::render(dialect),
            family: TurnIngressStatements::render(dialect),
            family_sqlite: TurnIngressSqliteStatements::render(dialect),
            pending_inputs: PendingInputStatements::render(dialect),
            pending_inputs_sqlite: PendingInputSqliteStatements::render(dialect),
            queued_batches: QueuedBatchStatements::render(dialect),
            queued_batches_sqlite: QueuedBatchSqliteStatements::render(dialect),
            queued_items: QueuedItemStatements::render(dialect),
            queued_items_sqlite: QueuedItemSqliteStatements::render(dialect),
            leases: SessionExecutionLeaseStatements::render(dialect),
            cancel_requests: CancelRequestStatements::render(dialect),
            cancel_requests_sqlite: CancelRequestSqliteStatements::render(dialect),
            bindings: CancellationBindingStatements::render(dialect),
            bindings_sqlite: CancellationBindingSqliteStatements::render(dialect),
            closures: ClosureAuthorizationStatements::render(dialect),
            closures_sqlite: ClosureAuthorizationSqliteStatements::render(dialect),
            retired_scopes: RetiredScopeStatements::render(dialect),
            retired_scopes_sqlite: RetiredScopeSqliteStatements::render(dialect),
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

static CLOSURE_PARTICIPANT_SQL: LazyLock<[ClosureParticipantStatements; 3]> = LazyLock::new(|| {
    Schema::ALL.map(|schema| ClosureParticipantStatements::render(schema.dialect()))
});

/// The cancellation-closure participant statements addressed through `schema`.
pub(crate) fn closure_participant_sql(schema: Schema) -> &'static ClosureParticipantStatements {
    &CLOSURE_PARTICIPANT_SQL[schema.index()]
}

#[cfg(test)]
#[path = "turn_ingress/tests.rs"]
mod tests;
