//! Active schema admission catalog. Every row targets the current component.
//! Historical declarations live in the test-only historical_migrations module.

use super::*;

/// Component 88 has no durable cancellation authority and must be recreated.
pub(super) const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    // The immediate predecessor is explicitly refusal-only.
    SchemaMigration {
        from: 88,
        to: 89,
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
        // Old rows lack cancellation authority; this is an explicit recreation
        // boundary, not a migration that invents authority for existing work.
        statements: &[],
    },
];
