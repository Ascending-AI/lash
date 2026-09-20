//! Active schema admission catalog. Historical declarations live in the
//! test-only `historical_migrations` module.

use super::*;

/// The current cutover is refusal-only: no predecessor shape can be upgraded
/// by moving each cancellation's affected-input evidence into the child table
/// component 102 installed (FIG-3263), and component 103 changes no shape at
/// all — it is the store-version window's floor move (FIG-2082), which retires
/// every migration arm below component 101 rather than a relation. Component
/// 102 is therefore retained as the refusal-only endpoint and no row targets
/// component 103.
///
/// Component 102's child table is why the cutover is a refusal rather than a
/// creation migration: a component-101 store recorded the evidence as two
/// parallel arrays on the request row, and no DDL can re-derive the snapshot
/// each child row now owns. The missing fact is data, and no DDL can invent
/// it.
pub(super) const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    // Keep the outer list expanded for the source-derived fixture checker.
    SchemaMigration {
        from: 101,
        to: 102,
        // The lists are keyed to the floor, not to one generation: a relation
        // or column introduced after 102 belongs here too, so the fixture
        // rebuilds the published component-101 catalog by removing them.
        source_missing_tables: &["lash_turn_cancel_affected_inputs"],
        source_missing_columns: &[],
        source_missing_guards: &[],
        introduced_relations: &["lash_turn_cancel_affected_inputs"],
        statements: &[],
    },
];
