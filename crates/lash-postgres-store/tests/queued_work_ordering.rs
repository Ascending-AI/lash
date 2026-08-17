//! `cancel` rows are not an ingress family.
//!
//! Cancellation preempts on its own path, so the ordering projection the idle
//! drain reads must ignore a `cancel` row entirely rather than let it decide
//! whether session commands drain before pending turn input.

use lash_core::{QueuedWorkStore, TurnInputStore};
use lash_postgres_store::PostgresStorage;

mod support;

/// A `cancel` row is not an ingress family.
///
/// Cancellation preempts on its own path, so the ordering projection must
/// ignore it entirely rather than let it decide whether session commands
/// drain before pending turn input. The row is written through the public
/// enqueue path and then restamped, because a draft derives `control` or
/// `turn` from its payloads and can never assert `cancel`.
#[tokio::test]
async fn postgres_ordering_projection_ignores_cancel_rows() {
    let Some(database_url) = support::database_url() else {
        eprintln!("skipping cancel-ordering projection proof: database URL is not set");
        return;
    };
    let _database_lock = support::SharedDatabaseLock::acquire(&database_url).await;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .expect("connect cancel-ordering projection storage");
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let session_id = format!("cancel-ordering-session:{nonce}");
    let store = storage.session_store(&session_id);
    store
        .enqueue_queued_work(lash_core::runtime::QueuedWorkBatchDraft::new(
            session_id.clone(),
            lash_core::DeliveryPolicy::EarliestSafeBoundary,
            vec![lash_core::runtime::QueuedWorkPayload::session_command(
                lash_core::facade_support::SessionCommand::RefreshToolCatalog {
                    reason: "cancel-ordering".to_string(),
                },
            )],
        ))
        .await
        .expect("enqueue session command");
    store
        .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
            session_id.clone(),
            lash_core::TurnInputIngress::next_turn(),
            lash_core::TurnInput::text("input"),
        ))
        .await
        .expect("enqueue next-turn input");

    let ordering = store
        .pending_session_work_ordering(&session_id)
        .await
        .expect("read ordering with a control row");
    assert!(
        ordering.session_command.is_some() && ordering.turn_input.is_some(),
        "both families must be visible before the row is restamped: {ordering:?}"
    );

    sqlx::query("UPDATE lash_queued_work_batches SET work_kind = 'cancel' WHERE session_id = $1")
        .bind(&session_id)
        .execute(storage.pool())
        .await
        .expect("restamp the queued row as cancellation");

    let ordering = store
        .pending_session_work_ordering(&session_id)
        .await
        .expect("read ordering with a cancel row");
    assert_eq!(
        ordering.session_command, None,
        "a cancel row must not enter the session-command family: {ordering:?}"
    );
    assert!(
        ordering.turn_input.is_some(),
        "the pending turn input must be untouched by a cancel row: {ordering:?}"
    );
    assert!(!ordering.session_command_precedes_turn_input());
}
