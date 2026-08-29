use super::*;

struct PostgresWakeDeliveryOrderingGroupFaultInjector {
    pool: sqlx::PgPool,
}

#[async_trait::async_trait]
impl lash_core::testing::conformance::WakeDeliveryOrderingGroupFaultInjector
    for PostgresWakeDeliveryOrderingGroupFaultInjector
{
    async fn discard_without_reason(&self, delivery_id: &str) {
        assert_eq!(
            sqlx::query(
                "UPDATE lash_process_wake_deliveries
                 SET state = 'discarded', claim_token = NULL, discard_reason = NULL
                 WHERE delivery_id = $1 AND state = 'enqueuing'",
            )
            .bind(delivery_id)
            .execute(&self.pool)
            .await
            .expect("inject reasonless Postgres wake discard")
            .rows_affected(),
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_wake_delivery_crash_matrix_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres wake-delivery crash matrix: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let clock = Arc::new(lash_core::testing::TestClock::new(1_800_000_000_000));
    let factory = Arc::new(
        storage
            .session_store_factory()
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
    ) as Arc<dyn SessionStoreFactory>;
    let registry = Arc::new(
        storage
            .process_registry_with_wake_delivery_config(
                lash_core::WakeDeliveryConfig::new(10_000)
                    .expect("valid test retention")
                    .with_enqueuing_stale_after_ms(25)
                    .expect("valid short stale-claim age"),
            )
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
    ) as Arc<dyn ProcessRegistry>;
    let process_work = Arc::new(lash_core::NativeProcessWork::for_registry(Arc::clone(
        &registry,
    )));
    Box::pin(lash_core::testing::conformance::wake_delivery_crash_matrix(
        factory,
        registry,
        clock,
        process_work,
        lash_core::testing::conformance::ProcessTerminalWaitWitness::Direct,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_wake_delivery_ordering_group_conformance_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres wake ordering-group conformance: \
             LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(&storage).await;
    let registry = Arc::new(storage.process_registry());
    let process_work = Arc::new(lash_core::NativeProcessWork::for_registry(
        Arc::clone(&registry) as Arc<dyn ProcessRegistry>,
    ));
    lash_core::testing::conformance::wake_delivery_ordering_group_conformance(
        registry as Arc<dyn ProcessRegistry>,
        Arc::new(PostgresWakeDeliveryOrderingGroupFaultInjector {
            pool: storage.pool().clone(),
        }),
        process_work,
        lash_core::testing::conformance::ProcessTerminalWaitWitness::Direct,
    )
    .await;
}
