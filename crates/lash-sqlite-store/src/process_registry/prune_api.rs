use super::*;
use lash_vocabulary::identity::ProcessId;

/// The survey half of the prune: the same predicate the prune's final
/// transaction re-evaluates, read without deleting.
pub(super) async fn prunable_terminal_processes(
    registry: &SqliteProcessRegistry,
    cutoff_epoch_ms: u64,
    filter: Option<ProcessListFilter>,
    watermark: lash_execution_contracts::process::ProjectionWatermark,
) -> Result<Vec<ProcessId>, lash_execution_contracts::plugin_error::PluginError> {
    let cutoff = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
    let max_change_seq = match watermark {
        lash_execution_contracts::process::ProjectionWatermark::UpTo(cursor) => {
            Some(cursor.store_sequence())
        }
        lash_execution_contracts::process::ProjectionWatermark::NoProjector => None,
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
    watermark: lash_execution_contracts::process::ProjectionWatermark,
) -> Result<ProcessPruneReport, lash_execution_contracts::plugin_error::PluginError> {
    let cutoff = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
    let pruned_at_ms = registry.clock.timestamp_ms() as i64;
    let max_change_seq = match watermark {
        lash_execution_contracts::process::ProjectionWatermark::UpTo(cursor) => {
            Some(cursor.store_sequence())
        }
        lash_execution_contracts::process::ProjectionWatermark::NoProjector => None,
    };
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
impl lash_execution_contracts::process::ProcessRetention for SqliteProcessRegistry {
    async fn compact_process_tombstones(
        &self,
        cutoff_epoch_ms: u64,
        watermark: lash_execution_contracts::process::ProjectionWatermark,
    ) -> Result<usize, lash_execution_contracts::plugin_error::PluginError> {
        let max_change_seq = crate::process_registry_change::max_change_sequence(watermark);
        let cutoff_epoch_ms = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome(
                    crate::process_registry_change::compact_process_tombstones_conn(
                        tx,
                        cutoff_epoch_ms,
                        max_change_seq,
                    ),
                ))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn release_process_events(
        &self,
        process_id: &ProcessId,
        through: u64,
    ) -> Result<
        lash_execution_contracts::process::ProcessEventRelease,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        self.release_process_events_impl(process_id, through).await
    }

    async fn prune_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: lash_execution_contracts::process::ProjectionWatermark,
    ) -> Result<ProcessPruneReport, lash_execution_contracts::plugin_error::PluginError> {
        prune_api::prune_terminal_processes(self, cutoff_epoch_ms, filter, watermark).await
    }

    async fn prunable_terminal_processes(
        &self,
        cutoff_epoch_ms: u64,
        filter: Option<ProcessListFilter>,
        watermark: lash_execution_contracts::process::ProjectionWatermark,
    ) -> Result<Vec<ProcessId>, lash_execution_contracts::plugin_error::PluginError> {
        prune_api::prunable_terminal_processes(self, cutoff_epoch_ms, filter, watermark).await
    }

    async fn release_consumer_hold(
        &self,
        process_id: &ProcessId,
        key: &str,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        let process_id = process_id.to_string();
        let key = key.to_string();
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome(
                    crate::conn::cached_execute(
                        tx,
                        process_sql().process.release_consumer_hold.sql(),
                        params![process_id, key],
                    )
                    .map(|_| ())
                    .map_err(process_sqlite_error),
                ))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    async fn abandon_consumer_hold(
        &self,
        key: &str,
        owner: &lash_core_store::scope_identity::ScopeId,
    ) -> Result<Vec<ProcessId>, lash_execution_contracts::plugin_error::PluginError> {
        let key = key.to_string();
        let owner = owner.clone();
        let owner_kind = owner.storage_kind();
        let owner_id = owner.storage_id();
        let now = self.clock.timestamp_ms() as i64;
        // One write flow marks and reads, as a registration's fence reads the
        // mark in its own: the two are serialized either way round.
        let ids = self
            .conn
            .write_flow(move |tx| {
                if matches!(
                    owner.opener(),
                    Some(lash_core_store::effect_opener::EffectOpener::Turn { .. })
                ) {
                    match super::parent_end::plan_exists_conn(tx, &owner) {
                        Ok(true) => return Ok(tx_outcome(Ok(Vec::new()))),
                        Ok(false) => {}
                        Err(error) => return Ok(tx_outcome(Err(error))),
                    }
                }
                if let Err(error) = super::parent_end::mark_holds_conn(tx, &owner) {
                    return Ok(tx_outcome(Err(error)));
                }
                Ok(tx_outcome(
                    (|| {
                        crate::conn::cached_execute(
                            tx,
                            process_sql().abandoned_hold.mark.sql(),
                            params![key, owner_kind, owner_id, now],
                        )?;
                        let mut statement =
                            tx.prepare_cached(process_sql().process.select_owed_cancels.sql())?;
                        let rows =
                            statement.query_map(params![key], |row| row.get::<_, String>(0))?;
                        rows.collect::<Result<Vec<_>, _>>()
                    })()
                    .map_err(process_sqlite_error),
                ))
            })
            .await
            .map_err(process_sqlite_error)??;
        ids.iter()
            .map(|id| {
                ProcessId::parse(id).map_err(|error| {
                    lash_execution_contracts::plugin_error::PluginError::Session(format!(
                        "a held process row names an invalid id: {error}"
                    ))
                })
            })
            .collect()
    }
}
