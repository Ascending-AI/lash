//! SQLite proof that the process-prune delete path obeys the tombstone-reclaim
//! law: pruned process-session ids join the deleted set, and a delete drains
//! tombstoned rows owned by them.

use std::sync::Arc;

use lash_core_execution::{ProcessRegistry, SessionStoreFactory};
use lash_sqlite_store::SqliteDatabase;

use super::SUBSTRATE;
use crate::deployment_fixture::TestDeployment;

// The deployment's registry prunes the process-owned session stores out of
// the deployment's own catalog, which the factory owns.
lash_conformance::process_prune_reclaim_tests!({
    let deployment = TestDeployment::open(SUBSTRATE).await;
    let factory = deployment.session_store_factory() as Arc<dyn SessionStoreFactory>;
    let registry = deployment.process_registry() as Arc<dyn ProcessRegistry>;
    let probe = Arc::new(crate::blob_probe::SqliteBlobProbe::new(
        deployment.database_uri(SqliteDatabase::DurableCore),
        "fail_process_prune_blob_delete",
        None,
    ));
    (deployment, "sqlite", factory, registry, probe)
});
