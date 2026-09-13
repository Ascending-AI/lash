use std::sync::Arc;

use lash_core::ProcessRegistry;
use lash_sqlite_store::SqliteProcessRegistry;

lash_conformance::process_change_horizon_tests!({
    let dir = tempfile::tempdir().expect("prune-horizon tempdir");
    let registry = Arc::new(
        SqliteProcessRegistry::open(
            &dir.path().join("processes.db"),
            dir.path().join("sessions"),
        )
        .await
        .expect("open prune-horizon registry"),
    ) as Arc<dyn ProcessRegistry>;
    (dir, registry)
});
