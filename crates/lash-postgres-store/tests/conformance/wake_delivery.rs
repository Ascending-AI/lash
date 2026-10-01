use super::*;

struct PostgresWakeDeliveryOrderingGroupFaultInjector {
    pool: sqlx::PgPool,
}

#[async_trait::async_trait]
impl lash_conformance::WakeDeliveryOrderingGroupFaultInjector
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

lash_conformance::wake_delivery_crash_tests!({
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres wake-delivery crash matrix: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(
        1_800_000_000_000,
    ));
    let factory = Arc::new(
        storage
            .session_store_factory()
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>),
    ) as Arc<dyn DeploymentStore>;
    let registry = Arc::new(
        storage
            .process_registry_with_wake_delivery_config(
                lash_core_execution::WakeDeliveryConfig::new(10_000)
                    .expect("valid test retention")
                    .with_enqueuing_stale_after_ms(25)
                    .expect("valid short stale-claim age"),
            )
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core_execution::Clock>),
    ) as Arc<dyn lash_core_execution::ConformanceProcessRegistry>;
    let process_work = Arc::new(lash_core_execution::NoProcessWork::for_registry(
        Arc::clone(&registry) as Arc<dyn ProcessRegistry>,
    ));
    (
        _database_fixture,
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
    let Some((_database_fixture, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres wake ordering-group conformance: \
             LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let registry = Arc::new(storage.process_registry());
    let process_work = Arc::new(lash_core_execution::NoProcessWork::for_registry(
        Arc::clone(&registry) as Arc<dyn ProcessRegistry>,
    ));
    (
        _database_fixture,
        registry as Arc<dyn ProcessRegistry>,
        Arc::new(PostgresWakeDeliveryOrderingGroupFaultInjector {
            pool: storage.pool().clone(),
        }),
        process_work,
        lash_conformance::ProcessTerminalWaitWitness::Direct,
        || async {},
        || async {},
    )
});

struct PostgresWakeDeliveryIsolationBackend {
    storage: lash_postgres_store::PostgresStorage,
    clock: Arc<lash_core_execution::testing::TestClock>,
}

#[async_trait::async_trait]
impl lash_conformance::WakeDeliveryIsolationBackend for PostgresWakeDeliveryIsolationBackend {
    async fn corrupt_source(&self, process_id: &ProcessId) {
        assert_eq!(
            sqlx::query("UPDATE lash_processes SET record_json = '{}' WHERE process_id = $1")
                .bind(process_id.as_str())
                .execute(self.storage.pool())
                .await
                .expect("corrupt Postgres wake source")
                .rows_affected(),
            1
        );
    }

    async fn reopen(&self) -> (Arc<dyn DeploymentStore>, Arc<dyn ProcessRegistry>) {
        (
            Arc::new(
                self.storage
                    .session_store_factory()
                    .with_clock(self.clock.clone()),
            ),
            Arc::new(
                self.storage
                    .process_registry_with_wake_delivery_config(
                        lash_core_execution::WakeDeliveryConfig::new(10_000)
                            .expect("valid wake expiry")
                            .with_enqueuing_stale_after_ms(25)
                            .expect("valid claim lapse"),
                    )
                    .with_clock(self.clock.clone()),
            ),
        )
    }
}

lash_conformance::wake_delivery_isolation_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        panic!("wake isolation conformance requires LASH_POSTGRES_DATABASE_URL");
    };
    reset(storage.pool()).await;
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(
        1_800_000_000_000,
    ));
    let backend = Arc::new(PostgresWakeDeliveryIsolationBackend {
        storage,
        clock: clock.clone(),
    });
    let (factory, registry) =
        lash_conformance::WakeDeliveryIsolationBackend::reopen(backend.as_ref()).await;
    (
        database_fixture,
        factory,
        registry,
        clock,
        backend as Arc<dyn lash_conformance::WakeDeliveryIsolationBackend>,
    )
});

struct PostgresWakeRedeliveryFloors {
    pool: sqlx::PgPool,
}

#[async_trait::async_trait]
impl lash_conformance::WakeRedeliveryFloorProbe for PostgresWakeRedeliveryFloors {
    async fn receiver_floor(&self, session_id: &SessionId, process_id: &ProcessId) -> Option<u64> {
        sqlx::query_scalar::<_, i64>(
            "SELECT allocation_floor FROM lash_wake_redelivery_fences
             WHERE session_id = $1 AND process_id = $2",
        )
        .bind(session_id.as_str())
        .bind(process_id.as_str())
        .fetch_optional(&self.pool)
        .await
        .expect("read Postgres receiver floor")
        .map(|floor| u64::try_from(floor).expect("non-negative receiver floor"))
    }
}

lash_conformance::wake_delivery_conflict_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        panic!("wake conflict conformance requires LASH_POSTGRES_DATABASE_URL");
    };
    reset(storage.pool()).await;
    let clock = Arc::new(lash_core_execution::testing::TestClock::new(
        1_800_000_000_000,
    ));
    let factory = Arc::new(storage.session_store_factory().with_clock(clock.clone()))
        as Arc<dyn DeploymentStore>;
    let registry = Arc::new(
        storage
            .process_registry_with_wake_delivery_config(
                lash_core_execution::WakeDeliveryConfig::new(60_000).expect("valid wake expiry"),
            )
            .with_clock(clock.clone()),
    ) as Arc<dyn ProcessRegistry>;
    let floors = Arc::new(PostgresWakeRedeliveryFloors {
        pool: storage.pool().clone(),
    }) as Arc<dyn lash_conformance::WakeRedeliveryFloorProbe>;
    (
        database_fixture,
        factory,
        registry,
        clock,
        Arc::new(lash_core_execution::NoSessionWork::new())
            as Arc<dyn lash_core_execution::SessionWorkEngine>,
        floors,
    )
});
