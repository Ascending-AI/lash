use crate::process_sql::process_sql;
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
        process_sql()
            .process_postgres
            .classify_unregistered_candidates
            .sql(),
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
        process_sql()
            .process_postgres
            .classify_tombstoned_candidates
            .sql(),
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
