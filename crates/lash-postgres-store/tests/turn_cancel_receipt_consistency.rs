//! PostgreSQL cancellation receipt integrity under vacuum and corrupt durable rows.

use std::{sync::Arc, time::Duration};

use lash_core::{
    PendingTurnInputDraft, StoreError, StoreMaintenance, TurnCancelDisposition, TurnInput,
    TurnInputCheckpointBoundary, TurnInputIngress, TurnInputStore,
    facade_support::{TurnAddress, TurnCancelRequest},
};
use lash_postgres_store::{PostgresStorage, testing::TurnCancelReadPause};
use lash_sansio::{SessionId, TurnId};

use crate::support::{SharedDatabaseLock, database_url};

async fn storage() -> Option<(SharedDatabaseLock, PostgresStorage)> {
    let url = database_url()?;
    let database_lock = SharedDatabaseLock::acquire(&url).await;
    let storage = PostgresStorage::connect(&url)
        .await
        .expect("connect Postgres cancellation receipt fixture");
    reset(&storage).await;
    Some((database_lock, storage))
}

async fn reset(storage: &PostgresStorage) {
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_tables
         WHERE schemaname = 'public'
           AND tablename LIKE 'lash\\_%'
           AND tablename NOT IN ('lash_schema_versions', 'lash_await_event_meta')
         ORDER BY tablename",
    )
    .fetch_all(storage.pool())
    .await
    .expect("list cancellation receipt fixture tables");
    assert!(!tables.is_empty(), "lash_* schema tables must exist");
    sqlx::query(&format!(
        "TRUNCATE {} RESTART IDENTITY CASCADE",
        tables.join(", ")
    ))
    .execute(storage.pool())
    .await
    .expect("reset cancellation receipt fixture tables");
    sqlx::query(
        "INSERT INTO lash_process_change_clock (singleton, current_seq)
         VALUES (TRUE, 0)
         ON CONFLICT (singleton) DO UPDATE SET current_seq = EXCLUDED.current_seq",
    )
    .execute(storage.pool())
    .await
    .expect("reset cancellation receipt process change clock");
}

