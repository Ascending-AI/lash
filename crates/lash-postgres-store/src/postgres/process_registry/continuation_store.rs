use super::*;
use lash_sansio::ProcessId;

#[async_trait::async_trait]
impl ProcessContinuationStore for PostgresProcessRegistry {
    async fn put_segment_handover(
        &self,
        process_id: &ProcessId,
        handover: PersistedSegmentHandover,
    ) -> Result<
        lash_core_execution::store::StoreTransition<lash_core_execution::SegmentHandoverCommit>,
        PluginError,
    > {
        let encoded = serde_json::to_string(&handover).map_err(process_decode_error)?;
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        // An ended process takes no handover: its stored terminal revokes every
        // execution that would carry it on. The row lock the load takes holds to
        // the commit, so a terminal append serialises fully before or after this
        // put (FIG-3820).
        let record = require_process_tx(&mut tx, process_id).await?;
        if record.is_terminal() {
            return Err(PluginError::ProcessAlreadyTerminal {
                process_id: record.id.clone(),
                status: record.status(),
            });
        }
        let existing: Option<String> =
            sqlx::query_scalar(process_sql().handover.select_by_ordinal.sql())
                .bind(process_id.as_str())
                .bind(handover.segment_ordinal as i64)
                .fetch_optional(&mut **tx)
                .await
                .map_err(plugin_sqlx_error)?;
        if let Some(existing) = existing {
            let parked: PersistedSegmentHandover =
                serde_json::from_str(&existing).map_err(process_decode_error)?;
            if existing != encoded
                && (handover.writer.is_empty() || parked.writer != handover.writer)
            {
                return Err(PluginError::Session(format!(
                    "process `{process_id}` segment {} handover conflict",
                    handover.segment_ordinal
                )));
            }
            let committed_at_ms: i64 =
                sqlx::query_scalar(process_sql().handover.select_committed_at.sql())
                    .bind(process_id.as_str())
                    .bind(handover.segment_ordinal as i64)
                    .fetch_one(&mut **tx)
                    .await
                    .map_err(plugin_sqlx_error)?;
            tx.commit().await.map_err(plugin_sqlx_error)?;
            return Ok(lash_core_execution::store::StoreTransition::unchanged(
                lash_core_execution::SegmentHandoverCommit {
                    scope: record.trace.clone(),
                    handover: parked,
                    committed_at_ms: committed_at_ms as u64,
                },
            ));
        }
        let committed_at_ms = self.clock.timestamp_ms();
        let result = sqlx::query(process_sql().handover_postgres.insert.sql())
            .bind(process_id.as_str())
            .bind(handover.segment_ordinal as i64)
            .bind(encoded)
            .bind(handover.written_generation.as_str())
            .bind(handover.route.as_str())
            .bind(committed_at_ms as i64)
            .execute(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
        if result.rows_affected() == 0 {
            return Err(PluginError::Session(format!(
                "process `{process_id}` segment {} handover conflict",
                handover.segment_ordinal
            )));
        }
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(lash_core_execution::store::StoreTransition::changed(
            lash_core_execution::SegmentHandoverCommit {
                scope: record.trace.clone(),
                handover,
                committed_at_ms,
            },
        ))
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
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        // An ended process starts no segment. The row lock the load takes
        // holds to the commit, so a terminal append serialises fully before or
        // after this marker (FIG-3819).
        let record = require_process_tx(&mut tx, process_id).await?;
        if record.is_terminal() {
            return Err(PluginError::ProcessAlreadyTerminal {
                process_id: record.id.clone(),
                status: record.status(),
            });
        }
        sqlx::query(process_sql().handover.mark_started.sql())
            .bind(process_id.as_str())
            .bind(segment_ordinal as i64)
            .bind(encoded)
            .execute(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
        let started: Option<Option<String>> =
            sqlx::query_scalar(process_sql().handover.select_started.sql())
                .bind(process_id.as_str())
                .bind(segment_ordinal as i64)
                .fetch_optional(&mut **tx)
                .await
                .map_err(plugin_sqlx_error)?;
        let Some(Some(recorded)) = started else {
            return Err(PluginError::Session(format!(
                "process `{process_id}` segment {segment_ordinal} has no retained handover to mark started"
            )));
        };
        let recorded: lash_core_execution::SegmentStartMarker =
            serde_json::from_str(&recorded).map_err(process_decode_error)?;
        // The recorded marker's admission stamp projects onto the process
        // row in the same transaction (FIG-3795 S2): the drain's
        // live-generation index reads the generation the segment's start
        // actually recorded, never a losing caller's.
        sqlx::query(process_sql().process.set_segment_generation.sql())
            .bind(process_id.as_str())
            .bind(
                recorded
                    .build_generation
                    .as_ref()
                    .map(|generation| generation.as_str()),
            )
            .execute(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(recorded)
    }

    async fn record_segment_handover_route(
        &self,
        process_id: &ProcessId,
        segment_ordinal: u64,
        route: &str,
    ) -> Result<(), PluginError> {
        let mut tx = begin_guarded(&self.pool, &self.fence)
            .await
            .map_err(plugin_store_error)?;
        let existing: Option<String> =
            sqlx::query_scalar(process_sql().handover.select_by_ordinal.sql())
                .bind(process_id.as_str())
                .bind(segment_ordinal as i64)
                .fetch_optional(&mut **tx)
                .await
                .map_err(plugin_sqlx_error)?;
        let Some(existing) = existing else {
            return Ok(());
        };
        let mut handover: PersistedSegmentHandover =
            serde_json::from_str(&existing).map_err(process_decode_error)?;
        if handover.route == route {
            return Ok(());
        }
        handover.route = route.to_owned();
        let encoded = serde_json::to_string(&handover).map_err(process_decode_error)?;
        sqlx::query(process_sql().handover.set_route.sql())
            .bind(process_id.as_str())
            .bind(segment_ordinal as i64)
            .bind(encoded)
            .bind(route)
            .execute(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
        tx.commit().await.map_err(plugin_sqlx_error)?;
        Ok(())
    }

    async fn retire_segment_handovers_through(
        &self,
        process_id: &ProcessId,
        segment_ordinal: u64,
    ) -> Result<(), PluginError> {
        crate::guarded_tx::guarded(&self.pool, &self.fence, |tx| {
            Box::pin(async move {
                sqlx::query(process_sql().handover.delete_through.sql())
                    .bind(process_id.as_str())
                    .bind(segment_ordinal as i64)
                    .execute(tx.as_mut())
                    .await
                    .map_err(crate::store_sqlx_error)
            })
        })
        .await
        .map_err(crate::plugin_store_error)?;
        Ok(())
    }

    async fn delete_segment_handovers(&self, process_id: &ProcessId) -> Result<(), PluginError> {
        crate::guarded_tx::guarded(&self.pool, &self.fence, |tx| {
            Box::pin(async move {
                sqlx::query(process_sql().handover.delete_by_process.sql())
                    .bind(process_id.as_str())
                    .execute(tx.as_mut())
                    .await
                    .map_err(crate::store_sqlx_error)
            })
        })
        .await
        .map_err(crate::plugin_store_error)?;
        Ok(())
    }
}
