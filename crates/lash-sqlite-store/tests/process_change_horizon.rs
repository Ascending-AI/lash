use std::sync::Arc;

use lash_core::ProcessRegistry;
use lash_sqlite_store::SqliteProcessRegistry;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_change_feed_refuses_cursor_below_tombstone_compaction_horizon() {
    let dir = tempfile::tempdir().expect("prune-horizon tempdir");
    let registry = Arc::new(
        SqliteProcessRegistry::open(
            &dir.path().join("processes.db"),
            dir.path().join("sessions"),
        )
        .await
        .expect("open prune-horizon registry"),
    ) as Arc<dyn ProcessRegistry>;
    lash_core::testing::conformance::process_change_cursor_below_tombstone_compaction_horizon_is_refused(
        registry,
    )
    .await;
}
