//! Proofs about the session-core statement set itself.
//!
//! Two things are checked here that no behavioural test can see: that every
//! statement in the set renders at all (rendering is lazy, so a malformed
//! neutral statement would otherwise surface as a panic in whichever caller
//! happened to touch it first), and that each named filter shape keeps the
//! query plan it was named for.

use super::*;
use crate::session_sql::session_sql;

/// The query plan of every named filter shape, pinned.
///
/// Window and page reads must seek within a session's generations. Retention
/// keeps its two filter shapes because an empty exclusion list needs no scan.
#[tokio::test]
async fn every_named_filter_shape_keeps_its_query_plan() {
    let store = crate::test_support::sqlite_memory_store()
        .await
        .expect("open plan-probe store");
    let sql = session_sql();
    let plans = store
        .conn
        .call(|conn| {
            Ok([
                explain(conn, sql.graph_sqlite.window_rows.sql())?,
                explain(conn, sql.graph_sqlite.page_headers.sql())?,
                explain(conn, sql.turn_commits_sqlite.delete_retained.sql())?,
                explain(
                    conn,
                    sql.turn_commits_sqlite.delete_retained_except_live.sql(),
                )?,
            ])
        })
        .await
        .expect("explain every named filter shape");
    let [window, page, retained, retained_except_live] = plans;
    for (name, plan) in [("window", window), ("page", page)] {
        assert!(
            plan.contains("SEARCH node USING INDEX sqlite_autoindex_graph_nodes_2")
                || plan.contains("SEARCH node USING INDEX sqlite_autoindex_graph_nodes_1"),
            "{name} must seek by session and generation: {plan}"
        );
        assert!(
            !plan.contains("SCAN node"),
            "{name} scanned graph nodes: {plan}"
        );
    }

    // The retention sweep's two shapes differ exactly where they are meant to:
    // the exclusion list is a `json_each` scan the plain shape does not pay
    // for, and both reach the deleted set through its primary key.
    assert!(
        retained.contains("sqlite_autoindex_deleted_sessions_1"),
        "the plain retention shape probes the deleted set by key; plan was:\n{retained}"
    );
    assert!(
        retained_except_live.contains("sqlite_autoindex_deleted_sessions_1"),
        "the exclusion retention shape probes the deleted set by key; plan was:\n\
         {retained_except_live}"
    );
    assert!(
        !retained.contains("json_each"),
        "the plain shape carries no exclusion list; plan was:\n{retained}"
    );
    assert!(
        retained_except_live.contains("json_each"),
        "the exclusion shape binds its live keys as one JSON array; plan was:\n\
         {retained_except_live}"
    );
}

