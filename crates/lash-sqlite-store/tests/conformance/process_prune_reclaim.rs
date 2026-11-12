//! SQLite proof that the process-prune delete path obeys the tombstone-reclaim
//! law: pruned process-session ids join the deleted set, and a delete drains
//! tombstoned rows owned by them.

use std::sync::Arc;

use lash_execution_contracts::deployment::DeploymentStore;
use lash_execution_contracts::process::ProcessEnvStore;
use lash_execution_contracts::process::ProcessRegistry;

use super::SUBSTRATE;
use crate::backend_fixture::TestBackend;

// The backend's registry prunes the process-owned session stores out of
// the backend's own catalog, which the factory owns.
lash_conformance::process_prune_reclaim_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let factory = backend.store().await as Arc<dyn DeploymentStore>;
    lash_execution_contracts::testing::process_execution_env_fixture(
        backend.process_env_store().as_ref(),
    )
    .await;
    let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
    let probe = Arc::new(crate::blob_probe::SqliteBlobProbe::new(
        backend.database_uri(),
        "fail_process_prune_blob_delete",
        None,
    ));
    (backend, "sqlite", factory, registry, probe)
});

/// Reads a process row's state revision and faults the parent-end table.
struct SqliteProcessRowProbe(TestBackend);

#[async_trait::async_trait]
impl lash_conformance::ProcessRowProbe for SqliteProcessRowProbe {
    async fn state_rev(&self, process_id: &lash_vocabulary::identity::ProcessId) -> i64 {
        self.0
            .raw()
            .query_row(
                "SELECT state_rev FROM processes WHERE process_id = ?1",
                [process_id.as_str()],
                |row| row.get(0),
            )
            .expect("read the process row's state revision")
    }

    async fn fail_parent_end_plan_writes(&self) {
        self.0
            .raw()
            .execute_batch(
                "CREATE TRIGGER fail_parent_end_plan_write BEFORE INSERT ON parent_end_plans \
                 BEGIN SELECT RAISE(ABORT, 'injected parent-end plan write failure'); END;",
            )
            .expect("install the plan-write fault");
    }

    async fn clear_injected_failure(&self) {
        self.0
            .raw()
            .execute_batch("DROP TRIGGER fail_parent_end_plan_write")
            .expect("remove the plan-write fault");
    }
}

lash_conformance::process_row_packing_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    lash_execution_contracts::testing::process_execution_env_fixture(
        backend.process_env_store().as_ref(),
    )
    .await;
    let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
    let probe = Arc::new(SqliteProcessRowProbe(backend.clone()))
        as Arc<dyn lash_conformance::ProcessRowProbe>;
    (backend, registry, probe)
});

lash_conformance::process_start_staging_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
    let ports = lash_core_execution::runtime::ArtifactReferrerPorts::new(
        lash_execution_contracts::backend::StoreSet::module_artifacts(&*backend),
        backend.process_env_store(),
        lash_execution_contracts::backend::StoreSet::definition_store(&*backend),
        lash_execution_contracts::backend::StoreSet::attachment_referrers(&*backend),
        lash_execution_contracts::backend::StoreSet::artifact_cleanup(&*backend),
        Arc::new(lash_core_ids::clock::SystemClock),
    );
    (backend, registry, ports)
});

lash_conformance::start_operation_staging_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let law = backend.as_backend();
    (backend, law)
});

lash_conformance::process_definition_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
    let ports = lash_core_execution::runtime::ArtifactReferrerPorts::new(
        lash_execution_contracts::backend::StoreSet::module_artifacts(&*backend),
        backend.process_env_store(),
        lash_execution_contracts::backend::StoreSet::definition_store(&*backend),
        lash_execution_contracts::backend::StoreSet::attachment_referrers(&*backend),
        lash_execution_contracts::backend::StoreSet::artifact_cleanup(&*backend),
        Arc::new(lash_core_ids::clock::SystemClock),
    );
    (backend, registry, ports)
});

lash_conformance::process_definition_graph_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
    let ports = lash_core_execution::runtime::ArtifactReferrerPorts::new(
        lash_execution_contracts::backend::StoreSet::module_artifacts(&*backend),
        backend.process_env_store(),
        lash_execution_contracts::backend::StoreSet::definition_store(&*backend),
        lash_execution_contracts::backend::StoreSet::attachment_referrers(&*backend),
        lash_execution_contracts::backend::StoreSet::artifact_cleanup(&*backend),
        Arc::new(lash_core_ids::clock::SystemClock),
    );
    let probe = Arc::new(crate::blob_probe::SqliteDefinitionGraphProbe::new(
        backend.database_uri(),
    )) as Arc<dyn lash_conformance::DefinitionGraphProbe>;
    (backend, registry, ports, probe)
});

lash_conformance::process_prune_start_staging_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
    let env_store = backend.blocking_store() as Arc<dyn ProcessEnvStore>;
    (backend, registry, env_store)
});
