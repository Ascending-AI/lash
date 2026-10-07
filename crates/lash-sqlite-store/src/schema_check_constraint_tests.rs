//! The schema's named-`CHECK` and fragment laws, split from `schema.rs` to
//! keep the production file inside its line budget.

use super::*;

fn assert_check_rejects(connection: &Connection, statement: &str, constraint: &str) {
    let error = connection
        .execute_batch(statement)
        .expect_err("an illegal durable vocabulary must violate its schema CHECK");
    assert!(
        error.to_string().contains(constraint),
        "SQLite reported the wrong CHECK for {constraint}: {error}"
    );
}

#[test]
fn sqlite_checks_reject_every_registered_illegal_vocabulary_cluster() {
    let core = Connection::open_in_memory().expect("open durable-core constraint fixture");
    core.execute_batch(SCHEMA)
        .expect("create durable-core constraint fixture");
    // The three illegal scope/state pairs must name the correlation CHECK.
    // An ingress_json without a scope key passes both CHECKs under SQL NULL
    // semantics; serde cannot emit it, so both backends behave identically.
    assert_check_rejects(
        &core,
        "INSERT INTO pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-state', 'session',
             '{\"scope\":\"active_turn\",\"turn_id\":\"turn\"}',
             'waiting', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-pair', 'session', '{\"scope\":\"next_turn\"}',
             'pending_active', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state_ingress",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-accepted-pair', 'session', '{\"scope\":\"next_turn\"}',
             'accepted', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state_ingress",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-deferred-pair', 'session',
             '{\"scope\":\"active_turn\",\"turn_id\":\"turn\"}',
             'deferred_next_turn', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state_ingress",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO session_meta (session_id, relation_kind) VALUES ('bad-relation', 'sibling')",
        "ck_session_meta_relation_kind",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_kind) VALUES ('bad-cause', 'child', 'parent', 'timer')",
        "ck_session_meta_caused_by_kind",
    );
    core.execute_batch(
        "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_kind, caused_by_effect_id) VALUES ('effect-address-cause', 'child', 'parent', 'effect_address', '{}')",
    )
    .expect("current effect-address discriminator is admitted");
    assert_check_rejects(
        &core,
        "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_kind) VALUES ('legacy-effect-cause', 'child', 'parent', 'effect')",
        "ck_session_meta_caused_by_kind",
    );

    assert_check_rejects(
        &core,
        "INSERT INTO session_meta (session_id, relation_kind) VALUES ('childless-child', 'child')",
        "ck_session_meta_relation_family",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO session_meta (session_id, relation_kind, caused_by_kind, caused_by_session_id, caused_by_turn_id) VALUES ('caused-run', 'root', 'turn', 'cause-session', 'cause-turn')",
        "ck_session_meta_relation_family",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_kind) VALUES ('bare-discriminator', 'child', 'parent', 'turn')",
        "ck_session_meta_caused_by_family",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_kind, caused_by_session_id, caused_by_turn_id, caused_by_node_id) VALUES ('crossed-family', 'child', 'parent', 'turn', 'cause-session', 'cause-turn', 'stray-node')",
        "ck_session_meta_caused_by_family",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO session_meta (session_id, relation_kind, parent_session_id, caused_by_session_id) VALUES ('kindless-payload', 'child', 'parent', 'cause-session')",
        "ck_session_meta_caused_by_family",
    );

    let process = Connection::open_in_memory().expect("open process constraint fixture");
    process
        .execute_batch(PROCESS_SCHEMA)
        .expect("create process constraint fixture");
    let process_columns = "process_id, originator_id,
        identity_kind, created_at_ms, updated_at_ms, last_event_sequence, change_seq,
        status, lifetime_scope_kind, lifetime_scope_id, lifetime, record_json";
    assert_check_rejects(
        &process,
        &format!(
            "INSERT INTO processes ({process_columns}) VALUES
             ('bad-status', 'originator', 'standard', 0, 0, 0, 0,
              'paused', NULL, NULL, 'detached', '{{}}')"
        ),
        "ck_processes_status",
    );
    assert_check_rejects(
        &process,
        &format!(
            "INSERT INTO processes ({process_columns}) VALUES
             ('bad-lifetime', 'originator', 'standard', 0, 0, 0, 0,
              'running', NULL, NULL, 'abandon', '{{}}')"
        ),
        "ck_processes_lifetime",
    );
    assert_check_rejects(
        &process,
        &format!(
            "INSERT INTO processes ({process_columns}) VALUES
             ('bad-scope-kind', 'originator', 'standard', 0, 0, 0, 0,
              'running', 'host', 'scope', 'until', '{{}}')"
        ),
        "ck_processes_lifetime_scope",
    );
    assert_check_rejects(
        &process,
        &format!(
            "INSERT INTO processes ({process_columns}) VALUES
             ('detached-with-scope', 'originator', 'standard', 0, 0, 0, 0,
              'running', 'turn', 'scope', 'detached', '{{}}')"
        ),
        "ck_processes_lifetime_scope",
    );
    assert_check_rejects(
        &process,
        &format!(
            "INSERT INTO processes ({process_columns}) VALUES
             ('until-without-id', 'originator', 'standard', 0, 0, 0, 0,
              'running', 'session', NULL, 'until', '{{}}')"
        ),
        "ck_processes_lifetime_scope",
    );
    assert_check_rejects(
        &process,
        "INSERT INTO parent_end_plans (parent_kind, parent_id, parent_payload, ended_at_ms)
         VALUES ('host', 'scope', '{}', 0)",
        "ck_parent_end_plans_kind",
    );
    process
        .execute_batch(&format!(
            "INSERT INTO processes ({process_columns}) VALUES
             ('wake-parent', 'originator', 'standard', 0, 0, 0, 0,
              'running', NULL, NULL, 'detached', '{{}}')"
        ))
        .expect("insert valid wake parent");
    assert_check_rejects(
        &process,
        "INSERT INTO process_event_horizons (process_id, released_through)
         VALUES ('wake-parent', 0)",
        "ck_process_event_horizons_positive",
    );
    // A tombstone names the retired status its process was pruned in.
    for label in ["running", "waiting", "finished"] {
        assert_check_rejects(
            &process,
            &format!(
                "INSERT INTO process_tombstones (
                     process_id, terminal_label, pruned_at_ms, pruned_change_seq
                 ) VALUES ('tombstone-{label}', '{label}', 0, 1)"
            ),
            "ck_process_tombstones_terminal_label",
        );
    }

    let triggers = Connection::open_in_memory().expect("open trigger constraint fixture");
    triggers
        .execute_batch(TRIGGER_SCHEMA)
        .expect("create trigger constraint fixture");
    assert_check_rejects(
        &triggers,
        "INSERT INTO trigger_subscriptions (
             subscription_id, owner_scope, subscription_key, incarnation, revision,
             definition_fingerprint, source_type, source_key, lifecycle, deleted_at_ms,
             created_at_ms, updated_at_ms, record_json
         ) VALUES (
             'bad-vocabulary', 'owner', 'key', 'incarnation', 1, 'fingerprint',
             'source', 'key', 'archived', NULL, 0, 0, '{}'
         )",
        "ck_trigger_subscriptions_lifecycle",
    );
    assert_check_rejects(
        &triggers,
        "INSERT INTO trigger_subscriptions (
             subscription_id, owner_scope, subscription_key, incarnation, revision,
             definition_fingerprint, source_type, source_key, lifecycle, deleted_at_ms,
             created_at_ms, updated_at_ms, record_json
         ) VALUES (
             'tombstone-without-time', 'owner', 'key', 'incarnation', 1, 'fingerprint',
             'source', 'key', 'tombstoned', NULL, 0, 0, '{}'
         )",
        "ck_trigger_subscriptions_lifecycle_deleted_at",
    );
    assert_check_rejects(
        &triggers,
        "INSERT INTO trigger_subscriptions (
             subscription_id, owner_scope, subscription_key, incarnation, revision,
             definition_fingerprint, source_type, source_key, lifecycle, deleted_at_ms,
             created_at_ms, updated_at_ms, record_json
         ) VALUES (
             'live-with-a-deletion-time', 'owner', 'key', 'incarnation', 1, 'fingerprint',
             'source', 'key', 'enabled', 7, 0, 0, '{}'
         )",
        "ck_trigger_subscriptions_lifecycle_deleted_at",
    );
    assert_check_rejects(
        &triggers,
        "INSERT INTO trigger_mutation_receipts (
             operation_id, owner_kind, owner_id,
             request_fingerprint, result_json, created_at_ms
         ) VALUES ('bad-owner-kind', 'workflow', 'owner', 'fingerprint', '{}', 0)",
        "ck_trigger_receipts_owner_kind",
    );
    assert_check_rejects(
        &triggers,
        "INSERT INTO trigger_deliveries (
             occurrence_id, subscription_id, process_id, subscription_incarnation,
             subscription_revision, subscription_snapshot_json, created_at_ms,
             obligation_id, obligation_state, obligation_due_at_ms
         ) VALUES ('occurrence', 'subscription', NULL, 'incarnation', 1, '{}', 0,
                   NULL, 'due', 0)",
        "ck_trigger_deliveries_obligation",
    );
}

