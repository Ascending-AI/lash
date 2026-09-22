//! Active schema admission catalog. Historical declarations live in the
//! test-only `historical_migrations` module.

use super::*;

/// These declarations retain the component-115 endpoint, including the five
/// effect-replay constraints its 114 -> 115 migration installed. Component
/// 116 adds durable queued-run admission and normalized membership. No old
/// catalog carries their replay ownership, so the current build offers no
/// migration and refuses every predecessor, including 115. Source-shape
/// declarations remain keyed to this build's catalog for precise older-store
/// fixture construction.
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

/// The same five constraints as bare names, for `introduced_constraints`: the
/// divergence probe resolves them through `pg_constraint`, which a constraint
/// name alone addresses.
const EFFECT_REPLAY_CONSTRAINT_NAMES: &[&str] = &[
    "ck_runtime_effect_replay_outcome_json",
    "ck_runtime_effect_replay_error_json",
    "ck_runtime_effect_replay_settlement_seq",
    "fk_runtime_effect_replay_group",
    "fk_runtime_effect_group_child_group",
];

/// The two foreign keys component 115 adds, at the structural granularity the
/// shape diff reports them — the declaration `matches_source_shape` tolerates
/// on the arm's source and nothing else.
const REPLAY_GROUP_FOREIGN_KEY: DeclaredForeignKey = DeclaredForeignKey {
    table: "lash_runtime_effect_replay",
    columns: &["group_key"],
    parent_table: "lash_runtime_effect_group",
    parent_columns: &["group_key"],
    on_delete: ForeignKeyAction::NoAction,
    deferrable: true,
    initially_deferred: true,
};
const GROUP_CHILD_GROUP_FOREIGN_KEY: DeclaredForeignKey = DeclaredForeignKey {
    table: "lash_runtime_effect_group_child",
    columns: &["group_key"],
    parent_table: "lash_runtime_effect_group",
    parent_columns: &["group_key"],
    on_delete: ForeignKeyAction::NoAction,
    deferrable: true,
    initially_deferred: true,
};
const EFFECT_REPLAY_FOREIGN_KEYS: &[DeclaredForeignKey] =
    &[REPLAY_GROUP_FOREIGN_KEY, GROUP_CHILD_GROUP_FOREIGN_KEY];

