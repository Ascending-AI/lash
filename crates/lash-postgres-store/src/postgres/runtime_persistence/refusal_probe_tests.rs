// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::*;

fn new_arrival_wake() -> lash_core_execution::ProcessWakeDelivery {
    let process_id = || lash_core_execution::ProcessId::from("refusal-probe-process");
    lash_core_execution::ProcessWakeDelivery {
        version: lash_core_execution::PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        wake_id: "refusal-probe-process-wake-1".to_string(),
        target_session_id: SessionId::from("refusal-probe"),
        process_id: process_id(),
        process_incarnation: lash_core_execution::ProcessIncarnation::from_registration_sequence(1),
        sequence: 1,
        event_type: "process.wake".to_string(),
        event_invocation: lash_core_execution::RuntimeInvocation {
            attribution: lash_core_execution::RuntimeAttribution::for_session("refusal-probe"),
            subject: lash_core_execution::runtime::RuntimeSubject::ProcessEvent {
                process_id: process_id(),
                sequence: 1,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: lash_core_execution::QueuedWorkAuthority::default(),
        input: "new arrival".to_string(),
        created_at_ms: 1,
    }
}

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
        .try_claim_session_execution_lease(
            &SessionId::from("refusal-probe"),
            &owner,
            "probe-executor",
            60_000,
        )
        .await
        .unwrap()
        .acquired()
        .unwrap();
    let mut tx = storage.pool().begin().await.unwrap();
    ensure_session_execution_lease_tx(&mut tx, &SessionId::from("refusal-probe"), &lease.fence())
        .await
        .unwrap();
    // The candidate scan binds its ready cutoff now (FIG-3383), from the
    // transaction timestamp the claim path samples once per transaction.
    let now = postgres_transaction_epoch_ms(&mut tx).await.unwrap();
    let rows = sqlx::query(postgres_queued_work_claim_candidates_sql(
        QueuedWorkClaimBoundary::Idle,
    ))
    .bind("refusal-probe")
    .bind(now as i64)
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
        .enqueue_queued_work(lash_core_execution::runtime::process_wake_batch_draft(
            new_arrival_wake(),
        ))
        .await
        .unwrap();
    let diagnostic = postgres_refusal_for_empty_scan(
        &mut tx,
        &SessionId::from("refusal-probe"),
        lease.fencing_token,
        QueuedWorkClaimBoundary::Idle,
        &lash_core_execution::testing::queued_work_claim_policy(10),
    )
    .await
    .unwrap();
    assert_eq!(
        diagnostic,
        TurnWorkEmptyScanDiagnostic::BecameSelectable,
        "the later statement snapshot observes selectable work"
    );
    assert_eq!(
        diagnostic.into_refusal(),
        QueuedWorkClaimRefusal::Empty,
        "a diagnostic must preserve Empty without admitting the new head"
    );
    let ready: i64 = sqlx::query_scalar("SELECT count(*) FROM lash_queued_work_batches WHERE session_id = 'refusal-probe' AND claim_token IS NULL")
        .fetch_one(&mut *tx).await.unwrap();
    assert_eq!(ready, 1, "the later probe sees the newly enqueued head");
    tx.rollback().await.unwrap();
    assert!(
        store
            .claim_ready_queued_work(
                &SessionId::from("refusal-probe"),
                &lease.fence(),
                &owner,
                QueuedWorkClaimBoundary::Idle,
                lash_core_execution::testing::queued_work_claim_policy(10)
            )
            .await
            .unwrap()
            .claim()
            .is_some()
    );
}
