//! PostgreSQL cancellation receipt integrity: affected-input evidence is a
//! snapshot on `lash_turn_cancel_affected_inputs`, so vacuuming the pending
//! rows cannot split request metadata from the payloads it reports.

use lash_core_execution::{
    PendingTurnInputDraft, StoreMaintenance, TurnCancelUndeliveredInputPolicy, TurnInput,
    TurnInputIngress, TurnInputStore,
    facade_support::{TurnAddress, TurnCancelRequest},
};
use lash_postgres_store::{PostgresStorage, testing::IsolatedDatabase};
use lash_sansio::{SessionId, TurnId};

use crate::support::database_url;

async fn storage() -> Option<(IsolatedDatabase, PostgresStorage)> {
    let url = database_url()?;
    let database = IsolatedDatabase::create(&url).await;
    let storage = PostgresStorage::connect(database.url())
        .await
        .expect("connect Postgres cancellation receipt fixture");
    Some((database, storage))
}

async fn seed_cancelled_inputs(
    storage: &PostgresStorage,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> (
    lash_core_execution::PendingTurnInput,
    lash_core_execution::PendingTurnInput,
) {
    let store = storage.store();
    // The receipt's evidence is the child-table snapshot below, not the rows'
    // delivery: the rows are plain next-turn input, since an input addressed
    // to a turn that never ran is refused (ADR 0101 §5.1).
    let first = store
        .enqueue_pending_turn_input(
            PendingTurnInputDraft::new(
                session_id,
                TurnInputIngress::NextTurn,
                TurnInput::text("first exact payload"),
            )
            .with_input_id(lash_core::InputId::fixture(format!("{session_id}:first"))),
        )
        .await
        .expect("seed first affected input");
    let second = store
        .enqueue_pending_turn_input(
            PendingTurnInputDraft::new(
                session_id,
                TurnInputIngress::NextTurn,
                TurnInput::text("second exact payload"),
            )
            .with_input_id(lash_core::InputId::fixture(format!("{session_id}:second"))),
        )
        .await
        .expect("seed second affected input");
    let request = TurnCancelRequest::new(
        TurnAddress::new(session_id, turn_id),
        format!("{session_id}:request"),
        Some("receipt-test".to_string()),
    )
    .with_reason("deterministic vacuum race")
    .undelivered(TurnCancelUndeliveredInputPolicy::Drop);
    store
        .record_turn_cancel_request(request)
        .await
        .expect("seed cancel request");
    sqlx::query(
        "UPDATE lash_pending_turn_inputs
         SET state = $2, terminal_at_ms = 0
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
                 session_id, turn_id, ordinal, input_id, disposition, input_json, item_kind
             ) VALUES ($1, $2, $3, $4, $5, $6, 'input')",
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
    let Some((_database, storage)) = storage().await else {
        eprintln!("skipping Postgres cancellation receipt vacuum check: database URL is not set");
        return;
    };
    let session_id = SessionId::from("turn-cancel-receipt-vacuum");
    let turn_id = TurnId::from("turn-cancel-receipt-vacuum:turn");
    let store = storage.store();
    let (first, second) = seed_cancelled_inputs(&storage, &session_id, &turn_id).await;
    let address = TurnAddress::new(&session_id, &turn_id);

    // The pending rows are gone before the receipt is read: the evidence must
    // come entirely from the child-table snapshot.
    let report = store
        .vacuum(&session_id)
        .await
        .expect("vacuum affected inputs");
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
    assert_eq!(
        affected[0].disposition,
        TurnCancelUndeliveredInputPolicy::Drop
    );
    assert_eq!(
        serde_json::to_value(&affected[0].payload).expect("encode first returned payload"),
        serde_json::to_value(&first.input).expect("encode first submitted payload")
    );
    assert_eq!(affected[1].input_id, second.input_id);
    assert_eq!(
        affected[1].disposition,
        TurnCancelUndeliveredInputPolicy::Defer
    );
    assert_eq!(
        serde_json::to_value(&affected[1].payload).expect("encode second returned payload"),
        serde_json::to_value(&second.input).expect("encode second submitted payload")
    );
}
