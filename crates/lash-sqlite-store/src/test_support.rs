//! [`StoreTestSupport`] is the home for every `*_for_testing` hook the
//! lash-core conformance and differential suites need from this backend; the
//! production store traits carry none.

use super::*;
use lash_core_execution::store::{
    ConformancePersistence, ConformanceSessionStoreFactory, StoreTestSupport,
};

#[async_trait::async_trait]
impl StoreTestSupport for Store {
    async fn settle_session_ingress_for_testing(
        &self,
        fence: &lash_core_execution::store::DriveFence,
        settlement: lash_core_execution::store::IngressClaimSettlement,
    ) -> Result<lash_core_execution::store::IngressSettlementReceipt, StoreError> {
        let fence = fence.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                Ok(
                    match crate::persistence::apply_session_ingress_settlement_conn(
                        tx,
                        &fence,
                        &settlement,
                        now,
                    ) {
                        Ok(receipt) => TxOutcome::Commit(Ok(receipt)),
                        Err(error) => TxOutcome::Rollback(Err(error)),
                    },
                )
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn session_ingress_rows_for_testing(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<lash_core_execution::store::IngressItem>, StoreError> {
        let session_id = session_id.clone();
        self.conn
            .call(move |conn| {
                Ok(crate::persistence::session_ingress_rows_conn(
                    conn,
                    &session_id,
                ))
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn rewrite_session_tool_access_for_testing(
        &self,
        schema_version: u32,
        tool_access: Option<serde_json::Value>,
    ) -> Result<(), StoreError> {
        let session_id = self
            .resolve_session_id_for_read()
            .await?
            .ok_or(StoreError::SessionNotBound)?;
        self.conn
            .write(move |tx| {
                let head_json: String = tx.query_row(
                    crate::session_sql::session_sql()
                        .head
                        .select_head_json
                        .sql(),
                    params![session_id.as_str()],
                    |row| row.get(0),
                )?;
                let mut head: serde_json::Value = serde_json::from_str(&head_json)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                head["schema_version"] = serde_json::json!(schema_version);
                let config = head
                    .get_mut("config")
                    .and_then(serde_json::Value::as_object_mut)
                    .ok_or(rusqlite::Error::InvalidQuery)?;
                match tool_access {
                    Some(tool_access) => {
                        config.insert("tool_access".to_string(), tool_access);
                    }
                    None => {
                        config.remove("tool_access");
                    }
                }
                let head_json = serde_json::to_string(&head)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                tx.execute(
                    crate::session_sql::session_sql().head.set_head_json.sql(),
                    params![session_id.as_str(), head_json],
                )?;
                Ok(())
            })
            .await
            .map_err(sqlite_error)
    }

    async fn stamp_session_state_version_for_testing(
        &self,
        version: u32,
    ) -> Result<(), StoreError> {
        let session_id = self.selected_session_id()?;
        self.conn
            .write(move |tx| {
                tx.execute(
                    crate::session_sql::session_sql()
                        .meta
                        .set_state_version
                        .sql(),
                    params![session_id.as_str(), i64::from(version)],
                )?;
                Ok(())
            })
            .await
            .map_err(sqlite_error)
    }

    async fn stamp_session_state_version_and_corrupt_payload_for_testing(
        &self,
        version: u32,
    ) -> Result<(), StoreError> {
        let session_id = self.selected_session_id()?;
        self.conn
            .write(move |tx| {
                tx.execute(
                    crate::session_sql::session_sql()
                        .meta
                        .set_state_version
                        .sql(),
                    params![session_id.as_str(), i64::from(version)],
                )?;
                tx.execute(
                    crate::session_sql::session_sql()
                        .head
                        .corrupt_head_json
                        .sql(),
                    params![session_id.as_str()],
                )?;
                Ok(())
            })
            .await
            .map_err(sqlite_error)
    }
}

#[async_trait::async_trait]
impl ConformanceSessionStoreFactory for SqliteSessionStoreFactory {
    async fn create_conformance_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Arc<dyn ConformancePersistence>, StoreError> {
        Ok(self.create_bound_store(request).await?)
    }

    async fn open_existing_conformance_store(
        &self,
        request: &SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn ConformancePersistence>>, String> {
        Ok(self
            .open_existing_bound_store(request)
            .await?
            .map(|store| store as Arc<dyn ConformancePersistence>))
    }
}

/// An unbound durable-core store on a fresh memory backend. The store holds
/// the backend's anchors, so the database lives as long as it does.
#[cfg(test)]
pub(crate) async fn memory_store() -> tokio_rusqlite::Result<Store> {
    memory_store_with_options(crate::SqliteBackendOptions::memory().store).await
}

/// [`memory_store`] with explicit store options.
#[cfg(test)]
pub(crate) async fn memory_store_with_options(
    options: StoreOptions,
) -> tokio_rusqlite::Result<Store> {
    crate::SqliteBackend::memory_with_options_and_clock(
        crate::SqliteBackendOptions {
            store: options,
            ..crate::SqliteBackendOptions::memory()
        },
        Arc::new(lash_core_execution::facade_support::SystemClock),
    )
    .await?
    .open_store()
    .await
}
