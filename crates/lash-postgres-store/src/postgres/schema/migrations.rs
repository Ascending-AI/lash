//! Active schema admission catalog. Historical declarations live in the
//! test-only `historical_migrations` module.

use super::*;

/// The current cutover rides the chain's only executable arm. Components
/// 101 through 113 are reject-and-recreate boundaries wearing the current
/// target: their empty `statements` never run, but matching `from` and `to`
/// lets the divergence probe enumerate the artifacts a rewound ledger would
/// be claiming to own before the refusal is rendered. Their `source_missing_*`
/// lists stay keyed to this build's catalog — a pre-cutover store lacks the
/// component-107 membership table, the component-109 and -110 additions, and
/// the component-115 effect-replay constraints — so the older-store fixture
/// drops those columns and constraints by name like every other post-floor
/// artifact.
///
/// The history behind those boundaries: component 102 installed the
/// cancellation affected-input child table (FIG-3263); 103 changed no shape
/// (the store-version window's floor move, FIG-2082); 104 added only CHECK
/// constraints to `lash_runtime_effect_group` (FIG-2811); 105 added the
/// `lash_trigger_mutation_receipts` owner columns (FIG-1956); 106 was the
/// creation-request cutover (FIG-3376); 107 added
/// `lash_runtime_effect_group_child` (ADR 0099 §3, FIG-3408); 108 typed the
/// journaled `exec_code` outcome failure (FIG-2362); 109 typed the parent-end
/// plan payload (FIG-3418); 110 carried ADR 0099 §§4–5 (FIG-3409) — the
/// arbitration columns, the sealed `drain_input`, the group counters and
/// `lifecycle`, the two renames, and the uniqueness guards; 111 added
/// settlement-fact carriage and 112 cut over message parts, both encoded
/// payloads only; 113 removed implicit fork observer selection and
/// attribution (FIG-1281); 114 put ADR 0099 §7's group lifecycle in service
/// (FIG-3410) — the reserved `lifecycle` column carrying `closing` and
/// `settled` beside `live`, a values-only cutover with no relational DDL.
/// None is reconstructable by a DDL arm, so each is refused rather than
/// migrated.
///
/// Component 114 is the last reject-and-recreate generation. The
/// component-115 arm (FIG-1947) is the catalog's only executable migration
/// and it is constraint-only: the two payload-pairing `CHECK`s, the
/// settlement-rank `CHECK`, and the two deferred group foreign keys the
/// effect-replay protocol (ADR 0099 §§4–5) already writes are installed with
/// `ADD CONSTRAINT ... NOT VALID` and a separate `VALIDATE CONSTRAINT` for
/// each, so the arm takes no table scan at add time and proves existing rows
/// conform explicitly. A component-114 catalog differs from the endpoint by
/// exactly those five `pg_constraint` rows — no table, column, or guard
/// moved — which is why this boundary is the one the chain can ride forward
/// in place.
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

