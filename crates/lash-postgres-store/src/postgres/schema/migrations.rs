//! Active schema admission catalog. Historical declarations live in the
//! test-only `historical_migrations` module.

use super::*;

/// These declarations retain the component-127 endpoint: the five
/// effect-replay constraints component 115 installed, the durable queued-run
/// admission component 116 adds, the runtime-commit receipt version component
/// 117 stamps, the NOT NULL `submitted_ingress_json` and
/// `submission_digest` columns component 118 (FIG-3544) gives
/// `lash_pending_turn_inputs`, the runtime-error and turn-outcome
/// vocabularies component 119 (FIG-3532) persists, and the
/// `claim_bound_turn_id` / `claim_bound_receipt_input_id` binding and its
/// CHECK component 120 (FIG-3589) gives `lash_pending_turn_inputs`, and the
/// `COLLATE "C"` effect-journal key columns and `lash_turn_parks` component 121
/// (FIG-3586) adds, and the runtime-error vocabulary component 122
/// (FIG-3598) persists, which moves no relation. Component 123 (FIG-3588)
/// adds the nullable `started_json` segment start marker to
/// `lash_process_segment_handovers`. Component 124 (FIG-3587) changes
/// persisted vocabularies (runtime-error codes, turn-park reasons) and moves
/// no relation, and component 125 replaces a runtime-error code
/// (`worker_replacement_abort` by `effect_replay_divergence`) and moves no
/// relation either. Component 126 is reserved by FIG-3585, and component 127
/// (FIG-3540) adds the `lash_session_ingress` table and the `drive_epoch` /
/// `drive_admission_id` columns of `lash_session_meta`. Component 128
/// (FIG-3659) reshapes `lash_turn_parks` into the enriched parked record and
/// adds `lash_turn_park_clock` and `lash_turn_park_events`. No arm targets
/// 128: the current build refuses every predecessor.
/// Source-shape
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
        to: 127,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_runtime_effect_group_child",
            "lash_session_ingress",
            "lash_turn_cancel_affected_inputs",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "submitted_ingress_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_pending_turn_inputs", "submission_digest"),
            ("lash_trigger_mutation_receipts", "owner_kind"),
            ("lash_trigger_mutation_receipts", "owner_id"),
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
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
        to: 127,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_runtime_effect_group_child",
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "submitted_ingress_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_pending_turn_inputs", "submission_digest"),
            ("lash_trigger_mutation_receipts", "owner_kind"),
            ("lash_trigger_mutation_receipts", "owner_id"),
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
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
        to: 127,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_runtime_effect_group_child",
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "submitted_ingress_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_pending_turn_inputs", "submission_digest"),
            ("lash_trigger_mutation_receipts", "owner_kind"),
            ("lash_trigger_mutation_receipts", "owner_id"),
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
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
        to: 127,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_runtime_effect_group_child",
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "submitted_ingress_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_pending_turn_inputs", "submission_digest"),
            ("lash_trigger_mutation_receipts", "owner_kind"),
            ("lash_trigger_mutation_receipts", "owner_id"),
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
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
        to: 127,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_runtime_effect_group_child",
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "submitted_ingress_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_pending_turn_inputs", "submission_digest"),
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
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
        to: 127,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_runtime_effect_group_child",
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "submitted_ingress_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_pending_turn_inputs", "submission_digest"),
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
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
        to: 127,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "submitted_ingress_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_pending_turn_inputs", "submission_digest"),
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
            ("lash_runtime_effect_group_child", "command_version"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
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
        to: 127,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "submitted_ingress_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_pending_turn_inputs", "submission_digest"),
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
            ("lash_runtime_effect_group_child", "command_version"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
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
        to: 127,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "submitted_ingress_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_pending_turn_inputs", "submission_digest"),
            ("lash_runtime_effect_group", "next_commit_seq"),
            ("lash_runtime_effect_group", "lifecycle"),
            ("lash_runtime_effect_group", "expected_children"),
            ("lash_runtime_effect_replay", "commit_state"),
            ("lash_runtime_effect_replay", "commit_seq"),
            ("lash_runtime_effect_replay", "drain_input"),
            ("lash_runtime_effect_group_child", "command_version"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
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
        to: 127,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "submitted_ingress_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_pending_turn_inputs", "submission_digest"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: EFFECT_REPLAY_FOREIGN_KEYS,
        introduced_relations: &[],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    SchemaMigration {
        from: 111,
        to: 127,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "submitted_ingress_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_pending_turn_inputs", "submission_digest"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: EFFECT_REPLAY_FOREIGN_KEYS,
        introduced_relations: &[],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    SchemaMigration {
        from: 112,
        to: 127,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "submitted_ingress_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_pending_turn_inputs", "submission_digest"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: EFFECT_REPLAY_FOREIGN_KEYS,
        introduced_relations: &[],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    SchemaMigration {
        from: 113,
        to: 127,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "submitted_ingress_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_pending_turn_inputs", "submission_digest"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: EFFECT_REPLAY_FOREIGN_KEYS,
        introduced_relations: &[],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    // Component 114 lacked the five effect-replay constraints installed at
    // 115. The retained declaration records their historical shape while the
    // current component-117 cutover offers no executable migration.
    SchemaMigration {
        from: 114,
        to: 127,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "submitted_ingress_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_pending_turn_inputs", "submission_digest"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: EFFECT_REPLAY_FOREIGN_KEYS,
        introduced_relations: &[],
        introduced_constraints: EFFECT_REPLAY_CONSTRAINT_NAMES,
        statements: &[],
    },
    // A component-115 catalog predates the queued-run cutover wholesale: it
    // lacks the admission and membership tables and the pending-run index the
    // component-116 endpoint carries, and nothing else. Their constraints and
    // foreign key ride the table drops, so only the relations are enumerated.
    SchemaMigration {
        from: 115,
        to: 127,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "submitted_ingress_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_pending_turn_inputs", "submission_digest"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[
            "lash_queued_run_members",
            "lash_queued_runs",
            "lash_queued_runs_pending",
        ],
        introduced_constraints: &[],
        statements: &[],
    },
    // The immediate predecessor of the retained endpoint 117. Component 117
    // versioned the runtime-commit receipt payload and moved no relation, so a
    // component-116 catalog lacks only the FIG-3544 submission columns this
    // build carries; component 118 is destructive, so the row is a refusal
    // boundary that introduces nothing a rewound stamp could be claiming.
    SchemaMigration {
        from: 116,
        to: 127,
        source_missing_tables: &[
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "submitted_ingress_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_pending_turn_inputs", "submission_digest"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    }, // A component-117 catalog lacks the FIG-3544 submission columns
    // component 118 added and the FIG-3589 binding columns component 120 adds;
    // the row is a refusal boundary that introduces nothing a rewound stamp
    // could be claiming.
    SchemaMigration {
        from: 117,
        to: 127,
        source_missing_tables: &[
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "submitted_ingress_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_pending_turn_inputs", "submission_digest"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // A component-118 catalog lacks exactly the FIG-3589 binding columns
    // component 120 adds (component 119 moved no relation); the row is a
    // refusal boundary that introduces nothing a rewound stamp could be
    // claiming.
    SchemaMigration {
        from: 118,
        to: 127,
        source_missing_tables: &[
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // A component-119 catalog lacks the FIG-3589 binding columns component
    // 120 added; the row is a refusal boundary that introduces nothing a
    // rewound stamp could be claiming.
    SchemaMigration {
        from: 119,
        to: 127,
        source_missing_tables: &[
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_pending_turn_inputs", "claim_bound_turn_id"),
            ("lash_pending_turn_inputs", "claim_bound_receipt_input_id"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // A component-120 catalog lacks the `lash_turn_parks` table component 121
    // (FIG-3586) added; the row is a refusal boundary that introduces nothing
    // a rewound stamp could be claiming.
    SchemaMigration {
        from: 120,
        to: 127,
        source_missing_tables: &[
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
            "lash_turn_parks",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // Component 122
    // (FIG-3598) changed the persisted runtime-error vocabulary and moved no
    // relation, so a component-121 catalog lacks nothing the endpoint models.
    // The current build refuses every predecessor, so the row is a refusal
    // boundary that introduces nothing a rewound stamp could be claiming.
    SchemaMigration {
        from: 121,
        to: 127,
        source_missing_tables: &[
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
            ("lash_turn_parks", "park_id"),
            ("lash_turn_parks", "reason_code"),
            ("lash_turn_parks", "since_ms"),
            ("lash_turn_parks", "last_refused_ms"),
            ("lash_turn_parks", "attempts"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // Component 123 (FIG-3588) added the nullable `started_json` segment
    // start marker, which a component-122 catalog lacks; the row is a refusal
    // boundary that introduces nothing a rewound stamp could be claiming.
    SchemaMigration {
        from: 122,
        to: 127,
        source_missing_tables: &[
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_process_segment_handovers", "started_json"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
            ("lash_turn_parks", "park_id"),
            ("lash_turn_parks", "reason_code"),
            ("lash_turn_parks", "since_ms"),
            ("lash_turn_parks", "last_refused_ms"),
            ("lash_turn_parks", "attempts"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // Components 124 (FIG-3587) and 125 changed persisted vocabularies and
    // moved no relation, so a component-123 catalog lacks nothing they moved;
    // the row is a refusal boundary that introduces nothing a rewound stamp
    // could be claiming.
    SchemaMigration {
        from: 123,
        to: 127,
        source_missing_tables: &[
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
            ("lash_turn_parks", "park_id"),
            ("lash_turn_parks", "reason_code"),
            ("lash_turn_parks", "since_ms"),
            ("lash_turn_parks", "last_refused_ms"),
            ("lash_turn_parks", "attempts"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // Components 124 and 125 changed persisted vocabularies and moved no
    // relation. A component-124, -125, or -126 catalog lacks the
    // `lash_session_ingress` table and the `lash_session_meta` drive-epoch
    // columns component 127 (FIG-3540) adds and the feed catalog component
    // 128 (FIG-3659) adds; component 128 is destructive, so each row is a
    // refusal boundary that introduces nothing a rewound stamp could be
    // claiming.
    SchemaMigration {
        from: 124,
        to: 127,
        source_missing_tables: &[
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
            ("lash_turn_parks", "park_id"),
            ("lash_turn_parks", "reason_code"),
            ("lash_turn_parks", "since_ms"),
            ("lash_turn_parks", "last_refused_ms"),
            ("lash_turn_parks", "attempts"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    SchemaMigration {
        from: 125,
        to: 127,
        source_missing_tables: &[
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
            ("lash_turn_parks", "park_id"),
            ("lash_turn_parks", "reason_code"),
            ("lash_turn_parks", "since_ms"),
            ("lash_turn_parks", "last_refused_ms"),
            ("lash_turn_parks", "attempts"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // The immediate predecessor of the retained endpoint 127. Component 126
    // is reserved by FIG-3585 and shipped no catalog of its own, so a
    // component-126 stamp carries the published component-125 shape and lacks
    // the session-ingress and feed catalogs; component 128 is destructive, so
    // the row is a refusal boundary that introduces nothing a rewound stamp
    // could be claiming.
    SchemaMigration {
        from: 126,
        to: 127,
        source_missing_tables: &[
            "lash_session_ingress",
            "lash_turn_park_clock",
            "lash_turn_park_events",
        ],
        source_missing_columns: &[
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
            ("lash_turn_parks", "park_id"),
            ("lash_turn_parks", "reason_code"),
            ("lash_turn_parks", "since_ms"),
            ("lash_turn_parks", "last_refused_ms"),
            ("lash_turn_parks", "attempts"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
];