async fn seed_cancelled_inputs(
    storage: &PostgresStorage,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> (lash_core::PendingTurnInput, lash_core::PendingTurnInput) {
    let store = storage.session_store(session_id.clone());
    let first = store
        .enqueue_pending_turn_input(
            PendingTurnInputDraft::new(
                session_id,
                TurnInputIngress::active_turn(turn_id, TurnInputCheckpointBoundary::AfterWork),
                TurnInput::text("first exact payload"),
            )
            .with_input_id(format!("{session_id}:first")),
        )
        .await
        .expect("seed first affected input");
    let second = store
        .enqueue_pending_turn_input(
            PendingTurnInputDraft::new(
                session_id,
                TurnInputIngress::active_turn(
                    turn_id,
                    TurnInputCheckpointBoundary::BeforeCompletion,
                ),
                TurnInput::text("second exact payload"),
            )
            .with_input_id(format!("{session_id}:second")),
        )
        .await
        .expect("seed second affected input");
    let request = TurnCancelRequest::new(
        TurnAddress::new(session_id, turn_id),
        format!("{session_id}:request"),
        Some("receipt-test".to_string()),
    )
    .with_reason("deterministic vacuum race")
    .undelivered(TurnCancelDisposition::Drop);
    store
        .record_turn_cancel_request(request)
        .await
        .expect("seed cancel request");
    sqlx::query(
        "UPDATE lash_pending_turn_inputs
         SET state = $2
         WHERE session_id = $1",
    )
    .bind(session_id.as_str())
    .bind(lash_core::TurnInputState::Cancelled.as_str())
    .execute(storage.pool())
    .await
    .expect("make affected inputs vacuum eligible");
    sqlx::query(
        "UPDATE lash_turn_cancel_requests
         SET affected_input_ids = $3, affected_dispositions = $4
         WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .bind(vec![first.input_id.clone(), second.input_id.clone()])
    .bind(vec!["drop", "defer"])
    .execute(storage.pool())
    .await
    .expect("attach ordered affected input evidence");
    (first, second)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_turn_cancel_read_is_complete_across_concurrent_vacuum_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres cancellation receipt race: database URL is not set");
        return;
    };
    let session_id = SessionId::from("turn-cancel-receipt-vacuum");
    let turn_id = TurnId::from("turn-cancel-receipt-vacuum:turn");
    let store = storage.session_store(session_id.clone());
    let (first, second) = seed_cancelled_inputs(&storage, &session_id, &turn_id).await;
    let address = TurnAddress::new(&session_id, &turn_id);
    let pause = Arc::new(TurnCancelReadPause::default());
    let reader_store = store.clone();
    let reader_pause = Arc::clone(&pause);
    let reader = tokio::spawn(async move {
        reader_store
            .turn_cancel_request_paused_for_testing(&address, &reader_pause)
            .await
    });

    tokio::time::timeout(Duration::from_secs(10), pause.wait_until_metadata_read())
        .await
        .expect("reader reached the deterministic pause");
    let report = store.vacuum().await.expect("vacuum affected inputs");
    assert_eq!(report.removed_pending_turn_input_tombstone_count, 2);
    pause.resume();

    let record = tokio::time::timeout(Duration::from_secs(10), reader)
        .await
        .expect("paused receipt read completed")
        .expect("receipt read task did not panic")
        .expect("receipt read succeeded")
        .expect("reader observed request metadata before vacuum");
    let affected = record
        .outcome
        .expect("observed request retains a complete outcome")
        .affected_inputs;
    assert_eq!(affected.len(), 2);
    assert_eq!(affected[0].input_id, first.input_id);
    assert_eq!(affected[0].disposition, TurnCancelDisposition::Drop);
    assert_eq!(
        serde_json::to_value(&affected[0].payload).expect("encode first returned payload"),
        serde_json::to_value(&first.input).expect("encode first submitted payload")
    );
    assert_eq!(affected[1].input_id, second.input_id);
    assert_eq!(affected[1].disposition, TurnCancelDisposition::Defer);
    assert_eq!(
        serde_json::to_value(&affected[1].payload).expect("encode second returned payload"),
        serde_json::to_value(&second.input).expect("encode second submitted payload")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_turn_cancel_read_rejects_malformed_or_missing_affected_evidence_when_configured()
{
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres cancellation receipt integrity: database URL is not set");
        return;
    };
    let session_id = SessionId::from("turn-cancel-receipt-integrity");
    let turn_id = TurnId::from("turn-cancel-receipt-integrity:turn");
    let store = storage.session_store(session_id.clone());
    let (_first, second) = seed_cancelled_inputs(&storage, &session_id, &turn_id).await;
    let address = TurnAddress::new(&session_id, &turn_id);
    let request = TurnCancelRequest::new(
        address.clone(),
        format!("{session_id}:repeat"),
        Some("receipt-test".to_string()),
    );

    sqlx::query(
        "UPDATE lash_turn_cancel_requests
         SET affected_dispositions = $3
         WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .bind(vec!["drop"])
    .execute(storage.pool())
    .await
    .expect("corrupt affected disposition cardinality");
    assert_integrity_error(
        store
            .turn_cancel_request(&address)
            .await
            .expect_err("public reader rejects unequal affected arrays"),
        "cardinality",
    );
    assert_integrity_error(
        store
            .record_turn_cancel_request(request.clone())
            .await
            .expect_err("transactional reader rejects unequal affected arrays"),
        "cardinality",
    );

    sqlx::query(
        "UPDATE lash_turn_cancel_requests
         SET affected_dispositions = $3
         WHERE session_id = $1 AND turn_id = $2",
    )
    .bind(session_id.as_str())
    .bind(turn_id.as_str())
    .bind(vec!["drop", "defer"])
    .execute(storage.pool())
    .await
    .expect("restore affected disposition cardinality");
    sqlx::query("DELETE FROM lash_pending_turn_inputs WHERE input_id = $1")
        .bind(&second.input_id)
        .execute(storage.pool())
        .await
        .expect("remove one affected payload");
    assert_integrity_error(
        store
            .turn_cancel_request(&address)
            .await
            .expect_err("public reader rejects a missing affected payload"),
        &second.input_id,
    );
    assert_integrity_error(
        store
            .record_turn_cancel_request(request)
            .await
            .expect_err("transactional reader rejects a missing affected payload"),
        &second.input_id,
    );
}

fn assert_integrity_error(error: StoreError, expected_message_fragment: &str) {
    match error {
        StoreError::StoredDataCorrupt {
            record_kind: "TurnCancelRequest",
            message,
        } => assert!(
            message.contains(expected_message_fragment),
            "unexpected integrity message: {message}"
        ),
        other => panic!("expected TurnCancelRequest integrity error, got {other:?}"),
    }
}
