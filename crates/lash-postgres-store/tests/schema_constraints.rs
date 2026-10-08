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
                     batch_id, session_id, delivery_policy, authority_json,
                     submission_digest, enqueued_at_ms, payload_json, {fields}
                 ) VALUES (1, 'batch', 'session', 'earliest_safe_boundary',
                           '{{}}', 'digest', 0, jsonb_build_object('type', 'session_command')::text, {values})"
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
             batch_id, session_id, delivery_policy, authority_json,
             submission_digest, enqueued_at_ms, payload_json
         ) VALUES (1, 'bad-policy', 'session', 'eventually', '{}', 'digest', 0, jsonb_build_object('type', 'session_command')::text)",
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
async fn turn_cancellation_shape_is_guarded() {
    let url = lash_postgres_store::testing::required_database_url();
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::testing::connect(database.url())
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

// FIG-5282: independent cells must not force cluster-wide checkpoints.
#[tokio::test]
async fn isolated_cells_do_not_checkpoint_or_share_state() {
    use lash_postgres_store::testing::{IsolatedDatabase, connect, required_database_url};
    let url = required_database_url();
    let mut admin = PgConnection::connect(&url).await.expect("connect admin");
    let before: i64 = sqlx::query_scalar("SELECT num_requested FROM pg_stat_checkpointer")
        .fetch_one(&mut admin)
        .await
        .expect("read requested checkpoints");
    let (first, second) = tokio::join!(
        IsolatedDatabase::create(&url),
        IsolatedDatabase::create(&url)
    );
    let first_oid: i64 =
        sqlx::query_scalar("SELECT oid::bigint FROM pg_database WHERE datname = $1")
            .bind(first.database_name())
            .fetch_one(&mut admin)
            .await
            .expect("read first shell identity");
    let retired_url = first.url().to_owned();
    let first_storage = connect(first.url())
        .await
        .expect("verify first cell catalog");
    let second_storage = connect(second.url())
        .await
        .expect("verify second cell catalog");
    let first_id: String = sqlx::query_scalar("SELECT catalog_id FROM lash_catalog_identity")
        .fetch_one(first_storage.pool())
        .await
        .expect("first catalog identity");
    let second_id: String = sqlx::query_scalar("SELECT catalog_id FROM lash_catalog_identity")
        .fetch_one(second_storage.pool())
        .await
        .expect("second catalog identity");
    assert_ne!(first_id, second_id, "each cell owns an independent catalog");
    sqlx::query("INSERT INTO lash_blobs (hash, content) VALUES ('cell', 'first'::bytea)")
        .execute(first_storage.pool())
        .await
        .expect("write first cell");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM lash_blobs")
        .fetch_one(second_storage.pool())
        .await
        .expect("read second cell");
    assert_eq!(count, 0, "a concurrent cell sees no other cell's rows");
    // Checkpointer statistics are published asynchronously; no snapshot is
    // retained while the two cells are made.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    sqlx::query("SELECT pg_stat_clear_snapshot()")
        .execute(&mut admin)
        .await
        .expect("refresh checkpoint statistics");
    let after: i64 = sqlx::query_scalar("SELECT num_requested FROM pg_stat_checkpointer")
        .fetch_one(&mut admin)
        .await
        .expect("read requested checkpoints");
    assert_eq!(
        after, before,
        "cell setup must not request a cluster checkpoint"
    );
    sqlx::query("DROP INDEX idx_lash_process_events_key")
        .execute(first_storage.pool())
        .await
        .expect("damage only the first cell's catalog");
    assert!(
        connect(first.url()).await.is_err(),
        "every open still verifies the catalog"
    );
    sqlx::raw_sql("CREATE SCHEMA cell_scratch; CREATE TABLE cell_scratch.residue (value int)")
        .execute(first_storage.pool())
        .await
        .expect("a law may also leave a scratch schema");
    drop(first_storage);
    drop(first);
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    sqlx::query("SELECT pg_stat_clear_snapshot()")
        .execute(&mut admin)
        .await
        .expect("refresh teardown checkpoint statistics");
    let after_teardown: i64 = sqlx::query_scalar("SELECT num_requested FROM pg_stat_checkpointer")
        .fetch_one(&mut admin)
        .await
        .expect("read teardown checkpoints");
    assert_eq!(
        after_teardown, after,
        "cell teardown must not checkpoint a live companion cell"
    );
    let third = IsolatedDatabase::create(&url).await;
    let third_oid: i64 =
        sqlx::query_scalar("SELECT oid::bigint FROM pg_database WHERE datname = $1")
            .bind(third.database_name())
            .fetch_one(&mut admin)
            .await
            .expect("read later shell identity");
    assert_eq!(
        third_oid, first_oid,
        "retired shells bound disk use across cuts"
    );
    assert!(
        PgConnection::connect(&retired_url).await.is_err(),
        "a retired URL cannot enter the later cell"
    );
    let third_storage = connect(third.url())
        .await
        .expect("verify later cell catalog");
    let third_id: String = sqlx::query_scalar("SELECT catalog_id FROM lash_catalog_identity")
        .fetch_one(third_storage.pool())
        .await
        .expect("later catalog identity");
    assert_ne!(third_id, first_id);
    assert_ne!(third_id, second_id);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM lash_blobs")
        .fetch_one(third_storage.pool())
        .await
        .expect("read later cell");
    assert_eq!(count, 0, "a later cell sees no previous cell's rows");
    let scratch: i64 =
        sqlx::query_scalar("SELECT count(*) FROM pg_namespace WHERE nspname = 'cell_scratch'")
            .fetch_one(third_storage.pool())
            .await
            .expect("check the later cell's scratch schemas");
    assert_eq!(
        scratch, 0,
        "a later cell sees no previous cell's scratch schema"
    );
    drop((second_storage, third_storage));
    drop((second, third));
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    sqlx::query("SELECT pg_stat_clear_snapshot()")
        .execute(&mut admin)
        .await
        .expect("refresh recycled-cell checkpoint statistics");
    let final_checkpoints: i64 =
        sqlx::query_scalar("SELECT num_requested FROM pg_stat_checkpointer")
            .fetch_one(&mut admin)
            .await
            .expect("read recycled-cell checkpoints");
    assert_eq!(
        final_checkpoints, before,
        "the entire cell lifetime must not force a checkpoint"
    );
}
