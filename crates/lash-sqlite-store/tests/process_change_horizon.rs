//! The process change-horizon laws over a file backend and a named
//! in-memory one (ADR 0102).

use std::sync::Arc;

use lash_core_execution::ProcessRegistry;
use lash_sqlite_store::SqliteStoreSet;

mod file {
    use super::*;

    lash_conformance::process_change_horizon_tests!({
        let dir = tempfile::tempdir().expect("prune-horizon tempdir");
        let backend = SqliteStoreSet::open(dir.path())
            .await
            .expect("open the prune-horizon file backend");
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
        let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
        (backend, registry)
    });
}
