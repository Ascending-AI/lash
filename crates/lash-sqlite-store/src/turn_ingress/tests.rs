//! What the turn-ingress family's SQLite statements are held to.
//!
//! The plan tests run `EXPLAIN QUERY PLAN` against the real
//! schema and assert each named statement still seeks the index it was written
//! for: the statements that replaced a `format!` used to splice their filter in
//! per call, and the whole reason each filter shape has its own name is that a
//! spliced optional predicate cannot seek.

use rusqlite::Connection;

use super::{tool_intent_sql, turn_ingress_sql};

/// A catalog with this crate's real durable-core schema, in memory: the
/// session schema and the fragments the durable core database carries beside
/// it (the retention delete reads the run family's input bindings).
fn catalog() -> Connection {
    let conn = Connection::open_in_memory().expect("in-memory database opens");
    conn.execute_batch(crate::schema::SCHEMA)
        .expect("session schema applies");
    for fragment in [
        crate::schema_fragments::SESSION_INGRESS_TABLE,
        crate::schema_fragments::SESSION_RUNS_TABLES,
    ] {
        conn.execute_batch(fragment)
            .expect("durable-core fragment applies");
    }
    conn
}

/// The plan `sql` produces, as one line per step.
///
/// `EXPLAIN QUERY PLAN` is the only way to see whether a predicate seeks: a
/// statement that scans and a statement that seeks return the same rows, and
/// only the plan tells them apart.
fn plan(conn: &Connection, sql: &str) -> String {
    let mut statement = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap_or_else(|error| panic!("statement prepares: {error}\n{sql}"));
    // A plan is produced from the statement's shape, not its values, but SQLite
    // still wants every parameter bound before it will produce one.
    let bindings = (1..=statement.parameter_count())
        .map(|_| rusqlite::types::Value::Null)
        .collect::<Vec<_>>();
    let rows = statement
        .query_map(rusqlite::params_from_iter(bindings.iter()), |row| {
            row.get::<_, String>(3)
        })
        .expect("plan rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("plan rows decode");
    rows.join("\n")
}

fn assert_uses(conn: &Connection, statement: &lash_store_sql::Rendered, index: &str) {
    let plan = plan(conn, statement.sql());
    assert!(
        plan.contains(index),
        "`{}` no longer uses `{index}`:\n{plan}",
        statement.name(),
    );
}

#[test]
fn every_turn_ingress_statement_prepares_against_the_real_schema() {
    // Rendering proves the neutral text is well-formed; only the database
    // proves it is SQL over the columns this schema has. A statement that
    // names a dropped column renders happily and fails here.
    let conn = catalog();
    let sql = turn_ingress_sql();
    for statement in [
        sql.family_sqlite.input_admission_identities.sql(),
        sql.family.has_admissible_work.sql(),
        sql.family.pending_session_work_ordering.sql(),
        sql.family.turn_address_ended.sql(),
        sql.family.run_ended.sql(),
        sql.family_sqlite.checkpoint_work_pending_after_work.sql(),
        sql.family_sqlite
            .checkpoint_work_pending_before_completion
            .sql(),
        sql.pending_inputs.select_by_id.sql(),
        sql.pending_inputs.select_by_source_key.sql(),
        sql.pending_inputs.list_undelivered.sql(),
        sql.pending_inputs.list_accepted.sql(),
        sql.pending_inputs.cancel.sql(),
        sql.pending_inputs.earliest_next_turn_candidate_seq.sql(),
        sql.pending_inputs.select_admitted_by_step.sql(),
        sql.pending_inputs.admit.sql(),
        sql.pending_inputs.settle_admitted.sql(),
        sql.pending_inputs.release_admitted.sql(),
        sql.pending_inputs.release_run.sql(),
        sql.pending_inputs.delete_withdrawn.sql(),
        sql.pending_inputs.delete_by_session.sql(),
        sql.pending_inputs.insert_new.sql(),
        sql.pending_inputs.select_id_by_source_key.sql(),
        sql.pending_inputs_sqlite.settlement_facts.sql(),
        sql.pending_inputs_sqlite.select_suffix.sql(),
        sql.pending_inputs_sqlite.select_pending_active.sql(),
        sql.pending_inputs_sqlite
            .admission_candidates_next_turn
            .sql(),
        sql.pending_inputs_sqlite
            .admission_candidates_active_turn_after_work
            .sql(),
        sql.pending_inputs_sqlite
            .admission_candidates_active_turn_before_completion
            .sql(),
        sql.queued_batches.select_by_id.sql(),
        sql.queued_batches.select_id_by_source_key.sql(),
        sql.queued_batches.list_by_session.sql(),
        sql.queued_batches.list_open.sql(),
        sql.queued_batches.select_admitted_by_step.sql(),
        sql.queued_batches.admit.sql(),
        sql.queued_batches.select_admitted_batch_payload.sql(),
        sql.queued_batches.settle_admitted.sql(),
        sql.queued_batches.settle_command.sql(),
        sql.queued_batches.withdraw_open.sql(),
        sql.queued_batches.delete_tombstones.sql(),
        sql.queued_batches.release_admitted.sql(),
        sql.queued_batches.release_run.sql(),
        sql.queued_batches.delete_by_session.sql(),
        sql.queued_batches_sqlite.insert_new.sql(),
        sql.queued_batches_sqlite.settlement_facts.sql(),
        sql.queued_batches_sqlite.select_cancelable.sql(),
        sql.queued_batches_sqlite.admission_candidates_idle.sql(),
        sql.cancel_requests.delete_by_session.sql(),
        sql.cancel_requests.select_request.sql(),
    ] {
        conn.prepare(statement)
            .unwrap_or_else(|error| panic!("statement prepares: {error}\n{statement}"));
    }
}

#[test]
fn every_tool_intent_statement_prepares_against_the_process_schema() {
    // The tool-intent ledger is the process registry's, rendered unqualified;
    // its payload follows the session's retained revisions and run
    // terminals (FIG-6612), so it is proved against the database that holds
    // both.
    let conn = catalog();
    conn.execute_batch(crate::schema::PROCESS_SCHEMA)
        .expect("process schema applies");
    let sql = tool_intent_sql();
    for statement in [
        sql.shared.select_by_replay_key.sql(),
        sql.shared.settle.sql(),
        sql.shared.release_settled_payloads.sql(),
        sql.sqlite.insert_new.sql(),
    ] {
        conn.prepare(statement)
            .unwrap_or_else(|error| panic!("statement prepares: {error}\n{statement}"));
    }
}

#[test]
fn an_admission_candidate_scan_seeks_the_open_row_index() {
    // Each named shape replaced one `format!` that spliced the mode's filter
    // in per call, and each has to seek the partial index over open rows
    // (FIG-3927): the settled rows a session keeps forever are outside it, so
    // an admission's cost does not grow with the session's history.
    let conn = catalog();
    let statements = &turn_ingress_sql().pending_inputs_sqlite;
    for statement in [
        &statements.select_pending_active,
        &statements.admission_candidates_next_turn,
        &statements.admission_candidates_active_turn_after_work,
        &statements.admission_candidates_active_turn_before_completion,
    ] {
        assert_uses(&conn, statement, "idx_pending_turn_inputs_open_state");
    }
}

#[test]
fn every_open_input_read_seeks_the_state_index() {
    let conn = catalog();
    let sql = turn_ingress_sql();
    for statement in [
        &sql.pending_inputs.list_undelivered,
        &sql.pending_inputs.earliest_next_turn_candidate_seq,
        &sql.family.has_admissible_work,
        &sql.family.pending_session_work_ordering,
        &sql.family_sqlite.checkpoint_work_pending_after_work,
        &sql.family_sqlite.checkpoint_work_pending_before_completion,
    ] {
        assert_uses(&conn, statement, "idx_pending_turn_inputs_open_state");
    }
}

#[test]
fn accepted_input_read_seeks_its_state_index() {
    let conn = catalog();
    assert_uses(
        &conn,
        &turn_ingress_sql().pending_inputs.list_accepted,
        "idx_pending_turn_inputs_accepted_state",
    );
}

#[test]
fn every_open_queued_work_read_seeks_the_admission_index() {
    let conn = catalog();
    let sql = turn_ingress_sql();
    for statement in [
        &sql.queued_batches.list_open,
        &sql.queued_batches_sqlite.admission_candidates_idle,
    ] {
        assert_uses(&conn, statement, "idx_queued_work_admission_order");
    }
}

#[test]
fn reopening_replaces_obsolete_ingress_indexes() {
    let conn = catalog();
    conn.execute_batch(
        "CREATE INDEX idx_queued_work_admitted
             ON queued_work_batches(session_id, admitted_run);
         CREATE INDEX idx_pending_turn_inputs_session
             ON pending_turn_inputs(session_id, state, enqueue_seq);
         CREATE INDEX idx_pending_turn_input_order
             ON pending_turn_inputs(session_id, state, enqueued_at_ms, enqueue_seq);
         CREATE INDEX idx_pending_turn_inputs_open
             ON pending_turn_inputs(session_id, state, enqueue_seq)
             WHERE admitted_run IS NULL
               AND state IN ('pending_active', 'deferred_next_turn');
         CREATE INDEX idx_pending_turn_inputs_admitted
             ON pending_turn_inputs(session_id, admitted_run);",
    )
    .expect("install the previous index set");
    conn.execute_batch(crate::schema::SCHEMA)
        .expect("reopen applies the current index set");
    let mut names = conn
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'index'
               AND tbl_name IN ('queued_work_batches', 'pending_turn_inputs')",
        )
        .expect("inspect ingress indexes");
    let names = names
        .query_map([], |row| row.get::<_, String>(0))
        .expect("list ingress indexes")
        .collect::<Result<Vec<_>, _>>()
        .expect("decode ingress indexes");
    for old in [
        "idx_queued_work_admitted",
        "idx_pending_turn_inputs_session",
        "idx_pending_turn_input_order",
        "idx_pending_turn_inputs_open",
        "idx_pending_turn_inputs_admitted",
    ] {
        assert!(
            !names.iter().any(|name| name == old),
            "old index remains: {old}"
        );
    }
    for current in [
        "idx_queued_work_admission_order",
        "idx_pending_turn_inputs_open_state",
        "idx_pending_turn_inputs_bound_run",
    ] {
        assert!(
            names.iter().any(|name| name == current),
            "current index is missing: {current}"
        );
    }
}

