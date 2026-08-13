use std::num::NonZeroUsize;

use lash_core::{PluginError, ProcessParentEndPlan};
use rusqlite::{OptionalExtension, params};

use super::{SqliteProcessRegistry, process_decode_error, process_sqlite_error, tx_outcome};

pub(super) async fn list(
    registry: &SqliteProcessRegistry,
    limit: NonZeroUsize,
) -> Result<Vec<ProcessParentEndPlan>, PluginError> {
    let rows = registry
        .conn
        .call(move |conn| {
            let mut statement = conn.prepare(
                "SELECT process_id, actions_json
                 FROM process_parent_end_plans
                 ORDER BY process_id
                 LIMIT ?1",
            )?;
            let rows = statement.query_map(params![limit.get() as i64], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await
        .map_err(process_sqlite_error)?;
    rows.into_iter()
        .map(|(process_id, actions_json)| {
            let actions = serde_json::from_str(&actions_json).map_err(process_decode_error)?;
            Ok(ProcessParentEndPlan {
                process_id,
                actions,
            })
        })
        .collect()
}

pub(super) async fn get(
    registry: &SqliteProcessRegistry,
    process_id: &str,
) -> Result<Option<ProcessParentEndPlan>, PluginError> {
    let process_id = process_id.to_string();
    let query_process_id = process_id.clone();
    let row = registry
        .conn
        .call(move |conn| {
            conn.query_row(
                "SELECT actions_json FROM process_parent_end_plans WHERE process_id = ?1",
                params![query_process_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
        })
        .await
        .map_err(process_sqlite_error)?;
    row.map(|actions_json| {
        let actions = serde_json::from_str(&actions_json).map_err(process_decode_error)?;
        Ok(ProcessParentEndPlan {
            process_id,
            actions,
        })
    })
    .transpose()
}

pub(super) async fn complete(
    registry: &SqliteProcessRegistry,
    process_id: &str,
) -> Result<(), PluginError> {
    let process_id = process_id.to_string();
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                tx.execute(
                    "DELETE FROM process_parent_end_plans WHERE process_id = ?1",
                    params![process_id],
                )
                .map_err(process_sqlite_error)?;
                Ok(())
            })()))
        })
        .await
        .map_err(process_sqlite_error)?
}
