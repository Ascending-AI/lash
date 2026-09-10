//! Test-only store probes, compiled only under `cfg(any(test, feature = "testing"))`.
//!
//! [`StoreTestSupport`] is the home for every `*_for_testing` hook the
//! lash-core conformance and differential suites need from this backend; the
//! production store traits carry none.

use super::*;
use lash_core::store::{ConformancePersistence, ConformanceSessionStoreFactory, StoreTestSupport};
use lash_sansio::SessionId;

#[async_trait::async_trait]
impl StoreTestSupport for Store {
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

    async fn seed_session_trigger_manifest_ref_for_testing(
        &self,
        session_id: &SessionId,
    ) -> Result<bool, StoreError> {
        let artifact_ref = lash_core::TriggerOwnerScope::session(session_id).namespace();
        let blob_ref = format!("testing-trigger-manifest:{session_id}");
        self.conn
            .write(move |tx| {
                tx.execute(
                    "INSERT OR IGNORE INTO blobs (hash, content) VALUES (?1, X'01')",
                    params![blob_ref],
                )?;
                tx.execute(
                    "INSERT OR REPLACE INTO artifact_refs (namespace, artifact_ref, blob_ref)
                     VALUES (?1, ?2, ?3)",
                    params![
                        crate::attachments::CURRENT_TRIGGER_MANIFEST_NAMESPACE,
                        artifact_ref,
                        blob_ref
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(sqlite_error)?;
        Ok(true)
    }

    async fn raw_session_owned_artifact_refs_for_testing(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<(String, String)>, StoreError> {
        let artifact_ref = lash_core::TriggerOwnerScope::session(session_id).namespace();
        self.conn
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "SELECT namespace, artifact_ref
                     FROM artifact_refs
                     WHERE namespace = ?1 AND artifact_ref = ?2
                     ORDER BY namespace, artifact_ref",
                )?;
                statement
                    .query_map(
                        params![
                            crate::attachments::CURRENT_TRIGGER_MANIFEST_NAMESPACE,
                            artifact_ref
                        ],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?
                    .collect()
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
