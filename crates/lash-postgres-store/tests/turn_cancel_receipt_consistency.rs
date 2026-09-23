//! PostgreSQL cancellation receipt integrity: affected-input evidence is a
//! snapshot on `lash_turn_cancel_affected_inputs`, so vacuuming the pending
//! rows cannot split request metadata from the payloads it reports.

use lash_core_execution::{
    PendingTurnInputDraft, StoreMaintenance, TurnCancelDisposition, TurnInput,
    TurnInputCheckpointBoundary, TurnInputIngress, TurnInputStore,
    facade_support::{TurnAddress, TurnCancelRequest},
};
use lash_postgres_store::PostgresStorage;
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
) -> (
    lash_core_execution::PendingTurnInput,
    lash_core_execution::PendingTurnInput,
) {
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
    .bind(lash_core_execution::runtime::TurnInputStateKind::Cancelled.as_str())
    .execute(storage.pool())
    .await
    .expect("make affected inputs vacuum eligible");
    for (ordinal, (input, disposition)) in [
        (first.input.clone(), "drop"),
        (second.input.clone(), "defer"),
    ]
    .into_iter()
    .enumerate()
    {
        let input_id = match ordinal {
            0 => first.input_id.to_string(),
            _ => second.input_id.to_string(),
        };
        sqlx::query(
            "INSERT INTO lash_turn_cancel_affected_inputs (
                 session_id, turn_id, ordinal, input_id, disposition, input_json
             ) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(session_id.as_str())
        .bind(turn_id.as_str())
        .bind(ordinal as i64)
        .bind(input_id)
        .bind(disposition)
        .bind(serde_json::to_string(&input).expect("encode affected input payload"))
        .execute(storage.pool())
        .await
        .expect("attach ordered affected input evidence");
    }
    (first, second)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_turn_cancel_receipt_survives_vacuum_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!("skipping Postgres cancellation receipt vacuum check: database URL is not set");
        return;
    };
    let session_id = SessionId::from("turn-cancel-receipt-vacuum");
    let turn_id = TurnId::from("turn-cancel-receipt-vacuum:turn");
    let store = storage.session_store(session_id.clone());
    let (first, second) = seed_cancelled_inputs(&storage, &session_id, &turn_id).await;
    let address = TurnAddress::new(&session_id, &turn_id);

    // The pending rows are gone before the receipt is read: the evidence must
    // come entirely from the child-table snapshot.
    let report = store.vacuum().await.expect("vacuum affected inputs");
    assert_eq!(report.removed_pending_turn_input_tombstone_count, 2);

    let record = store
        .turn_cancel_request(&address)
        .await
        .expect("receipt read succeeded")
        .expect("request metadata survives the vacuum");
    let affected = record
        .outcome
        .expect("vacuumed request retains a complete outcome")
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
