//! What the turn-ingress family's PostgreSQL statements and plans are held to.
//!
//! Rendering proofs pin the lifecycle vocabulary and dialect. The planner
//! witness uses a real PostgreSQL with settled history and collected statistics.

use super::turn_ingress_sql;
use lash_core_execution::store_backend_support as vocabulary;

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
             submitted_ingress_json, submission_digest, enqueued_at_ms)
         SELECT n, 'settled-' || n, 'history', '{\"scope\":\"next_turn\"}',
                'completed', '{}', '{}', 'digest', 0
         FROM generate_series(1, 10000) AS n",
    )
    .execute(&mut *connection)
    .await
    .expect("seed settled input history");
    sqlx::query(
        "INSERT INTO lash_pending_turn_inputs
            (enqueue_seq, input_id, session_id, ingress_json, state, input_json,
             submitted_ingress_json, submission_digest, enqueued_at_ms)
         VALUES (10001, 'next', 'history', '{\"scope\":\"next_turn\"}',
                 'deferred_next_turn', '{}', '{}', 'digest', 0),
                (10002, 'active', 'history', '{\"scope\":\"active_turn\",\"turn_id\":\"turn\"}',
                 'pending_active', '{}', '{}', 'digest', 0)",
    )
    .execute(&mut *connection)
    .await
    .expect("seed open inputs");
    sqlx::query(
        "INSERT INTO lash_queued_work_batches
            (enqueue_seq, batch_id, session_id, delivery_policy, work_kind,
             authority_json, enqueued_at_ms, admitted_root, admitted_by)
         SELECT n, 'admitted-' || n, 'history', 'earliest_safe_boundary',
                'turn', '{}', 0, 'root', 'admit'
         FROM generate_series(1, 10000) AS n",
    )
    .execute(&mut *connection)
    .await
    .expect("seed admitted queued work");
    sqlx::query(
        "INSERT INTO lash_queued_work_batches
            (enqueue_seq, batch_id, session_id, delivery_policy, work_kind,
             authority_json, enqueued_at_ms)
         VALUES (10001, 'open', 'history', 'earliest_safe_boundary', 'turn', '{}', 0)",
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
            vec![PlanParam::Text("history")],
        ),
        (
            &sql.family.has_claimable_work,
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

#[test]
fn every_statement_renders_for_this_backend() {
    // Rendering happens once, lazily; touching the set is what makes a
    // malformed neutral statement a test failure here rather than a startup
    // failure in front of a caller.
    let sql = turn_ingress_sql();
    assert!(
        sql.pending_inputs
            .select_by_id
            .sql()
            .contains("lash_pending_turn_inputs")
    );
    assert!(sql.pending_inputs.select_by_id.sql().contains("$1"));
    assert!(!sql.pending_inputs.select_by_id.sql().contains("?1"));
    assert!(
        sql.queued_batches
            .list_by_session
            .sql()
            .contains("lash_queued_work_batches")
    );
}

#[test]
fn a_state_token_renders_to_the_predicate_its_generator_spells() {
    // A `{{term(column)}}` token is only worth having if it renders to exactly
    // what the generator produces: the enum stays the one source of the
    // vocabulary, and the two backends' predicates cannot drift apart.
    let sql = turn_ingress_sql();
    assert!(sql.pending_inputs.delete_withdrawn.sql().contains(
        &vocabulary::cancelled_turn_input_state_predicate_sql("state")
    ),);
    assert!(
        sql.pending_inputs
            .release_root
            .sql()
            .contains(&vocabulary::active_turn_input_state_predicate_sql("state")),
    );
    assert!(sql.pending_inputs.list_undelivered.sql().contains(
        &vocabulary::undelivered_turn_input_state_predicate_sql("state")
    ),);
    assert!(
        sql.pending_inputs_postgres
            .admission_candidates_next_turn
            .sql()
            .contains(&vocabulary::undelivered_turn_input_state_predicate_sql(
                "state"
            )),
    );
}

#[test]
fn a_checkpoint_statement_spells_the_boundary_its_generator_spells() {
    // The minimum-boundary predicate cannot be a vocabulary token: its column
    // is this backend's `jsonb` extraction, not an identifier. So the
    // statements spell it, and this holds the spelling to
    // `admitted_min_boundary_sql` — the one place the boundary enum reaches
    // SQL.
    let sql = turn_ingress_sql();
    let expression = "ingress_json::jsonb ->> 'min_boundary'";
    for (statement, checkpoint) in [
        (
            &sql.pending_inputs_postgres
                .admission_candidates_active_turn_after_work,
            lash_core_execution::CheckpointKind::AfterWork,
        ),
        (
            &sql.pending_inputs_postgres
                .admission_candidates_active_turn_before_completion,
            lash_core_execution::CheckpointKind::BeforeCompletion,
        ),
        (
            &sql.family_postgres.checkpoint_work_pending_after_work,
            lash_core_execution::CheckpointKind::AfterWork,
        ),
        (
            &sql.family_postgres
                .checkpoint_work_pending_before_completion,
            lash_core_execution::CheckpointKind::BeforeCompletion,
        ),
    ] {
        let admitted = vocabulary::admitted_min_boundary_sql(expression, checkpoint);
        let spelled = statement
            .sql()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let admitted = admitted.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            spelled.contains(&admitted),
            "`{}` does not spell `{admitted}`",
            statement.name(),
        );
    }
}
