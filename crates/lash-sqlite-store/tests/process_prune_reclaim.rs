//! SQLite proof that the process-prune delete path obeys the tombstone-reclaim
//! law: pruned process-session ids join the deleted set, and a delete drains
//! tombstoned rows owned by them.
//!
//! This lives in its own test target rather than the conformance suite, which is
//! at its line budget.

use std::sync::Arc;

use lash_core::{ProcessRegistry, SessionStoreFactory};
use lash_sqlite_store::{SqliteProcessRegistry, SqliteSessionStoreFactory};

#[path = "blob_probe.rs"]
mod blob_probe;

lash_conformance::process_prune_reclaim_tests!({
    let (dir, factory, registry) = backend().await;
    let probe = Arc::new(blob_probe::SqliteBlobProbe::new(
        dir.path().join("sessions/durable-core.db"),
        "fail_process_prune_blob_delete",
        None,
    ));
    (dir, "sqlite", factory, registry, probe)
});

/// A session factory and process registry sharing one temp root, so the prune
/// path deletes the process-owned session stores the factory owns. The returned
/// temp dir must outlive both.
async fn backend() -> (
    tempfile::TempDir,
    Arc<dyn SessionStoreFactory>,
    Arc<dyn ProcessRegistry>,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    let process_path = dir.path().join("processes.db");
    let sessions = dir.path().join("sessions");
    let registry = Arc::new(
        SqliteProcessRegistry::open(&process_path, &sessions)
            .await
            .expect("process registry"),
    ) as Arc<dyn ProcessRegistry>;
    let factory = Arc::new(SqliteSessionStoreFactory::new_with_process_registry(
        &sessions,
        &process_path,
    )) as Arc<dyn SessionStoreFactory>;
    (dir, factory, registry)
}
