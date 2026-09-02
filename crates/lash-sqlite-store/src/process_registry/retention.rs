use super::*;

pub(super) async fn filter_unregistered_process_ids(
    registry: &SqliteProcessRegistry,
    process_ids: &[String],
) -> Result<Vec<String>, lash_core::PluginError> {
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
                        "SELECT candidate.value
                         FROM json_each(?1) AS candidate
                         WHERE NOT EXISTS (
                             SELECT 1 FROM processes p
                             WHERE p.process_id = candidate.value
                         )
                           AND NOT EXISTS (
                             SELECT 1 FROM process_tombstones t
                             WHERE t.process_id = candidate.value
                         )
                         ORDER BY candidate.key ASC",
                    )
                    .map_err(process_sqlite_error)?;
                let rows = stmt
                    .query_map(params![process_ids_json], |row| row.get::<_, String>(0))
                    .map_err(process_sqlite_error)?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(process_sqlite_error)
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn filter_tombstoned_process_ids(
    registry: &SqliteProcessRegistry,
    process_ids: &[String],
) -> Result<Vec<String>, lash_core::PluginError> {
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
                        "SELECT candidate.value
                         FROM json_each(?1) AS candidate
                         WHERE EXISTS (
                             SELECT 1 FROM process_tombstones t
                             WHERE t.process_id = candidate.value
                         )
                           AND NOT EXISTS (
                             SELECT 1 FROM processes p
                             WHERE p.process_id = candidate.value
                         )
                         ORDER BY candidate.key ASC",
                    )
                    .map_err(process_sqlite_error)?;
                let rows = stmt
                    .query_map(params![process_ids_json], |row| row.get::<_, String>(0))
                    .map_err(process_sqlite_error)?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(process_sqlite_error)
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}