/// The shift-authority columns as `(shift_epoch, shift_admission_id,
/// shift_run_start, closing_intent)` literals: every combination no raise
/// writes, and every one a raise does.
const UNREAL_SHIFT_STATES: &[(&str, &str)] = &[
    (
        "an unraised epoch naming an admission",
        "0, 'a', NULL, NULL",
    ),
    (
        "an unraised epoch naming a start marker",
        "0, NULL, 'n', NULL",
    ),
    ("an unraised epoch naming a seal", "0, 'a', 'n', NULL"),
    ("a raised epoch naming no admission", "1, NULL, NULL, NULL"),
    ("a start marker without its admission", "1, NULL, 'n', NULL"),
    ("a closing session no close raised", "0, NULL, NULL, 1"),
    ("a closing session an execution sealed", "1, 'a', 'n', 1"),
];
const REAL_SHIFT_STATES: &[(&str, &str)] = &[
    ("unraised", "0, NULL, NULL, NULL"),
    ("sealed by an execution", "1, 'a', 'n', NULL"),
    ("raised by a control verb", "1, 'a', NULL, NULL"),
    ("closing under the raise of its close", "1, 'a', NULL, 1"),
];

#[test]
fn sqlite_shift_authority_check_admits_only_real_shift_states() {
    let core = Connection::open_in_memory().expect("open shift-authority constraint fixture");
    core.execute_batch(SCHEMA)
        .expect("create shift-authority constraint fixture");
    let insert = |case: &str, values: &str| {
        format!(
            "INSERT INTO session_meta (session_id, relation_kind, shift_epoch,
                 shift_admission_id, shift_run_start, closing_intent)
             VALUES ('{case}', 'root', {values})"
        )
    };
    for (case, values) in UNREAL_SHIFT_STATES {
        let error = core
            .execute_batch(&insert(case, values))
            .expect_err(&format!("{case} must violate the shift-authority CHECK"));
        assert!(
            error
                .to_string()
                .contains("ck_session_meta_shift_authority"),
            "{case}: SQLite reported the wrong CHECK: {error}"
        );
    }
    for (case, values) in REAL_SHIFT_STATES {
        core.execute_batch(&insert(case, values))
            .unwrap_or_else(|error| panic!("{case} is a real shift state: {error}"));
    }
}

