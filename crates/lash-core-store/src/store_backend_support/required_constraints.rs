//! Shared expected definitions and comparison support for named SQL `CHECK`s.
//!
//! The source-congruence gate imports the same registry, so the expected expressions have one
//! owner.

use std::collections::BTreeMap;

use crate::StoreError;

mod foreign_keys;
mod parser;

use parser::Parser;

pub use foreign_keys::{
    EXPECTED_FOREIGN_KEYS, ExpectedForeignKey, InspectedForeignKey, ParsedForeignKeyClause,
    RenderedForeignKey, RequiredForeignKeyFinding, compare_required_foreign_keys,
    extract_foreign_key_clauses,
};

/// One named `CHECK` Lash requires in the published store schemas: the
/// same logical constraint rendered once per backend, so a row added
/// here is gated on both stores at once.
///
/// `sqlite_databases` lists every SQLite component that carries the
/// table: shared-fragment tables live in more than one database, so
/// each carrier's inspection sees the constraint. `sqlite` is `None`
/// for checks that have no SQLite counterpart (e.g. a PostgreSQL-only
/// child table), and `postgres` is `None` for checks that have no
/// PostgreSQL counterpart (e.g. integer-boolean vocabularies where
/// Postgres uses a native `BOOLEAN` instead).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExpectedConstraint {
    /// Which SQLite schema components carry the table holding the check.
    pub sqlite_databases: &'static [SqliteConstraintDatabase],
    /// The check as SQLite declares it, when a counterpart exists.
    pub sqlite: Option<RenderedConstraint>,
    /// The check as PostgreSQL declares it, when a counterpart exists.
    pub postgres: Option<RenderedConstraint>,
}

/// A named `CHECK` as one backend declares it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RenderedConstraint {
    pub table: &'static str,
    pub name: &'static str,
    pub expression: &'static str,
}

/// SQLite schema component that owns a registered named `CHECK`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SqliteConstraintDatabase {
    DurableCore,
    ProcessRegistry,
    Triggers,
    EffectReplay,
}

const fn rendered(
    table: &'static str,
    name: &'static str,
    expression: &'static str,
) -> RenderedConstraint {
    RenderedConstraint {
        table,
        name,
        expression,
    }
}

const fn expected_constraint(
    sqlite_databases: &'static [SqliteConstraintDatabase],
    sqlite: RenderedConstraint,
    postgres: RenderedConstraint,
) -> ExpectedConstraint {
    ExpectedConstraint {
        sqlite_databases,
        sqlite: Some(sqlite),
        postgres: Some(postgres),
    }
}

const fn sqlite_only_constraint(
    sqlite_databases: &'static [SqliteConstraintDatabase],
    sqlite: RenderedConstraint,
) -> ExpectedConstraint {
    ExpectedConstraint {
        sqlite_databases,
        sqlite: Some(sqlite),
        postgres: None,
    }
}

const fn postgres_only_constraint(postgres: RenderedConstraint) -> ExpectedConstraint {
    ExpectedConstraint {
        sqlite_databases: &[],
        sqlite: None,
        postgres: Some(postgres),
    }
}

