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