#[test]
fn a_run_release_seeks_the_admission_index() {
    // A run's terminal write releases every row it still holds; the
    // `(session_id, admitted_run)` index over nonterminal bindings keeps
    // that one seek per table and leaves settled history outside the seek.
    let conn = catalog();
    let sql = turn_ingress_sql();
    assert_uses(
        &conn,
        &sql.pending_inputs.release_run,
        "idx_pending_turn_inputs_open_bound_run",
    );
    assert_uses(
        &conn,
        &sql.queued_batches.release_run,
        "idx_queued_work_admission_order",
    );
}
/// A settled input retains its run for observation, but never keeps that run
/// active or lengthens the session actor's next bound-run probe (FIG-6909).
#[test]
fn a_bound_run_probe_skips_settled_history_without_changing_bindings() {
    use lash_core_store::turn_input_vocabulary::TurnInputStateKind;
    use rusqlite::{OptionalExtension as _, StatementStatus};

    let conn = catalog();
    conn.execute_batch(
        "WITH RECURSIVE history(n) AS (
             VALUES(1) UNION ALL SELECT n + 1 FROM history WHERE n < 1024
         )
         INSERT INTO pending_turn_inputs
             (origin_json, enqueue_seq, input_id, session_id, ingress_json, state, input_json,
              submission_digest, enqueued_at_ms, admitted_run, terminal_at_ms)
         SELECT '\"submitted\"', n, 'settled-' || n, 'history', '{\"scope\":\"next_turn\"}',
                CASE WHEN n % 2 = 0 THEN 'completed' ELSE 'cancelled' END,
                '{}', 'digest', 0, 'past-' || n, 0 FROM history",
    )
    .expect("seed settled history retaining its run bindings");
    let bound = crate::durable::session_mail::bound_run_query();
    let mut probe = conn.prepare(&bound).unwrap();
    for &state in TurnInputStateKind::ALL {
        for bound in [false, true] {
            let ingress = if state == TurnInputStateKind::DeferredNextTurn || state.is_terminal() {
                r#"{"scope":"next_turn"}"#
            } else {
                r#"{"scope":"active_turn","turn_id":"candidate"}"#
            };
            conn.execute(
                "INSERT INTO pending_turn_inputs
                     (origin_json, enqueue_seq, input_id, session_id, ingress_json, state, input_json,
                      submission_digest, enqueued_at_ms, admitted_run, admitted_by, terminal_at_ms)
                 VALUES ('\"submitted\"', 2048, 'candidate', 'history', ?1, ?2, '{}', 'digest', 0, ?3, ?4, ?5)",
                rusqlite::params![
                    ingress,
                    state.as_str(),
                    bound.then_some("candidate-run"),
                    (bound && !state.is_terminal()).then_some("admit"),
                    state.is_terminal().then_some(0),
                ],
            )
            .unwrap();
            probe.reset_status(StatementStatus::VmStep);
            let actual: Option<String> = probe
                .query_row(["history"], |row| row.get(0))
                .optional()
                .unwrap();
            assert_eq!(
                actual.as_deref(),
                (bound && !state.is_terminal()).then_some("candidate-run"),
                "state={state:?}, bound={bound}",
            );
            let steps = probe.get_status(StatementStatus::VmStep);
            eprintln!("bound_run state={state:?} bound={bound} vm_steps={steps}");
            assert!(
                steps <= 100,
                "bound_run visited settled history: {steps} VM steps"
            );
            conn.execute(
                "DELETE FROM pending_turn_inputs WHERE input_id = 'candidate'",
                [],
            )
            .unwrap();
        }
    }
    conn.execute_batch(
        "INSERT INTO queued_work_batches
             (enqueue_seq, batch_id, session_id, delivery_policy, authority_json,
              submission_digest, enqueued_at_ms, payload_json, admitted_run, admitted_by)
         VALUES (2049, 'queued', 'history', 'earliest_safe_boundary', '{}',
                 'digest', 0, '{\"type\":\"session_command\"}', 'queued-run', 'admit')",
    )
    .unwrap();
    assert_eq!(
        probe
            .query_row(["history"], |row| row.get::<_, String>(0))
            .unwrap(),
        "queued-run",
        "a queued-work binding remains visible beside settled input bindings",
    );
}