/// The named `CHECK`s Lash's published schemas must declare, one row per
/// constraint with each backend's rendering beside the other.
pub const EXPECTED_CONSTRAINTS: &[ExpectedConstraint] = &[
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "queued_runs",
            "ck_queued_runs_status",
            "status IN ('pending', 'settled')",
        ),
        rendered(
            "lash_queued_runs",
            "ck_queued_runs_status",
            "status IN ('pending', 'settled')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered("queued_runs", "ck_queued_runs_revision", "revision >= 0"),
        rendered(
            "lash_queued_runs",
            "ck_queued_runs_revision",
            "revision >= 0",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "queued_run_members",
            "ck_queued_run_members_collection_kind",
            "collection_kind IN ('initial', 'current', 'withheld', 'assigned')",
        ),
        rendered(
            "lash_queued_run_members",
            "ck_queued_run_members_collection_kind",
            "collection_kind IN ('initial', 'current', 'withheld', 'assigned')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "queued_run_members",
            "ck_queued_run_members_ordinal",
            "ordinal >= 0",
        ),
        rendered(
            "lash_queued_run_members",
            "ck_queued_run_members_ordinal",
            "ordinal >= 0",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "queued_run_members",
            "ck_queued_run_members_member_kind",
            "member_kind IN ('input', 'batch')",
        ),
        rendered(
            "lash_queued_run_members",
            "ck_queued_run_members_member_kind",
            "member_kind IN ('input', 'batch')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "attachment_manifest",
            "ck_attachment_manifest_owner_identity",
            "(owner_kind IS NULL AND owner_id IS NULL AND owner_incarnation IS NULL) OR (owner_kind = 'turn' AND owner_id IS NOT NULL AND owner_incarnation IS NULL) OR (owner_kind = 'process' AND owner_id IS NOT NULL AND owner_incarnation IS NOT NULL)",
        ),
        rendered(
            "lash_attachment_manifest",
            "ck_lash_attachment_manifest_owner_identity",
            "(owner_kind IS NULL AND owner_id IS NULL AND owner_incarnation IS NULL) OR (owner_kind = 'turn' AND owner_id IS NOT NULL AND owner_incarnation IS NULL) OR (owner_kind = 'process' AND owner_id IS NOT NULL AND owner_incarnation IS NOT NULL)",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "pending_turn_inputs",
            "ck_pending_turn_inputs_state",
            "state IN ('pending_active', 'deferred_next_turn', 'accepted', 'cancelled', 'completed')",
        ),
        rendered(
            "lash_pending_turn_inputs",
            "ck_pending_turn_inputs_state",
            "state IN ('pending_active', 'deferred_next_turn', 'accepted', 'cancelled', 'completed')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "pending_turn_inputs",
            "ck_pending_turn_inputs_state_ingress",
            "(json_extract(ingress_json, '$.scope') = 'active_turn' AND state IN ('pending_active', 'accepted', 'cancelled', 'completed')) OR (json_extract(ingress_json, '$.scope') = 'next_turn' AND state IN ('deferred_next_turn', 'cancelled', 'completed'))",
        ),
        rendered(
            "lash_pending_turn_inputs",
            "ck_pending_turn_inputs_state_ingress",
            "((ingress_json::jsonb ->> 'scope') = 'active_turn' AND state IN ('pending_active', 'accepted', 'cancelled', 'completed')) OR ((ingress_json::jsonb ->> 'scope') = 'next_turn' AND state IN ('deferred_next_turn', 'cancelled', 'completed'))",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "pending_turn_inputs",
            "ck_pending_turn_inputs_claim_identity_all_or_none",
            "(claim_id IS NULL AND claim_owner_id IS NULL AND claim_owner_incarnation_id IS NULL AND claim_token IS NULL) OR (claim_id IS NOT NULL AND claim_owner_id IS NOT NULL AND claim_owner_incarnation_id IS NOT NULL AND claim_token IS NOT NULL)",
        ),
        rendered(
            "lash_pending_turn_inputs",
            "ck_pending_turn_inputs_claim_identity_all_or_none",
            "(claim_id IS NULL AND claim_owner_id IS NULL AND claim_owner_incarnation_id IS NULL AND claim_token IS NULL) OR (claim_id IS NOT NULL AND claim_owner_id IS NOT NULL AND claim_owner_incarnation_id IS NOT NULL AND claim_token IS NOT NULL)",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "pending_turn_inputs",
            "ck_pending_turn_inputs_bound_claim_is_next_turn",
            "claim_bound_turn_id IS NULL OR (claim_token IS NOT NULL AND state = 'deferred_next_turn')",
        ),
        rendered(
            "lash_pending_turn_inputs",
            "ck_pending_turn_inputs_bound_claim_is_next_turn",
            "claim_bound_turn_id IS NULL OR (claim_token IS NOT NULL AND state = 'deferred_next_turn')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "queued_work_batches",
            "ck_queued_work_batches_work_kind",
            "work_kind IN ('turn', 'control')",
        ),
        rendered(
            "lash_queued_work_batches",
            "ck_queued_work_batches_work_kind",
            "work_kind IN ('turn', 'control')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "queued_work_batches",
            "ck_queued_work_batches_delivery_policy",
            "delivery_policy IN ('earliest_safe_boundary', 'after_current_turn_commit')",
        ),
        rendered(
            "lash_queued_work_batches",
            "ck_queued_work_batches_delivery_policy",
            "delivery_policy IN ('earliest_safe_boundary', 'after_current_turn_commit')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "queued_work_batches",
            "ck_queued_work_batches_claim_id_token_all_or_none",
            "(claim_id IS NULL AND claim_token IS NULL) OR (claim_id IS NOT NULL AND claim_token IS NOT NULL)",
        ),
        rendered(
            "lash_queued_work_batches",
            "ck_queued_work_batches_claim_id_token_all_or_none",
            "(claim_id IS NULL AND claim_token IS NULL) OR (claim_id IS NOT NULL AND claim_token IS NOT NULL)",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "session_execution_leases",
            "ck_session_execution_leases_identity_all_or_none",
            "(lease_owner_id IS NULL AND lease_owner_incarnation_id IS NULL AND lease_executor_id IS NULL AND lease_token IS NULL) OR (lease_owner_id IS NOT NULL AND lease_owner_incarnation_id IS NOT NULL AND lease_executor_id IS NOT NULL AND lease_token IS NOT NULL)",
        ),
        rendered(
            "lash_session_execution_leases",
            "ck_session_execution_leases_identity_all_or_none",
            "(lease_owner_id IS NULL AND lease_owner_incarnation_id IS NULL AND lease_executor_id IS NULL AND lease_token IS NULL) OR (lease_owner_id IS NOT NULL AND lease_owner_incarnation_id IS NOT NULL AND lease_executor_id IS NOT NULL AND lease_token IS NOT NULL)",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "session_meta",
            "ck_session_meta_relation_kind",
            "relation_kind IN ('root', 'child', 'fork')",
        ),
        rendered(
            "lash_session_meta",
            "ck_session_meta_relation_kind",
            "relation_kind IN ('root', 'child', 'fork')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "session_meta",
            "ck_session_meta_caused_by_kind",
            "caused_by_kind IN ('turn', 'effect_address', 'tool_call', 'process', 'process_event', 'trigger_occurrence', 'session_node')",
        ),
        rendered(
            "lash_session_meta",
            "ck_session_meta_caused_by_kind",
            "caused_by_kind IN ('turn', 'effect_address', 'tool_call', 'process', 'process_event', 'trigger_occurrence', 'session_node')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "session_meta",
            "ck_session_meta_relation_family",
            "(relation_kind = 'root' AND parent_session_id IS NULL AND caused_by_kind IS NULL AND source_session_id IS NULL AND source_node_id IS NULL) OR (relation_kind = 'child' AND parent_session_id IS NOT NULL AND source_session_id IS NULL AND source_node_id IS NULL) OR (relation_kind = 'fork' AND parent_session_id IS NULL AND caused_by_kind IS NULL AND source_session_id IS NOT NULL AND source_node_id IS NOT NULL) OR (relation_kind IS NOT NULL AND NOT (relation_kind IN ('root', 'child', 'fork')))",
        ),
        rendered(
            "lash_session_meta",
            "ck_session_meta_relation_family",
            "(relation_kind = 'root' AND parent_session_id IS NULL AND caused_by_kind IS NULL AND source_session_id IS NULL AND source_node_id IS NULL) OR (relation_kind = 'child' AND parent_session_id IS NOT NULL AND source_session_id IS NULL AND source_node_id IS NULL) OR (relation_kind = 'fork' AND parent_session_id IS NULL AND caused_by_kind IS NULL AND source_session_id IS NOT NULL AND source_node_id IS NOT NULL) OR (relation_kind IS NOT NULL AND NOT (relation_kind IN ('root', 'child', 'fork')))",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "session_meta",
            "ck_session_meta_caused_by_family",
            "(caused_by_kind IS NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'turn' AND caused_by_session_id IS NOT NULL AND caused_by_turn_id IS NOT NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'effect_address' AND caused_by_effect_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'tool_call' AND caused_by_session_id IS NOT NULL AND caused_by_call_id IS NOT NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'process' AND caused_by_process_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'process_event' AND caused_by_process_id IS NOT NULL AND caused_by_process_event_sequence IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'trigger_occurrence' AND caused_by_occurrence_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'session_node' AND caused_by_session_id IS NOT NULL AND caused_by_node_id IS NOT NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL) OR (caused_by_kind IS NOT NULL AND NOT (caused_by_kind IN ('turn', 'effect_address', 'tool_call', 'process', 'process_event', 'trigger_occurrence', 'session_node')))",
        ),
        rendered(
            "lash_session_meta",
            "ck_session_meta_caused_by_family",
            "(caused_by_kind IS NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'turn' AND caused_by_session_id IS NOT NULL AND caused_by_turn_id IS NOT NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'effect_address' AND caused_by_effect_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'tool_call' AND caused_by_session_id IS NOT NULL AND caused_by_call_id IS NOT NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'process' AND caused_by_process_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'process_event' AND caused_by_process_id IS NOT NULL AND caused_by_process_event_sequence IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'trigger_occurrence' AND caused_by_occurrence_id IS NOT NULL AND caused_by_session_id IS NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_node_id IS NULL) OR (caused_by_kind = 'session_node' AND caused_by_session_id IS NOT NULL AND caused_by_node_id IS NOT NULL AND caused_by_turn_id IS NULL AND caused_by_effect_id IS NULL AND caused_by_call_id IS NULL AND caused_by_process_id IS NULL AND caused_by_process_event_sequence IS NULL AND caused_by_occurrence_id IS NULL AND caused_by_subscription_id IS NULL AND caused_by_subscription_incarnation IS NULL AND caused_by_subscription_revision IS NULL) OR (caused_by_kind IS NOT NULL AND NOT (caused_by_kind IN ('turn', 'effect_address', 'tool_call', 'process', 'process_event', 'trigger_occurrence', 'session_node')))",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "process_definitions",
            "ck_process_definitions_lifecycle",
            "(lifecycle IN ('enabled', 'disabled') AND deleted_at_ms IS NULL) OR (lifecycle = 'tombstoned' AND deleted_at_ms IS NOT NULL)",
        ),
        rendered(
            "lash_process_definitions",
            "ck_process_definitions_lifecycle",
            "(lifecycle IN ('enabled', 'disabled') AND deleted_at_ms IS NULL) OR (lifecycle = 'tombstoned' AND deleted_at_ms IS NOT NULL)",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::ProcessRegistry],
        rendered(
            "processes",
            "ck_processes_status",
            "status IN ('running', 'waiting', 'completed', 'failed', 'cancelled', 'abandoned', 'caller_departed')",
        ),
        rendered(
            "lash_processes",
            "ck_processes_status",
            "status IN ('running', 'waiting', 'completed', 'failed', 'cancelled', 'abandoned', 'caller_departed')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::ProcessRegistry],
        rendered(
            "processes",
            "ck_processes_parent_scope_kind",
            "parent_scope_kind IN ('turn', 'queue_drain', 'process', 'host')",
        ),
        rendered(
            "lash_processes",
            "ck_processes_parent_scope_kind",
            "parent_scope_kind IN ('turn', 'queue_drain', 'process', 'host')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::ProcessRegistry],
        rendered(
            "processes",
            "ck_processes_parent_scope_id",
            "(parent_scope_kind = 'host' AND parent_scope_id IS NULL) OR (parent_scope_kind IN ('turn', 'queue_drain', 'process') AND parent_scope_id IS NOT NULL)",
        ),
        rendered(
            "lash_processes",
            "ck_processes_parent_scope_id",
            "(parent_scope_kind = 'host' AND parent_scope_id IS NULL) OR (parent_scope_kind IN ('turn', 'queue_drain', 'process') AND parent_scope_id IS NOT NULL)",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::ProcessRegistry],
        rendered(
            "processes",
            "ck_processes_on_parent_end",
            "on_parent_end IN ('abandon', 'cancel')",
        ),
        rendered(
            "lash_processes",
            "ck_processes_on_parent_end",
            "on_parent_end IN ('abandon', 'cancel')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::ProcessRegistry],
        rendered(
            "parent_end_plans",
            "ck_parent_end_plans_kind",
            "parent_kind IN ('turn', 'queue_drain', 'process')",
        ),
        rendered(
            "lash_parent_end_plans",
            "ck_parent_end_plans_kind",
            "parent_kind IN ('turn', 'queue_drain', 'process')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::ProcessRegistry],
        rendered(
            "process_wake_deliveries",
            "ck_process_wake_deliveries_state",
            "state IN ('pending', 'enqueuing', 'enqueued', 'discarded')",
        ),
        rendered(
            "lash_process_wake_deliveries",
            "ck_process_wake_deliveries_state",
            "state IN ('pending', 'enqueuing', 'enqueued', 'discarded')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::ProcessRegistry],
        rendered(
            "process_wake_deliveries",
            "ck_process_wake_deliveries_discard_reason",
            "discard_reason IN ('expired', 'target_gone', 'retargeted', 'sequence_rewound')",
        ),
        rendered(
            "lash_process_wake_deliveries",
            "ck_process_wake_deliveries_discard_reason",
            "discard_reason IN ('expired', 'target_gone', 'retargeted', 'sequence_rewound')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::ProcessRegistry],
        rendered(
            "tool_intent_submissions",
            "ck_tool_intent_submissions_kind",
            "kind IN ('start_process', 'signal_process', 'cancel_process', 'emit_process_event', 'emit_trigger')",
        ),
        rendered(
            "lash_tool_intent_submissions",
            "ck_tool_intent_submissions_kind",
            "kind IN ('start_process', 'signal_process', 'cancel_process', 'emit_process_event', 'emit_trigger')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::Triggers],
        rendered(
            "trigger_subscriptions",
            "ck_trigger_subscriptions_lifecycle",
            "lifecycle IN ('enabled', 'disabled', 'tombstoned')",
        ),
        rendered(
            "lash_trigger_subscriptions",
            "ck_trigger_subscriptions_lifecycle",
            "lifecycle IN ('enabled', 'disabled', 'tombstoned')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::Triggers],
        rendered(
            "trigger_subscriptions",
            "ck_trigger_subscriptions_lifecycle_deleted_at",
            "(lifecycle IN ('enabled', 'disabled') AND deleted_at_ms IS NULL) OR (lifecycle = 'tombstoned' AND deleted_at_ms IS NOT NULL)",
        ),
        rendered(
            "lash_trigger_subscriptions",
            "ck_trigger_subscriptions_lifecycle_deleted_at",
            "(lifecycle IN ('enabled', 'disabled') AND deleted_at_ms IS NULL) OR (lifecycle = 'tombstoned' AND deleted_at_ms IS NOT NULL)",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::Triggers],
        rendered(
            "trigger_mutation_receipts",
            "ck_trigger_receipts_owner_kind",
            "owner_kind IN ('session', 'host', 'platform')",
        ),
        rendered(
            "lash_trigger_mutation_receipts",
            "ck_trigger_receipts_owner_kind",
            "owner_kind IN ('session', 'host', 'platform')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::EffectReplay],
        rendered(
            "runtime_effect_replay",
            "ck_runtime_effect_replay_status",
            "status IN ('in_progress', 'completed', 'failed')",
        ),
        rendered(
            "lash_runtime_effect_replay",
            "ck_runtime_effect_replay_status",
            "status IN ('in_progress', 'completed', 'failed')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::EffectReplay],
        rendered(
            "runtime_effect_group",
            "ck_runtime_effect_group_wake",
            "wake IN ('first', 'first_success', 'all')",
        ),
        rendered(
            "lash_runtime_effect_group",
            "ck_runtime_effect_group_wake",
            "wake IN ('first', 'first_success', 'all')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::EffectReplay],
        rendered(
            "runtime_effect_group",
            "ck_runtime_effect_group_loser_disposition",
            "loser_disposition IN ('run_to_completion', 'cancel')",
        ),
        rendered(
            "lash_runtime_effect_group",
            "ck_runtime_effect_group_loser_disposition",
            "loser_disposition IN ('run_to_completion', 'cancel')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::EffectReplay],
        rendered(
            "runtime_effect_replay",
            "ck_runtime_effect_replay_commit_state",
            "commit_state IN ('pending', 'committed', 'drained', 'cancel_decided')",
        ),
        rendered(
            "lash_runtime_effect_replay",
            "ck_runtime_effect_replay_commit_state",
            "commit_state IN ('pending', 'committed', 'drained', 'cancel_decided')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::EffectReplay],
        rendered(
            "runtime_effect_replay",
            "ck_runtime_effect_replay_commit_seq",
            "(commit_seq IS NULL OR (group_key IS NOT NULL AND commit_state IN ('committed', 'drained'))) AND (group_key IS NULL OR NOT (commit_state IN ('committed', 'drained')) OR commit_seq IS NOT NULL)",
        ),
        rendered(
            "lash_runtime_effect_replay",
            "ck_runtime_effect_replay_commit_seq",
            "(commit_seq IS NULL OR (group_key IS NOT NULL AND commit_state IN ('committed', 'drained'))) AND (group_key IS NULL OR NOT (commit_state IN ('committed', 'drained')) OR commit_seq IS NOT NULL)",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::EffectReplay],
        rendered(
            "runtime_effect_replay",
            "ck_runtime_effect_replay_drain_input",
            "drain_input IS NULL OR (group_key IS NOT NULL AND commit_state IN ('committed', 'drained'))",
        ),
        rendered(
            "lash_runtime_effect_replay",
            "ck_runtime_effect_replay_drain_input",
            "drain_input IS NULL OR (group_key IS NOT NULL AND commit_state IN ('committed', 'drained'))",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::EffectReplay],
        rendered(
            "runtime_effect_replay",
            "ck_runtime_effect_replay_outcome_json",
            "(status = 'completed' AND outcome_json IS NOT NULL) OR (status <> 'completed' AND outcome_json IS NULL)",
        ),
        rendered(
            "lash_runtime_effect_replay",
            "ck_runtime_effect_replay_outcome_json",
            "(status = 'completed' AND outcome_json IS NOT NULL) OR (status <> 'completed' AND outcome_json IS NULL)",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::EffectReplay],
        rendered(
            "runtime_effect_replay",
            "ck_runtime_effect_replay_error_json",
            "(status = 'failed' AND error_json IS NOT NULL) OR (status <> 'failed' AND error_json IS NULL)",
        ),
        rendered(
            "lash_runtime_effect_replay",
            "ck_runtime_effect_replay_error_json",
            "(status = 'failed' AND error_json IS NOT NULL) OR (status <> 'failed' AND error_json IS NULL)",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::EffectReplay],
        rendered(
            "runtime_effect_replay",
            "ck_runtime_effect_replay_settlement_seq",
            "(settlement_seq IS NULL AND NOT (commit_state IN ('drained', 'cancel_decided'))) OR (settlement_seq IS NOT NULL AND commit_state IN ('drained', 'cancel_decided'))",
        ),
        rendered(
            "lash_runtime_effect_replay",
            "ck_runtime_effect_replay_settlement_seq",
            "(settlement_seq IS NULL AND NOT (commit_state IN ('drained', 'cancel_decided'))) OR (settlement_seq IS NOT NULL AND commit_state IN ('drained', 'cancel_decided'))",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::EffectReplay],
        rendered(
            "runtime_effect_group",
            "ck_runtime_effect_group_lifecycle",
            "json_extract(lifecycle, '$.type') IN ('live', 'closing', 'settled')",
        ),
        rendered(
            "lash_runtime_effect_group",
            "ck_runtime_effect_group_lifecycle",
            "lifecycle->>'type' IN ('live', 'closing', 'settled')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "graph_nodes",
            "ck_graph_nodes_generation",
            "generation >= 0",
        ),
        rendered(
            "lash_graph_nodes",
            "ck_graph_nodes_generation",
            "generation >= 0",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "fork_lineage",
            "ck_fork_lineage_fork_generation",
            "fork_generation >= 0",
        ),
        rendered(
            "lash_fork_lineage",
            "ck_fork_lineage_fork_generation",
            "fork_generation >= 0",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "runtime_turn_commits",
            "ck_runtime_turn_commits_identity",
            "(request_identity_hash IS NULL) = (identity_encoding_version IS NULL) AND (requested_node_count IS NULL OR request_identity_hash IS NOT NULL)",
        ),
        rendered(
            "lash_runtime_turn_commits",
            "ck_runtime_turn_commits_identity",
            "(request_identity_hash IS NULL) = (identity_encoding_version IS NULL) AND (requested_node_count IS NULL OR request_identity_hash IS NOT NULL)",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "turn_cancel_requests",
            "ck_turn_cancel_requests_intent_revision",
            "intent_revision >= 1",
        ),
        rendered(
            "lash_turn_cancel_requests",
            "ck_turn_cancel_requests_intent_revision",
            "intent_revision >= 1",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "turn_cancellation_bindings",
            "ck_turn_cancellation_bindings_binding_id",
            "length(binding_id) > 0",
        ),
        rendered(
            "lash_turn_cancellation_bindings",
            "ck_turn_cancellation_bindings_binding_id",
            "length(binding_id) > 0",
        ),
    ),
    // SQLite folds the whole cancel record into `record_json`, so the
    // affected-input evidence table — and its disposition vocabulary — is a
    // PostgreSQL-only shape (FIG-3263).
    postgres_only_constraint(rendered(
        "lash_turn_cancel_affected_inputs",
        "ck_turn_cancel_affected_inputs_disposition",
        "disposition IN ('defer', 'drop')",
    )),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "attachment_manifest",
            "ck_attachment_manifest_owner_kind",
            "owner_kind IN ('turn', 'process')",
        ),
        rendered(
            "lash_attachment_manifest",
            "ck_attachment_manifest_owner_kind",
            "owner_kind IN ('turn', 'process')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "attachment_condemnations",
            "ck_attachment_condemnations_phase",
            "phase IN ('condemned', 'deleting')",
        ),
        rendered(
            "lash_attachment_condemnations",
            "ck_attachment_condemnations_phase",
            "phase IN ('condemned', 'deleting')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "attachment_condemnations",
            "ck_attachment_condemnations_write_token_pairing",
            "(write_token IS NULL) = (write_session_id IS NULL)",
        ),
        rendered(
            "lash_attachment_condemnations",
            "ck_attachment_condemnations_write_token_pairing",
            "(write_token IS NULL) = (write_session_id IS NULL)",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "attachment_condemnations",
            "ck_attachment_condemnations_write_token_phase",
            "write_token IS NULL OR phase = 'condemned'",
        ),
        rendered(
            "lash_attachment_condemnations",
            "ck_attachment_condemnations_write_token_phase",
            "write_token IS NULL OR phase = 'condemned'",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "artifact_owners",
            "ck_artifact_owners_owner_kind",
            "owner_kind IN ('host', 'process', 'execution')",
        ),
        rendered(
            "lash_artifact_owners",
            "ck_artifact_owners_owner_kind",
            "owner_kind IN ('host', 'process', 'execution')",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "artifact_owner_retirements",
            "ck_artifact_owner_retirements_owner_kind",
            "owner_kind = 'execution'",
        ),
        rendered(
            "lash_artifact_owner_retirements",
            "ck_artifact_owner_retirements_owner_kind",
            "owner_kind = 'execution'",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::DurableCore],
        rendered(
            "release_stamp",
            "ck_release_stamp_singleton",
            "singleton = 1",
        ),
        rendered(
            "lash_release_stamp",
            "ck_release_stamp_singleton",
            "singleton",
        ),
    ),
    expected_constraint(
        &[SqliteConstraintDatabase::ProcessRegistry],
        rendered(
            "process_change_clock",
            "ck_process_change_clock_singleton",
            "singleton = 1",
        ),
        rendered(
            "lash_process_change_clock",
            "ck_process_change_clock_singleton",
            "singleton",
        ),
    ),
    // `await_event_meta` is carried by the shared await-event fragment into
    // both the durable-core and effect-replay databases; each carrier's
    // inspection must find the check.
    expected_constraint(
        &[
            SqliteConstraintDatabase::DurableCore,
            SqliteConstraintDatabase::EffectReplay,
        ],
        rendered(
            "await_event_meta",
            "ck_await_event_meta_singleton",
            "singleton = 1",
        ),
        rendered(
            "lash_await_event_meta",
            "ck_await_event_meta_singleton",
            "singleton",
        ),
    ),
    // Postgres stores these flags as native BOOLEAN, so the integer-domain
    // vocabulary checks exist only on the SQLite side.
    sqlite_only_constraint(
        &[
            SqliteConstraintDatabase::DurableCore,
            SqliteConstraintDatabase::EffectReplay,
        ],
        rendered(
            "await_event_waits",
            "ck_await_event_waits_turn_control",
            "turn_control IN (0, 1)",
        ),
    ),
    sqlite_only_constraint(
        &[
            SqliteConstraintDatabase::ProcessRegistry,
            SqliteConstraintDatabase::EffectReplay,
        ],
        rendered(
            "effect_scope_retirements",
            "ck_effect_scope_retirements_artifact_cleanup_completed",
            "artifact_cleanup_completed IN (0, 1)",
        ),
    ),
];

/// One required named `CHECK` that did not match the published definition.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RequiredConstraintFinding {
    Missing {
        table: String,
        name: String,
        expected_expression: String,
    },
    Altered {
        table: String,
        name: String,
        expected_expression: String,
        actual_expression: String,
    },
    Unvalidated {
        table: String,
        name: String,
    },
    Unenforced {
        table: String,
        name: String,
    },
}

/// Result of one explicit read-only inspection of registered named `CHECK`s
/// and foreign keys.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequiredConstraintReport {
    findings: Vec<RequiredConstraintFinding>,
    foreign_key_findings: Vec<RequiredForeignKeyFinding>,
}

impl RequiredConstraintReport {
    /// Whether every registered named `CHECK` and foreign key matched in the
    /// inspected snapshot.
    ///
    /// This does not establish schema-version compatibility, database
    /// openability, the state of unregistered constraints, or row integrity.
    pub fn is_conformant(&self) -> bool {
        self.findings.is_empty() && self.foreign_key_findings.is_empty()
    }

    /// Missing, altered, unvalidated, and unenforced required checks.
    pub fn findings(&self) -> &[RequiredConstraintFinding] {
        &self.findings
    }

    /// Missing, altered, unexpected, unvalidated, and unenforced required
    /// foreign keys.
    pub fn foreign_key_findings(&self) -> &[RequiredForeignKeyFinding] {
        &self.foreign_key_findings
    }

    /// Records the foreign-key half of an inspection.
    pub fn set_foreign_key_findings(&mut self, findings: Vec<RequiredForeignKeyFinding>) {
        self.foreign_key_findings = findings;
    }
}

/// One live named `CHECK` read by a store adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectedConstraint {
    pub table: String,
    pub name: String,
    pub expression: String,
    pub validated: bool,
    pub enforced: bool,
}

pub fn compare_required_constraints(
    backend: &'static str,
    expected: &[RenderedConstraint],
    actual: Vec<InspectedConstraint>,
) -> Result<RequiredConstraintReport, StoreError> {
    let mut actual_by_name = BTreeMap::new();
    for constraint in actual {
        let key = (constraint.table.clone(), constraint.name.clone());
        if actual_by_name.insert(key, constraint).is_some() {
            return Err(StoreError::RequiredConstraintInspectionInconclusive {
                backend,
                table: "<catalog>".to_string(),
                constraint: "<duplicate name>".to_string(),
                detail: "the catalog returned a duplicate table/constraint identity".to_string(),
            });
        }
    }

    let mut findings = Vec::new();
    for expected in expected {
        let key = (expected.table.to_string(), expected.name.to_string());
        let Some(actual) = actual_by_name.remove(&key) else {
            findings.push(RequiredConstraintFinding::Missing {
                table: expected.table.to_string(),
                name: expected.name.to_string(),
                expected_expression: expected.expression.to_string(),
            });
            continue;
        };
        let expected_ast =
            parse_expression_for_backend(backend, expected.expression).map_err(|detail| {
                StoreError::RequiredConstraintInspectionInconclusive {
                    backend,
                    table: expected.table.to_string(),
                    constraint: expected.name.to_string(),
                    detail: format!(
                        "published expression is outside the supported grammar: {detail}"
                    ),
                }
            })?;
        let actual_ast =
            parse_expression_for_backend(backend, &actual.expression).map_err(|detail| {
                StoreError::RequiredConstraintInspectionInconclusive {
                    backend,
                    table: expected.table.to_string(),
                    constraint: expected.name.to_string(),
                    detail: format!("live expression is outside the supported grammar: {detail}"),
                }
            })?;
        if expected_ast != actual_ast {
            findings.push(RequiredConstraintFinding::Altered {
                table: expected.table.to_string(),
                name: expected.name.to_string(),
                expected_expression: expected.expression.to_string(),
                actual_expression: actual.expression,
            });
        }
        if !actual.validated {
            findings.push(RequiredConstraintFinding::Unvalidated {
                table: expected.table.to_string(),
                name: expected.name.to_string(),
            });
        }
        if !actual.enforced {
            findings.push(RequiredConstraintFinding::Unenforced {
                table: expected.table.to_string(),
                name: expected.name.to_string(),
            });
        }
    }
    Ok(RequiredConstraintReport {
        findings,
        foreign_key_findings: Vec::new(),
    })
}

/// Extract named `CHECK` bodies from one SQLite `CREATE TABLE` statement.
pub fn extract_named_check_expressions(source: &str) -> Result<BTreeMap<String, String>, String> {
    let tokens = lex_sqlite_ddl(source)?;
    let mut checks = BTreeMap::new();
    let opening = sqlite_create_table_body_opening(&tokens)?;
    let mut item_start = opening + 1;
    let mut depth = 1_usize;
    for index in opening + 1..tokens.len() {
        match tokens[index].kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => {
                depth -= 1;
                if depth == 0 {
                    extract_checks_from_sqlite_table_item(
                        source,
                        &tokens[item_start..index],
                        &mut checks,
                    )?;
                    return Ok(checks);
                }
            }
            TokenKind::Comma if depth == 1 => {
                extract_checks_from_sqlite_table_item(
                    source,
                    &tokens[item_start..index],
                    &mut checks,
                )?;
                item_start = index + 1;
            }
            _ => {}
        }
    }
    Err("CREATE TABLE statement has no closing `)`".to_string())
}

fn sqlite_create_table_body_opening(tokens: &[Token]) -> Result<usize, String> {
    let mut index = 0;
    if !tokens
        .get(index)
        .is_some_and(|token| token.is_ident("create"))
    {
        return Err("schema SQL is not an ordinary `CREATE TABLE` statement".to_string());
    }
    index += 1;
    if tokens
        .get(index)
        .is_some_and(|token| token.is_ident("virtual"))
    {
        return Err("virtual tables do not have an ordinary `CREATE TABLE` body".to_string());
    }
    if !tokens
        .get(index)
        .is_some_and(|token| token.is_ident("table"))
    {
        return Err("schema SQL is not an ordinary `CREATE TABLE` statement".to_string());
    }
    index += 1;
    if tokens.get(index).is_some_and(|token| token.is_ident("if")) {
        if !tokens
            .get(index + 1)
            .is_some_and(|token| token.is_ident("not"))
            || !tokens
                .get(index + 2)
                .is_some_and(|token| token.is_ident("exists"))
        {
            return Err("malformed `CREATE TABLE IF NOT EXISTS` header".to_string());
        }
        index += 3;
    }
    if tokens.get(index).and_then(Token::identifier).is_none() {
        return Err("ordinary `CREATE TABLE` header has no table name".to_string());
    }
    index += 1;
    if !tokens
        .get(index)
        .is_some_and(|token| token.kind == TokenKind::LParen)
    {
        return Err("ordinary `CREATE TABLE` header has no column-definition body".to_string());
    }
    Ok(index)
}

fn extract_checks_from_sqlite_table_item(
    source: &str,
    tokens: &[Token],
    checks: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let mut depth = 0_usize;
    let mut index = 0_usize;
    while index < tokens.len() {
        match tokens[index].kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => depth = depth.saturating_sub(1),
            _ if depth == 0 && tokens[index].is_ident("constraint") => {
                let Some(name) = tokens.get(index + 1).and_then(Token::identifier) else {
                    index += 1;
                    continue;
                };
                if !tokens
                    .get(index + 2)
                    .is_some_and(|token| token.is_ident("check"))
                    || !tokens
                        .get(index + 3)
                        .is_some_and(|token| token.kind == TokenKind::LParen)
                {
                    index += 1;
                    continue;
                }
                let body_start = tokens[index + 3].end;
                let mut check_depth = 1_usize;
                let mut closing_index = None;
                for (offset, token) in tokens[index + 4..].iter().enumerate() {
                    match token.kind {
                        TokenKind::LParen => check_depth += 1,
                        TokenKind::RParen => {
                            check_depth -= 1;
                            if check_depth == 0 {
                                closing_index = Some(index + 4 + offset);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let closing_index = closing_index
                    .ok_or_else(|| format!("constraint `{name}` has no closing `)`"))?;
                if checks
                    .insert(
                        name.to_string(),
                        source[body_start..tokens[closing_index].start]
                            .trim()
                            .to_string(),
                    )
                    .is_some()
                {
                    return Err(format!("constraint name `{name}` appears more than once"));
                }
                index = closing_index;
            }
            _ => {}
        }
        index += 1;
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Expr {
    Identifier(SqlIdentifier),
    String(String),
    Number(String),
    Boolean(bool),
    Cast(Box<Self>, SqlIdentifier),
    Call(SqlIdentifier, Vec<Self>),
    JsonText(Box<Self>, Box<Self>),
    Not(Box<Self>),
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
    Compare(Box<Self>, Comparison, Box<Self>),
    IsNull(Box<Self>, bool),
    In(Box<Self>, Vec<Self>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SqlIdentifier {
    Folded(String),
    Exact(String),
}

impl SqlIdentifier {
    fn unquoted(value: String) -> Self {
        Self::Folded(value)
    }

    fn quoted(value: String) -> Self {
        if is_unquoted_identifier(&value) && value == value.to_ascii_lowercase() {
            Self::Folded(value)
        } else {
            Self::Exact(value)
        }
    }
}

fn is_unquoted_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Comparison {
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Token {
    kind: TokenKind,
    start: usize,
    end: usize,
}

impl Token {
    fn is_ident(&self, expected: &str) -> bool {
        matches!(&self.kind, TokenKind::Ident(found) if found.eq_ignore_ascii_case(expected))
    }

    fn identifier(&self) -> Option<&str> {
        match &self.kind {
            TokenKind::Ident(value) | TokenKind::QuotedIdent(value) => Some(value),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TokenKind {
    Ident(String),
    QuotedIdent(String),
    String(String),
    Number(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Cast,
    JsonText,
    Comparison(Comparison),
    Other(char),
}

fn lex_sqlite_ddl(source: &str) -> Result<Vec<Token>, String> {
    lex_with_mode(source, LexMode::Sqlite)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LexMode {
    PostgresExpression,
    Sqlite,
}

fn lex_with_mode(source: &str, mode: LexMode) -> Result<Vec<Token>, String> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if source[index..].starts_with("--") {
            index = source[index..]
                .find('\n')
                .map_or(bytes.len(), |end| index + end + 1);
            continue;
        }
        if source[index..].starts_with("/*") {
            let Some(end) = source[index + 2..].find("*/") else {
                return Err("unterminated block comment".to_string());
            };
            index += end + 4;
            continue;
        }
        let start = index;
        let kind = match byte {
            b'\'' => {
                index += 1;
                let mut value = String::new();
                loop {
                    let Some(next) = bytes.get(index).copied() else {
                        return Err("unterminated string literal".to_string());
                    };
                    if next == b'\'' {
                        if bytes.get(index + 1) == Some(&b'\'') {
                            value.push('\'');
                            index += 2;
                        } else {
                            index += 1;
                            break;
                        }
                    } else {
                        let character = source[index..]
                            .chars()
                            .next()
                            .ok_or_else(|| "invalid UTF-8 boundary".to_string())?;
                        value.push(character);
                        index += character.len_utf8();
                    }
                }
                TokenKind::String(value)
            }
            b'"' | b'`' => {
                let closing = byte;
                index += 1;
                let mut value = String::new();
                loop {
                    let Some(next) = bytes.get(index).copied() else {
                        return Err("unterminated quoted identifier".to_string());
                    };
                    if next == closing {
                        if bytes.get(index + 1) == Some(&closing) && closing != b']' {
                            value.push(closing as char);
                            index += 2;
                        } else {
                            index += 1;
                            break;
                        }
                    } else {
                        let character = source[index..]
                            .chars()
                            .next()
                            .ok_or_else(|| "invalid UTF-8 boundary".to_string())?;
                        value.push(character);
                        index += character.len_utf8();
                    }
                }
                TokenKind::QuotedIdent(if mode == LexMode::Sqlite {
                    value.to_ascii_lowercase()
                } else {
                    value
                })
            }
            b'(' => {
                index += 1;
                TokenKind::LParen
            }
            b')' => {
                index += 1;
                TokenKind::RParen
            }
            b'[' if mode == LexMode::Sqlite => {
                index += 1;
                let mut value = String::new();
                loop {
                    let Some(next) = bytes.get(index).copied() else {
                        return Err("unterminated bracket-quoted identifier".to_string());
                    };
                    if next == b']' {
                        index += 1;
                        break;
                    }
                    let character = source[index..]
                        .chars()
                        .next()
                        .ok_or_else(|| "invalid UTF-8 boundary".to_string())?;
                    value.push(character);
                    index += character.len_utf8();
                }
                TokenKind::QuotedIdent(value.to_ascii_lowercase())
            }
            b'[' => {
                index += 1;
                TokenKind::LBracket
            }
            b']' => {
                index += 1;
                TokenKind::RBracket
            }
            b',' => {
                index += 1;
                TokenKind::Comma
            }
            b':' if bytes.get(index + 1) == Some(&b':') => {
                index += 2;
                TokenKind::Cast
            }
            b'-' if bytes.get(index + 1) == Some(&b'>') && bytes.get(index + 2) == Some(&b'>') => {
                index += 3;
                TokenKind::JsonText
            }
            b'=' => {
                index += 1;
                TokenKind::Comparison(Comparison::Equal)
            }
            b'!' if bytes.get(index + 1) == Some(&b'=') => {
                index += 2;
                TokenKind::Comparison(Comparison::NotEqual)
            }
            b'<' if bytes.get(index + 1) == Some(&b'=') => {
                index += 2;
                TokenKind::Comparison(Comparison::LessEqual)
            }
            b'>' if bytes.get(index + 1) == Some(&b'=') => {
                index += 2;
                TokenKind::Comparison(Comparison::GreaterEqual)
            }
            b'<' if bytes.get(index + 1) == Some(&b'>') => {
                index += 2;
                TokenKind::Comparison(Comparison::NotEqual)
            }
            b'<' => {
                index += 1;
                TokenKind::Comparison(Comparison::Less)
            }
            b'>' => {
                index += 1;
                TokenKind::Comparison(Comparison::Greater)
            }
            b'0'..=b'9' => {
                index += 1;
                while bytes.get(index).is_some_and(u8::is_ascii_digit) {
                    index += 1;
                }
                TokenKind::Number(source[start..index].to_string())
            }
            _ if byte.is_ascii_alphabetic() || byte == b'_' => {
                index += 1;
                while bytes
                    .get(index)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                {
                    index += 1;
                }
                TokenKind::Ident(source[start..index].to_ascii_lowercase())
            }
            _ => {
                let character = source[index..]
                    .chars()
                    .next()
                    .ok_or_else(|| "invalid UTF-8 boundary".to_string())?;
                index += character.len_utf8();
                TokenKind::Other(character)
            }
        };
        tokens.push(Token {
            kind,
            start,
            end: index,
        });
    }
    Ok(tokens)
}

#[cfg(test)]
fn parse_expression(source: &str) -> Result<Expr, String> {
    parse_expression_with_mode(source, LexMode::PostgresExpression)
}

fn parse_expression_for_backend(backend: &str, source: &str) -> Result<Expr, String> {
    let mode = if backend == "sqlite" {
        LexMode::Sqlite
    } else {
        LexMode::PostgresExpression
    };
    parse_expression_with_mode(source, mode)
}

fn parse_expression_with_mode(source: &str, mode: LexMode) -> Result<Expr, String> {
    let tokens = lex_with_mode(source, mode)?;
    let mut parser = Parser { tokens, index: 0 };
    let expression = parser.parse_or()?;
    if parser.index != parser.tokens.len() {
        return Err(format!(
            "unexpected token at byte {}",
            parser.tokens[parser.index].start
        ));
    }
    Ok(expression)
}

#[cfg(test)]
#[path = "required_constraints/tests.rs"]
mod tests;
