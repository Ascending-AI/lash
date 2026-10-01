use super::*;

struct SqliteWakeDeliveryIsolationBackend(TestBackend);

#[async_trait::async_trait]
impl lash_conformance::WakeDeliveryIsolationBackend for SqliteWakeDeliveryIsolationBackend {
    async fn corrupt_source(&self, process_id: &lash_sansio::ProcessId) {
        assert_eq!(
            self.0
                .raw(SqliteDatabase::ProcessRegistry)
                .execute(
                    "UPDATE processes SET record_json = '{}' WHERE process_id = ?1",
                    [process_id.as_str()],
                )
                .expect("corrupt SQLite wake source"),
            1
        );
    }

    async fn reopen(&self) -> (Arc<dyn DeploymentStore>, Arc<dyn ProcessRegistry>) {
        let backend = self.0.reopen().await;
        (backend.session_store_factory(), backend.process_registry())
    }
}

lash_conformance::wake_delivery_isolation_tests!({
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(
        1_800_000_000_000,
    ));
    let backend = TestBackend::open_with(
        SUBSTRATE,
        |mut options| {
            options.wake_delivery = lash_core_execution::WakeDeliveryConfig::new(10_000)
                .expect("valid wake expiry")
                .with_enqueuing_stale_after_ms(25)
                .expect("valid claim lapse");
            options
        },
        clock.clone(),
    )
    .await;
    (
        backend.clone(),
        backend.session_store_factory() as Arc<dyn DeploymentStore>,
        backend.process_registry() as Arc<dyn ProcessRegistry>,
        clock,
        Arc::new(SqliteWakeDeliveryIsolationBackend(backend))
            as Arc<dyn lash_conformance::WakeDeliveryIsolationBackend>,
    )
});

struct SqliteWakeRedeliveryFloors(TestBackend);

#[async_trait::async_trait]
impl lash_conformance::WakeRedeliveryFloorProbe for SqliteWakeRedeliveryFloors {
    async fn receiver_floor(
        &self,
        session_id: &lash_sansio::SessionId,
        process_id: &lash_sansio::ProcessId,
    ) -> Option<u64> {
        use rusqlite::OptionalExtension as _;
        self.0
            .raw(SqliteDatabase::DurableCore)
            .query_row(
                "SELECT allocation_floor FROM wake_redelivery_fences
                 WHERE session_id = ?1 AND process_id = ?2",
                [session_id.as_str(), process_id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .expect("read SQLite receiver floor")
            .map(|floor| u64::try_from(floor).expect("non-negative receiver floor"))
    }
}

lash_conformance::wake_delivery_conflict_tests!({
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(
        1_800_000_000_000,
    ));
    let backend = TestBackend::open_with(SUBSTRATE, |options| options, clock.clone()).await;
    (
        backend.clone(),
        backend.session_store_factory() as Arc<dyn DeploymentStore>,
        backend.process_registry() as Arc<dyn ProcessRegistry>,
        clock,
        Arc::new(lash_core_execution::NoSessionWork::new())
            as Arc<dyn lash_core_execution::SessionWorkEngine>,
        Arc::new(SqliteWakeRedeliveryFloors(backend))
            as Arc<dyn lash_conformance::WakeRedeliveryFloorProbe>,
    )
});
