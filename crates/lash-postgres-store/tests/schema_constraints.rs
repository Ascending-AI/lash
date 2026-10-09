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
        "INSERT INTO lash_session_meta (session_id, relation_kind, retention_kind)
         VALUES ('bad-relation', 'sibling', 'until_gc')",
        "ck_session_meta_relation_kind",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_session_meta (session_id, relation_kind, retention_kind, parent_session_id,
                                        caused_by_kind)
         VALUES ('bad-cause', 'child', 'until_gc', 'parent', 'timer')",
        "ck_session_meta_caused_by_kind",
    )
    .await;
    sqlx::query(
        "INSERT INTO lash_session_meta (session_id, relation_kind, retention_kind, parent_session_id,
                                        caused_by_kind, caused_by_effect_id)
         VALUES ('effect-address-cause', 'child', 'until_gc', 'parent', 'effect_address', '{}')",
    )
    .execute(&mut connection)
    .await
    .expect("current effect-address discriminator is admitted");
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_session_meta (session_id, relation_kind, retention_kind, parent_session_id,
                                        caused_by_kind)
         VALUES ('legacy-effect-cause', 'child', 'until_gc', 'parent', 'effect')",
        "ck_session_meta_caused_by_kind",
    )
    .await;

    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_session_meta (session_id, relation_kind, retention_kind)
         VALUES ('childless-child', 'child', 'until_gc')",
        "ck_session_meta_relation_family",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_session_meta (session_id, relation_kind, retention_kind, caused_by_kind,
                                        caused_by_session_id, caused_by_turn_id)
         VALUES ('caused-run', 'root', 'until_gc', 'turn', 'cause-session', 'cause-turn')",
        "ck_session_meta_relation_family",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_session_meta (session_id, relation_kind, retention_kind, parent_session_id,
                                        caused_by_kind)
         VALUES ('bare-discriminator', 'child', 'until_gc', 'parent', 'turn')",
        "ck_session_meta_caused_by_family",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_session_meta (session_id, relation_kind, retention_kind, parent_session_id,
                                        caused_by_kind, caused_by_session_id,
                                        caused_by_turn_id, caused_by_node_id)
         VALUES ('crossed-family', 'child', 'until_gc', 'parent', 'turn', 'cause-session',
                 'cause-turn', 'stray-node')",
        "ck_session_meta_caused_by_family",
    )
    .await;
    assert_check_rejects(
        &mut connection,
        "INSERT INTO lash_session_meta (session_id, relation_kind, retention_kind, parent_session_id,
                                        caused_by_session_id)
         VALUES ('kindless-payload', 'child', 'until_gc', 'parent', 'cause-session')",
        "ck_session_meta_caused_by_family",
    )
    .await;

    // A process's status is its record's alone:
    // `a_process_row_takes_its_lifecycle_columns_from_its_record_alone`.
    let process_columns = PROCESS_FIXTURE_COLUMNS;
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
                 ('{process_id}', 'originator', 'standard', 0, 0, 0,
                  {scope_kind}, {scope_id}, {lifetime}, '{{\"last_event_sequence\":0,\"lifecycle\":{{\"state\":\"running\"}}}}')"
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
         ('wake-parent', 'originator', 'standard', 0, 0, 0,
          NULL, NULL, 'detached', '{{\"last_event_sequence\":0,\"lifecycle\":{{\"state\":\"running\"}}}}')"
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

/// The columns a statement may write on a process row.
const PROCESS_FIXTURE_COLUMNS: &str = "process_id, originator_id,
        identity_kind, created_at_ms, updated_at_ms, change_seq,
        lifetime_scope_kind, lifetime_scope_id, lifetime, record_json";

/// A process record's JSON, as far as its row's columns read it: the event
/// sequence, the lifecycle and the cancel request, each in its own codec.
fn process_record_json(
    status: lash_core_execution::ProcessStatus,
    last_event_sequence: u64,
    cancel_requested_at_ms: Option<u64>,
) -> String {
    let mut record = serde_json::json!({
        "last_event_sequence": last_event_sequence,
        "lifecycle": lash_core_execution::ProcessLifecycleState::fixture(status),
    });
    if let Some(at) = cancel_requested_at_ms {
        record["cancel_request"] = serde_json::json!(lash_core_execution::CancelRequest::new(
            lash_core_execution::CancelOrigin::OperatorRequested,
            "operator",
            at,
        ));
    }
    record.to_string()
}