#[test]
fn turn_cancellation_shape_is_guarded() {
    let conn = Connection::open_in_memory().expect("open cancellation fixture");
    conn.execute_batch(SCHEMA).expect("create schema");
    conn.execute_batch("INSERT INTO turn_cancel_requests (session_id, turn_id, request_id, disposition, mode, intent_revision) VALUES ('session', 'turn', 'request', 'defer', 'immediate', 1)").expect("record relational request");
    for (column, value, constraint) in [
        (
            "disposition",
            "discard",
            "ck_turn_cancel_requests_disposition",
        ),
        ("mode", "later", "ck_turn_cancel_requests_mode"),
        (
            "intent_revision",
            "0",
            "ck_turn_cancel_requests_intent_revision",
        ),
    ] {
        assert_check_rejects(
            &conn,
            &format!("UPDATE turn_cancel_requests SET {column} = '{value}'"),
            constraint,
        );
    }
}

#[test]
fn reclaim_markers_require_terminal_owners() {
    let process = Connection::open_in_memory().expect("open parent-end fixture");
    process
        .execute_batch(PROCESS_SCHEMA)
        .expect("create process schema");
    let triggers = Connection::open_in_memory().expect("open change-feed fixture");
    triggers
        .execute_batch(TRIGGER_SCHEMA)
        .expect("create trigger schema");
    for lifecycle in [
        serde_json::json!({}),
        serde_json::json!({"lifecycle":"enabled"}),
        serde_json::json!({"lifecycle":"disabled"}),
        serde_json::json!({"lifecycle":"unknown"}),
    ] {
        let json = serde_json::json!({"lifecycle": lifecycle}).to_string();
        triggers
            .execute("DELETE FROM trigger_subscription_changes", [])
            .expect("clear fixture");
        triggers
            .execute(
                "INSERT INTO trigger_subscription_changes VALUES ('subscription', 1, NULL, ?1)",
                [&json],
            )
            .expect("a live change is retained");
        assert_check_rejects(
            &triggers,
            "UPDATE trigger_subscription_changes SET deleted_at_ms = 1",
            "ck_trigger_subscription_changes_reclaimable",
        );
    }
    triggers
        .execute("DELETE FROM trigger_subscription_changes", [])
        .expect("clear fixture");
    let json = serde_json::json!({"lifecycle": lash_core_execution::triggers::TriggerSubscriptionLifecycle::Tombstoned(1)}).to_string();
    triggers
        .execute(
            "INSERT INTO trigger_subscription_changes VALUES ('subscription', 1, 1, ?1)",
            [&json],
        )
        .expect("a tombstoned change may be reclaimed");
}

