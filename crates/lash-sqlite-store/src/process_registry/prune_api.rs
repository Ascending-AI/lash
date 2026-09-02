use super::*;

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