/// `EXPLAIN QUERY PLAN` for `statement`, as one newline-joined string.
fn explain(conn: &Connection, statement: &str) -> rusqlite::Result<String> {
    let mut prepared = conn.prepare(&format!("EXPLAIN QUERY PLAN {statement}"))?;
    // The plan is a property of the statement, not of the values, so every
    // parameter is bound NULL purely to satisfy the binding count.
    let bound = vec![rusqlite::types::Value::Null; prepared.parameter_count()];
    let rows = prepared
        .query_map(rusqlite::params_from_iter(bound), |row| {
            row.get::<_, String>(3)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows.join("\n"))
}

/// FIG-7224: retained command tombstones are outside the open-bound-run probe.
#[test]
fn bound_run_discovery_work_ignores_settled_commands() {
    use rusqlite::OptionalExtension as _;
    let conn = lookup_connection();
    let mail = lash_store_sql::durable::session_mail::SessionMailStatements::render(
        crate::schema_layout::MAIN.with_vocabulary(crate::turn_ingress::TURN_INPUT_LIFECYCLE),
    );
    let read = || {
        let found: Option<String> = conn
            .query_row(mail.bound_run.sql(), ["session"], |row| row.get(0))
            .optional()
            .expect("read the bound run");
        assert_eq!(found, None);
    };
    let early = lookup_steps(&conn, read);
    for n in 0..128 {
        conn.execute(
            "INSERT INTO queued_work_batches
             (enqueue_seq, batch_id, session_id, delivery_policy, authority_json,
              enqueued_at_ms, submission_digest, terminal_cause, terminal_at_ms)
             VALUES (?1, ?2, 'session', 'earliest_safe_boundary', '{}', 0, 'digest', 'applied', 1)",
            params![n, format!("settled-{n}")],
        )
        .expect("retain a settled command");
    }
    let retained: i64 = conn
        .query_row("SELECT COUNT(*) FROM queued_work_batches", [], |row| {
            row.get(0)
        })
        .expect("witness retained command rows");
    assert_eq!(retained, 128);
    let late = lookup_steps(&conn, read);
    assert!(
        late * 100 <= early * 105,
        "bound-run discovery VM work grew: {early} -> {late}"
    );
    conn.execute(
        "INSERT INTO queued_work_batches
         (enqueue_seq, batch_id, session_id, delivery_policy, payload_json, authority_json,
          enqueued_at_ms, submission_digest, admitted_run, admitted_by)
         VALUES (128, 'live', 'session', 'earliest_safe_boundary',
                 '{\"type\":\"session_command\"}', '{}', 0, 'digest', 'run', 'admit')",
        [],
    )
    .expect("an open command is bound");
    let bound: String = conn
        .query_row(mail.bound_run.sql(), ["session"], |row| row.get(0))
        .expect("discover the bound command after retained tombstones");
    assert_eq!(bound, "run");
}

/// FIG-7224: ending an execution pays only for the frames gated on that identity.
#[test]
fn frame_cleanup_lookup_work_ignores_other_gates() {
    use lash_core_store::artifact_referrer::{
        ArtifactCleanup, ArtifactReferrer, FrameEnvironmentId,
    };
    use lash_core_store::session_identity::FrameNodeId;
    let conn = lookup_connection();
    let scope = lash_protocol_types::ExecutionScope::runtime_operation("current-%_\"-gate");
    let end =
        || crate::obligation_ledger::end_execution_tx(&conn, &scope, 1).expect("end execution");
    end(); // Both measured reads see the same already fenced execution.
    let early = lookup_steps(&conn, end);
    for n in 0..128 {
        let referrer = ArtifactReferrer::FrameEnvironment(FrameEnvironmentId::new(
            SessionId::from("session"),
            FrameNodeId::new(format!("old-{n}")).expect("frame id"),
        ));
        let gate = lash_protocol_types::ExecutionScope::runtime_operation(format!("old-gate-{n}"))
            .journal_identity()
            .expect("gate identity");
        crate::obligation_ledger::arm_cleanup_tx(
            &conn,
            &ArtifactCleanup::ended(referrer, Vec::new(), Some(gate)),
            0,
        )
        .expect("retain a frame cleanup");
    }
    let retained: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifact_cleanup_obligations",
            [],
            |row| row.get(0),
        )
        .expect("witness other gated cleanups");
    assert_eq!(retained, 128);
    let late = lookup_steps(&conn, end);
    assert!(
        late * 100 <= early * 105,
        "frame cleanup VM work grew: {early} -> {late}"
    );
}

fn lookup_connection() -> Connection {
    let conn = Connection::open_in_memory().expect("SQLite");
    for ddl in crate::schema::provisioning_statements() {
        conn.execute_batch(ddl).expect("provision lookup store");
    }
    conn
}

fn lookup_steps(conn: &Connection, read: impl FnOnce()) -> usize {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let steps = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&steps);
    conn.progress_handler(
        1,
        Some(move || {
            counter.fetch_add(1, Ordering::Relaxed);
            false
        }),
    );
    read();
    conn.progress_handler(0, None::<fn() -> bool>);
    steps.load(Ordering::Relaxed)
}
