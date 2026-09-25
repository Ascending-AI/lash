//! PostgreSQL under the shared release-stamp law.

use async_trait::async_trait;
use lash_conformance::{ReleaseStampDeployment, release_stamp_conformance};
use lash_core_execution::{StoreError, StorePreflight, StoreSchemaStatus};
use lash_postgres_store::{PostgresStorePreflight, SchemaCheck};

use crate::harness::ScratchSchema;
use crate::support::database_url;

struct PostgresDeployment {
    scratch: ScratchSchema,
}

#[async_trait]
impl ReleaseStampDeployment for PostgresDeployment {
    fn build_release(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    async fn open(&self) -> Result<(), StoreError> {
        self.scratch
            .open_host_provisioned(SchemaCheck::Enforce)
            .await
            .map(|_storage| ())
    }

    async fn preflight(&self) -> Result<StoreSchemaStatus, StoreError> {
        PostgresStorePreflight::from_pool(self.scratch.pool.clone())
            .schema_status()
            .await
    }

    async fn force_release(&self, release: &str) -> Result<(), StoreError> {
        let changed = sqlx::query(
            "UPDATE lash_release_stamp SET release_version = $1 WHERE singleton = TRUE",
        )
        .bind(release)
        .execute(&self.scratch.pool)
        .await
        .map_err(|err| StoreError::Backend(err.to_string()))?;
        assert_eq!(
            changed.rows_affected(),
            1,
            "the database must already carry a stamp to force"
        );
        Ok(())
    }
}

#[tokio::test]
async fn postgres_release_stamp_conformance() {
    let Some(database_url) = database_url() else {
        eprintln!("skipping release stamp conformance: database URL is not set");
        return;
    };
    // The committed DDL creates `lash_release_stamp` and seeds no row, so a
    // host-provisioned schema starts unstamped — the law's first precondition.
    let deployment = PostgresDeployment {
        scratch: ScratchSchema::provision(&database_url).await,
    };
    release_stamp_conformance(&deployment).await;
}
