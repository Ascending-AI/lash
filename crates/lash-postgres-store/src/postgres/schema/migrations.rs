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
/// membership of an effect group (ADR 0099 §3, FIG-3408). Component 108 types
/// the journaled `exec_code` outcome failure (FIG-2362) — a journaled-encoding
/// cutover with no relational DDL. Component 109 types the parent-end plan
/// payload (FIG-3418): `lash_parent_end_plans.parent_payload` is a column a
/// pre-cutover row cannot be retrofitted with, so the boundary is again
/// reject-and-recreate and the retained endpoint moves to 108. Component 110
/// carries ADR 0099 §§4–5 (FIG-3409): the `commit_state` / `commit_seq`
/// arbitration columns and the sealed `drain_input` on
/// `lash_runtime_effect_replay`, the `next_commit_seq` counter and `lifecycle`
/// phase value on `lash_runtime_effect_group`, the `children` →
/// `expected_children` arity rename, the membership's `request_version` →
/// `command_version` rename, and the commit-order and replay-key uniqueness
/// guards — a shape no migration arm rebuilds into, so the boundary is again
/// reject-and-recreate. Component 111 adds settlement-fact carriage and
/// component 112 cuts over message parts; both change encoded payloads.
/// Component 113 removes observer selectors and attribution. Component 114
/// adds durable queued-run admissions and normalized membership. The retained
/// endpoint is 113; no arm targets the current component 114, so admission
/// refuses every predecessor without migrating it. The
/// `source_missing_*` lists stay keyed to this build's catalog — a
/// pre-cutover store lacks the component-109 and -110 additions against it —
/// so the older-store fixture drops those columns by name like every other
/// post-floor column.
///
/// Component 102's child table is why the cutover is a refusal rather than a
/// creation migration: a component-101 store recorded the evidence as two
/// parallel arrays on the request row, and no DDL can re-derive the snapshot
/// each child row now owns. The missing fact is data, and no DDL can invent
/// it.
/// The uniqueness guards component 110 installs (FIG-3409): the §4
/// commit-order backstop (one commit position per group across the replay
/// rows) and one membership row per declared replay key. Shared by every arm
/// whose source predates the arbitration generation.
const ARBITRATION_GUARDS: &[DeclaredGuard] = &[
    DeclaredGuard {
        table: "lash_runtime_effect_replay",
        columns: &["group_key", "commit_seq"],
        predicate: Some("commit_seq is not null"),
    },
    DeclaredGuard {
        table: "lash_runtime_effect_group_child",
        columns: &["group_key", "replay_key"],
        predicate: None,
    },
];

