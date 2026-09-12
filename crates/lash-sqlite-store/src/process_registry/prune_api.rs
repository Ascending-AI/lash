use super::*;
use lash_sansio::ProcessId;

/// The survey half of the prune: the same predicate the prune's final
/// transaction re-evaluates, read without deleting.
pub(super) async fn prunable_terminal_processes(
    registry: &SqliteProcessRegistry,
    cutoff_epoch_ms: u64,
    filter: Option<ProcessListFilter>,
    watermark: lash_core::ProjectionWatermark,
) -> Result<Vec<ProcessId>, lash_core::PluginError> {
    let cutoff = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
    let max_change_seq = match watermark {
        lash_core::ProjectionWatermark::UpTo(cursor) => Some(cursor.store_sequence()),
        lash_core::ProjectionWatermark::NoProjector => None,
    };
    registry
        .conn
        .call(move |conn| {
            crate::process_registry_change::prunable_terminal_process_ids_conn(
                conn,
                cutoff,
                filter,
                max_change_seq,
            )
            .map_err(|err| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                    err.to_string(),
                )))
            })
        })
        .await
        .map_err(process_sqlite_error)
}

pub(super) async fn prune_terminal_processes(
    registry: &SqliteProcessRegistry,
    cutoff_epoch_ms: u64,
    filter: Option<ProcessListFilter>,
    watermark: lash_core::ProjectionWatermark,
) -> Result<ProcessPruneReport, lash_core::PluginError> {
    let cutoff = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
    let pruned_at_ms = registry.clock.timestamp_ms() as i64;
    let max_change_seq = match watermark {
        lash_core::ProjectionWatermark::UpTo(cursor) => Some(cursor.store_sequence()),
        lash_core::ProjectionWatermark::NoProjector => None,
    };
    if let Some(root) = registry.process_session_store_root.as_ref() {
        let selection_filter = filter.clone();
        let prunable = registry
            .conn
            .call(move |conn| {
                crate::process_registry_change::prunable_terminal_process_ids_conn(
                    conn,
                    cutoff,
                    selection_filter,
                    max_change_seq,
                )
                .map_err(|err| {
                    rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(
                        err.to_string(),
                    )))
                })
            })
            .await
            .map_err(process_sqlite_error)?;
        // Delete process-owned session stores first. If this fails, the
        // terminal process row remains and the prune leaks conservatively;
        // the final transaction below revalidates eligibility before it
        // removes any process row.
        for process_id in prunable {
            for session_id in facade_support::process_runtime_session_ids(&process_id) {
                delete_session_from_catalog(root, &session_id, SqliteConnectionPolicy::default())
                    .await
                    .map_err(|error| lash_core::PluginError::Session(error.to_string()))?;
            }
        }
    }
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome(
                crate::process_registry_change::prune_terminal_processes_conn(
                    tx,
                    cutoff,
                    pruned_at_ms,
                    filter,
                    max_change_seq,
                ),
            ))
        })
        .await
        .map_err(process_sqlite_error)?
}

#[async_trait::async_trait]
impl lash_core::ProcessRetention for SqliteProcessRegistry {
    async fn pending_process_artifact_cleanup(
        &self,
    ) -> Result<Vec<lash_core::ProcessArtifactCleanup>, lash_core::PluginError> {
        self.conn
            .call(|conn| {
                let mut statement = conn.prepare(
                    "SELECT cleanup_json FROM process_artifact_cleanup
                     ORDER BY process_id, incarnation",
                )?;
                statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .map(|row| {
                        let json = row?;
                        serde_json::from_str(&json).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
            .map_err(process_sqlite_error)
    }

    async fn complete_process_artifact_cleanup(
        &self,
        process_id: &ProcessId,
        incarnation: lash_core::ProcessIncarnation,
    ) -> Result<(), lash_core::PluginError> {
        let process_id = process_id.to_string();
        self.conn
            .write(move |tx| {
                tx.execute(
                    "DELETE FROM process_artifact_cleanup
                     WHERE process_id = ?1 AND incarnation = ?2",
                    params![process_id, incarnation.registration_sequence() as i64],
                )?;
                Ok(())
            })
            .await
            .map_err(process_sqlite_error)
    }

    async fn compact_process_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: lash_core::ProjectionWatermark,
        trigger_store: Option<&dyn lash_core::TriggerStore>,
    ) -> Result<usize, lash_core::PluginError> {
        let max_change_seq = crate::process_registry_change::max_change_sequence(watermark);
        let cutoff_epoch_ms = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        let outstanding_trigger_delivery_process_ids = match trigger_store {
            Some(trigger_store) => trigger_store.list_delivery_process_ids().await?,
            None => Vec::new(),
        };
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome(
                    crate::process_registry_change::compact_process_tombstones_conn(
                        tx,
                        cutoff_epoch_ms,
                        max_change_seq,
                        &outstanding_trigger_delivery_process_ids,
                    ),
                ))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn prune_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: lash_core::ProjectionWatermark,
    ) -> Result<ProcessPruneReport, lash_core::PluginError> {
        prune_api::prune_terminal_processes(self, cutoff_epoch_ms, filter, watermark).await
    }

    async fn prunable_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: lash_core::ProjectionWatermark,
    ) -> Result<Vec<ProcessId>, lash_core::PluginError> {
        prune_api::prunable_terminal_processes(self, cutoff_epoch_ms, filter, watermark).await
    }
}
