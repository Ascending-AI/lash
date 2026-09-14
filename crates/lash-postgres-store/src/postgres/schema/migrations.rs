//! Active schema admission catalog. Historical declarations live in the
//! test-only `historical_migrations` module.

use super::*;

/// The current cutovers are refusal-only: no predecessor shape can be upgraded
/// by inventing the parent scope every child registration carries (component
/// 93), and none can be upgraded by inventing the cancellation timestamp the
/// process rows now index (component 94). Component 93 is therefore retained as
/// the refusal-only endpoint and no row targets component 94.
pub(super) const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    // Keep the outer list expanded for the source-derived fixture checker.
    SchemaMigration {
        from: 92,
        to: 93,
        // Current-catalog relations and columns a component-92 store does not
        // have. The list is keyed to the floor, not to one generation: the
        // fixture rebuilds the published component-92 catalog by removing
        // these from the schema this build installs, so a column introduced
        // after 93 belongs here too.
        source_missing_tables: &["lash_parent_end_plans"],
        source_missing_columns: &[
            ("lash_processes", "parent_scope_kind"),
            ("lash_processes", "parent_scope_id"),
            ("lash_processes", "on_parent_end"),
            ("lash_processes", "cancel_requested_at_ms"),
        ],
        source_missing_guards: &[],
        introduced_relations: &[
            "lash_parent_end_plans",
            "idx_lash_parent_end_plans_pending",
            "idx_lash_processes_parent_scope",
            "idx_lash_processes_parent_end_pending",
            "idx_lash_processes_pending_cancel",
        ],
        statements: &[],
    },
];