pub(super) const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    // Keep the outer list expanded for the source-derived fixture checker.
    SchemaMigration {
        from: 101,
        to: 113,
        // The lists are keyed to the floor, not to one generation: a relation
        // or column introduced after 105 belongs here too, so the fixture
        // rebuilds the published component-101 catalog by removing them.
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_turn_cancel_affected_inputs",
            "lash_runtime_effect_group_child",
        ],
        source_missing_columns: &[
            ("lash_trigger_mutation_receipts", "owner_kind"),
            ("lash_trigger_mutation_receipts", "owner_id"),
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
        ],
        source_missing_guards: ARBITRATION_GUARDS,
        // Every post-floor relation the rebuild must account for: the
        // component-102 table the floor arm's range introduces and the
        // commit-order unique component 110 adds to the pre-floor replay
        // table, which no table drop covers.
        introduced_relations: &[
            "lash_turn_cancel_affected_inputs",
            "uq_lash_runtime_effect_replay_commit_seq",
        ],
        statements: &[],
    },
    // Component 102 to 105 moved no shape the endpoint models beyond what the
    // 101 arm already lists — the cancellation affected-input child table
    // landed *at* component 102 — so a component-102 catalog lacks nothing
    // further the endpoint carries.
    SchemaMigration {
        from: 102,
        to: 113,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_runtime_effect_group_child",
        ],
        source_missing_columns: &[
            ("lash_trigger_mutation_receipts", "owner_kind"),
            ("lash_trigger_mutation_receipts", "owner_id"),
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
        ],
        source_missing_guards: ARBITRATION_GUARDS,
        introduced_relations: &[],
        statements: &[],
    },
    // Component 103 to 105: 104 added only CHECK constraints (unmodeled) and
    // 105 added the owner columns, so a component-103 catalog lacks exactly
    // those columns the endpoint carries.
    SchemaMigration {
        from: 103,
        to: 113,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_runtime_effect_group_child",
        ],
        source_missing_columns: &[
            ("lash_trigger_mutation_receipts", "owner_kind"),
            ("lash_trigger_mutation_receipts", "owner_id"),
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
        ],
        source_missing_guards: ARBITRATION_GUARDS,
        introduced_relations: &[],
        statements: &[],
    },
    // Component 104 to 107: 105 added the owner columns, which this catalog
    // models, so a component-104 catalog lacks exactly those columns.
    SchemaMigration {
        from: 104,
        to: 113,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_runtime_effect_group_child",
        ],
        source_missing_columns: &[
            ("lash_trigger_mutation_receipts", "owner_kind"),
            ("lash_trigger_mutation_receipts", "owner_id"),
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
        ],
        source_missing_guards: ARBITRATION_GUARDS,
        introduced_relations: &[],
        statements: &[],
    },
    // Component 105 to 107: 106 moved no relational DDL, so a component-105
    // catalog differs from the endpoint by the membership table, the typed
    // parent payload, and the arbitration state alone.
    SchemaMigration {
        from: 105,
        to: 113,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_runtime_effect_group_child",
        ],
        source_missing_columns: &[
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
        ],
        source_missing_guards: ARBITRATION_GUARDS,
        introduced_relations: &[],
        statements: &[],
    },
    // The retained membership's introduction. Component 106 was the
    // creation-request cutover, which moved payload generations without
    // touching the relational DDL, so a component-106 catalog is the endpoint
    // minus the table component 107 adds and what components 109 and 110 add
    // to its siblings.
    SchemaMigration {
        from: 106,
        to: 113,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_runtime_effect_group_child",
        ],
        source_missing_columns: &[
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
        ],
        source_missing_guards: ARBITRATION_GUARDS,
        // The relation the membership generation creates, which the divergence
        // refusal over a component-106 catalog enumerates by name.
        introduced_relations: &["lash_runtime_effect_group_child"],
        statements: &[],
    },
    // A component-107 catalog carries the membership table but none of what
    // components 109 and 110 add — the typed parent payload and the ADR 0099
    // §§4–5 columns and guards — and it still spells the membership's version
    // column `request_version`, which 110 renamed `command_version`. Component
    // 108 added no relational DDL of its own.
    SchemaMigration {
        from: 107,
        to: 113,
        source_missing_tables: &["lash_queued_run_members", "lash_queued_runs"],
        source_missing_columns: &[
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
            ("lash_runtime_effect_group_child", "command_version"),
        ],
        source_missing_guards: ARBITRATION_GUARDS,
        introduced_relations: &[],
        statements: &[],
    },
    // A component-108 catalog adds only the journaled `exec_code` outcome
    // cutover to 107's relational shape, so it lacks the same columns and
    // guards a component-107 catalog does against this build.
    SchemaMigration {
        from: 108,
        to: 113,
        source_missing_tables: &["lash_queued_run_members", "lash_queued_runs"],
        source_missing_columns: &[
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
            ("lash_runtime_effect_group_child", "command_version"),
        ],
        source_missing_guards: ARBITRATION_GUARDS,
        // The relations the arbitration generation creates, which the
        // divergence refusal over a component-108 catalog enumerates by name:
        // indexes are relations too, and these two are what the generation
        // adds that a component-108 catalog can already be carrying.
        introduced_relations: &[
            "uq_lash_runtime_effect_replay_commit_seq",
            "uq_lash_runtime_effect_group_child_replay_key",
        ],
        statements: &[],
    },
    // A component-109 catalog predates the ADR 0099 §§4–5 arbitration state
    // wholesale: the commit-protocol columns and guards on the replay row, the
    // group counters, and both renames. Component 111 adds no relational DDL
    // of its own; this historical arm now targets the retained endpoint 113.
    SchemaMigration {
        from: 109,
        to: 113,
        source_missing_tables: &["lash_queued_run_members", "lash_queued_runs"],
        source_missing_columns: &[
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
            ("lash_runtime_effect_group_child", "command_version"),
        ],
        source_missing_guards: ARBITRATION_GUARDS,
        // The relations the arbitration generation creates, which the
        // divergence refusal over a component-109 catalog enumerates by name.
        introduced_relations: &[
            "uq_lash_runtime_effect_replay_commit_seq",
            "uq_lash_runtime_effect_group_child_replay_key",
        ],
        statements: &[],
    },
    // Components 111 and 112 changed encoded payloads only. This endpoint
    // declares no route across the current queued-run cutover.
    SchemaMigration {
        from: 110,
        to: 113,
        source_missing_tables: &["lash_queued_run_members", "lash_queued_runs"],
        source_missing_columns: &[],
        source_missing_guards: &[],
        introduced_relations: &[],
        statements: &[],
    },
    // Retained historical endpoint only: component 114 remains unreachable.
    SchemaMigration {
        from: 111,
        to: 113,
        source_missing_tables: &["lash_queued_run_members", "lash_queued_runs"],
        source_missing_columns: &[],
        source_missing_guards: &[],
        introduced_relations: &[],
        statements: &[],
    },
    SchemaMigration {
        from: 112,
        to: 113,
        source_missing_tables: &["lash_queued_run_members", "lash_queued_runs"],
        source_missing_columns: &[],
        source_missing_guards: &[],
        introduced_relations: &[],
        statements: &[],
    },
];
