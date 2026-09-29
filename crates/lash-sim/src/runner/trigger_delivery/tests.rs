use super::*;
use lash_core::StoreSet;
use lash_core::store::ObligationKind;

/// Cross-backend law for the generated world's trigger boundary: success
/// means a bound process and a delivered obligation, including on replay.
async fn generated_trigger_delivery_is_bound_and_settled(stores: Arc<dyn StoreSet>) {
    let triggers = stores.trigger_store();
    let registry = stores.process_registry();
    let ledger = stores.obligation_ledger(ObligationKind::TriggerDelivery);
    let mut harness = SimTriggerHarness::over(Arc::clone(&stores));
    let event = BoundaryEvent::new(
        "trigger-boundary",
        "trigger-session",
        BoundaryKind::Trigger,
        0,
        "trigger delivery",
        json!({"source_key": "trigger-source"}),
    );
    let first = harness.deliver(&event).await.expect("deliver trigger");
    let occurrence = first["occurrence_id"].as_str().expect("occurrence id");
    let deliveries = triggers
        .list_deliveries_by_occurrence_id(occurrence)
        .await
        .expect("read delivery");
    assert_eq!(deliveries.len(), 1);
    let delivery = &deliveries[0];
    let process_id = delivery
        .process_id
        .as_ref()
        .expect("a successful trigger boundary binds its process");
    let process = registry
        .get_process_by_start_key(&lash_core::facade_support::trigger_delivery_start_key(
            delivery,
        ))
        .await
        .expect("read delivery's start key")
        .expect("the delivery's process is registered");
    assert_eq!(&process.id, process_id);
    assert!(process.input.is_externally_owned());
    assert_eq!(first["started_process"], true);
    assert!(
        stores
            .obligation_ledger(ObligationKind::ProcessStart)
            .claim_due(
                stores.clock().timestamp_ms(),
                60_000,
                std::num::NonZeroUsize::MIN
            )
            .await
            .expect("read engine start obligations")
            .is_empty(),
        "the scheduler owns the process, so no engine start is owed"
    );
    assert!(
        ledger
            .claim_due(
                stores.clock().timestamp_ms(),
                60_000,
                std::num::NonZeroUsize::MIN
            )
            .await
            .expect("read due deliveries")
            .is_empty(),
        "a successful boundary leaves no due trigger delivery"
    );
    let replay = SimTriggerHarness::over(Arc::clone(&stores))
        .deliver(&event)
        .await
        .expect("replay trigger after restarting the boundary helper");
    assert_eq!(replay["occurrence_id"], first["occurrence_id"]);
    let replayed = triggers
        .list_deliveries_by_occurrence_id(occurrence)
        .await
        .expect("read replayed delivery");
    assert_eq!(replayed.len(), 1);
    assert_eq!(replayed[0].process_id.as_ref(), Some(process_id));
    let stalled = ledger.count_stalled().await.expect("count stalled");
    assert_eq!(stalled, 0);
}

#[tokio::test]
async fn sqlite_generated_trigger_delivery_is_bound_and_settled() {
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("SQLite stores");
    generated_trigger_delivery_is_bound_and_settled(Arc::new(stores)).await;
}

#[tokio::test]
async fn postgres_generated_trigger_delivery_is_bound_and_settled() {
    let Some(database) = crate::postgres_test_isolation::isolated_database().await else {
        return;
    };
    let storage = lash_postgres_store::PostgresStorage::connect(database.url())
        .await
        .expect("PostgreSQL storage");
    let attachments = tempfile::tempdir().expect("attachment root");
    let stores = lash_postgres_store::PostgresStoreSet::with_clock(
        &storage,
        Arc::new(lash::persistence::FileAttachmentStore::new(
            attachments.path().join("attachments"),
        )),
        lash_core::WakeDeliveryConfig::default(),
        Arc::new(lash_core::testing::TestClock::new(4_000_000_000_000)),
    );
    generated_trigger_delivery_is_bound_and_settled(Arc::new(stores)).await;
}
