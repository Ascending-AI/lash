//! The process change-horizon laws over a file deployment and a named
//! in-memory one (ADR 0102).

use std::sync::Arc;

use lash_core_execution::ProcessRegistry;
use lash_sqlite_store::SqliteDeployment;

mod file {
    use super::*;

    lash_conformance::process_change_horizon_tests!({
        let dir = tempfile::tempdir().expect("prune-horizon tempdir");
        let deployment = SqliteDeployment::open(dir.path())
            .await
            .expect("open the prune-horizon file deployment");
        let registry = deployment.process_registry() as Arc<dyn ProcessRegistry>;
        ((dir, deployment), registry)
    });
}

mod memory {
    use super::*;

    lash_conformance::process_change_horizon_tests!({
        let deployment = SqliteDeployment::memory()
            .await
            .expect("open the prune-horizon memory deployment");
        let registry = deployment.process_registry() as Arc<dyn ProcessRegistry>;
        (deployment, registry)
    });
}
