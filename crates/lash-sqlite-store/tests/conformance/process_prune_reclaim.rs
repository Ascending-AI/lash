//! SQLite proof that the process-prune delete path obeys the tombstone-reclaim
//! law: pruned process-session ids join the deleted set, and a delete drains
//! tombstoned rows owned by them.

use std::sync::Arc;

use lash_core_execution::{DeploymentStore, ProcessExecutionEnvStore, ProcessRegistry};
use lash_sqlite_store::SqliteDatabase;

use super::SUBSTRATE;
use crate::backend_fixture::TestBackend;

// The backend's registry prunes the process-owned session stores out of
// the backend's own catalog, which the factory owns.
lash_conformance::process_prune_reclaim_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let factory = backend.store().await as Arc<dyn DeploymentStore>;
    let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
    let probe = Arc::new(crate::blob_probe::SqliteBlobProbe::new(
        backend.database_uri(SqliteDatabase::DurableCore),
        "fail_process_prune_blob_delete",
        None,
    ));
    (backend, "sqlite", factory, registry, probe)
});

lash_conformance::process_start_staging_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
    let ports = lash_core_execution::runtime::ArtifactReferrerPorts::new(
        lash_core_execution::StoreSet::module_artifacts(&*backend),
        backend.process_env_store(),
        lash_core_execution::StoreSet::artifact_cleanup(&*backend),
        Arc::new(lash_core_execution::facade_support::SystemClock),
    );
    (backend, registry, ports)
});

lash_conformance::process_prune_start_staging_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
    let env_store = backend.blocking_store() as Arc<dyn ProcessExecutionEnvStore>;
    (backend, registry, env_store)
});
