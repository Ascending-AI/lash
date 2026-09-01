use lash_postgres_store::PostgresStorage;
use sqlx::{Connection, PgConnection};

mod support;

use support::{SharedDatabaseLock, database_url};

const FIXTURE_SCHEMA: &str = "lash_fig2003_constraints";

async fn assert_check_rejects(connection: &mut PgConnection, statement: &str, constraint: &str) {
    sqlx::query("SAVEPOINT illegal_vocabulary")
        .execute(&mut *connection)
        .await
        .expect("create illegal-vocabulary savepoint");
    let error = sqlx::query(statement)
        .execute(&mut *connection)
        .await
        .expect_err("an illegal durable vocabulary must violate its schema CHECK");
    assert_eq!(
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some(constraint),
        "Postgres reported the wrong CHECK: {error}"
    );
    sqlx::query("ROLLBACK TO SAVEPOINT illegal_vocabulary")
        .execute(&mut *connection)
        .await
        .expect("recover from expected CHECK violation");
    sqlx::query("RELEASE SAVEPOINT illegal_vocabulary")
        .execute(&mut *connection)
        .await
        .expect("release illegal-vocabulary savepoint");
}

#[tokio::test]
async fn postgres_checks_reject_every_registered_illegal_vocabulary_cluster_when_configured() {
    let Some(url) = database_url() else {
        eprintln!("skipping Postgres schema CHECK witnesses: database URL is not set");
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&url).await;
    let mut connection = PgConnection::connect(&url)
        .await
        .expect("connect Postgres CHECK fixture");
    sqlx::raw_sql(&format!(
        "DROP SCHEMA IF EXISTS {FIXTURE_SCHEMA} CASCADE;
         CREATE SCHEMA {FIXTURE_SCHEMA};
         SET search_path TO {FIXTURE_SCHEMA};"
    ))
    .execute(&mut connection)
    .await
    .expect("create isolated Postgres CHECK fixture schema");
    sqlx::raw_sql(PostgresStorage::schema_ddl())
        .execute(&mut connection)
        .await
        .expect("apply Postgres schema DDL to CHECK fixture");
    sqlx::query("BEGIN")
        .execute(&mut connection)
        .await
        .expect("begin CHECK witness transaction");

    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_pending_turn_inputs (
             input_id, session_id, ingress_json, state, input_json, enqueued_at_ms
         ) VALUES (
             'bad-turn-input-state', 'session',
             '{\"scope\":\"active_turn\",\"turn_id\":\"turn\"}',
             'waiting', '{}', 0
         )",
        "ck_pending_turn_inputs_state",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_pending_turn_inputs (
             input_id, session_id, ingress_json, state, input_json, enqueued_at_ms
         ) VALUES (
             'bad-turn-input-pair', 'session', '{\"scope\":\"next_turn\"}',
             'pending_active', '{}', 0
         )",
        "ck_pending_turn_inputs_state_ingress",
    )
    .await;

    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_session_execution_leases (session_id, lease_token)
         VALUES ('partial-identity', 'token-without-executor')",
        "ck_session_execution_leases_identity_all_or_none",
    )
    .await;

    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_queued_work_batches (
             batch_id, session_id, delivery_policy, work_kind, authority_json,
             available_at_ms, enqueued_at_ms
         ) VALUES (
             'bad-kind', 'session', 'earliest_safe_boundary', 'cancel', '{}', 0, 0
         )",
        "ck_queued_work_batches_work_kind",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_queued_work_batches (
             batch_id, session_id, delivery_policy, work_kind, authority_json,
             available_at_ms, enqueued_at_ms
         ) VALUES ('bad-policy', 'session', 'eventually', 'turn', '{}', 0, 0)",
        "ck_queued_work_batches_delivery_policy",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_queued_work_batches (
             batch_id, session_id, delivery_policy, work_kind, authority_json,
             available_at_ms, enqueued_at_ms, claim_id
         ) VALUES (
             'claim-id-only', 'session', 'earliest_safe_boundary', 'turn', '{}', 0, 0,
             'claim'
         )",
        "ck_queued_work_batches_claim_id_token_all_or_none",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_queued_work_batches (
             batch_id, session_id, delivery_policy, work_kind, authority_json,
             available_at_ms, enqueued_at_ms, claim_token
         ) VALUES (
             'claim-token-only', 'session', 'earliest_safe_boundary', 'turn', '{}', 0, 0,
             'token'
         )",
        "ck_queued_work_batches_claim_id_token_all_or_none",
    )
    .await;

    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_session_meta (session_id, relation_kind)
         VALUES ('bad-relation', 'sibling')",
        "ck_session_meta_relation_kind",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_session_meta (session_id, relation_kind, caused_by_kind)
         VALUES ('bad-cause', 'child', 'timer')",
        "ck_session_meta_caused_by_kind",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_session_meta (
             session_id, relation_kind, observer_inheritance_kind
         ) VALUES ('bad-inheritance', 'fork', 'selected')",
        "ck_session_meta_observer_inheritance_kind",
    )
    .await;

    let process_columns = "process_id, registration_fingerprint, originator_id,
        identity_kind, is_waiting, created_at_ms, updated_at_ms, change_seq,
        status, record_json";
    assert_check_rejects(
        &mut connection,
        &format!(
            "INSERT INTO lash_processes ({process_columns}) VALUES
             ('bad-status', 'fingerprint', 'originator', 'standard', FALSE, 0, 0, 0,
              'paused', '{{}}')"
        ),
        "ck_processes_status",
    )
    .await;
    sqlx::query(&format!(
        "INSERT INTO lash_processes ({process_columns}) VALUES
         ('wake-parent', 'fingerprint', 'originator', 'standard', FALSE, 0, 0, 0,
          'running', '{{}}')"
    ))
    .execute(&mut connection)
    .await
    .expect("insert valid wake parent");
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_process_wake_deliveries (
             delivery_id, process_id, target_session_id, sequence, state,
             next_attempt_at_ms, expires_at_ms, delivery_json
         ) VALUES ('bad-state', 'wake-parent', 'target', 1, 'claimed', 0, 1, '{}')",
        "ck_process_wake_deliveries_state",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_process_wake_deliveries (
             delivery_id, process_id, target_session_id, sequence, state,
             next_attempt_at_ms, expires_at_ms, discard_reason, delivery_json
         ) VALUES (
             'bad-discard', 'wake-parent', 'target', 2, 'discarded', 0, 1,
             'unroutable', '{}'
         )",
        "ck_process_wake_deliveries_discard_reason",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_tool_intent_submissions (
             replay_key, session_id, execution_scope_id, tool_call_id,
             intent_index, kind, payload_hash, submission_json
         ) VALUES ('bad-tool-kind', 'session', 'scope', 'call', 0,
                   'restart_process', 'hash', '{}')",
        "ck_tool_intent_submissions_kind",
    )
    .await;

    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_trigger_subscriptions (
             subscription_id, owner_scope, subscription_key, incarnation, revision,
             definition_fingerprint, source_type, source_key, enabled, tombstoned,
             created_at_ms, updated_at_ms, record_json
         ) VALUES (
             'bad-pair', 'owner', 'key', 'incarnation', 1, 'fingerprint',
             'source', 'key', TRUE, TRUE, 0, 0, '{}'
         )",
        "ck_trigger_subscriptions_live_enabled",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_runtime_effect_replay (
             scope_id, replay_key, envelope_hash, envelope_json, status,
             created_at_ms, updated_at_ms
         ) VALUES ('scope', 'bad-effect-status', 'hash', '{}', 'cancelled', 0, 0)",
        "ck_runtime_effect_replay_status",
    )
    .await;

    sqlx::query("ROLLBACK")
        .execute(&mut connection)
        .await
        .expect("roll back CHECK witness transaction");
    sqlx::query("SET search_path TO public")
        .execute(&mut connection)
        .await
        .expect("restore Postgres search path");
    sqlx::raw_sql(&format!("DROP SCHEMA {FIXTURE_SCHEMA} CASCADE"))
        .execute(&mut connection)
        .await
        .expect("drop Postgres CHECK fixture schema");
}
