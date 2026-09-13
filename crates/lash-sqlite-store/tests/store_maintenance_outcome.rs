//! SQLite's answer to the store maintenance outcome contract (ADR 0067 §4).

use lash_sansio::SessionId;
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
impl lash_conformance::StoreMaintenanceFaultInjector for SqliteCorruptRootedManifest {
    async fn break_gc_scope(&self, _session_id: &SessionId) {
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

lash_conformance::store_maintenance_tests!({
    let dirs = Arc::new(Mutex::new(Vec::new()));
    let retained_dirs = Arc::clone(&dirs);
    (retained_dirs, "sqlite", move || {
        let dir = tempfile::tempdir().expect("tempdir");
        let factory =
            Arc::new(SqliteSessionStoreFactory::new(dir.path())) as Arc<dyn SessionStoreFactory>;
        dirs.lock_recover().push(dir);
        factory
    })
});

lash_conformance::store_maintenance_fault_tests!({
    let dirs = Arc::new(Mutex::new(Vec::new()));
    let catalog = Arc::new(Mutex::new(None));
    let make_catalog = Arc::clone(&catalog);
    (
        Arc::clone(&dirs),
        "sqlite",
        move || {
            let dir = tempfile::tempdir().expect("tempdir");
            *make_catalog.lock_recover() = Some(dir.path().join("durable-core.db"));
            let factory = Arc::new(SqliteSessionStoreFactory::new(dir.path()))
                as Arc<dyn SessionStoreFactory>;
            dirs.lock_recover().push(dir);
            factory
        },
        Arc::new(SqliteCorruptRootedManifest { catalog }),
    )
});
