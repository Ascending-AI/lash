//! Stored wait, actor and execution constraint laws.

use sqlx::Connection as _;

#[tokio::test]
async fn durable_waits_and_actors_refuse_impossible_rows() {
    let url = crate::testing::required_database_url();
    let database = crate::testing::IsolatedDatabase::create(&url).await;
    let mut conn = sqlx::PgConnection::connect(database.url())
        .await
        .expect("open constraint fixture");
    sqlx::raw_sql(crate::PostgresStorage::schema_ddl())
        .execute(crate::observed_sql::executor(&mut conn))
        .await
        .expect("create constraint fixture");
    crate::observed_sql::query("INSERT INTO lash_waits (wait_id, owner_actor, owner_scope, kind, host_resolvable, state, created_epoch, key_name) VALUES ('wait', 's/session', 's/session', 'engine_key', true, 'pending', 1, 'key')").execute(crate::observed_sql::executor(&mut conn)).await.expect("pending engine key wait");
    for (assignment, constraint) in [
        (
            "kind = 'timer', host_resolvable = false, key_name = NULL",
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
        let error = crate::observed_sql::query(&format!("UPDATE lash_waits SET {assignment}"))
            .execute(crate::observed_sql::executor(&mut conn))
            .await
            .expect_err("impossible wait must be refused");
        assert_eq!(
            error
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::constraint),
            Some(constraint)
        );
    }
    crate::observed_sql::query("UPDATE lash_waits SET kind = 'timer', host_resolvable = false, key_name = NULL, deadline_ms = 1, state = 'resolved', resolution_digest = 'timer', resolved_at_ms = 1").execute(crate::observed_sql::executor(&mut conn)).await.expect("resolved timer without value");
    let error = crate::observed_sql::query("UPDATE lash_waits SET resolution_ref = 'payload'")
        .execute(crate::observed_sql::executor(&mut conn))
        .await
        .expect_err("timer has no payload");
    assert_eq!(
        error
            .as_database_error()
            .and_then(sqlx::error::DatabaseError::constraint),
        Some("ck_waits_resolution_ref")
    );
    for (key, kind) in [
        ("s/session", "process"),
        ("p/process", "session"),
        ("x/unknown", "session"),
    ] {
        let error = crate::observed_sql::query(&format!("INSERT INTO lash_actors (actor_key, kind, state, epoch, formats, state_revision, mail_seq, acked_seq, created_at_ms) VALUES ('{key}', '{kind}', 'idle', 0, '[]', 0, 0, 0, 0)")).execute(crate::observed_sql::executor(&mut conn)).await.expect_err("key must agree with kind");
        assert_eq!(
            error
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::constraint),
            Some("ck_lash_actors_key_kind")
        );
    }
    for (rev, commit_id, constraint) in [
        (0_i64, vec![0_u8; 32], "ck_exec_snapshots_rev"),
        (1, vec![0; 31], "ck_exec_snapshots_commit_id"),
    ] {
        let error = crate::observed_sql::query("INSERT INTO lash_exec_snapshots (exec_key, rev, root_ref, commit_id, written_epoch) VALUES ('p/process', $1, 'root', $2, 1)").bind(rev).bind(commit_id).execute(crate::observed_sql::executor(&mut conn)).await.expect_err("rev and commit id are checked");
        assert_eq!(
            error
                .as_database_error()
                .and_then(sqlx::error::DatabaseError::constraint),
            Some(constraint)
        );
    }
}