pub(super) const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    // Keep the outer list expanded for the source-derived fixture checker.
    SchemaMigration {
        from: 101,
        // The lists are keyed to the floor, not to one generation: a relation,
        // column, or constraint introduced after 105 belongs here too, so the
        // fixture rebuilds the published component-101 catalog by removing them.
        to: 115,
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
        source_missing_foreign_keys: &[REPLAY_GROUP_FOREIGN_KEY],
        // Every post-floor relation the rebuild must account for: the
        // component-102 table the floor arm's range introduces and the
        // commit-order unique component 110 adds to the pre-floor replay
        // table, which no table drop covers.
        introduced_relations: &[
            "lash_turn_cancel_affected_inputs",
            "uq_lash_runtime_effect_replay_commit_seq",
        ],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    // Component 102 to 105 moved no shape the endpoint models beyond what the
    // 101 arm already lists — the cancellation affected-input child table
    // landed *at* component 102 — so a component-102 catalog lacks nothing
    // further the endpoint carries.
    SchemaMigration {
        from: 102,
        to: 115,
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
        source_missing_foreign_keys: &[REPLAY_GROUP_FOREIGN_KEY],
        introduced_relations: &[],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    // Component 103 to 105: 104 added only CHECK constraints (unmodeled) and
    // 105 added the owner columns, so a component-103 catalog lacks exactly
    // those columns the endpoint carries.
    SchemaMigration {
        from: 103,
        to: 115,
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
        source_missing_foreign_keys: &[REPLAY_GROUP_FOREIGN_KEY],
        introduced_relations: &[],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    // Component 104 to 107: 105 added the owner columns, which this catalog
    // models, so a component-104 catalog lacks exactly those columns.
    SchemaMigration {
        from: 104,
        to: 115,
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
        source_missing_foreign_keys: &[REPLAY_GROUP_FOREIGN_KEY],
        introduced_relations: &[],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    // Component 105 to 107: 106 moved no relational DDL, so a component-105
    // catalog differs from the endpoint by the membership table, the typed
    // parent payload, and the arbitration state alone.
    SchemaMigration {
        from: 105,
        to: 115,
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
        source_missing_foreign_keys: &[REPLAY_GROUP_FOREIGN_KEY],
        introduced_relations: &[],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    // The retained membership's introduction. Component 106 was the
    // creation-request cutover, which moved payload generations without
    // touching the relational DDL, so a component-106 catalog is the endpoint
    // minus the table component 107 adds and what components 109 and 110 add
    // to its siblings.
    SchemaMigration {
        from: 106,
        to: 115,
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
        source_missing_foreign_keys: &[REPLAY_GROUP_FOREIGN_KEY],
        // The relation the membership generation creates, which the divergence
        // refusal over a component-106 catalog enumerates by name.
        introduced_relations: &["lash_runtime_effect_group_child"],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    // A component-107 catalog carries the membership table but none of what
    // components 109 and 110 add — the typed parent payload and the ADR 0099
    // §§4–5 columns and guards — and it still spells the membership's version
    // column `request_version`, which 110 renamed `command_version`. Component
    // 108 added no relational DDL of its own.
    SchemaMigration {
        from: 107,
        to: 115,
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
        source_missing_foreign_keys: EFFECT_REPLAY_FOREIGN_KEYS,
        introduced_relations: &[],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    // A component-108 catalog adds only the journaled `exec_code` outcome
    // cutover to 107's relational shape, so it lacks the same columns and
    // guards a component-107 catalog does against this build.
    SchemaMigration {
        from: 108,
        to: 115,
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
        source_missing_foreign_keys: EFFECT_REPLAY_FOREIGN_KEYS,
        // The relations the arbitration generation creates, which the
        // divergence refusal over a component-108 catalog enumerates by name:
        // indexes are relations too, and these two are what the generation
        // adds that a component-108 catalog can already be carrying.
        introduced_relations: &[
            "uq_lash_runtime_effect_replay_commit_seq",
            "uq_lash_runtime_effect_group_child_replay_key",
        ],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    // A component-109 catalog predates the ADR 0099 §§4–5 arbitration state
    // wholesale: the commit-protocol columns and guards on the replay row, the
    // group counters, and both renames. Components 110 through 113 add no
    // modeled relational DDL of their own beyond what this row already lists;
    // the catalog it lacks against the component-114 endpoint is exactly those
    // columns and guards plus the five effect-replay constraints.
    SchemaMigration {
        from: 109,
        to: 115,
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
        source_missing_foreign_keys: EFFECT_REPLAY_FOREIGN_KEYS,
        // The relations the arbitration generation creates, which the
        // divergence refusal over a component-109 catalog enumerates by name.
        introduced_relations: &[
            "uq_lash_runtime_effect_replay_commit_seq",
            "uq_lash_runtime_effect_group_child_replay_key",
        ],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    // Components 110 through 113 share the endpoint's relational shape minus
    // the component-115 constraints and component-116 queued-run tables.
    // All are refused at the queued-run cutover.
    SchemaMigration {
        from: 110,
        to: 115,
        source_missing_tables: &["lash_queued_run_members", "lash_queued_runs"],
        source_missing_columns: &[],
        source_missing_guards: &[],
        source_missing_foreign_keys: EFFECT_REPLAY_FOREIGN_KEYS,
        introduced_relations: &[],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    SchemaMigration {
        from: 111,
        to: 115,
        source_missing_tables: &["lash_queued_run_members", "lash_queued_runs"],
        source_missing_columns: &[],
        source_missing_guards: &[],
        source_missing_foreign_keys: EFFECT_REPLAY_FOREIGN_KEYS,
        introduced_relations: &[],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    SchemaMigration {
        from: 112,
        to: 115,
        source_missing_tables: &["lash_queued_run_members", "lash_queued_runs"],
        source_missing_columns: &[],
        source_missing_guards: &[],
        source_missing_foreign_keys: EFFECT_REPLAY_FOREIGN_KEYS,
        introduced_relations: &[],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    SchemaMigration {
        from: 113,
        to: 115,
        source_missing_tables: &["lash_queued_run_members", "lash_queued_runs"],
        source_missing_columns: &[],
        source_missing_guards: &[],
        source_missing_foreign_keys: EFFECT_REPLAY_FOREIGN_KEYS,
        introduced_relations: &[],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    // Component 114 lacked the five effect-replay constraints installed at
    // 115. The retained declaration records their historical shape while the
    // current component-116 cutover offers no executable migration.
    SchemaMigration {
        from: 114,
        to: 115,
        source_missing_tables: &["lash_queued_run_members", "lash_queued_runs"],
        source_missing_columns: &[],
        source_missing_guards: &[],
        source_missing_foreign_keys: EFFECT_REPLAY_FOREIGN_KEYS,
        introduced_relations: &[],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
];
