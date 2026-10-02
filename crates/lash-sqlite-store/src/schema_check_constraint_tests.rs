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
        "INSERT INTO session_meta (session_id, relation_kind, caused_by_kind, caused_by_session_id, caused_by_turn_id) VALUES ('caused-root', 'root', 'turn', 'cause-session', 'cause-turn')",
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
        "INSERT INTO process_wake_deliveries (
             delivery_id, process_id, target_session_id, sequence, state,
             next_attempt_at_ms, expires_at_ms, delivery_json
         ) VALUES ('bad-state', 'wake-parent', 'target', 1, 'claimed', 0, 1, '{}')",
        "ck_process_wake_deliveries_state",
    );
    assert_check_rejects(
        &process,
        "INSERT INTO process_wake_deliveries (
             delivery_id, process_id, target_session_id, sequence, state,
             next_attempt_at_ms, expires_at_ms, discard_reason, delivery_json
         ) VALUES (
             'bad-discard', 'wake-parent', 'target', 2, 'discarded', 0, 1,
             'unroutable', '{}'
         )",
        "ck_process_wake_deliveries_discard_reason",
    );
    assert_check_rejects(
        &process,
        "INSERT INTO tool_intent_submissions (
             replay_key, owner, execution_scope_id, tool_call_id,
             intent_index, kind, payload_hash, submission_json
         ) VALUES ('bad-tool-kind', 'session:session', 'scope', 'call', 0,
                   'restart_process', 'hash', '{}')",
        "ck_tool_intent_submissions_kind",
    );

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

/// The drive-authority columns as `(drive_epoch, drive_admission_id,
/// drive_root_start, closing_intent)` literals: every combination no raise
/// writes, and every one a raise does.
const UNREAL_DRIVE_STATES: &[(&str, &str)] = &[
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
const REAL_DRIVE_STATES: &[(&str, &str)] = &[
    ("unraised", "0, NULL, NULL, NULL"),
    ("sealed by an execution", "1, 'a', 'n', NULL"),
    ("raised by a control verb", "1, 'a', NULL, NULL"),
    ("closing under the raise of its close", "1, 'a', NULL, 1"),
];

#[test]
fn sqlite_drive_authority_check_admits_only_real_drive_states() {
    let core = Connection::open_in_memory().expect("open drive-authority constraint fixture");
    core.execute_batch(SCHEMA)
        .expect("create drive-authority constraint fixture");
    let insert = |case: &str, values: &str| {
        format!(
            "INSERT INTO session_meta (session_id, relation_kind, drive_epoch,
                 drive_admission_id, drive_root_start, closing_intent)
             VALUES ('{case}', 'root', {values})"
        )
    };
    for (case, values) in UNREAL_DRIVE_STATES {
        let error = core
            .execute_batch(&insert(case, values))
            .expect_err(&format!("{case} must violate the drive-authority CHECK"));
        assert!(
            error
                .to_string()
                .contains("ck_session_meta_drive_authority"),
            "{case}: SQLite reported the wrong CHECK: {error}"
        );
    }
    for (case, values) in REAL_DRIVE_STATES {
        core.execute_batch(&insert(case, values))
            .unwrap_or_else(|error| panic!("{case} is a real drive state: {error}"));
    }
}

#[test]
fn turn_cancellation_shape_is_guarded() {
    let conn = Connection::open_in_memory().expect("open cancellation fixture");
    conn.execute_batch(SCHEMA).expect("create schema");
    conn.execute_batch("PRAGMA foreign_keys = ON")
        .expect("enable foreign keys");
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
    for (ordinal, kind, batch, constraint) in [
        (
            -1,
            "input",
            "NULL",
            "ck_turn_cancel_affected_inputs_ordinal",
        ),
        (
            0,
            "unknown",
            "NULL",
            "ck_turn_cancel_affected_inputs_item_kind",
        ),
        (
            0,
            "process_wake",
            "NULL",
            "ck_turn_cancel_affected_inputs_item_kind",
        ),
        (
            0,
            "input",
            "'batch'",
            "ck_turn_cancel_affected_inputs_item_kind",
        ),
    ] {
        assert_check_rejects(
            &conn,
            &format!(
                "INSERT INTO turn_cancel_affected_inputs (session_id, turn_id, ordinal, input_id, disposition, input_json, item_kind, batch_id) VALUES ('session', 'turn', {ordinal}, 'item', 'defer', '{{}}', '{kind}', {batch})"
            ),
            constraint,
        );
    }
    conn.execute_batch("INSERT INTO turn_cancel_affected_inputs (session_id, turn_id, ordinal, input_id, disposition, input_json, item_kind, batch_id) VALUES ('session', 'turn', 0, 'item', 'defer', '{}', 'input', NULL)").expect("record snapshot");
    let duplicate = conn.execute_batch("INSERT INTO turn_cancel_affected_inputs (session_id, turn_id, ordinal, input_id, disposition, input_json, item_kind, batch_id) VALUES ('session', 'turn', 1, 'item', 'defer', '{}', 'input', NULL)").expect_err("duplicate receipt must be refused");
    assert_eq!(
        duplicate.sqlite_error_code(),
        Some(rusqlite::ErrorCode::ConstraintViolation)
    );
    let orphan = conn.execute_batch("INSERT INTO turn_cancel_affected_inputs (session_id, turn_id, ordinal, input_id, disposition, input_json, item_kind, batch_id) VALUES ('missing', 'turn', 0, 'item', 'defer', '{}', 'input', NULL)").expect_err("orphan receipt must be refused");
    assert!(orphan.to_string().contains("FOREIGN KEY"));
    conn.execute_batch("DELETE FROM turn_cancel_requests")
        .expect("delete owner");
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM turn_cancel_affected_inputs",
            [],
            |row| row.get::<_, i64>(0)
        )
        .expect("count snapshots"),
        0
    );
}
