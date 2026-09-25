//! Active schema admission catalog. Historical declarations live in the
//! test-only `historical_migrations` module.

use super::*;

/// These declarations retain the component-130 endpoint: the five
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
/// adds `lash_turn_park_clock` and `lash_turn_park_events`, and component
/// 129 (FIG-3585) drops two runtime-error codes and moves no relation, and
/// component 130 (FIG-3682) adds the `admission_base_checkpoint_ref` column
/// of `lash_session_meta`, and component 131 (FIG-3735) extends the turn-park
/// reason vocabulary and moves no relation. No arm targets 130 or 131: the
/// current build refuses every predecessor.
/// Source-shape
/// declarations remain keyed to this build's catalog for precise older-store
/// fixture construction.
pub(super) const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    // Keep the outer list expanded for the source-derived fixture checker.
    SchemaMigration {
        from: 101,
        // The lists are keyed to the floor, not to one generation: a relation,
        // column, or constraint introduced after 105 belongs here too, so the
        // fixture rebuilds the published component-101 catalog by removing them.
        to: 130,
        source_missing_tables: &[
            "lash_queued_run_members",
            "lash_queued_runs",
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
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        // Every post-floor relation the rebuild must account for: the
        // component-102 table the floor arm's range introduces and the
        // commit-order unique component 110 adds to the pre-floor replay
        // table, which no table drop covers.
        introduced_relations: &["lash_turn_cancel_affected_inputs"],
        introduced_constraints: &[],
        statements: &[],
    },
    // Component 102 to 105 moved no shape the endpoint models beyond what the
    // 101 arm already lists — the cancellation affected-input child table
    // landed *at* component 102 — so a component-102 catalog lacks nothing
    // further the endpoint carries.
    SchemaMigration {
        from: 102,
        to: 130,
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
            ("lash_trigger_mutation_receipts", "owner_kind"),
            ("lash_trigger_mutation_receipts", "owner_id"),
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // Component 103 to 105: 104 added only CHECK constraints (unmodeled) and
    // 105 added the owner columns, so a component-103 catalog lacks exactly
    // those columns the endpoint carries.
    SchemaMigration {
        from: 103,
        to: 130,
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
            ("lash_trigger_mutation_receipts", "owner_kind"),
            ("lash_trigger_mutation_receipts", "owner_id"),
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // Component 104 to 107: 105 added the owner columns, which this catalog
    // models, so a component-104 catalog lacks exactly those columns.
    SchemaMigration {
        from: 104,
        to: 130,
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
            ("lash_trigger_mutation_receipts", "owner_kind"),
            ("lash_trigger_mutation_receipts", "owner_id"),
            ("lash_parent_end_plans", "parent_payload"),
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // Component 105 to 107: 106 moved no relational DDL, so a component-105
    // catalog differs from the endpoint by the membership table, the typed
    // parent payload, and the arbitration state alone.
    SchemaMigration {
        from: 105,
        to: 130,
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
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // The retained membership's introduction. Component 106 was the
    // creation-request cutover, which moved payload generations without
    // touching the relational DDL, so a component-106 catalog is the endpoint
    // minus the table component 107 adds and what components 109 and 110 add
    // to its siblings.
    SchemaMigration {
        from: 106,
        to: 130,
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
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        // The relation the membership generation creates, which the divergence
        // refusal over a component-106 catalog enumerates by name.
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // A component-107 catalog carries the membership table but none of what
    // components 109 and 110 add — the typed parent payload and the ADR 0099
    // §§4–5 columns and guards — and it still spells the membership's version
    // column `request_version`, which 110 renamed `command_version`. Component
    // 108 added no relational DDL of its own.
    SchemaMigration {
        from: 107,
        to: 130,
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
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // A component-108 catalog adds only the journaled `exec_code` outcome
    // cutover to 107's relational shape, so it lacks the same columns and
    // guards a component-107 catalog does against this build.
    SchemaMigration {
        from: 108,
        to: 130,
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
            ("lash_session_meta", "drive_epoch"),
            ("lash_session_meta", "drive_admission_id"),
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        // The relations the arbitration generation creates, which the
        // divergence refusal over a component-108 catalog enumerates by name:
        // indexes are relations too, and these two are what the generation
        // adds that a component-108 catalog can already be carrying.
        introduced_relations: &[],
        introduced_constraints: &[],
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
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        // The relations the arbitration generation creates, which the
        // divergence refusal over a component-109 catalog enumerates by name.
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // Components 110 through 113 share the endpoint's relational shape minus
    // the component-115 constraints and component-116 queued-run tables.
    // All are refused at the queued-run cutover.
    SchemaMigration {
        from: 110,
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    SchemaMigration {
        from: 111,
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    SchemaMigration {
        from: 112,
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    SchemaMigration {
        from: 113,
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // Component 114 lacked the five effect-replay constraints installed at
    // 115. The retained declaration records their historical shape while the
    // current component-117 cutover offers no executable migration.
    SchemaMigration {
        from: 114,
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // A component-115 catalog predates the queued-run cutover wholesale: it
    // lacks the admission and membership tables and the pending-run index the
    // component-116 endpoint carries, and nothing else. Their constraints and
    // foreign key ride the table drops, so only the relations are enumerated.
    SchemaMigration {
        from: 115,
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
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
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
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
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
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
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
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
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
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
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
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
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
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
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
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
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
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
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    SchemaMigration {
        from: 125,
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // Component 126 was reserved by FIG-3585 and shipped no catalog of its
    // own, so a component-126 stamp carries the published component-125 shape
    // and lacks the session-ingress and feed catalogs; the row is a refusal
    // boundary that introduces nothing a rewound stamp could be claiming.
    SchemaMigration {
        from: 126,
        to: 130,
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
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // A predecessor of the retained endpoint 130. A
    // component-127 catalog lacks the feed catalog component 128 (FIG-3659)
    // adds; component 129 is destructive, so the row is a refusal boundary
    // that introduces nothing a rewound stamp could be claiming.
    SchemaMigration {
        from: 127,
        to: 130,
        source_missing_tables: &["lash_turn_park_clock", "lash_turn_park_events"],
        source_missing_columns: &[
            ("lash_turn_parks", "park_id"),
            ("lash_turn_parks", "reason_code"),
            ("lash_turn_parks", "since_ms"),
            ("lash_turn_parks", "last_refused_ms"),
            ("lash_turn_parks", "attempts"),
            ("lash_session_meta", "admission_base_checkpoint_ref"),
        ],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // A predecessor of the retained endpoint 130. Component 129 (FIG-3585)
    // moved no relation, so a component-128 catalog lacks only the
    // admission-base column component 130 (FIG-3682) adds; the row is a
    // refusal boundary that introduces nothing a rewound stamp could be
    // claiming.
    SchemaMigration {
        from: 128,
        to: 130,
        source_missing_tables: &[],
        source_missing_columns: &[("lash_session_meta", "admission_base_checkpoint_ref")],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
    // The immediate predecessor of the retained endpoint 130. A
    // component-129 catalog lacks the admission-base column component 130
    // (FIG-3682) adds; component 131 (FIG-3735) moves persisted vocabulary
    // only, so the row is a refusal boundary that introduces nothing a
    // rewound stamp could be claiming.
    SchemaMigration {
        from: 129,
        to: 130,
        source_missing_tables: &[],
        source_missing_columns: &[("lash_session_meta", "admission_base_checkpoint_ref")],
        source_missing_guards: &[],
        source_missing_foreign_keys: &[],
        introduced_relations: &[],
        introduced_constraints: &[],
        statements: &[],
    },
];
