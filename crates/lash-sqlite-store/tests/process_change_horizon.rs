//! The process change-horizon laws over file and named memory
//! SQLite store sets (ADR 0102).

use std::sync::Arc;

use lash_core_execution::ProcessRegistry;
use lash_sqlite_store::SqliteStoreSet;

mod file {
    use super::*;

    lash_conformance::process_change_horizon_tests!({
        let dir = tempfile::tempdir().expect("prune-horizon tempdir");
        let backend = SqliteStoreSet::open(dir.path().join("lash.db"))
            .await
            .expect("open the prune-horizon file backend");
        lash_core::testing::process_execution_env_fixture(backend.process_env_store().as_ref())
            .await;
        let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
        ((dir, backend), registry)
    });
}

mod memory {
    use super::*;

    lash_conformance::process_change_horizon_tests!({
        let backend = SqliteStoreSet::memory()
            .await
            .expect("open the prune-horizon memory backend");
        lash_core::testing::process_execution_env_fixture(backend.process_env_store().as_ref())
            .await;
        let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
        (backend, registry)
    });
}
