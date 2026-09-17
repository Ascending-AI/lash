//! Active schema admission catalog. Historical declarations live in the
//! test-only `historical_migrations` module.

use super::*;

/// The current cutovers are refusal-only: no predecessor shape can be upgraded
/// by inventing the source contract and provider route every trigger
/// subscription now captures (component 95), and none can be upgraded by
/// turning a journaled sleep's resolved duration back into the deadline the
/// guest asked for (component 96). Component 98 is therefore retained as the
/// refusal-only endpoint and no row targets component 99.
///
/// Neither refusal-only generation installs a relation: component 95's capture
/// lives inside the trigger subscription record document, and component 96
/// moves only the journaled effect-command encoding. That is precisely why both
/// cutovers are refusals rather than creation migrations — the missing fact is
/// data, and no DDL can invent it.
pub(super) const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    // Keep the outer list expanded for the source-derived fixture checker.
    SchemaMigration {
        from: 94,
        to: 98,
        // Component 95 (the source-call contract capture) and component 96
        // (the SleepSpec encoding) were refusal-only cutovers; component 97
        // creates the named process-definition registry (FIG-2995), component
        // 98 the release stamp (FIG-3092) and component 99 the trigger
        // subscription lifecycle column (FIG-1951). No predecessor records
        // existed at any of these moves, so the retained endpoint carries all
        // five: a pre-cutover store is refused at open rather than migrated
        // (its schema lacks both relations, the lifecycle columns, and the
        // retained trigger capture).
        source_missing_tables: &["lash_process_definitions", "lash_release_stamp"],
        source_missing_columns: &[
            ("lash_trigger_subscriptions", "lifecycle"),
            ("lash_trigger_subscriptions", "deleted_at_ms"),
        ],
        source_missing_guards: &[],
        introduced_relations: &["lash_process_definitions", "lash_release_stamp"],
        statements: &[],
    },
    SchemaMigration {
        from: 95,
        to: 98,
        source_missing_tables: &[],
        source_missing_columns: &[],
        source_missing_guards: &[],
        introduced_relations: &[],
        statements: &[],
    },
    SchemaMigration {
        from: 96,
        to: 98,
        source_missing_tables: &[],
        source_missing_columns: &[],
        source_missing_guards: &[],
        introduced_relations: &[],
        statements: &[],
    },
    SchemaMigration {
        from: 97,
        to: 98,
        source_missing_tables: &[],
        source_missing_columns: &[],
        source_missing_guards: &[],
        introduced_relations: &[],
        statements: &[],
    },
];
