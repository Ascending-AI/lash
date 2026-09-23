//! SQLite proof that the process-prune delete path obeys the tombstone-reclaim
//! law: pruned process-session ids join the deleted set, and a delete drains
//! tombstoned rows owned by them.

use std::sync::Arc;

use lash_core_execution::{ProcessRegistry, SessionStoreFactory};
use lash_sqlite_store::SqliteDatabase;

use super::SUBSTRATE;
use crate::backend_fixture::TestBackend;

// The backend's registry prunes the process-owned session stores out of
// the backend's own catalog, which the factory owns.
lash_conformance::process_prune_reclaim_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let factory = backend.session_store_factory() as Arc<dyn SessionStoreFactory>;
    let registry = backend.process_registry() as Arc<dyn ProcessRegistry>;
    let probe = Arc::new(crate::blob_probe::SqliteBlobProbe::new(
        backend.database_uri(SqliteDatabase::DurableCore),
        "fail_process_prune_blob_delete",
        None,
    ));
    (backend, "sqlite", factory, registry, probe)
});
