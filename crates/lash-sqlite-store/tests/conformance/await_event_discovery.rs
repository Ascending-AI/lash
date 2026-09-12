use super::*;

#[tokio::test]
async fn sqlite_await_event_discovery_refuses_inconsistent_persisted_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("inconsistent-await-event-discovery.db");
    let host = SqliteEffectHost::open(&path)
        .await
        .expect("SQLite effect host");

    let inconsistent_session = SessionId::from("inconsistent-discovery-session");
    let inconsistent_scope = durable_turn_scope(&inconsistent_session, "turn");
    let inconsistent_wait = AwaitEventWaitIdentity::tool_completion("ordinary");
    let inconsistent_key = host
        .await_event_key(&inconsistent_scope, inconsistent_wait.clone())
        .await
        .expect("mint inconsistent-row witness key");
    assert_eq!(
        host.resolve_await_event(&inconsistent_key, Resolution::Cancelled)
            .await
            .expect("materialize inconsistent-row witness"),
        ResolveOutcome::Accepted
    );
    let connection = rusqlite::Connection::open(&path).expect("open raw effect database");
    connection
        .execute(
            "UPDATE await_event_waits
             SET turn_control = 1, terminal_json = NULL, resolved_at_ms = NULL
             WHERE key_id = ?1",
            rusqlite::params![inconsistent_key.key_id.as_str()],
        )
        .expect("inject inconsistent turn-control classification");
    drop(connection);

    let error = host
        .list_outstanding_await_event_keys(&inconsistent_session)
        .await
        .expect_err("inconsistent stored identity must be refused");
    assert_eq!(error.code.as_str(), "sqlite_await_event_decode");

    let revoked_session = SessionId::from("revoked-discovery-session");
    let revoked_scope = durable_turn_scope(&revoked_session, "turn");
    let revoked_wait = AwaitEventWaitIdentity::tool_completion("ordinary");
    let revoked_key = host
        .await_event_key(&revoked_scope, revoked_wait.clone())
        .await
        .expect("mint revoked-row witness key");
    host.revoke_await_events_for_session(&revoked_session)
        .await
        .expect("tombstone revoked-row witness session");
    let connection = rusqlite::Connection::open(&path).expect("open raw effect database");
    connection
        .execute(
            "INSERT INTO await_event_waits (
                key_id, scope_json, wait_json, session_id, turn_control,
                terminal_json, created_at_ms, resolved_at_ms
             ) VALUES (?1, ?2, ?3, ?4, 0, NULL, 0, NULL)",
            rusqlite::params![
                revoked_key.key_id,
                serde_json::to_string(&revoked_scope).expect("encode scope"),
                serde_json::to_string(&revoked_wait).expect("encode wait"),
                revoked_session.as_str(),
            ],
        )
        .expect("inject row behind revocation tombstone");
    drop(connection);

    assert!(
        host.list_outstanding_await_event_keys(&revoked_session)
            .await
            .expect("list tombstoned session")
            .is_empty(),
        "a row injected behind a revocation tombstone must stay undiscoverable"
    );
}
