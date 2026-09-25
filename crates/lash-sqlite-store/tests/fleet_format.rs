//! SQLite under the shared fleet-format law.

use std::path::PathBuf;

use async_trait::async_trait;
use lash_conformance::{FleetFormatDeployment, fleet_format_conformance};
use lash_core_execution::{FleetFormat, StoreError, StorePreflight, StoreSchemaStatus};
use lash_sqlite_store::{SqliteStorePreflight, Store};

struct SqliteBackend {
    _root: tempfile::TempDir,
    durable_core: PathBuf,
}

#[async_trait]
impl FleetFormatDeployment for SqliteBackend {
    async fn open(&self) -> Result<FleetFormat, StoreError> {
        Store::open(&self.durable_core)
            .await
            .map(|store| store.fleet_format())
            .map_err(|err| StoreError::Backend(err.to_string()))
    }

    async fn preflight(&self) -> Result<StoreSchemaStatus, StoreError> {
        SqliteStorePreflight::for_durable_core(&self.durable_core)
            .schema_status()
            .await
    }
}

#[tokio::test]
async fn sqlite_fleet_format_conformance() {
    let root = tempfile::tempdir().expect("scratch directory");
    let durable_core = root.path().join("durable-core.db");
    let backend = SqliteBackend {
        _root: root,
        durable_core,
    };
    fleet_format_conformance(&backend).await;
}
