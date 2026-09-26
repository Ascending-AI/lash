use super::*;

struct SqliteWakeDeliveryOrderingGroupFaultInjector {
    backend: TestBackend,
}

#[async_trait::async_trait]
impl lash_conformance::WakeDeliveryOrderingGroupFaultInjector
    for SqliteWakeDeliveryOrderingGroupFaultInjector
{
    async fn discard_without_reason(&self, delivery_id: &str) {
        let conn = self.backend.raw(SqliteDatabase::ProcessRegistry);
        assert_eq!(
            conn.execute(
                "UPDATE process_wake_deliveries
                 SET state = 'discarded', claim_token = NULL, discard_reason = NULL
                 WHERE delivery_id = ?1 AND state = 'enqueuing'",
                rusqlite::params![delivery_id],
            )
            .expect("inject reasonless SQLite wake discard"),
            1
        );
    }
}

lash_conformance::wake_delivery_crash_tests!({
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(
        1_800_000_000_000,
    ));
    let backend = TestBackend::open_with(
        SUBSTRATE,
        |options| SqliteStoreSetOptions {
            wake_delivery: lash_core_execution::WakeDeliveryConfig::new(10_000)
                .expect("valid test retention")
                .with_enqueuing_stale_after_ms(25)
                .expect("valid short stale-claim age"),
            ..options
        },
        Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>,
    )
    .await;
    let registry =
        backend.process_registry() as Arc<dyn lash_core_execution::ConformanceProcessRegistry>;
    let factory = backend.session_store_factory() as Arc<dyn SessionStoreFactory>;
    let process_work = Arc::new(lash_core_execution::NativeProcessWork::for_registry(
        Arc::clone(&registry) as Arc<dyn ProcessRegistry>,
    ));
    (
        backend,
        factory,
        registry,
        clock,
        process_work,
        lash_conformance::ProcessTerminalWaitWitness::Direct,
        || async {},
        || async {},
    )
});

lash_conformance::wake_delivery_ordering_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let registry = backend.process_registry();
    let process_work = Arc::new(lash_core_execution::NativeProcessWork::for_registry(
        Arc::clone(&registry) as Arc<dyn ProcessRegistry>,
    ));
    (
        backend.clone(),
        registry as Arc<dyn ProcessRegistry>,
        Arc::new(SqliteWakeDeliveryOrderingGroupFaultInjector { backend }),
        process_work,
        lash_conformance::ProcessTerminalWaitWitness::Direct,
        || async {},
        || async {},
    )
});
