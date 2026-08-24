use super::*;

struct SqliteWakeDeliveryOrderingGroupFaultInjector {
    path: PathBuf,
}

#[async_trait::async_trait]
impl lash_core::testing::conformance::WakeDeliveryOrderingGroupFaultInjector
    for SqliteWakeDeliveryOrderingGroupFaultInjector
{
    async fn discard_without_reason(&self, delivery_id: &str) {
        let conn = rusqlite::Connection::open(&self.path)
            .expect("open SQLite process registry for reasonless discard injection");
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_wake_delivery_crash_matrix() {
    let dir = tempfile::tempdir().expect("tempdir");
    let process_registry_path = dir.path().join("processes.db");
    let clock = Arc::new(lash_core::testing::TestClock::new(1_800_000_000_000));
    let registry = Arc::new(
        SqliteProcessRegistry::open_with_clock(
            &process_registry_path,
            Arc::clone(&clock) as Arc<dyn lash_core::Clock>,
            dir.path().join("sessions"),
        )
        .await
        .expect("open process registry")
        .with_wake_delivery_config(
            lash_core::WakeDeliveryConfig::new(10_000)
                .expect("valid test retention")
                .with_enqueuing_stale_after_ms(25)
                .expect("valid short stale-claim age"),
        ),
    ) as Arc<dyn ProcessRegistry>;
    let factory = Arc::new(
        SqliteSessionStoreFactory::new_with_process_registry(dir.path(), process_registry_path)
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core::Clock>),
    ) as Arc<dyn SessionStoreFactory>;
    Box::pin(lash_core::testing::conformance::wake_delivery_crash_matrix(
        factory, registry, clock,
    ))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_wake_delivery_ordering_group_conformance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let process_registry_path = dir.path().join("processes.db");
    let registry = Arc::new(
        SqliteProcessRegistry::open(&process_registry_path, dir.path().join("sessions"))
            .await
            .expect("open process registry"),
    );
    lash_core::testing::conformance::wake_delivery_ordering_group_conformance(
        registry as Arc<dyn ProcessRegistry>,
        Arc::new(SqliteWakeDeliveryOrderingGroupFaultInjector {
            path: process_registry_path,
        }),
    )
    .await;
}
