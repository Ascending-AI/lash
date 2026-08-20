//! SQLite's answer to the store maintenance outcome contract (ADR 0067 §4).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use lash_core::SessionStoreFactory;
use lash_sansio::sync::MutexExt;
use lash_sqlite_store::SqliteSessionStoreFactory;

/// Corrupt the live checkpoint manifest so the mark phase cannot decode the
/// root it must follow. The sweep then has a real failure to report, which is
/// the arm SQLite used to swallow into `GcReport::default()`.
struct SqliteCorruptRootedManifest {
    catalog: Arc<Mutex<Option<PathBuf>>>,
}

#[async_trait::async_trait]
impl lash_core::testing::conformance::StoreMaintenanceFaultInjector
    for SqliteCorruptRootedManifest
{
    async fn break_gc_scope(&self, _session_id: &str) {
        let catalog = self
            .catalog
            .lock_recover()
            .clone()
            .expect("the law makes a factory before breaking it");
        let conn = rusqlite::Connection::open(&catalog).expect("open catalog for corruption");
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

#[tokio::test]
async fn sqlite_store_satisfies_the_maintenance_outcome_contract() {
    let dirs = Arc::new(Mutex::new(Vec::new()));
    let catalog = Arc::new(Mutex::new(None));
    let injector_catalog = Arc::clone(&catalog);
    lash_core::testing::conformance::store_maintenance_outcome_contract(
        "sqlite",
        || {
            let dir = tempfile::tempdir().expect("tempdir");
            *catalog.lock_recover() = Some(dir.path().join("durable-core.db"));
            let factory = Arc::new(SqliteSessionStoreFactory::new(dir.path()))
                as Arc<dyn SessionStoreFactory>;
            dirs.lock_recover().push(dir);
            factory
        },
        Some(Arc::new(SqliteCorruptRootedManifest {
            catalog: injector_catalog,
        })),
    )
    .await;
}
