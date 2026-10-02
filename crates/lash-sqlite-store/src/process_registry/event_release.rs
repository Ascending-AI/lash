//! Host release of a retained process's event prefix (FIG-3482).

use super::*;
use lash_sansio::ProcessId;

/// Rows one release round reads and rewrites.
const RELEASE_PAGE_ROWS: i64 = 256;

impl SqliteProcessRegistry {
    /// The highest sequence `process_id` released, `0` when it released none.
    pub(crate) fn released_through_conn(
        conn: &Connection,
        process_id: &ProcessId,
    ) -> Result<u64, lash_core_execution::PluginError> {
        conn.query_row(
            process_sql().event_horizon.select_released_through.sql(),
            params![process_id.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(process_sqlite_error)?
        .map(|released| plugin_u64_from_sql("ProcessEventHorizon", "released_through", released))
        .transpose()
        .map(Option::unwrap_or_default)
    }

    pub(super) async fn release_process_events_impl(
        &self,
        process_id: &ProcessId,
        through: u64,
    ) -> Result<lash_core_execution::ProcessEventRelease, lash_core_execution::PluginError> {
        let process_id = process_id.clone();
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome(Self::release_process_events_conn(
                    tx,
                    &process_id,
                    through,
                )))
            })
            .await
            .map_err(process_sqlite_error)?
    }

    /// Rewrite the events after the current horizon and at or below
    /// `through`, clamped to the last event, into their released form, and
    /// raise the horizon, in the caller's transaction.
    fn release_process_events_conn(
        conn: &Connection,
        process_id: &ProcessId,
        through: u64,
    ) -> Result<lash_core_execution::ProcessEventRelease, lash_core_execution::PluginError> {
        let record = Self::require_process_conn(conn, process_id)?;
        let previous = Self::released_through_conn(conn, process_id)?;
        let target = through.min(record.last_event_sequence);
        if target <= previous {
            return Ok(lash_core_execution::ProcessEventRelease {
                released_through: previous,
                released_events: 0,
            });
        }
        let target_bound = crate::clamp_sequence_bound(target);
        let mut after = previous;
        let mut released_events = 0_u64;
        loop {
            let rows = {
                let mut stmt = conn
                    .prepare_cached(process_sql().event.page_release.sql())
                    .map_err(process_sqlite_error)?;
                stmt.query_map(
                    params![
                        process_id.as_str(),
                        crate::clamp_sequence_bound(after),
                        target_bound,
                        RELEASE_PAGE_ROWS
                    ],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                )
                .map_err(process_sqlite_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(process_sqlite_error)?
            };
            if rows.is_empty() {
                break;
            }
            for (sequence, json) in rows {
                let mut event: ProcessEvent =
                    serde_json::from_str(&json).map_err(process_decode_error)?;
                if let Some(digest) =
                    lash_core_execution::runtime::release_process_event_payload(&mut event)
                {
                    crate::conn::cached_execute(
                        conn,
                        process_sql().event.release.sql(),
                        params![
                            process_id.as_str(),
                            sequence,
                            process_encode_json(&event)?,
                            digest
                        ],
                    )
                    .map_err(process_sqlite_error)?;
                }
                released_events += 1;
                after = plugin_u64_from_sql("ProcessEvent", "sequence", sequence)?;
            }
        }
        crate::conn::cached_execute(
            conn,
            process_sql().event_horizon.upsert.sql(),
            params![process_id.as_str(), target_bound],
        )
        .map_err(process_sqlite_error)?;
        Ok(lash_core_execution::ProcessEventRelease {
            released_through: target,
            released_events,
        })
    }
}
