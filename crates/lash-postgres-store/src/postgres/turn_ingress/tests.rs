//! What the turn-ingress family's PostgreSQL statements and plans are held to.
//!
//! Rendering proofs pin the lifecycle vocabulary and dialect. The planner
//! witness uses a real PostgreSQL with settled history and collected statistics.

use super::turn_ingress_sql;

enum PlanParam {
    Text(&'static str),
    Number(i64),
}

async fn explain(connection: &mut sqlx::PgConnection, sql: &str, params: &[PlanParam]) -> String {
    let explain = format!("EXPLAIN (FORMAT TEXT) {sql}");
    let mut query = sqlx::query_scalar::<_, String>(&explain);
    for param in params {
        query = match param {
            PlanParam::Text(value) => query.bind(*value),
            PlanParam::Number(value) => query.bind(*value),
        };
    }
    query
        .fetch_all(connection)
        .await
        .expect("explain the rendered store statement")
        .join("\n")
}

#[tokio::test]
async fn open_ingress_reads_seek_state_indexes_with_settled_history() {
    let Some(url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping ingress plan witness: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let database = crate::testing::IsolatedDatabase::create(&url).await;
    let storage = crate::PostgresStorage::connect(database.url())
        .await
        .expect("connect ingress plan database");
    let mut connection = storage
        .pool()
        .acquire()
        .await
        .expect("acquire plan connection");
    sqlx::query(
        "INSERT INTO lash_pending_turn_inputs
            (enqueue_seq, input_id, session_id, ingress_json, state, input_json,
             submission_digest, enqueued_at_ms, terminal_at_ms)
         SELECT n, 'settled-' || n, 'history', '{\"scope\":\"next_turn\"}',
                'completed', '{}', 'digest', 0, 0
         FROM generate_series(1, 10000) AS n",
    )
    .execute(&mut *connection)
    .await
    .expect("seed settled input history");
    sqlx::query(
        "INSERT INTO lash_pending_turn_inputs
            (enqueue_seq, input_id, session_id, ingress_json, state, input_json,
             submission_digest, enqueued_at_ms)
         VALUES (10001, 'next', 'history', '{\"scope\":\"next_turn\"}',
                 'deferred_next_turn', '{}', 'digest', 0),
                (10002, 'active', 'history', '{\"scope\":\"active_turn\",\"turn_id\":\"turn\"}',
                 'pending_active', '{}', 'digest', 0)",
    )
    .execute(&mut *connection)
    .await
    .expect("seed open inputs");
    sqlx::query(
        "INSERT INTO lash_pending_turn_inputs
            (enqueue_seq, input_id, session_id, ingress_json, state, input_json,
             submission_digest, enqueued_at_ms,
             admitted_run, admitted_by)
         VALUES (10003, 'accepted', 'history', '{\"scope\":\"active_turn\",\"turn_id\":\"turn\"}',
                 'accepted', '{}', 'digest', 0, 'root', 'checkpoint')",
    )
    .execute(&mut *connection)
    .await
    .expect("seed checkpoint-accepted input");
    sqlx::query(
        "INSERT INTO lash_queued_work_batches
            (enqueue_seq, batch_id, session_id, delivery_policy, work_kind,
             authority_json, submission_digest, enqueued_at_ms, payload_json, admitted_run, admitted_by)
         SELECT n, 'admitted-' || n, 'history', 'earliest_safe_boundary',
                'turn', '{}', 'digest', 0, jsonb_build_object('type', 'process_wake')::text, 'root', 'admit'
         FROM generate_series(1, 10000) AS n",
    )
    .execute(&mut *connection)
    .await
    .expect("seed admitted queued work");
    sqlx::query(
        "INSERT INTO lash_queued_work_batches
            (enqueue_seq, batch_id, session_id, delivery_policy, work_kind,
             authority_json, submission_digest, enqueued_at_ms, payload_json)
         VALUES (10001, 'open', 'history', 'earliest_safe_boundary', 'turn', '{}', 'digest', 0, jsonb_build_object('type', 'process_wake')::text)",
    )
    .execute(&mut *connection)
    .await
    .expect("seed open queued work");
    for table in ["lash_pending_turn_inputs", "lash_queued_work_batches"] {
        sqlx::query(&format!("ANALYZE {table}"))
            .execute(&mut *connection)
            .await
            .expect("collect plan statistics");
    }

    let sql = turn_ingress_sql();
    let input_reads = [
        (
            &sql.pending_inputs.list_undelivered,
            vec![PlanParam::Text("history")],
        ),
        (
            &sql.pending_inputs.earliest_next_turn_candidate_seq,
            vec![PlanParam::Text("history"), PlanParam::Text("turn")],
        ),
        (
            &sql.family.has_admissible_work,
            vec![PlanParam::Text("history")],
        ),
        (
            &sql.family.pending_session_work_ordering,
            vec![PlanParam::Text("history"), PlanParam::Text("control")],
        ),
        (
            &sql.pending_inputs_postgres.select_pending_active,
            vec![PlanParam::Text("history")],
        ),
        (
            &sql.pending_inputs_postgres.admission_candidates_next_turn,
            vec![PlanParam::Text("history"), PlanParam::Number(16)],
        ),
        (
            &sql.pending_inputs_postgres
                .admission_candidates_active_turn_after_work,
            vec![
                PlanParam::Text("history"),
                PlanParam::Number(16),
                PlanParam::Text("turn"),
            ],
        ),
        (
            &sql.family_postgres.checkpoint_work_pending_after_work,
            vec![
                PlanParam::Text("history"),
                PlanParam::Text("turn"),
                PlanParam::Number(16),
                PlanParam::Number(16),
                PlanParam::Text("root"),
                PlanParam::Text("admit"),
            ],
        ),
        (
            &sql.family_postgres
                .checkpoint_work_pending_before_completion,
            vec![
                PlanParam::Text("history"),
                PlanParam::Text("turn"),
                PlanParam::Number(16),
                PlanParam::Number(16),
                PlanParam::Text("root"),
                PlanParam::Text("admit"),
            ],
        ),
        (
            &sql.pending_inputs_postgres
                .admission_candidates_active_turn_before_completion,
            vec![
                PlanParam::Text("history"),
                PlanParam::Number(16),
                PlanParam::Text("turn"),
            ],
        ),
    ];
    for (statement, params) in input_reads {
        let plan = explain(&mut connection, statement.sql(), &params).await;
        assert!(
            plan.contains("idx_lash_pending_turn_inputs_open_state"),
            "`{}` reads settled input history:\n{plan}",
            statement.name(),
        );
    }
    let accepted_plan = explain(
        &mut connection,
        sql.pending_inputs.list_accepted.sql(),
        &[PlanParam::Text("history")],
    )
    .await;
    assert!(
        accepted_plan.contains("idx_lash_pending_turn_inputs_accepted_state"),
        "`list_accepted` reads settled input history:\n{accepted_plan}",
    );
    let queued_reads = [
        (
            &sql.queued_batches.list_open,
            vec![PlanParam::Text("history")],
        ),
        (
            &sql.queued_batches_postgres.admission_candidates_idle,
            vec![PlanParam::Text("history"), PlanParam::Number(16)],
        ),
        (
            &sql.queued_batches_postgres.admission_candidates_turn_lane,
            vec![PlanParam::Text("history"), PlanParam::Number(16)],
        ),
        (
            &sql.queued_batches_postgres.admission_candidates_boundary,
            vec![PlanParam::Text("history"), PlanParam::Number(16)],
        ),
    ];
    for (statement, params) in queued_reads {
        let plan = explain(&mut connection, statement.sql(), &params).await;
        assert!(
            plan.contains("idx_lash_queued_work_admission_order"),
            "`{}` reads admitted queued work:\n{plan}",
            statement.name(),
        );
    }
}
