//! Active schema admission catalog. Historical declarations live in the
//! test-only `historical_migrations` module.

use super::*;

/// The current cutover is refusal-only: no predecessor shape can be upgraded
/// by moving each cancellation's affected-input evidence into the child table
/// component 102 installed (FIG-3263). Component 103 changed no shape at all —
/// it was the store-version window's floor move (FIG-2082), which retired
/// every migration arm below component 101 rather than a relation — component
/// 104 adds only CHECK constraints to `lash_runtime_effect_group` (FIG-2811),
/// component 105 adds NOT NULL owner columns to
/// `lash_trigger_mutation_receipts` (FIG-1956), and component 106 is the
/// creation-request cutover (FIG-3376), which moved the session-node body and
/// payload generations without touching the relational DDL. A component-101
/// or -102 catalog simply lacks those additions rather than contradicts them.
/// Component 107 adds `lash_runtime_effect_group_child`, the retained accepted
/// membership of an effect group (ADR 0099 §3, FIG-3408) — the first component
/// past 105 to move relational DDL, so the endpoint moves with it and every row
/// now targets 107. A pre-107 catalog lacks that table rather than contradicting
/// it, which is why it joins the source-missing lists: the fixture rebuilds an
/// older published catalog by removing what was introduced above its floor.
/// Component 108 types the journaled `exec_code` outcome failure (FIG-2362) —
/// again a journaled-encoding cutover with no relational DDL — so component
/// 107 is retained as the endpoint and no row targets component 108.
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
        to: 107,
        // The lists are keyed to the floor, not to one generation: a relation
        // or column introduced after 105 belongs here too, so the fixture
        // rebuilds the published component-101 catalog by removing them.
        source_missing_tables: &[
            "lash_turn_cancel_affected_inputs",
            "lash_runtime_effect_group_child",
        ],
        source_missing_columns: &[
            ("lash_trigger_mutation_receipts", "owner_kind"),
            ("lash_trigger_mutation_receipts", "owner_id"),
        ],
        source_missing_guards: &[],
        introduced_relations: &["lash_turn_cancel_affected_inputs"],
        statements: &[],
    },
    // Component 102 to 105 moved no shape the endpoint models beyond what the
    // 101 arm already lists — the cancellation affected-input child table
    // landed *at* component 102 — so a component-102 catalog lacks nothing
    // further the endpoint carries.
    SchemaMigration {
        from: 102,
        to: 107,
        source_missing_tables: &["lash_runtime_effect_group_child"],
        source_missing_columns: &[
            ("lash_trigger_mutation_receipts", "owner_kind"),
            ("lash_trigger_mutation_receipts", "owner_id"),
        ],
        source_missing_guards: &[],
        introduced_relations: &[],
        statements: &[],
    },
    // Component 103 to 105: 104 added only CHECK constraints (unmodeled) and
    // 105 added the owner columns, so a component-103 catalog lacks exactly
    // those columns the endpoint carries.
    SchemaMigration {
        from: 103,
        to: 107,
        source_missing_tables: &["lash_runtime_effect_group_child"],
        source_missing_columns: &[
            ("lash_trigger_mutation_receipts", "owner_kind"),
            ("lash_trigger_mutation_receipts", "owner_id"),
        ],
        source_missing_guards: &[],
        introduced_relations: &[],
        statements: &[],
    },
    // Component 104 to 107: 105 added the owner columns, which this catalog
    // models, so a component-104 catalog lacks exactly those columns.
    SchemaMigration {
        from: 104,
        to: 107,
        source_missing_tables: &["lash_runtime_effect_group_child"],
        source_missing_columns: &[
            ("lash_trigger_mutation_receipts", "owner_kind"),
            ("lash_trigger_mutation_receipts", "owner_id"),
        ],
        source_missing_guards: &[],
        introduced_relations: &[],
        statements: &[],
    },
    // Component 105 to 107: 106 moved no relational DDL, so a component-105
    // catalog differs from the endpoint by the membership table alone.
    SchemaMigration {
        from: 105,
        to: 107,
        source_missing_tables: &["lash_runtime_effect_group_child"],
        source_missing_columns: &[],
        source_missing_guards: &[],
        introduced_relations: &[],
        statements: &[],
    },
    // The immediate predecessor of the retained generation. Component 106 was
    // the creation-request cutover, which moved payload generations without
    // touching the relational DDL, so a component-106 catalog is the endpoint
    // minus exactly the table component 107 adds.
    SchemaMigration {
        from: 106,
        to: 107,
        source_missing_tables: &["lash_runtime_effect_group_child"],
        source_missing_columns: &[],
        source_missing_guards: &[],
        // The relation this generation creates, which the divergence refusal
        // over a component-106 catalog enumerates by name.
        introduced_relations: &["lash_runtime_effect_group_child"],
        statements: &[],
    },
];
