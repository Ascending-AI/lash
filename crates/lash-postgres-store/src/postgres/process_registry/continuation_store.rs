use super::*;

#[async_trait::async_trait]
impl ProcessContinuationStore for PostgresProcessRegistry {
    async fn put_segment_handover(
        &self,
        process_id: &str,
        handover: PersistedSegmentHandover,
    ) -> Result<(), PluginError> {
        let encoded = serde_json::to_string(&handover).map_err(process_decode_error)?;
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        let result = sqlx::query(
            "INSERT INTO lash_process_segment_handovers
             (process_id, segment_ordinal, handover_json) VALUES ($1, $2, $3)
             ON CONFLICT (process_id, segment_ordinal) DO UPDATE
             SET handover_json = EXCLUDED.handover_json
             WHERE lash_process_segment_handovers.handover_json = EXCLUDED.handover_json",
        )
        .bind(process_id)
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
        sqlx::query(
            "DELETE FROM lash_process_segment_handovers
             WHERE process_id = $1 AND segment_ordinal < $2 - 1",
        )
        .bind(process_id)
        .bind(handover.segment_ordinal as i64)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(())
    }

    async fn get_segment_handover(
        &self,
        process_id: &str,
        segment_ordinal: u64,
    ) -> Result<Option<PersistedSegmentHandover>, PluginError> {
        let json: Option<String> = sqlx::query_scalar(
            "SELECT handover_json FROM lash_process_segment_handovers
             WHERE process_id = $1 AND segment_ordinal = $2",
        )
        .bind(process_id)
        .bind(segment_ordinal as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        json.map(|json| serde_json::from_str(&json).map_err(process_decode_error))
            .transpose()
    }

    async fn latest_segment_handover(
        &self,
        process_id: &str,
    ) -> Result<Option<PersistedSegmentHandover>, PluginError> {
        let json: Option<String> = sqlx::query_scalar(
            "SELECT handover_json FROM lash_process_segment_handovers
             WHERE process_id = $1 ORDER BY segment_ordinal DESC LIMIT 1",
        )
        .bind(process_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(plugin_sqlx_error)?;
        json.map(|json| serde_json::from_str(&json).map_err(process_decode_error))
            .transpose()
    }

    async fn delete_segment_handovers(&self, process_id: &str) -> Result<(), PluginError> {
        sqlx::query("DELETE FROM lash_process_segment_handovers WHERE process_id = $1")
            .bind(process_id)
            .execute(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        Ok(())
    }
}
