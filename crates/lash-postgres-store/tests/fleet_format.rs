//! PostgreSQL under the shared fleet-format law.

use async_trait::async_trait;
use lash_conformance::{FleetFormatDeployment, fleet_format_conformance};
use lash_core_execution::{FleetFormat, StoreError, StorePreflight, StoreSchemaStatus};
use lash_postgres_store::{PostgresStorePreflight, SchemaCheck};

use crate::harness::ScratchSchema;
use crate::support::database_url;

struct PostgresDeployment {
    scratch: ScratchSchema,
}

#[async_trait]
impl FleetFormatDeployment for PostgresDeployment {
    async fn open(&self) -> Result<FleetFormat, StoreError> {
        self.scratch
            .open_host_provisioned(SchemaCheck::Enforce)
            .await
            .map(|storage| storage.fleet_format())
    }

    async fn preflight(&self) -> Result<StoreSchemaStatus, StoreError> {
        PostgresStorePreflight::from_pool(self.scratch.pool.clone())
            .schema_status()
            .await
    }
}

#[tokio::test]
async fn postgres_fleet_format_conformance() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping fleet format conformance: database URL is not set");
        return;
    };
    // The committed DDL creates `lash_fleet_format` and seeds no row, so a
    // host-provisioned schema starts unrecorded — the law's first
    // precondition.
    let deployment = PostgresDeployment {
        scratch: ScratchSchema::provision(&database_url).await,
    };
    fleet_format_conformance(&deployment).await;
}
