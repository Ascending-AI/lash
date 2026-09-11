//! Test-only store probes, compiled only under `cfg(any(test, feature = "testing"))`.
//!
//! [`StoreTestSupport`] is the home for every `*_for_testing` hook the
//! lash-core conformance and differential suites need from this backend; the
//! production store traits carry none.

use crate::*;
use lash_core::store::{ConformancePersistence, ConformanceSessionStoreFactory, StoreTestSupport};

#[async_trait::async_trait]
impl StoreTestSupport for PostgresSessionStore {
    async fn stamp_session_state_version_and_corrupt_payload_for_testing(
        &self,
        version: u32,
    ) -> Result<(), StoreError> {
        let mut connection = acquire_runtime_connection(&self.pool).await?;
        let mut tx = connection.begin().await.map_err(store_sqlx_error)?;
        sqlx::query(
            "UPDATE lash_session_meta SET session_state_version = $2 WHERE session_id = $1",
        )
        .bind(self.session_id.as_str())
        .bind(
            i32::try_from(version).map_err(|_| StoreError::StoredDataCorrupt {
                record_kind: "SessionStateVersion",
                message: format!("test marker {version} exceeds PostgreSQL INTEGER"),
            })?,
        )
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        sqlx::query(
            "UPDATE lash_sessions SET head_json = '{not-current-json' WHERE session_id = $1",
        )
        .bind(self.session_id.as_str())
        .execute(&mut *tx)
        .await
        .map_err(store_sqlx_error)?;
        tx.commit().await.map_err(store_sqlx_error)
    }
}

#[async_trait::async_trait]
impl ConformanceSessionStoreFactory for PostgresSessionStoreFactory {
    async fn create_conformance_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn ConformancePersistence>, StoreError> {
        Ok(self.create_session_store(request).await?)
    }

    async fn open_existing_conformance_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn ConformancePersistence>>, String> {
        Ok(self
            .open_existing_session_store(request)
            .await?
            .map(|store| store as Arc<dyn ConformancePersistence>))
    }
}

impl PostgresSessionStoreFactory {
    /// Drive transaction admission time independently of record timestamps.
    pub fn with_lease_clock_for_testing(mut self, clock: Arc<dyn lash_core::Clock>) -> Self {
        self.lease_clock_for_testing = Some(clock);
        self
    }
}

impl PostgresSessionStore {
    /// Drive transaction admission time independently of record timestamps.
    pub fn with_lease_clock_for_testing(mut self, clock: Arc<dyn lash_core::Clock>) -> Self {
        self.lease_clock_for_testing = Some(clock);
        self
    }
}

pub(crate) async fn set_transaction_lease_clock_for_testing(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    clock: Option<&Arc<dyn lash_core::Clock>>,
) -> Result<(), StoreError> {
    if let Some(clock) = clock {
        sqlx::query("SELECT set_config('lash.test_lease_epoch_ms', $1, true)")
            .bind(clock.timestamp_ms().to_string())
            .execute(&mut **tx)
            .await
            .map_err(store_sqlx_error)?;
    }
    Ok(())
}

impl PostgresSessionStore {
    pub(crate) async fn set_transaction_lease_clock_for_testing(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), StoreError> {
        set_transaction_lease_clock_for_testing(tx, self.lease_clock_for_testing.as_ref()).await
    }
}
