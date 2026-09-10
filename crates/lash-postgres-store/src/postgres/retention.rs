use crate::*;
use lash_sansio::ProcessId;

pub(super) async fn filter_unregistered_process_ids(
    pool: &sqlx::PgPool,
    process_ids: &[ProcessId],
) -> Result<Vec<ProcessId>, PluginError> {
    if process_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_scalar(
        "SELECT candidate.process_id
         FROM UNNEST($1::TEXT[]) WITH ORDINALITY AS candidate(process_id, ordinal)
         WHERE NOT EXISTS (
             SELECT 1 FROM lash_processes p
             WHERE p.process_id = candidate.process_id
         )
           AND NOT EXISTS (
             SELECT 1 FROM lash_process_tombstones t
             WHERE t.process_id = candidate.process_id
         )
         ORDER BY candidate.ordinal ASC",
    )
    .bind(
        process_ids
            .iter()
            .map(ProcessId::as_str)
            .collect::<Vec<_>>(),
    )
    .fetch_all(pool)
    .await
    .map(|ids: Vec<String>| ids.into_iter().map(ProcessId::from).collect())
    .map_err(plugin_sqlx_error)
}

pub(super) async fn filter_tombstoned_process_ids(
    pool: &sqlx::PgPool,
    process_ids: &[ProcessId],
) -> Result<Vec<ProcessId>, PluginError> {
    if process_ids.is_empty() {
        return Ok(Vec::new());
    }
    sqlx::query_scalar(
        "SELECT candidate.process_id
         FROM UNNEST($1::TEXT[]) WITH ORDINALITY AS candidate(process_id, ordinal)
         WHERE EXISTS (
             SELECT 1 FROM lash_process_tombstones t
             WHERE t.process_id = candidate.process_id
         )
           AND NOT EXISTS (
             SELECT 1 FROM lash_processes p
             WHERE p.process_id = candidate.process_id
         )
         ORDER BY candidate.ordinal ASC",
    )
    .bind(
        process_ids
            .iter()
            .map(ProcessId::as_str)
            .collect::<Vec<_>>(),
    )
    .fetch_all(pool)
    .await
    .map(|ids: Vec<String>| ids.into_iter().map(ProcessId::from).collect())
    .map_err(plugin_sqlx_error)
}
