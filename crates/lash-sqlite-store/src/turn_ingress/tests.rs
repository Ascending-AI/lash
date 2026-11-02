//! What the turn-ingress family's SQLite statements are held to.
//!
//! The plan tests run `EXPLAIN QUERY PLAN` against the real
//! schema and assert each named statement still seeks the index it was written
//! for: the statements that replaced a `format!` used to splice their filter in
//! per call, and the whole reason each filter shape has its own name is that a
//! spliced optional predicate cannot seek.

use crate::Prepare as _;
use rusqlite::Connection;

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
    let mut probe = conn.statement_once(&bound).unwrap();
    for &state in TurnInputStateKind::ALL {
        for bound in [false, true] {
            let ingress = if state == TurnInputStateKind::DeferredNextTurn || state.is_terminal() {
                r#"{"scope":"next_turn"}"#
            } else {
                r#"{"scope":"active_turn","turn_id":"candidate"}"#
            };
            conn.execute_once(
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
            conn.execute_once(
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
        conn.execute_once(
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
        let mut probe = conn.statement_once(&inputs_query).expect("mailbox query");
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
