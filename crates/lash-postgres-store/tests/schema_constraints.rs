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
             submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-state', 'session',
             '{\"scope\":\"active_turn\",\"turn_id\":\"turn\"}',
             'waiting', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-pair', 'session', '{\"scope\":\"next_turn\"}',
             'pending_active', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state_ingress",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-accepted-pair', 'session', '{\"scope\":\"next_turn\"}',
             'accepted', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state_ingress",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submission_digest, enqueued_at_ms
         ) VALUES (1,
             'bad-turn-input-deferred-pair', 'session',
             '{\"scope\":\"active_turn\",\"turn_id\":\"turn\"}',
             'deferred_next_turn', '{}', 'digest', 0
         )",
        "ck_pending_turn_inputs_state_ingress",
    )
    .await;

    // A row is open or admitted to a run by a recorded step: a run without
    // its step, or a step without its run, is unrepresentable (FIG-3927).
    for (fields, values) in [("admitted_run", "'root'"), ("admitted_by", "'admit'")] {
        assert_check_rejects(
            &mut connection,
            &format!(
                "INSERT INTO lash_pending_turn_inputs (enqueue_seq,
                     input_id, session_id, ingress_json, state, input_json,
                     submission_digest, enqueued_at_ms, {fields}
                 ) VALUES (1, 'pending', 'session', '{{\"scope\":\"next_turn\"}}',
                           'deferred_next_turn', '{{}}', 'digest', 0, {values})"
            ),
            "ck_pending_turn_inputs_admission_all_or_none",
        )
        .await;
        assert_check_rejects(
            &mut connection,
            &format!(
                "INSERT INTO lash_queued_work_batches (enqueue_seq,
                     batch_id, session_id, delivery_policy, work_kind, authority_json,
                     submission_digest, enqueued_at_ms, payload_json, {fields}
                 ) VALUES (1, 'batch', 'session', 'earliest_safe_boundary', 'turn',
                           '{{}}', 'digest', 0, jsonb_build_object('type', 'process_wake')::text, {values})"
            ),
            "ck_queued_work_batches_admission_all_or_none",
        )
        .await;
    }
    // A settled input is answered, so no run holds it.
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_pending_turn_inputs (enqueue_seq,
             input_id, session_id, ingress_json, state, input_json,
             submission_digest, enqueued_at_ms,
             admitted_run, admitted_by
         ) VALUES (1, 'settled', 'session', '{\"scope\":\"next_turn\"}',
                   'completed', '{}', 'digest', 0, 'root', 'admit')",
        "ck_pending_turn_inputs_settled_unadmitted",
    )
    .await;

    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_queued_work_batches (enqueue_seq,
             batch_id, session_id, delivery_policy, work_kind, authority_json,
             submission_digest, enqueued_at_ms, payload_json
         ) VALUES (1,
             'bad-kind', 'session', 'earliest_safe_boundary', 'cancel', '{}', 'digest', 0, jsonb_build_object('type', 'process_wake')::text
         )",
        "ck_queued_work_batches_work_kind",
    )
    .await;
    for (kind, payload) in [
        ("turn", "{}"),
        ("turn", "null"),
        ("turn", "[]"),
        ("turn", r#"{"type":"session_command"}"#),
        ("control", r#"{"type":"process_wake"}"#),
    ] {
        assert_check_rejects(
            &mut connection,
            &format!(
                "INSERT INTO lash_queued_work_batches (enqueue_seq, batch_id, session_id,
                 delivery_policy, work_kind, authority_json, submission_digest,
                 enqueued_at_ms, payload_json) VALUES (1, 'bad-payload', 'session',
                 'earliest_safe_boundary', '{kind}', '{{}}', 'digest', 0, '{payload}')"
            ),
            "ck_queued_work_batches_work_kind",
        )
        .await;
    }
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_queued_work_batches (enqueue_seq,
             batch_id, session_id, delivery_policy, work_kind, authority_json,
             submission_digest, enqueued_at_ms, payload_json
         ) VALUES (1, 'bad-policy', 'session', 'eventually', 'turn', '{}', 'digest', 0, jsonb_build_object('type', 'process_wake')::text)",
        "ck_queued_work_batches_delivery_policy",
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
         VALUES ('caused-run', 'root', 'turn', 'cause-session', 'cause-turn')",
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
        "INSERT INTO lash_process_event_horizons (process_id, released_through)
         VALUES ('wake-parent', 0)",
        "ck_process_event_horizons_positive",
    )
    .await;
    // A tombstone names the retired status its process was pruned in.
    for label in ["running", "waiting", "finished"] {
        assert_check_rejects(
            &mut connection,
            &format!(
                "INSERT INTO lash_process_tombstones (
                     process_id, terminal_label, pruned_at_ms, pruned_change_seq
                 ) VALUES ('tombstone-{label}', '{label}', 0, 1)"
            ),
            "ck_process_tombstones_terminal_label",
        )
        .await;
    }

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