/// The SQLSTATE `statement`, bound to `binds`, is refused with, if it is
/// refused; the transaction carries on either way.
async fn refused(connection: &mut PgConnection, statement: &str, binds: &[&str]) -> Option<String> {
    sqlx::query("SAVEPOINT projection")
        .execute(&mut *connection)
        .await
        .expect("create projection savepoint");
    let mut query = sqlx::query(statement);
    for bind in binds {
        query = query.bind(*bind);
    }
    let error = query.execute(&mut *connection).await.err();
    sqlx::query("ROLLBACK TO SAVEPOINT projection")
        .execute(&mut *connection)
        .await
        .expect("return to the projection savepoint");
    error.map(|error| {
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::code)
            .map_or_else(|| error.to_string(), |code| code.into_owned())
    })
}

/// F1 (FIG-5557): `status`, `last_event_sequence` and
/// `cancel_requested_at_ms` are the database's projections of `record_json`.
/// No statement can write one, so a row whose columns disagree with its
/// record cannot be written; a write of the record moves all three; and a
/// record whose lifecycle names no status, or that lacks its sequence or
/// its cancel's time, is refused. A record whose strings hold a NUL is
/// still written, byte for byte.
#[tokio::test]
async fn a_process_row_takes_its_lifecycle_columns_from_its_record_alone() {
    use lash_core_execution::ProcessStatus;
    use sqlx::Row;

    const SCHEMA: &str = "lash_fig5557_projection";
    let Some(url) = database_url() else {
        eprintln!("skipping the process projection law: database URL is not set");
        return;
    };
    let _database_lock = SharedDatabaseLock::acquire(&url).await;
    let mut connection = PgConnection::connect(&url)
        .await
        .expect("connect the projection fixture");
    sqlx::raw_sql(&format!(
        "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE;
         CREATE SCHEMA {SCHEMA};
         SET search_path TO {SCHEMA};"
    ))
    .execute(&mut connection)
    .await
    .expect("create the projection fixture schema");
    sqlx::raw_sql(PostgresStorage::schema_ddl())
        .execute(&mut connection)
        .await
        .expect("apply the schema to the projection fixture");
    sqlx::query("BEGIN")
        .execute(&mut connection)
        .await
        .expect("begin the projection transaction");

    let insert = format!(
        "INSERT INTO lash_processes ({PROCESS_FIXTURE_COLUMNS}) VALUES
         ($1, 'originator', 'standard', 0, 0, 0, NULL, NULL, 'detached', $2)"
    );
    let save = "UPDATE lash_processes SET record_json = $2 WHERE process_id = $1";
    async fn columns(connection: &mut PgConnection, id: &str) -> (String, i64, Option<i64>) {
        let row = sqlx::query(
            "SELECT status, last_event_sequence, cancel_requested_at_ms
               FROM lash_processes WHERE process_id = $1",
        )
        .bind(id)
        .fetch_one(&mut *connection)
        .await
        .expect("read the projected columns");
        (row.get(0), row.get(1), row.get(2))
    }

    // Every status the record's lifecycle can be is the row's status.
    let statuses = [
        ProcessStatus::Running,
        ProcessStatus::Waiting,
        ProcessStatus::Completed,
        ProcessStatus::Failed,
        ProcessStatus::Cancelled,
        ProcessStatus::Abandoned,
    ];
    for (sequence, status) in (0_u64..).zip(statuses) {
        let id = format!("is-{}", status.label());
        let record = process_record_json(status, sequence, None);
        sqlx::query(&insert)
            .bind(&id)
            .bind(&record)
            .execute(&mut connection)
            .await
            .expect("insert the record");
        assert_eq!(
            columns(&mut connection, &id).await,
            (status.label().to_owned(), sequence as i64, None),
            "{id}"
        );
    }

    // A write of the record alone moves every projection with it.
    sqlx::query(&insert)
        .bind("moves")
        .bind(process_record_json(ProcessStatus::Running, 1, None))
        .execute(&mut connection)
        .await
        .expect("insert a running record");
    sqlx::query(save)
        .bind("moves")
        .bind(process_record_json(ProcessStatus::Cancelled, 7, Some(42)))
        .execute(&mut connection)
        .await
        .expect("save the cancelled record");
    let moved = ("cancelled".to_owned(), 7, Some(42));
    assert_eq!(columns(&mut connection, "moves").await, moved);

    // No statement writes a projection, with the record or without it.
    let running = process_record_json(ProcessStatus::Running, 0, None);
    for column in ["status", "last_event_sequence", "cancel_requested_at_ms"] {
        let value = if column == "status" {
            "'completed'"
        } else {
            "9"
        };
        for statement in [
            format!(
                "INSERT INTO lash_processes ({PROCESS_FIXTURE_COLUMNS}, {column}) VALUES
                 ('disagrees', 'originator', 'standard', 0, 0, 0, NULL, NULL, 'detached',
                  $1, {value})"
            ),
            format!("UPDATE lash_processes SET {column} = {value} WHERE $1 <> ''"),
        ] {
            // 428C9: a value was given for a generated column.
            assert_eq!(
                refused(&mut connection, &statement, &[&running]).await,
                Some("428C9".to_owned()),
                "{column} was written by: {statement}"
            );
        }
    }
    assert_eq!(columns(&mut connection, "moves").await, moved);

    // A record that names no status, or lacks what a column reads, is
    // refused whole.
    for (what, record) in [
        ("no lifecycle", "{\"last_event_sequence\":0}".to_owned()),
        (
            "an unknown state",
            running.replace("\"running\"", "\"paused\""),
        ),
        (
            "an unknown outcome",
            process_record_json(ProcessStatus::Abandoned, 0, None)
                .replace("\"abandoned\"", "\"vanished\""),
        ),
        (
            "an unknown settlement",
            process_record_json(ProcessStatus::Completed, 0, None)
                .replace("\"success\"", "\"shrugged\""),
        ),
        (
            "no event sequence",
            running.replace("\"last_event_sequence\"", "\"sequence\""),
        ),
        (
            "a cancel without its time",
            process_record_json(ProcessStatus::Running, 0, Some(42))
                .replace("\"requested_at_ms\"", "\"at\""),
        ),
        ("text that is not a record", "not json".to_owned()),
    ] {
        assert!(
            refused(&mut connection, &insert, &["refused", &record])
                .await
                .is_some(),
            "a record with {what} was written: {record}"
        );
        assert!(
            refused(&mut connection, save, &["moves", &record])
                .await
                .is_some(),
            "a record with {what} was saved: {record}"
        );
    }
    assert_eq!(columns(&mut connection, "moves").await, moved);

    // A record whose strings hold a NUL character, or the text of its
    // escape, is written and projected like any other: PostgreSQL's JSON
    // functions refuse the escape, so the projections read around it.
    let mut with_nul: serde_json::Value =
        serde_json::from_str(&running).expect("decode the fixture record");
    with_nul["input"] = serde_json::json!("a\u{0}b\\u0000");
    let with_nul = with_nul.to_string();
    sqlx::query(&insert)
        .bind("holds-a-nul")
        .bind(&with_nul)
        .execute(&mut connection)
        .await
        .expect("a record holding an escaped NUL is written");
    assert_eq!(
        columns(&mut connection, "holds-a-nul").await,
        ("running".to_owned(), 0, None)
    );
    let stored: String =
        sqlx::query_scalar("SELECT record_json FROM lash_processes WHERE process_id = $1")
            .bind("holds-a-nul")
            .fetch_one(&mut connection)
            .await
            .expect("read the record back");
    assert_eq!(stored, with_nul, "the record is stored as it was written");

    sqlx::query("ROLLBACK")
        .execute(&mut connection)
        .await
        .expect("end the projection transaction");
    sqlx::raw_sql(&format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE"))
        .execute(&mut connection)
        .await
        .expect("drop the projection fixture schema");
}

#[tokio::test]
async fn postgres_obligation_checks_reject_incomplete_variants() {
    obligation_constraint_cases::postgres_obligation_checks_reject_incomplete_variants().await;
}

#[tokio::test]
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
