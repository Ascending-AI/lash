use super::*;
use lash_sansio::ProcessId;

/// The survey half of the prune: the same predicate the prune's final
/// transaction re-evaluates, read without deleting.
pub(super) async fn prunable_terminal_processes(
    registry: &SqliteProcessRegistry,
    cutoff_epoch_ms: u64,
    filter: Option<ProcessListFilter>,
    watermark: lash_core_execution::ProjectionWatermark,
) -> Result<Vec<ProcessId>, lash_core_execution::PluginError> {
    let cutoff = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
    let max_change_seq = match watermark {
        lash_core_execution::ProjectionWatermark::UpTo(cursor) => Some(cursor.store_sequence()),
        lash_core_execution::ProjectionWatermark::NoProjector => None,
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
    watermark: lash_core_execution::ProjectionWatermark,
) -> Result<ProcessPruneReport, lash_core_execution::PluginError> {
    let cutoff = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
    let pruned_at_ms = registry.clock.timestamp_ms() as i64;
    let max_change_seq = match watermark {
        lash_core_execution::ProjectionWatermark::UpTo(cursor) => Some(cursor.store_sequence()),
        lash_core_execution::ProjectionWatermark::NoProjector => None,
    };
    let catalog = &registry.process_session_catalog;
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
    // If this fails, the terminal process row remains and the prune leaks conservatively;
    // the final transaction below revalidates eligibility before it removes any process
    // row.
    for process_id in prunable {
        for session_id in facade_support::process_runtime_session_ids(&process_id) {
            delete_session_from_catalog(
                catalog,
                &session_id,
                SqliteConnectionPolicy::default(),
                pruned_at_ms as u64,
            )
            .await
            .map_err(|error| lash_core_execution::PluginError::Session(error.to_string()))?;
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
impl lash_core_execution::ProcessRetention for SqliteProcessRegistry {
    async fn pending_process_artifact_cleanup(
        &self,
    ) -> Result<Vec<lash_core_execution::ProcessArtifactCleanup>, lash_core_execution::PluginError>
    {
        self.conn
            .call(|conn| {
                let mut statement = conn.prepare(process_sql().cleanup.list_pending.sql())?;
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
        incarnation: lash_core_execution::ProcessIncarnation,
    ) -> Result<lash_core_execution::ProcessArtifactCleanupAck, lash_core_execution::PluginError>
    {
        let process_id = process_id.clone();
        self.conn
            .write(move |tx| {
                let current_incarnation = tx
                    .query_row(
                        process_sql().process_sqlite.select_incarnation.sql(),
                        params![process_id.as_str()],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?;
                let removed = tx.execute(
                    process_sql().cleanup_sqlite.delete_for_incarnation.sql(),
                    params![
                        process_id.as_str(),
                        incarnation.registration_sequence() as i64
                    ],
                )?;
                let process_ref =
                    lash_core_execution::ProcessRef::new(process_id.clone(), incarnation);
                Ok(match current_incarnation {
                    Some(found) => {
                        let found =
                            lash_core_execution::ProcessIncarnation::from_registration_sequence(
                                u64_from_sql("ProcessRecord", "incarnation", found)?,
                            );
                        if found != incarnation {
                            lash_core_execution::ProcessArtifactCleanupAck::StaleIncarnation {
                                expected: process_ref,
                                found: lash_core_execution::ProcessRef::new(process_id, found),
                            }
                        } else if removed == 1 {
                            lash_core_execution::ProcessArtifactCleanupAck::Acknowledged {
                                process_ref,
                            }
                        } else {
                            lash_core_execution::ProcessArtifactCleanupAck::Unknown { process_ref }
                        }
                    }
                    None if removed == 1 => {
                        lash_core_execution::ProcessArtifactCleanupAck::Acknowledged { process_ref }
                    }
                    None => lash_core_execution::ProcessArtifactCleanupAck::Unknown { process_ref },
                })
            })
            .await
            .map_err(process_sqlite_error)
    }

    async fn compact_process_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: lash_core_execution::ProjectionWatermark,
        trigger_store: Option<&dyn lash_core_execution::TriggerStore>,
    ) -> Result<usize, lash_core_execution::PluginError> {
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
        watermark: lash_core_execution::ProjectionWatermark,
    ) -> Result<ProcessPruneReport, lash_core_execution::PluginError> {
        prune_api::prune_terminal_processes(self, cutoff_epoch_ms, filter, watermark).await
    }

    async fn prunable_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: lash_core_execution::ProjectionWatermark,
    ) -> Result<Vec<ProcessId>, lash_core_execution::PluginError> {
        prune_api::prunable_terminal_processes(self, cutoff_epoch_ms, filter, watermark).await
    }
}
