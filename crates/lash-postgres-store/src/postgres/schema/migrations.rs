//! Active schema admission catalog. Historical declarations live in the
//! test-only `historical_migrations` module.

use super::*;

/// The current cutovers are refusal-only: no predecessor shape can be upgraded
/// by inventing the cancellation timestamp the process rows now index
/// (component 94), and none can be upgraded by inventing the source contract
/// and provider route every trigger subscription now captures (component 95).
/// Component 94 is therefore retained as the refusal-only endpoint and no row
/// targets component 95.
///
/// Component 95 installs no new relation: the capture lives inside the trigger
/// subscription record document, which a component-94 store wrote without it.
/// That is precisely why the cutover is refusal-only rather than a creation
/// migration — the missing fact is data, and no DDL can invent it.
pub(super) const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    // Keep the outer list expanded for the source-derived fixture checker.
    SchemaMigration {
        from: 93,
        to: 94,
        // Current-catalog relations and columns a component-93 store does not
        // have. The list is keyed to the floor, not to one generation: the
        // fixture rebuilds the published component-93 catalog by removing
        // these from the schema this build installs, so a column introduced
        // after 94 belongs here too.
        source_missing_tables: &[],
        source_missing_columns: &[("lash_processes", "cancel_requested_at_ms")],
        source_missing_guards: &[],
        introduced_relations: &["idx_lash_processes_pending_cancel"],
        statements: &[],
    },
];