/// The constraint DDL the component-114 -> 115 arm executes, in order. Every
/// `ADD` lands `NOT VALID` so it takes only `SHARE ROW EXCLUSIVE` and never
/// scans; the matching `VALIDATE` then proves the existing rows conform under
/// a lock the advisory open already holds. A catalog whose rows violate a
/// constraint fails the `VALIDATE` and rolls the whole arm back — the operator
/// sees the conflict rather than a half-migrated schema, the same contract
/// `source_missing_guards` documents.
const EFFECT_REPLAY_CONSTRAINT_DDL: &[&str] = &[
    "ALTER TABLE lash_runtime_effect_replay ADD CONSTRAINT ck_runtime_effect_replay_outcome_json CHECK ((status = 'completed' AND outcome_json IS NOT NULL) OR (status <> 'completed' AND outcome_json IS NULL)) NOT VALID",
    "ALTER TABLE lash_runtime_effect_replay VALIDATE CONSTRAINT ck_runtime_effect_replay_outcome_json",
    "ALTER TABLE lash_runtime_effect_replay ADD CONSTRAINT ck_runtime_effect_replay_error_json CHECK ((status = 'failed' AND error_json IS NOT NULL) OR (status <> 'failed' AND error_json IS NULL)) NOT VALID",
    "ALTER TABLE lash_runtime_effect_replay VALIDATE CONSTRAINT ck_runtime_effect_replay_error_json",
    "ALTER TABLE lash_runtime_effect_replay ADD CONSTRAINT ck_runtime_effect_replay_settlement_seq CHECK ((settlement_seq IS NULL AND NOT (commit_state IN ('drained', 'cancel_decided'))) OR (settlement_seq IS NOT NULL AND commit_state IN ('drained', 'cancel_decided'))) NOT VALID",
    "ALTER TABLE lash_runtime_effect_replay VALIDATE CONSTRAINT ck_runtime_effect_replay_settlement_seq",
    "ALTER TABLE lash_runtime_effect_replay ADD CONSTRAINT fk_runtime_effect_replay_group FOREIGN KEY (group_key) REFERENCES lash_runtime_effect_group(group_key) DEFERRABLE INITIALLY DEFERRED NOT VALID",
    "ALTER TABLE lash_runtime_effect_replay VALIDATE CONSTRAINT fk_runtime_effect_replay_group",
    "ALTER TABLE lash_runtime_effect_group_child ADD CONSTRAINT fk_runtime_effect_group_child_group FOREIGN KEY (group_key) REFERENCES lash_runtime_effect_group(group_key) DEFERRABLE INITIALLY DEFERRED NOT VALID",
    "ALTER TABLE lash_runtime_effect_group_child VALIDATE CONSTRAINT fk_runtime_effect_group_child_group",
];

pub(super) const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    // Keep the outer list expanded for the source-derived fixture checker.
    SchemaMigration {
        from: 101,
        // The lists are keyed to the floor, not to one generation: a relation,
        // column, or constraint introduced after 105 belongs here too, so the
        // fixture rebuilds the published component-101 catalog by removing them.
        to: 115,
        source_missing_tables: &[
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
        source_missing_tables: &["lash_runtime_effect_group_child"],
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
        source_missing_tables: &["lash_runtime_effect_group_child"],
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
        source_missing_tables: &["lash_runtime_effect_group_child"],
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
        source_missing_tables: &["lash_runtime_effect_group_child"],
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
        source_missing_tables: &["lash_runtime_effect_group_child"],
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
        source_missing_tables: &[],
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
        source_missing_tables: &[],
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
        source_missing_tables: &[],
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
    // the component-115 constraints: 111 added settlement-fact carriage and
    // 112 cut over message parts, both encoded payloads only, 113 removed
    // observer-selection metadata, and 114 put the reserved `lifecycle` column
    // in service — a values-only cutover with no relational DDL of its own.
    // Each is refused — the chain's convention keeps one executable arm per
    // generation, and the arm out of the immediate predecessor is the only
    // one this build can prove a source shape for.
    SchemaMigration {
        from: 110,
        to: 115,
        source_missing_tables: &[],
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
        source_missing_tables: &[],
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
        source_missing_tables: &[],
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
        source_missing_tables: &[],
        source_missing_columns: &[],
        source_missing_guards: &[],
        source_missing_foreign_keys: EFFECT_REPLAY_FOREIGN_KEYS,
        introduced_relations: &[],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    // The constraint-only generation (FIG-1947): the only executable arm in
    // the catalog. A component-114 catalog is the endpoint minus the five
    // constraint rows, and the statements install them `NOT VALID` then
    // `VALIDATE` each in turn.
    SchemaMigration {
        from: 114,
        to: 115,
        source_missing_tables: &[],
        source_missing_columns: &[],
        source_missing_guards: &[],
        source_missing_foreign_keys: EFFECT_REPLAY_FOREIGN_KEYS,
        introduced_relations: &[],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: EFFECT_REPLAY_CONSTRAINT_DDL,
    },
];