/// FIG-7080: reading the next turn's mailbox visits only open inputs,
/// regardless of the settled tombstones the session retains.
#[test]
fn an_open_mailbox_probe_does_no_more_work_after_a_thousand_turns() {
    use rusqlite::StatementStatus;

    let work = [1, 10, 1000].map(|turns| {
        let conn = catalog();
        conn.execute(
            "WITH RECURSIVE history(n) AS (
                 VALUES(1) UNION ALL SELECT n + 1 FROM history WHERE n < ?1
             )
             INSERT INTO pending_turn_inputs
                 (origin_json, enqueue_seq, input_id, session_id, ingress_json, state, input_json,
                  submission_digest, enqueued_at_ms, admitted_run, terminal_at_ms)
             SELECT '\"submitted\"', n, 'settled-' || n, 'history', '{\"scope\":\"next_turn\"}',
                    CASE WHEN n % 2 = 0 THEN 'completed' ELSE 'cancelled' END,
                    '{}', 'digest', 0, 'past-' || n, 0 FROM history",
            [turns],
        )
        .expect("settled history retains its run bindings");
        conn.execute_batch(
            "INSERT INTO pending_turn_inputs
                 (origin_json, enqueue_seq, input_id, session_id, ingress_json, state, input_json,
                  submission_digest, enqueued_at_ms)
             VALUES ('\"submitted\"', 2002, 'next', 'history', '{\"scope\":\"next_turn\"}',
                         'deferred_next_turn', '{}', 'digest', 0),
                    ('\"submitted\"', 2001, 'active', 'history', '{\"scope\":\"active_turn\",\"turn_id\":\"current\"}',
                         'pending_active', '{}', 'digest', 0)",
        )
        .expect("open inputs of both delivery kinds");
        let inputs_query = crate::durable::session_mail::open_inputs_query();
        let mut probe = conn.prepare(&inputs_query).expect("mailbox query");
        let inputs = probe
            .query_map(["history"], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(inputs, ["active", "next"], "ingress order stays intact");
        probe.get_status(StatementStatus::VmStep)
    });
    eprintln!("open mailbox at 1, 10 and 1000 turns: {work:?} VM steps");
    assert!(
        work[1..].iter().all(|steps| *steps <= work[0]),
        "settled history must not lengthen an open mailbox read: {work:?}"
    );
}
