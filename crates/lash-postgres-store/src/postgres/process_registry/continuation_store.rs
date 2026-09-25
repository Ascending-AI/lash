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
        // An ended process takes no handover: its stored terminal revokes every
        // execution that would carry it on. The row lock the load takes holds to
        // the commit, so a terminal append serialises fully before or after this
        // put (FIG-3820).
        let record = require_process_tx(&mut tx, process_id).await?;
        if record.is_terminal() {
            return Err(PluginError::ProcessAlreadyTerminal {
                process_id: record.id.clone(),
                status: record.status,
            });
        }
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

    async fn segment_start(
        &self,
        segment: &lash_core_execution::ProcessSegmentKey,
    ) -> Result<Option<lash_core_execution::SegmentStartMarker>, PluginError> {
        let (process_id, segment_ordinal) = (&segment.process_id, segment.segment_ordinal);
        let started: Option<Option<String>> =
            sqlx::query_scalar(process_sql().handover.select_started.sql())
                .bind(process_id.as_str())
                .bind(segment_ordinal as i64)
                .fetch_optional(&self.pool)
                .await
                .map_err(plugin_sqlx_error)?;
        started
            .flatten()
            .map(|json| serde_json::from_str(&json).map_err(process_decode_error))
            .transpose()
    }

    async fn mark_segment_started(
        &self,
        segment: &lash_core_execution::ProcessSegmentKey,
        marker: lash_core_execution::SegmentStartMarker,
    ) -> Result<lash_core_execution::SegmentStartMarker, PluginError> {
        let (process_id, segment_ordinal) = (&segment.process_id, segment.segment_ordinal);
        let encoded = serde_json::to_string(&marker).map_err(process_decode_error)?;
        let mut tx = self.pool.begin().await.map_err(plugin_sqlx_error)?;
        sqlx::query(process_sql().handover.mark_started.sql())
            .bind(process_id.as_str())
            .bind(segment_ordinal as i64)
            .bind(encoded)
            .execute(&mut *tx)
            .await
            .map_err(plugin_sqlx_error)?;
        let started: Option<Option<String>> =
            sqlx::query_scalar(process_sql().handover.select_started.sql())
                .bind(process_id.as_str())
                .bind(segment_ordinal as i64)
                .fetch_optional(&mut *tx)
                .await
                .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        let Some(Some(recorded)) = started else {
            return Err(PluginError::Session(format!(
                "process `{process_id}` segment {segment_ordinal} has no retained handover to mark started"
            )));
        };
        serde_json::from_str(&recorded).map_err(process_decode_error)
    }

    async fn retire_segment_handovers_through(
        &self,
        process_id: &ProcessId,
        segment_ordinal: u64,
    ) -> Result<(), PluginError> {
        sqlx::query(process_sql().handover.delete_through.sql())
            .bind(process_id.as_str())
            .bind(segment_ordinal as i64)
            .execute(&self.pool)
            .await
            .map_err(plugin_sqlx_error)?;
        Ok(())
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
