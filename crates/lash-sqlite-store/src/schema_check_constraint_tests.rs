//! The schema's named-`CHECK` and fragment laws, split from `schema.rs` to
//! keep the production file inside its line budget.

use super::*;

/// The fragment dedup's whole point: every database that carries a shared
/// table must end up with the same stored DDL for it. This is the
/// invariant the two copy-pasted declarations silently assumed.
#[test]
fn shared_fragment_tables_carry_identical_ddl_in_every_carrier_database() {
    let carriers: &[(&[&str], &[SqliteDatabase])] = &[(
        &["effect_scope_retirements"],
        &[
            SqliteDatabase::ProcessRegistry,
            SqliteDatabase::EffectReplay,
        ],
    )];
    for &(objects, databases) in carriers {
        for &object in objects {
            let mut rendered = Vec::new();
            for &database in databases {
                let mut connection = Connection::open_in_memory().expect("open shared-DDL fixture");
                prepare_versioned_schema(&mut connection, database)
                    .expect("apply database schema and fragments")
                    .commit()
                    .expect("commit shared-DDL fixture");
                let sql: String = connection
                    .query_row(
                        "SELECT sql FROM sqlite_master WHERE name = ?1",
                        [object],
                        |row| row.get(0),
                    )
                    .unwrap_or_else(|error| {
                        panic!("{object} missing from {}: {error}", database.name())
                    });
                rendered.push((database.name(), sql));
            }
            let (first_database, first_sql) = &rendered[0];
            for (database, sql) in &rendered[1..] {
                assert_eq!(
                    first_sql, sql,
                    "{object} DDL drifted between {first_database} and {database}"
                );
            }
        }
    }
}

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
             submitted_ingress_json, submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-state', 'session',
             '{\"scope\":\"active_turn\",\"turn_id\":\"turn\"}',
             'waiting', '{}', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submitted_ingress_json, submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-pair', 'session', '{\"scope\":\"next_turn\"}',
             'pending_active', '{}', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state_ingress",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submitted_ingress_json, submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-accepted-pair', 'session', '{\"scope\":\"next_turn\"}',
             'accepted', '{}', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state_ingress",
    );
    assert_check_rejects(
        &core,
        "INSERT INTO pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submitted_ingress_json, submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-deferred-pair', 'session',
             '{\"scope\":\"active_turn\",\"turn_id\":\"turn\"}',
             'deferred_next_turn', '{}', '{}', 'digest', 0
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
             replay_key, session_id, execution_scope_id, tool_call_id,
             intent_index, kind, payload_hash, submission_json
         ) VALUES ('bad-tool-kind', 'session', 'scope', 'call', 0,
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

    let effects = Connection::open_in_memory().expect("open effect constraint fixture");
    effects
        .execute_batch(EFFECT_SCHEMA)
        .expect("create effect constraint fixture");
    assert_check_rejects(
        &effects,
        "INSERT INTO runtime_effect_replay (
             scope_id, replay_key, envelope_hash, envelope_json, status,
             created_at_ms, updated_at_ms
         ) VALUES ('scope', 'bad-effect-status', 'hash', '{}', 'cancelled', 0, 0)",
        "ck_runtime_effect_replay_status",
    );
    assert_check_rejects(
        &effects,
        "INSERT INTO runtime_effect_group (
             group_key, scope_id, session_id, wake, loser_disposition,
             expected_children, next_seq, next_commit_seq, created_at_ms
         ) VALUES ('bad-wake', 'scope', 'session', 'majority', 'cancel', 0, 0, 0, 0)",
        "ck_runtime_effect_group_wake",
    );
    assert_check_rejects(
        &effects,
        "INSERT INTO runtime_effect_group (
             group_key, scope_id, session_id, wake, loser_disposition,
             expected_children, next_seq, next_commit_seq, created_at_ms
         ) VALUES ('bad-disposition', 'scope', 'session', 'all', 'retry', 0, 0, 0, 0)",
        "ck_runtime_effect_group_loser_disposition",
    );
}