#[test]
fn parent_end_delivery_atomically_arms_reclaim() {
    use lash_store_sql::process::parent_end_plans::{
        ParentEndPlanObligationStatements, ParentEndPlanStatements,
    };
    let process = Connection::open_in_memory().expect("open parent-end fixture");
    process
        .execute_batch(PROCESS_SCHEMA)
        .expect("create process schema");
    let dialect = lash_store_sql::Dialect::sqlite_unqualified();
    let plan = ParentEndPlanStatements::render(dialect);
    let obligation = ParentEndPlanObligationStatements::render(dialect);
    process
        .execute_batch(
            "INSERT INTO parent_end_plans (parent_kind, parent_id, parent_payload, ended_at_ms,
        obligation_id, obligation_state, obligation_due_at_ms)
        VALUES ('session', 'parent', '{}', 0, 'id', 'due', 0)",
        )
        .expect("arm plan");
    process
        .execute(plan.settle.sql(), rusqlite::params!["session", "parent", 7])
        .expect("apply due plan");
    let stamps = || {
        process
            .query_row(
                "SELECT obligation_state, CASE WHEN obligation_state = 'delivered' THEN obligation_settled_at_ms END FROM parent_end_plans",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .expect("read stamps")
    };
    assert_eq!(stamps(), ("delivered".into(), Some(7)));
    process
        .execute(plan.settle.sql(), rusqlite::params!["session", "parent", 8])
        .expect("repeat delivery");
    assert_eq!(stamps(), ("delivered".into(), Some(7)));
    process
        .execute_batch(
            "UPDATE parent_end_plans SET
        obligation_state = 'claimed', obligation_due_at_ms = 1, obligation_claim_token = 'token',
        obligation_settled_at_ms = NULL",
        )
        .expect("claim another application");
    process
        .execute(plan.settle.sql(), rusqlite::params!["session", "parent", 9])
        .expect("apply claimed plan");
    assert_eq!(stamps(), ("claimed".into(), None));
    process
        .execute(
            obligation.obligation_settle_delivered.sql(),
            rusqlite::params!["id", "wrong-token", 11],
        )
        .expect("stale settlement");
    assert_eq!(stamps(), ("claimed".into(), None));
    process
        .execute(
            obligation.obligation_settle_delivered.sql(),
            rusqlite::params!["id", "token", 13],
        )
        .expect("fenced settlement");
    assert_eq!(stamps(), ("delivered".into(), Some(13)));
    process
        .execute_batch(
            "UPDATE parent_end_plans SET
        obligation_state = 'stalled', obligation_stall_reason = 'refused'",
        )
        .expect("retain a stalled application");
    process
        .execute(
            plan.settle.sql(),
            rusqlite::params!["session", "parent", 15],
        )
        .expect("apply stalled plan");
    assert_eq!(stamps(), ("stalled".into(), None));
}

#[test]
fn trigger_delivery_cannot_settle_without_binding() {
    let conn = Connection::open_in_memory().expect("trigger fixture");
    conn.execute_batch(crate::trigger_schema::TRIGGER_SCHEMA)
        .expect("trigger schema");
    conn.execute_batch("INSERT INTO trigger_occurrences
        (occurrence_id, idempotency_key, source_type, source_key, occurred_at_ms, outcome_kind, record_json)
        VALUES ('occurrence', 'key', 'source', 'key', 0, 'fired', '{}')").expect("occurrence");
    assert_check_rejects(&conn,
        "INSERT INTO trigger_deliveries (occurrence_id, subscription_id, subscription_incarnation,
         subscription_revision, subscription_snapshot_json, created_at_ms, obligation_id,
         obligation_state, obligation_settled_at_ms)
         VALUES ('occurrence', 'subscription', 'incarnation', 1, '{}', 0, 'obligation', 'delivered', 1)",
        "ck_trigger_deliveries_binding");
}

#[test]
fn park_feed_columns_refuse_mixed_variants() {
    let conn = Connection::open_in_memory().expect("park feed fixture");
    conn.execute_batch(SCHEMA).expect("schema");
    assert_check_rejects(
        &conn,
        "INSERT INTO turn_park_events
        (seq, session_id, turn_id, park_id, kind, cause_json, redrive_intent, at_ms)
        VALUES (1, 'session', 'turn', 1, 'redrive_requested', '7', 7, 0)",
        "ck_turn_park_events_parked_reason",
    );
    conn.execute_batch(
        "INSERT INTO turn_park_events
        (seq, session_id, turn_id, park_id, kind, redrive_intent, at_ms)
        VALUES (1, 'session', 'turn', 1, 'redrive_requested', 7, 0)",
    )
    .expect("integer redrive");
}
