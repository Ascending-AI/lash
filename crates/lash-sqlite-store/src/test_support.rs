//! Test-only store probes, compiled only under `cfg(any(test, feature = "testing"))`.
//!
//! [`StoreTestSupport`] is the home for every `*_for_testing` hook the
//! lash-core conformance and differential suites need from this backend; the
//! production store traits carry none.

use super::*;
use lash_core::store::{ConformancePersistence, ConformanceSessionStoreFactory, StoreTestSupport};

#[async_trait::async_trait]
impl StoreTestSupport for Store {
    async fn rewrite_session_tool_access_for_testing(
        &self,
        schema_version: u32,
        tool_access: Option<serde_json::Value>,
    ) -> Result<(), StoreError> {
        let session_id = self.selected_session_id()?;
        self.conn
            .write(move |tx| {
                let head_json: String = tx.query_row(
                    "SELECT head_json FROM session_head WHERE session_id = ?1",
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
                    "UPDATE session_head SET head_json = ?2 WHERE session_id = ?1",
                    params![session_id.as_str(), head_json],
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
                    "UPDATE session_meta SET session_state_version = ?2 WHERE session_id = ?1",
                    params![session_id.as_str(), i64::from(version)],
                )?;
                tx.execute(
                    "UPDATE session_head SET head_json = '{not-current-json' WHERE session_id = ?1",
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
