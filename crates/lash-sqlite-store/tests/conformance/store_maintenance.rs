//! SQLite's answer to the store maintenance outcome contract (ADR 0067 §4).

use lash_vocabulary::identity::SessionId;
use std::sync::{Arc, Mutex};

use lash_execution_contracts::deployment::DeploymentStore;
use lash_protocol_types::sync::MutexExt;

use super::Retained;
use crate::backend_fixture::TestBackend;

/// Drop the root catalog, so the collection cannot tell a root from an
/// orphan. The sweep then has a real failure to report, which is the arm
/// SQLite used to swallow into `GcReport::default()`.
struct SqliteBreakRootCatalog {
    backend: Arc<Mutex<Option<TestBackend>>>,
}

#[async_trait::async_trait]
impl lash_conformance::StoreMaintenanceFaultInjector for SqliteBreakRootCatalog {
    async fn break_gc_scope(&self, _session_id: &SessionId) {
        let conn = self
            .backend
            .lock_recover()
            .clone()
            .expect("the law makes a factory before breaking it")
            .raw();
        conn.execute_batch("DROP VIEW graph_root_catalog")
            .expect("drop the root catalog");
    }
}

lash_conformance::store_maintenance_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    let make_retained = retained.clone();
    let bytes_retained = retained.clone();
    (
        retained,
        "sqlite",
        move || make_retained.open_blocking().blocking_store() as Arc<dyn DeploymentStore>,
        move || {
            bytes_retained.open_blocking().attachment_store()
                as Arc<dyn lash_core_store::attachments::AttachmentStore>
        },
    )
});

lash_conformance::store_maintenance_fault_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    let backend = Arc::new(Mutex::new(None));
    let make_backend = Arc::clone(&backend);
    (
        retained.clone(),
        "sqlite",
        move || {
            let opened = retained.open_blocking();
            *make_backend.lock_recover() = Some(opened.clone());
            opened.blocking_store() as Arc<dyn DeploymentStore>
        },
        Arc::new(SqliteBreakRootCatalog { backend }),
    )
});
