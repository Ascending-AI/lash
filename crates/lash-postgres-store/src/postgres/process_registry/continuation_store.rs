use super::*;
use lash_sansio::ProcessId;

#[async_trait::async_trait]
impl ProcessContinuationStore for PostgresProcessRegistry {
    async fn put_segment_handover(
        &self,
        process_id: &ProcessId,
        handover: PersistedSegmentHandover,
    ) -> Result<(), PluginError> {
        let encoded = serde_json::to_string(&handover).map_err(process_decode_error)?;
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let result = sqlx::query(process_sql().handover_postgres.upsert_identical.sql())
            .bind(process_id.as_str())
            .bind(handover.segment_ordinal as i64)
            .bind(encoded)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        if result.rows_affected() == 0 {
            return Err(PluginError::Session(format!(
                "process `{process_id}` segment {} handover conflict",
                handover.segment_ordinal
            )));
        }
        sqlx::query(process_sql().handover.delete_superseded.sql())
            .bind(process_id.as_str())
            .bind(handover.segment_ordinal as i64)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(())
    }

    async fn get_segment_handover(
        &self,
        process_id: &ProcessId,
        segment_ordinal: u64,
    ) -> Result<Option<PersistedSegmentHandover>, PluginError> {
        let json: Option<String> =
            sqlx::query_scalar(process_sql().handover.select_by_ordinal.sql())
                .bind(process_id.as_str())
                .bind(segment_ordinal as i64)
                .fetch_optional(&self.pool)
                .await
                .map_err(plugin_sqlx_error)?;
        json.map(|json| serde_json::from_str(&json).map_err(process_decode_error))
            .transpose()
    }

    async fn latest_segment_handover(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<PersistedSegmentHandover>, PluginError> {
        let json: Option<String> = sqlx::query_scalar(process_sql().handover.select_latest.sql())
            .bind(process_id.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        json.map(|json| serde_json::from_str(&json).map_err(process_decode_error))
            .transpose()
    }

    async fn delete_segment_handovers(&self, process_id: &ProcessId) -> Result<(), PluginError> {
        sqlx::query(process_sql().handover.delete_by_process.sql())
            .bind(process_id.as_str())
            .execute(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        Ok(())
    }
}
