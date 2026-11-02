//! Proofs about the session-core statement set itself: every statement in
//! the set renders (rendering is lazy, so a malformed neutral statement would
//! otherwise surface as a panic in whichever caller happened to touch it
//! first).

use super::*;

/// FIG-7224: retained command tombstones are outside the open-bound-run probe.
#[test]
fn bound_run_discovery_work_ignores_settled_commands() {
    use rusqlite::OptionalExtension as _;
    let conn = lookup_connection();
    let bound = crate::durable::session_mail::bound_run_query();
    let read = || {
        let found: Option<String> = conn
            .query_row_once(&bound, ["session"], |row| row.get(0))
            .optional()
            .expect("read the bound run");
        assert_eq!(found, None);
    };
    let early = lookup_steps(&conn, read);
    for n in 0..128 {
        conn.execute_once(
            "INSERT INTO queued_work_batches
             (enqueue_seq, batch_id, session_id, delivery_policy, authority_json,
              enqueued_at_ms, submission_digest, terminal_cause, terminal_at_ms)
             VALUES (?1, ?2, 'session', 'earliest_safe_boundary', '{}', 0, 'digest', 'applied', 1)",
            params![n, format!("settled-{n}")],
        )
        .expect("retain a settled command");
    }
    let retained: i64 = conn
        .query_row_once("SELECT COUNT(*) FROM queued_work_batches", [], |row| {
            row.get(0)
        })
        .expect("witness retained command rows");
    assert_eq!(retained, 128);
    let late = lookup_steps(&conn, read);
    assert!(
        late * 100 <= early * 105,
        "bound-run discovery VM work grew: {early} -> {late}"
    );
    conn.execute_once(
        "INSERT INTO queued_work_batches
         (enqueue_seq, batch_id, session_id, delivery_policy, payload_json, authority_json,
          enqueued_at_ms, submission_digest, admitted_run, admitted_by)
         VALUES (128, 'live', 'session', 'earliest_safe_boundary',
                 '{\"type\":\"session_command\"}', '{}', 0, 'digest', 'run', 'admit')",
        [],
    )
    .expect("an open command is bound");
    let bound: String = conn
        .query_row_once(&bound, ["session"], |row| row.get(0))
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
        .query_row_once(
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
