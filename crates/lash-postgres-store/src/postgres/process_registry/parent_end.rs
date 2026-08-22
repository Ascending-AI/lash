use std::num::NonZeroUsize;

use lash_core::{PluginError, ProcessParentEndPlan, ToolIntentParentEndAction};
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::{plugin_sqlx_error, process_decode_error};

pub(crate) async fn insert(
    tx: &mut Transaction<'_, Postgres>,
    process_id: &str,
    actions: &[ToolIntentParentEndAction],
) -> Result<(), PluginError> {
    if actions.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO lash_process_parent_end_plans (process_id, actions_json)
         VALUES ($1, $2)",
    )
    .bind(process_id)
    .bind(serde_json::to_string(actions).map_err(process_decode_error)?)
    .execute(&mut **tx)
    .await
    .map(drop)
    .map_err(plugin_sqlx_error)
}

pub(super) async fn list(
    pool: &PgPool,
    limit: NonZeroUsize,
) -> Result<Vec<ProcessParentEndPlan>, PluginError> {
    let rows = sqlx::query(
        "SELECT process_id, actions_json
         FROM lash_process_parent_end_plans
         ORDER BY process_id
         LIMIT $1",
    )
    .bind(limit.get() as i64)
    .fetch_all(pool)
    .await
    .map_err(plugin_sqlx_error)?;
    rows.into_iter()
        .map(|row| {
            let process_id: String = row.get(0);
            let actions_json: String = row.get(1);
            let actions = serde_json::from_str(&actions_json).map_err(process_decode_error)?;
            Ok(ProcessParentEndPlan {
                process_id,
                actions,
            })
        })
        .collect()
}

pub(super) async fn get(
    pool: &PgPool,
    process_id: &str,
) -> Result<Option<ProcessParentEndPlan>, PluginError> {
    let row =
        sqlx::query("SELECT actions_json FROM lash_process_parent_end_plans WHERE process_id = $1")
            .bind(process_id)
            .fetch_optional(pool)
            .await
            .map_err(plugin_sqlx_error)?;
    row.map(|row| {
        let actions_json: String = row.get(0);
        let actions = serde_json::from_str(&actions_json).map_err(process_decode_error)?;
        Ok(ProcessParentEndPlan {
            process_id: process_id.to_string(),
            actions,
        })
    })
    .transpose()
}

pub(super) async fn complete(pool: &PgPool, process_id: &str) -> Result<(), PluginError> {
    sqlx::query("DELETE FROM lash_process_parent_end_plans WHERE process_id = $1")
        .bind(process_id)
        .execute(pool)
        .await
        .map(drop)
        .map_err(plugin_sqlx_error)
}
