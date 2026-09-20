use super::*;
use lash_sansio::ProcessId;

pub(super) async fn filter_unregistered_process_ids(
    registry: &SqliteProcessRegistry,
    process_ids: &[ProcessId],
) -> Result<Vec<ProcessId>, lash_core::PluginError> {
    if process_ids.is_empty() {
        return Ok(Vec::new());
    }
    let process_ids_json = serde_json::to_string(process_ids).map_err(process_decode_error)?;
    registry
        .conn
        .call(move |conn| {
            Ok((|| {
                let mut stmt = conn
                    .prepare(
                        process_sql()
                            .process_sqlite
                            .classify_unregistered_candidates
                            .sql(),
                    )
                    .map_err(process_sqlite_error)?;
                let rows = stmt
                    .query_map(params![process_ids_json], |row| row.get::<_, String>(0))
                    .map_err(process_sqlite_error)?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map(|ids: Vec<String>| ids.into_iter().map(ProcessId::from).collect())
                    .map_err(process_sqlite_error)
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn filter_tombstoned_process_ids(
    registry: &SqliteProcessRegistry,
    process_ids: &[ProcessId],
) -> Result<Vec<ProcessId>, lash_core::PluginError> {
    if process_ids.is_empty() {
        return Ok(Vec::new());
    }
    let process_ids_json = serde_json::to_string(process_ids).map_err(process_decode_error)?;
    registry
        .conn
        .call(move |conn| {
            Ok((|| {
                let mut stmt = conn
                    .prepare(
                        process_sql()
                            .process_sqlite
                            .classify_tombstoned_candidates
                            .sql(),
                    )
                    .map_err(process_sqlite_error)?;
                let rows = stmt
                    .query_map(params![process_ids_json], |row| row.get::<_, String>(0))
                    .map_err(process_sqlite_error)?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map(|ids: Vec<String>| ids.into_iter().map(ProcessId::from).collect())
                    .map_err(process_sqlite_error)
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}
