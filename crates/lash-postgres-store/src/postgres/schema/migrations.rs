//! Active schema admission catalog. Historical declarations live in the
//! test-only `historical_migrations` module.

use super::*;

/// The current cutovers are refusal-only: no predecessor shape can be upgraded
/// by inventing the registry-minted incarnation that qualifies a process
/// attachment owner (component 90), and none can be upgraded by inventing
/// durable cancellation facts (component 91). Component 90 is therefore
/// retained as the refusal-only endpoint and no row targets component 91.
pub(super) const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    // Keep the outer list expanded for the source-derived fixture checker.
    SchemaMigration {
        from: 89,
        to: 90,
        source_missing_tables: &[
            "lash_turn_cancellation_bindings",
            "lash_turn_cancel_closure_authorizations",
            "lash_turn_cancel_retired_scopes",
            "lash_turn_cancel_closure_participants",
        ],
        source_missing_columns: &[("lash_turn_cancel_requests", "intent_revision")],
        source_missing_guards: &[],
        introduced_relations: &[
            "lash_turn_cancellation_bindings",
            "lash_turn_cancel_closure_authorizations",
            "lash_turn_cancel_retired_scopes",
            "lash_turn_cancel_closure_participants",
        ],
        statements: &[],
    },
];
