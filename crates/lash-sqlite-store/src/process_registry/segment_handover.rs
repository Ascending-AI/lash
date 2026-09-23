use super::*;
use lash_sansio::ProcessId;

impl SqliteProcessRegistry {
    pub(super) async fn put_segment_handover_impl(
        &self,
        process_id: &ProcessId,
        handover: PersistedSegmentHandover,
    ) -> Result<(), lash_core_execution::PluginError> {
        let process_id = ProcessId::from(process_id.to_string());
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    Self::require_process_conn(tx, &process_id)?;
                    let existing: Option<String> = tx
                        .query_row(
                            process_sql().handover.select_by_ordinal.sql(),
                            params![process_id.as_str(), handover.segment_ordinal as i64],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(process_sqlite_error)?;
                    let encoded = process_encode_json(&handover)?;
                    if let Some(existing) = existing {
                        if existing == encoded {
                            return Ok(());
                        }
                        return Err(lash_core_execution::PluginError::Session(format!(
                            "process `{process_id}` segment {} handover conflict",
                            handover.segment_ordinal
                        )));
                    }
                    tx.execute(
                        process_sql().handover.delete_superseded.sql(),
                        params![process_id.as_str(), handover.segment_ordinal as i64],
                    )
                    .map_err(process_sqlite_error)?;
                    tx.execute(
                        process_sql().handover_sqlite.insert.sql(),
                        params![
                            process_id.as_str(),
                            handover.segment_ordinal as i64,
                            encoded
                        ],
                    )
                    .map_err(process_sqlite_error)?;
                    Ok(())
                })()))
            })
            .await
            .map_err(process_sqlite_error)??;
        Ok(())
    }

    pub(super) async fn get_segment_handover_impl(
        &self,
        process_id: &ProcessId,
        segment_ordinal: u64,
    ) -> Result<Option<PersistedSegmentHandover>, lash_core_execution::PluginError> {
        let process_id = ProcessId::from(process_id.to_string());
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let encoded: Option<String> = conn
                        .query_row(
                            process_sql().handover.select_by_ordinal.sql(),
                            params![process_id.as_str(), segment_ordinal as i64],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(process_sqlite_error)?;
                    encoded
                        .map(|encoded| serde_json::from_str(&encoded).map_err(process_decode_error))
                        .transpose()
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    pub(super) async fn latest_segment_handover_impl(
        &self,
        process_id: &ProcessId,
    ) -> Result<Option<PersistedSegmentHandover>, lash_core_execution::PluginError> {
        let process_id = ProcessId::from(process_id.to_string());
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let encoded: Option<String> = conn
                        .query_row(
                            process_sql().handover.select_latest.sql(),
                            params![process_id.as_str()],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(process_sqlite_error)?;
                    encoded
                        .map(|encoded| serde_json::from_str(&encoded).map_err(process_decode_error))
                        .transpose()
                })())
            })
            .await
            .map_err(process_sqlite_error)?
    }

    pub(super) async fn delete_segment_handovers_impl(
        &self,
        process_id: &ProcessId,
    ) -> Result<(), lash_core_execution::PluginError> {
        let process_id = ProcessId::from(process_id.to_string());
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    tx.execute(
                        process_sql().handover.delete_by_process.sql(),
                        params![process_id.as_str()],
                    )
                    .map_err(process_sqlite_error)?;
                    Ok(())
                })()))
            })
            .await
            .map_err(process_sqlite_error)??;
        Ok(())
    }
}