#[path = "support/obligation_constraint_cases.rs"]
mod obligation_constraint_cases;

#[tokio::test]
async fn postgres_obligation_checks_reject_incomplete_variants() {
    obligation_constraint_cases::postgres_obligation_checks_reject_incomplete_variants().await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn trigger_retention_uses_typed_outcomes() {
    use lash_core_execution::{
        TriggerOccurrenceOutcome, TriggerOccurrenceRequest, TriggerStore as _,
    };
    let database = lash_postgres_store::testing::IsolatedDatabase::create(
        &lash_postgres_store::testing::required_database_url(),
    )
    .await;
    let storage = PostgresStorage::connect(database.url())
        .await
        .expect("open isolated PostgreSQL");
    let store = storage.trigger_store();
    let registry = storage.process_registry();
    let durable = storage.durable_store();
    for (key, outcome) in [
        ("fired", TriggerOccurrenceOutcome::Fired),
        (
            "dropped",
            TriggerOccurrenceOutcome::Dropped {
                reason: "audit".into(),
            },
        ),
    ] {
        lash_core::testing::record_trigger_occurrence(
            &store,
            &registry,
            &durable,
            TriggerOccurrenceRequest::new("source", "key", serde_json::json!({}), key)
                .with_outcome(outcome),
        )
        .await
        .expect("record occurrence");
    }
    sqlx::query("UPDATE lash_trigger_occurrences SET record_json = '{broken'")
        .execute(storage.pool())
        .await
        .expect("corrupt presentation bytes");
    let report = store
        .reclaim_trigger_occurrences(u64::MAX)
        .await
        .expect("reclaim must never decode record_json");
    assert_eq!(report.reclaimed_occurrence_count, 1);
    assert_eq!(report.audit_retained_count, 1);
    assert_eq!(
        store
            .prune_non_fired_occurrences(u64::MAX)
            .await
            .expect("prune must never decode record_json"),
        1
    );
    let tombstones: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM lash_trigger_occurrence_tombstones")
            .fetch_one(storage.pool())
            .await
            .expect("tombstone count");
    assert_eq!(tombstones, 2);
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn dropped_trigger_occurrences_cannot_be_reclaimed_or_have_deliveries() {
    use lash_core_execution::{TriggerOccurrenceOutcome, TriggerOccurrenceRequest};
    let database = lash_postgres_store::testing::IsolatedDatabase::create(
        &lash_postgres_store::testing::required_database_url(),
    )
    .await;
    let storage = PostgresStorage::connect(database.url())
        .await
        .expect("open isolated PostgreSQL");
    let record = lash_core::testing::record_trigger_occurrence(
        &storage.trigger_store(),
        &storage.process_registry(),
        &storage.durable_store(),
        TriggerOccurrenceRequest::new("source", "key", serde_json::json!({}), "dropped")
            .with_outcome(TriggerOccurrenceOutcome::Dropped {
                reason: "audit".into(),
            }),
    )
    .await
    .expect("dropped occurrence")
    .occurrence;
    assert!(
        sqlx::query(
            "UPDATE lash_trigger_occurrences SET reclaimable_at_ms = 0 WHERE occurrence_id = $1"
        )
        .bind(&record.occurrence_id)
        .execute(storage.pool())
        .await
        .is_err(),
        "dropped rows cannot arm reclamation"
    );
    assert!(sqlx::query("INSERT INTO lash_trigger_deliveries (occurrence_id, subscription_id, subscription_incarnation, subscription_revision, subscription_snapshot_json, created_at_ms, process_id) VALUES ($1, 'sub', 'incarnation', 1, '{}', 0, 'process')").bind(&record.occurrence_id).execute(storage.pool()).await.is_err(), "dropped rows cannot reserve a delivery");
    assert!(
        sqlx::query("UPDATE lash_trigger_occurrences SET outcome_kind = 'unknown'")
            .execute(storage.pool())
            .await
            .is_err(),
        "outcome vocabulary is closed"
    );
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn turn_cancellation_shape_is_guarded() {
    let url = lash_postgres_store::testing::required_database_url();
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = PostgresStorage::connect(database.url())
        .await
        .expect("open cancellation fixture");
    let mut conn = storage.pool().acquire().await.expect("acquire connection");
    sqlx::query("BEGIN")
        .execute(&mut *conn)
        .await
        .expect("begin");
    sqlx::query("INSERT INTO lash_turn_cancel_requests (session_id, turn_id, request_id, disposition, mode, intent_revision) VALUES ('session', 'turn', 'request', 'defer', 'immediate', 1)").execute(&mut *conn).await.expect("record request");
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
            &mut conn,
            &format!("UPDATE lash_turn_cancel_requests SET {column} = '{value}'"),
            constraint,
        )
        .await;
    }

    sqlx::query("ROLLBACK")
        .execute(&mut *conn)
        .await
        .expect("rollback");
}

#[tokio::test]
async fn reclaim_markers_require_terminal_owners_when_configured() {
    let Some(url) = database_url() else {
        panic!("the reclaim CHECK witness requires hermetic PostgreSQL");
    };
    let _database_lock = SharedDatabaseLock::acquire(&url).await;
    let mut connection = PgConnection::connect(&url)
        .await
        .expect("connect CHECK fixture");
    sqlx::raw_sql("BEGIN; CREATE SCHEMA lash_fig1606_constraints; SET LOCAL search_path TO lash_fig1606_constraints;")
        .execute(&mut connection).await.expect("isolate reclaim fixture");
    sqlx::raw_sql(PostgresStorage::schema_ddl())
        .execute(&mut connection)
        .await
        .expect("apply production DDL");
    for tag in ["enabled", "disabled", "unknown"] {
        sqlx::raw_sql(&format!(
            r#"DELETE FROM lash_trigger_subscription_changes;
            INSERT INTO lash_trigger_subscription_changes VALUES ('subscription', 1, NULL,
                '{{"lifecycle":{{"lifecycle":"{tag}"}}}}')"#
        ))
        .execute(&mut connection)
        .await
        .expect("retain live change");
        assert_check_rejects(
            &mut connection,
            "UPDATE lash_trigger_subscription_changes SET deleted_at_ms = 1",
            "ck_trigger_subscription_changes_reclaimable",
        )
        .await;
    }
    sqlx::raw_sql(
        "DELETE FROM lash_trigger_subscription_changes;
        INSERT INTO lash_trigger_subscription_changes VALUES ('subscription', 1, NULL, '{}')",
    )
    .execute(&mut connection)
    .await
    .expect("retain untagged change");
    assert_check_rejects(
        &mut connection,
        "UPDATE lash_trigger_subscription_changes SET deleted_at_ms = 1",
        "ck_trigger_subscription_changes_reclaimable",
    )
    .await;
    sqlx::raw_sql(
        r#"DELETE FROM lash_trigger_subscription_changes;
        INSERT INTO lash_trigger_subscription_changes VALUES ('subscription', 1, 1,
            '{"lifecycle":{"lifecycle":"tombstoned","deleted_at_ms":1}}')"#,
    )
    .execute(&mut connection)
    .await
    .expect("a tombstoned change may be reclaimed");
    sqlx::query("ROLLBACK")
        .execute(&mut connection)
        .await
        .expect("remove isolated fixture");
}
