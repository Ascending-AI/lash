use super::*;
fn assert_check_rejects(connection: &Connection, statement: &str, constraint: &str) {
    let error = connection
        .execute_batch(statement)
        .expect_err("an illegal durable vocabulary must violate its schema CHECK");
    assert!(
        error.to_string().contains(constraint),
        "SQLite reported the wrong CHECK for {constraint}: {error}"
    );
}

#[test]
fn waits_refuse_impossible_purpose_and_lifecycle_rows() {
    let conn = Connection::open_in_memory().expect("open wait fixture");
    conn.execute_batch(crate::durable::WAITS_TABLES)
        .expect("create schema");
    conn.execute_batch("INSERT INTO waits (wait_id, owner_actor, owner_scope, kind, host_resolvable, state, created_epoch, key_name) VALUES ('wait', 's/session', 's/session', 'engine_key', 1, 'pending', 1, 'key')").expect("valid pending engine key wait");
    for (assignment, constraint) in [
        (
            "kind = 'timer', host_resolvable = 0, key_name = NULL",
            "ck_waits_timer_deadline",
        ),
        ("key_name = NULL", "ck_waits_key_name"),
        ("call_id = 'call', tool_id = 'tool'", "ck_waits_call"),
        ("resolved_at_ms = 1", "ck_waits_settled_at"),
        ("state = 'revoked'", "ck_waits_settled_at"),
        (
            "state = 'resolved', resolution_digest = 'digest', resolved_at_ms = 1",
            "ck_waits_resolution_ref",
        ),
        ("resolution_ref = 'payload'", "ck_waits_resolution_ref"),
    ] {
        assert_check_rejects(&conn, &format!("UPDATE waits SET {assignment}"), constraint);
    }
    conn.execute_batch("UPDATE waits SET kind = 'timer', host_resolvable = 0, key_name = NULL, deadline_ms = 1, state = 'resolved', resolution_digest = 'timer', resolved_at_ms = 1").expect("resolved timer needs no payload");
    assert_check_rejects(
        &conn,
        "UPDATE waits SET resolution_ref = 'payload'",
        "ck_waits_resolution_ref",
    );
}

#[test]
fn actors_refuse_kind_that_disagrees_with_key() {
    let conn = Connection::open_in_memory().expect("open actor fixture");
    conn.execute_batch(crate::durable::DURABLE_TABLES)
        .expect("create schema");
    for (key, kind) in [
        ("s/session", "process"),
        ("p/process", "session"),
        ("x/unknown", "session"),
    ] {
        assert_check_rejects(
            &conn,
            &format!(
                "INSERT INTO actors (actor_key, kind, state, epoch, formats, state_revision, mail_seq, acked_seq, created_at_ms) VALUES ('{key}', '{kind}', 'idle', 0, '[]', 0, 0, 0, 0)"
            ),
            "ck_actors_key_kind",
        );
    }
}
