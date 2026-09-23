//! SQLite's answer to the store maintenance outcome contract (ADR 0067 §4).

use lash_sansio::SessionId;
use std::sync::{Arc, Mutex};

use lash_core_execution::SessionStoreFactory;
use lash_sansio::sync::MutexExt;
use lash_sqlite_store::SqliteDatabase;

use super::Retained;
use crate::backend_fixture::TestBackend;

/// Corrupt the live checkpoint manifest so the mark phase cannot decode the
/// root it must follow. The sweep then has a real failure to report, which is
/// the arm SQLite used to swallow into `GcReport::default()`.
struct SqliteCorruptRootedManifest {
    backend: Arc<Mutex<Option<TestBackend>>>,
}

#[async_trait::async_trait]
impl lash_conformance::StoreMaintenanceFaultInjector for SqliteCorruptRootedManifest {
    async fn break_gc_scope(&self, _session_id: &SessionId) {
        let conn = self
            .backend
            .lock_recover()
            .clone()
            .expect("the law makes a factory before breaking it")
            .raw(SqliteDatabase::DurableCore);
        let corrupted = conn
            .execute(
                "UPDATE blobs SET content = X'FFFFFFFF'
                 WHERE hash IN (SELECT checkpoint_ref FROM session_head
                                WHERE checkpoint_ref IS NOT NULL)",
                [],
            )
            .expect("corrupt the rooted checkpoint manifest");
        assert!(
            corrupted >= 1,
            "the fault must corrupt at least one rooted manifest"
        );
    }
}

lash_conformance::store_maintenance_tests!({
    let retained = Retained::default();
    (retained.clone(), "sqlite", move || {
        retained.open_blocking().session_store_factory() as Arc<dyn SessionStoreFactory>
    })
});

lash_conformance::store_maintenance_fault_tests!({
    let retained = Retained::default();
    let backend = Arc::new(Mutex::new(None));
    let make_backend = Arc::clone(&backend);
    (
        retained.clone(),
        "sqlite",
        move || {
            let opened = retained.open_blocking();
            *make_backend.lock_recover() = Some(opened.clone());
            opened.session_store_factory() as Arc<dyn SessionStoreFactory>
        },
        Arc::new(SqliteCorruptRootedManifest { backend }),
    )
});
