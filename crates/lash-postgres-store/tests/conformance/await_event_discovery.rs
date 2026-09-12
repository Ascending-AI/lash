use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_await_event_discovery_refuses_inconsistent_persisted_rows_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres inconsistent await-event discovery test: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let host = storage.effect_host();

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
    sqlx::query(
        "UPDATE lash_await_event_waits
         SET turn_control = TRUE, terminal_json = NULL, resolved_at_ms = NULL
         WHERE key_id = $1",
    )
    .bind(&inconsistent_key.key_id)
    .execute(storage.pool())
    .await
    .expect("inject inconsistent turn-control classification");

    let error = host
        .list_outstanding_await_event_keys(&inconsistent_session)
        .await
        .expect_err("inconsistent stored identity must be refused");
    assert_eq!(error.code.as_str(), "postgres_await_event_decode");

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
    sqlx::query(
        "INSERT INTO lash_await_event_waits (
            key_id, scope_json, wait_json, session_id, turn_control,
            terminal_json, created_at_ms, resolved_at_ms
         ) VALUES ($1, $2, $3, $4, FALSE, NULL, 0, NULL)",
    )
    .bind(&revoked_key.key_id)
    .bind(serde_json::to_string(&revoked_scope).expect("encode scope"))
    .bind(serde_json::to_string(&revoked_wait).expect("encode wait"))
    .bind(revoked_session.as_str())
    .execute(storage.pool())
    .await
    .expect("inject row behind revocation tombstone");

    assert!(
        host.list_outstanding_await_event_keys(&revoked_session)
            .await
            .expect("list tombstoned session")
            .is_empty(),
        "a row injected behind a revocation tombstone must stay undiscoverable"
    );
}
