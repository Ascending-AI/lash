use super::*;

impl SqliteProcessRegistry {
    pub(super) async fn put_segment_handover_impl(
        &self,
        process_id: &str,
        handover: PersistedSegmentHandover,
    ) -> Result<(), lash_core::PluginError> {
        let process_id = process_id.to_string();
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    Self::require_process_conn(tx, &process_id)?;
                    let existing: Option<String> = tx
                        .query_row(
                            "SELECT handover_json FROM process_segment_handovers
                             WHERE process_id = ?1 AND segment_ordinal = ?2",
                            params![&process_id, handover.segment_ordinal as i64],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(process_sqlite_error)?;
                    let encoded = process_encode_json(&handover)?;
                    if let Some(existing) = existing {
                        if existing == encoded {
                            return Ok(());
                        }
                        return Err(lash_core::PluginError::Session(format!(
                            "process `{process_id}` segment {} handover conflict",
                            handover.segment_ordinal
                        )));
                    }
                    tx.execute(
                        "DELETE FROM process_segment_handovers
                         WHERE process_id = ?1 AND segment_ordinal < ?2 - 1",
                        params![&process_id, handover.segment_ordinal as i64],
                    )
                    .map_err(process_sqlite_error)?;
                    tx.execute(
                        "INSERT INTO process_segment_handovers
                         (process_id, segment_ordinal, handover_json) VALUES (?1, ?2, ?3)",
                        params![&process_id, handover.segment_ordinal as i64, encoded],
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
        process_id: &str,
        segment_ordinal: u64,
    ) -> Result<Option<PersistedSegmentHandover>, lash_core::PluginError> {
        let process_id = process_id.to_string();
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let encoded: Option<String> = conn
                        .query_row(
                            "SELECT handover_json FROM process_segment_handovers
                             WHERE process_id = ?1 AND segment_ordinal = ?2",
                            params![process_id, segment_ordinal as i64],
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
        process_id: &str,
    ) -> Result<Option<PersistedSegmentHandover>, lash_core::PluginError> {
        let process_id = process_id.to_string();
        self.conn
            .call(move |conn| {
                Ok((|| {
                    let encoded: Option<String> = conn
                        .query_row(
                            "SELECT handover_json FROM process_segment_handovers
                             WHERE process_id = ?1 ORDER BY segment_ordinal DESC LIMIT 1",
                            params![process_id],
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
        process_id: &str,
    ) -> Result<(), lash_core::PluginError> {
        let process_id = process_id.to_string();
        self.conn
            .write_flow(move |tx| {
                Ok(tx_outcome((|| {
                    tx.execute(
                        "DELETE FROM process_segment_handovers WHERE process_id = ?1",
                        params![process_id],
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
