//! Active schema admission catalog. Historical declarations live in the
//! test-only `historical_migrations` module.

use super::*;

/// The current cutovers are refusal-only: no predecessor shape can be upgraded
/// by inventing the source contract and provider route every trigger
/// subscription now captures (component 95), and none can be upgraded by
/// turning a journaled sleep's resolved duration back into the deadline the
/// guest asked for (component 96). Component 95 is therefore retained as the
/// refusal-only endpoint and no row targets component 96.
///
/// Neither generation installs a relation: component 95's capture lives inside
/// the trigger subscription record document, and component 96 moves only the
/// journaled effect-command encoding. That is precisely why both cutovers are
/// refusals rather than creation migrations — the missing fact is data, and no
/// DDL can invent it.
pub(super) const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    // Keep the outer list expanded for the source-derived fixture checker.
    SchemaMigration {
        from: 96,
        to: 97,
        // Component 97 creates the named process-definition registry
        // (FIG-2995). The table arrives empty in any predecessor, so a
        // creation-only migration carries it; no column elsewhere moves.
        source_missing_tables: &["lash_process_definitions"],
        source_missing_columns: &[],
        source_missing_guards: &[],
        introduced_relations: &["lash_process_definitions"],
        statements: &[
            "CREATE TABLE IF NOT EXISTS lash_process_definitions (definition_id TEXT PRIMARY KEY, owner_scope TEXT NOT NULL, name TEXT NOT NULL, revision BIGINT NOT NULL, fingerprint TEXT NOT NULL, lifecycle TEXT NOT NULL, deleted_at_ms BIGINT, change_seq BIGINT NOT NULL, created_at_ms BIGINT NOT NULL, updated_at_ms BIGINT NOT NULL, record_json TEXT NOT NULL, CONSTRAINT ck_process_definitions_lifecycle CHECK ((lifecycle IN ('enabled', 'disabled') AND deleted_at_ms IS NULL) OR (lifecycle = 'tombstoned' AND deleted_at_ms IS NOT NULL)), UNIQUE(owner_scope, name))",
            "CREATE INDEX IF NOT EXISTS idx_lash_process_definitions_registrant ON lash_process_definitions(owner_scope, name)",
            "CREATE INDEX IF NOT EXISTS idx_lash_process_definitions_change ON lash_process_definitions(change_seq)",
        ],
    },
];
