use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_empty_scan_refusal_probe_can_observe_concurrent_enqueue() {
    let Ok(url) = std::env::var("LASH_POSTGRES_DATABASE_URL") else {
        return;
    };
    let database = crate::testing::IsolatedDatabase::create(&url).await;
    let storage = crate::PostgresStorage::connect(database.url())
        .await
        .unwrap();
    let store = storage.session_store("refusal-probe");
    let owner = LeaseOwnerIdentity::opaque("probe", "probe-incarnation");
    let lease = store
        .try_claim_session_execution_lease("refusal-probe", &owner, "probe-executor", 60_000)
        .await
        .unwrap()
        .acquired()
        .unwrap();
    let mut tx = storage.pool().begin().await.unwrap();
    ensure_session_execution_lease_tx(&mut tx, "refusal-probe", &lease.fence())
        .await
        .unwrap();
    let rows = sqlx::query(&postgres_queued_work_claim_candidates_sql(
        QueuedWorkClaimBoundary::Idle,
    ))
    .bind("refusal-probe")
    .bind(sql_session_lease_generation(lease.fencing_token).unwrap())
    .bind(10_i64)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert!(rows.is_empty(), "the first candidate scan must be empty");
    // Another connection enqueues after the scan while the claim transaction
    // retains its session-execution lease row lock. Enqueue takes the distinct
    // history-mutation advisory lock and is allowed to finish here.
    store
        .enqueue_queued_work(QueuedWorkBatchDraft::new(
            "refusal-probe",
            DeliveryPolicy::EarliestSafeBoundary,
            lash_core::runtime::TurnWorkPayload::agent_frame_task(
                lash_core::facade_support::frame_node_id("refusal-probe", "frame"),
                "new arrival",
                None,
            ),
        ))
        .await
        .unwrap();
    let reason = postgres_refusal_for_empty_scan(
        &mut tx,
        "refusal-probe",
        lease.fencing_token,
        QueuedWorkClaimBoundary::Idle,
        &lash_core::testing::queued_work_claim_policy(10),
    )
    .await
    .unwrap();
    assert_eq!(
        reason,
        QueuedWorkClaimRefusal::Empty,
        "the existing fallback is observable across statement snapshots"
    );
    let ready: i64 = sqlx::query_scalar("SELECT count(*) FROM lash_queued_work_batches WHERE session_id = 'refusal-probe' AND claim_token IS NULL")
        .fetch_one(&mut *tx).await.unwrap();
    assert_eq!(ready, 1, "the later probe sees the newly enqueued head");
    tx.rollback().await.unwrap();
    assert!(
        store
            .claim_ready_queued_work(
                "refusal-probe",
                &lease.fence(),
                &owner,
                QueuedWorkClaimBoundary::Idle,
                lash_core::testing::queued_work_claim_policy(10)
            )
            .await
            .unwrap()
            .claim()
            .is_some()
    );
}
