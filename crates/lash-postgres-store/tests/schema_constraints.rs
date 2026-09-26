#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

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

async fn assert_integrity_rejects(
    connection: &mut PgConnection,
    statement: &str,
    kind: impl Fn(&dyn sqlx::error::DatabaseError) -> bool,
    what: &str,
) {
    sqlx::query("SAVEPOINT integrity_violation")
        .execute(&mut *connection)
        .await
        .expect("create integrity-violation savepoint");
    let error = sqlx::query(statement)
        .execute(&mut *connection)
        .await
        .expect_err("an impossible durable shape must violate the schema");
    let database_error = error
        .as_database_error()
        .unwrap_or_else(|| panic!("{what}: expected a database error, got {error}"));
    assert!(
        kind(database_error),
        "{what}: Postgres reported the wrong violation: {database_error}"
    );
    sqlx::query("ROLLBACK TO SAVEPOINT integrity_violation")
        .execute(&mut *connection)
        .await
        .expect("recover from expected integrity violation");
    sqlx::query("RELEASE SAVEPOINT integrity_violation")
        .execute(&mut *connection)
        .await
        .expect("release integrity-violation savepoint");
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

    // The three illegal scope/state pairs must name the correlation CHECK.
    // An ingress_json without a scope key passes both CHECKs under SQL NULL
    // semantics; serde cannot emit it, so both backends behave identically.
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submitted_ingress_json, submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-state', 'session',
             '{\"scope\":\"active_turn\",\"turn_id\":\"turn\"}',
             'waiting', '{}', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submitted_ingress_json, submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-pair', 'session', '{\"scope\":\"next_turn\"}',
             'pending_active', '{}', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state_ingress",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submitted_ingress_json, submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-accepted-pair', 'session', '{\"scope\":\"next_turn\"}',
             'accepted', '{}', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state_ingress",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submitted_ingress_json, submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-deferred-pair', 'session',
             '{\"scope\":\"active_turn\",\"turn_id\":\"turn\"}',
             'deferred_next_turn', '{}', '{}', 'digest', 0
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

    // Any strict subset of the four-column claim identity must be rejected —
    // including a claim id/token pair with no owner.
    for fields in [
        "claim_id",
        "claim_owner_id",
        "claim_owner_incarnation_id",
        "claim_token",
        "claim_id, claim_token",
        "claim_id, claim_owner_id, claim_token",
        "claim_owner_id, claim_owner_incarnation_id",
    ] {
        let values = fields
            .split(',')
            .map(|_| "'half'")
            .collect::<Vec<_>>()
            .join(", ");
        assert_check_rejects(
            &mut connection,
            &format!(
                "INSERT INTO lash_pending_turn_inputs (enqueue_seq,
                     input_id, session_id, ingress_json, state, input_json,
                     submitted_ingress_json, submission_digest, enqueued_at_ms, {fields}
                 ) VALUES (1, 'pending', 'session', '{{\"scope\":\"next_turn\"}}',
                           'deferred_next_turn', '{{}}', '{{}}', 'digest', 0, {values})"
            ),
            "ck_pending_turn_inputs_claim_identity_all_or_none",
        )
        .await;
    }

    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_queued_work_batches (enqueue_seq,
             batch_id, session_id, delivery_policy, work_kind, authority_json,
             available_at_ms, enqueued_at_ms
         ) VALUES (1,
             'bad-kind', 'session', 'earliest_safe_boundary', 'cancel', '{}', 0, 0
         )",
        "ck_queued_work_batches_work_kind",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_queued_work_batches (enqueue_seq,
             batch_id, session_id, delivery_policy, work_kind, authority_json,
             available_at_ms, enqueued_at_ms
         ) VALUES (1, 'bad-policy', 'session', 'eventually', 'turn', '{}', 0, 0)",
        "ck_queued_work_batches_delivery_policy",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_queued_work_batches (enqueue_seq,
             batch_id, session_id, delivery_policy, work_kind, authority_json,
             available_at_ms, enqueued_at_ms, claim_id
         ) VALUES (1,
             'claim-id-only', 'session', 'earliest_safe_boundary', 'turn', '{}', 0, 0,
             'claim'
         )",
        "ck_queued_work_batches_claim_id_token_all_or_none",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_queued_work_batches (enqueue_seq,
             batch_id, session_id, delivery_policy, work_kind, authority_json,
             available_at_ms, enqueued_at_ms, claim_token
         ) VALUES (1,
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
        "INSERT INTO lash_session_meta (session_id, relation_kind, parent_session_id,
                                        caused_by_kind)
         VALUES ('bad-cause', 'child', 'parent', 'timer')",
        "ck_session_meta_caused_by_kind",
    )
    .await;
    sqlx::query(
        "INSERT INTO lash_session_meta (session_id, relation_kind, parent_session_id,
                                        caused_by_kind, caused_by_effect_id)
         VALUES ('effect-address-cause', 'child', 'parent', 'effect_address', '{}')",
    )
    .execute(&mut connection)
    .await
    .expect("current effect-address discriminator is admitted");
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_session_meta (session_id, relation_kind, parent_session_id,
                                        caused_by_kind)
         VALUES ('legacy-effect-cause', 'child', 'parent', 'effect')",
        "ck_session_meta_caused_by_kind",
    )
    .await;

    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_session_meta (session_id, relation_kind)
         VALUES ('childless-child', 'child')",
        "ck_session_meta_relation_family",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_session_meta (session_id, relation_kind, caused_by_kind,
                                        caused_by_session_id, caused_by_turn_id)
         VALUES ('caused-root', 'root', 'turn', 'cause-session', 'cause-turn')",
        "ck_session_meta_relation_family",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_session_meta (session_id, relation_kind, parent_session_id,
                                        caused_by_kind)
         VALUES ('bare-discriminator', 'child', 'parent', 'turn')",
        "ck_session_meta_caused_by_family",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_session_meta (session_id, relation_kind, parent_session_id,
                                        caused_by_kind, caused_by_session_id,
                                        caused_by_turn_id, caused_by_node_id)
         VALUES ('crossed-family', 'child', 'parent', 'turn', 'cause-session',
                 'cause-turn', 'stray-node')",
        "ck_session_meta_caused_by_family",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_session_meta (session_id, relation_kind, parent_session_id,
                                        caused_by_session_id)
         VALUES ('kindless-payload', 'child', 'parent', 'cause-session')",
        "ck_session_meta_caused_by_family",
    )
    .await;

    let process_columns = "process_id, originator_id,
        identity_kind, created_at_ms, updated_at_ms, last_event_sequence, change_seq,
        status, lifetime_scope_kind, lifetime_scope_id, lifetime, record_json";
    assert_check_rejects(
        &mut connection,
        &format!(
            "INSERT INTO lash_processes ({process_columns}) VALUES
             ('bad-status', 'originator', 'standard', 0, 0, 0, 0,
              'paused', NULL, NULL, 'detached', '{{}}')"
        ),
        "ck_processes_status",
    )
    .await;
    for (process_id, scope_kind, scope_id, lifetime, constraint) in [
        (
            "bad-lifetime",
            "NULL",
            "NULL",
            "'abandon'",
            "ck_processes_lifetime",
        ),
        (
            "bad-scope-kind",
            "'host'",
            "'scope'",
            "'until'",
            "ck_processes_lifetime_scope",
        ),
        (
            "detached-with-scope",
            "'turn'",
            "'scope'",
            "'detached'",
            "ck_processes_lifetime_scope",
        ),
        (
            "until-without-id",
            "'session'",
            "NULL",
            "'until'",
            "ck_processes_lifetime_scope",
        ),
    ] {
        assert_check_rejects(
            &mut connection,
            &format!(
                "INSERT INTO lash_processes ({process_columns}) VALUES
                 ('{process_id}', 'originator', 'standard', 0, 0, 0, 0,
                  'running', {scope_kind}, {scope_id}, {lifetime}, '{{}}')"
            ),
            constraint,
        )
        .await;
    }
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_parent_end_plans (parent_kind, parent_id, parent_payload, ended_at_ms)
         VALUES ('host', 'scope', '{}', 0)",
        "ck_parent_end_plans_kind",
    )
    .await;
    sqlx::query(&format!(
        "INSERT INTO lash_processes ({process_columns}) VALUES
         ('wake-parent', 'originator', 'standard', 0, 0, 0, 0,
          'running', NULL, NULL, 'detached', '{{}}')"
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
             definition_fingerprint, source_type, source_key, lifecycle, deleted_at_ms,
             created_at_ms, updated_at_ms, record_json
         ) VALUES (
             'bad-vocabulary', 'owner', 'key', 'incarnation', 1, 'fingerprint',
             'source', 'key', 'archived', NULL, 0, 0, '{}'
         )",
        "ck_trigger_subscriptions_lifecycle",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_trigger_subscriptions (
             subscription_id, owner_scope, subscription_key, incarnation, revision,
             definition_fingerprint, source_type, source_key, lifecycle, deleted_at_ms,
             created_at_ms, updated_at_ms, record_json
         ) VALUES (
             'tombstone-without-time', 'owner', 'key', 'incarnation', 1, 'fingerprint',
             'source', 'key', 'tombstoned', NULL, 0, 0, '{}'
         )",
        "ck_trigger_subscriptions_lifecycle_deleted_at",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_trigger_subscriptions (
             subscription_id, owner_scope, subscription_key, incarnation, revision,
             definition_fingerprint, source_type, source_key, lifecycle, deleted_at_ms,
             created_at_ms, updated_at_ms, record_json
         ) VALUES (
             'live-with-a-deletion-time', 'owner', 'key', 'incarnation', 1, 'fingerprint',
             'source', 'key', 'enabled', 7, 0, 0, '{}'
         )",
        "ck_trigger_subscriptions_lifecycle_deleted_at",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_trigger_mutation_receipts (
             operation_id, owner_kind, owner_id,
             request_fingerprint, result_json, created_at_ms
         ) VALUES ('bad-owner-kind', 'workflow', 'owner', 'fingerprint', '{}', 0)",
        "ck_trigger_receipts_owner_kind",
    )
    .await;

    // The cancellation receipt's affected-input evidence is structural: the
    // states the parallel-array shape made representable are all rejected.
    sqlx::query(
        "INSERT INTO lash_turn_cancel_requests (session_id, turn_id, request_id, intent_revision)
         VALUES ('session', 'turn', 'request', 1)",
    )
    .execute(&mut connection)
    .await
    .expect("insert cancel request parent row");
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_turn_cancel_affected_inputs (
             session_id, turn_id, ordinal, input_id, disposition, input_json
         ) VALUES ('session', 'turn', 0, 'input', 'retry', '{}')",
        "ck_turn_cancel_affected_inputs_disposition",
    )
    .await;
    assert_integrity_rejects(
        &mut connection,
        "INSERT INTO lash_turn_cancel_affected_inputs (
             session_id, turn_id, ordinal, input_id, disposition, input_json
         ) VALUES ('session', 'turn', 0, 'input', 'defer', '{}'),
                  ('session', 'turn', 1, 'input', 'drop', '{}')",
        |error| error.kind() == sqlx::error::ErrorKind::UniqueViolation,
        "duplicate affected input id",
    )
    .await;
    assert_integrity_rejects(
        &mut connection,
        "INSERT INTO lash_turn_cancel_affected_inputs (
             session_id, turn_id, ordinal, input_id, disposition, input_json
         ) VALUES ('no-request', 'turn', 0, 'input', 'defer', '{}')",
        |error| error.kind() == sqlx::error::ErrorKind::ForeignKeyViolation,
        "affected evidence without a request",
    )
    .await;
    assert_integrity_rejects(
        &mut connection,
        "INSERT INTO lash_turn_cancel_affected_inputs (
             session_id, turn_id, ordinal, input_id, disposition
         ) VALUES ('session', 'turn', 0, 'input', 'defer')",
        |error| error.kind() == sqlx::error::ErrorKind::NotNullViolation,
        "affected evidence without the payload snapshot",
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
