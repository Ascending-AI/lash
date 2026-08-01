use super::*;

pub(crate) fn processes_changed_since_conn(
    conn: &Connection,
    cursor: ProcessChangeCursor,
    limit: usize,
) -> Result<(Vec<ProcessChange>, ProcessChangeCursor), lash_core::PluginError> {
    let mut stmt = conn
        .prepare(
            "SELECT change_seq, kind, payload FROM (
                 SELECT change_seq, 'upsert' AS kind, record_json AS payload
                 FROM processes WHERE change_seq > ?1
                 UNION ALL
                 SELECT pruned_change_seq, 'deleted' AS kind,
                        json_object(
                            'process_id', process_id,
                            'terminal_label', terminal_label,
                            'pruned_at_ms', pruned_at_ms,
                            'pruned_change_seq', pruned_change_seq
                        ) AS payload
                 FROM process_tombstones WHERE pruned_change_seq > ?1
             )
             ORDER BY change_seq ASC
             LIMIT ?2",
        )
        .map_err(process_sqlite_error)?;
    let rows = stmt
        .query_map(
            params![cursor.store_sequence() as i64, limit as i64],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(process_sqlite_error)?;
    let mut records = Vec::new();
    let mut next_cursor = cursor;
    for row in rows {
        let (change_seq, kind, record_json) = row.map_err(process_sqlite_error)?;
        let change = if kind == "upsert" {
            ProcessChange::Upsert {
                record: serde_json::from_str(&record_json).map_err(process_decode_error)?,
            }
        } else {
            ProcessChange::Deleted {
                tombstone: serde_json::from_str(&record_json).map_err(process_decode_error)?,
            }
        };
        next_cursor = ProcessChangeCursor::from_store_sequence(change_seq as u64);
        records.push(change);
    }
    Ok((records, next_cursor))
}

pub(crate) fn prune_terminal_processes_conn(
    conn: &Connection,
    cutoff: i64,
    pruned_at_ms: i64,
    filter: Option<ProcessListFilter>,
    max_change_seq: Option<u64>,
) -> Result<ProcessPruneReport, lash_core::PluginError> {
    let trigger_deliveries_exists = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'trigger_deliveries'",
            [],
            |_| Ok(()),
        )
        .optional()
        .map_err(process_sqlite_error)?
        .is_some();
    let prunable = prunable_terminal_process_ids_conn(conn, cutoff, filter, max_change_seq)?;

    let mut pruned_events = 0;
    let mut pruned_processes = 0;
    for process_id in prunable {
        let process_id = process_id.as_str();
        pruned_events += conn
            .execute(
                "DELETE FROM process_events WHERE process_id = ?1",
                params![process_id],
            )
            .map_err(process_sqlite_error)?;
        conn.execute(
            "DELETE FROM process_observers WHERE process_id = ?1",
            params![process_id],
        )
        .map_err(process_sqlite_error)?;
        conn.execute(
            "DELETE FROM process_leases WHERE process_id = ?1",
            params![process_id],
        )
        .map_err(process_sqlite_error)?;
        conn.execute(
            "DELETE FROM process_segment_handovers WHERE process_id = ?1",
            params![process_id],
        )
        .map_err(process_sqlite_error)?;
        if trigger_deliveries_exists {
            conn.execute(
                "DELETE FROM trigger_deliveries WHERE process_id = ?1",
                params![process_id],
            )
            .map_err(process_sqlite_error)?;
        }
        let terminal_label: String = conn
            .query_row(
                "SELECT status FROM processes WHERE process_id = ?1",
                params![process_id],
                |row| row.get(0),
            )
            .map_err(process_sqlite_error)?;
        let pruned_change_seq = SqliteProcessRegistry::next_change_seq_conn(conn)?;
        conn.execute(
            "INSERT INTO process_tombstones (
                process_id, terminal_label, pruned_at_ms, pruned_change_seq
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                process_id,
                terminal_label,
                pruned_at_ms,
                pruned_change_seq as i64
            ],
        )
        .map_err(process_sqlite_error)?;
        pruned_processes += conn
            .execute(
                "DELETE FROM processes WHERE process_id = ?1",
                params![process_id],
            )
            .map_err(process_sqlite_error)?;
    }
    Ok(ProcessPruneReport {
        pruned_processes,
        pruned_events,
    })
}

pub(crate) fn prunable_terminal_process_ids_conn(
    conn: &Connection,
    cutoff: i64,
    filter: Option<ProcessListFilter>,
    max_change_seq: Option<u64>,
) -> Result<Vec<String>, lash_core::PluginError> {
    let max_change_seq = max_change_seq.map(|seq| seq as i64);
    let mut stmt = conn
        .prepare(
            "SELECT process_id, record_json FROM processes
             WHERE status NOT IN ('running', 'waiting')
               AND updated_at_ms < ?1
               AND (?2 IS NULL OR change_seq <= ?2)
               AND NOT EXISTS (
                   SELECT 1 FROM process_wake_deliveries AS delivery
                   WHERE delivery.process_id = processes.process_id
                     AND delivery.state IN ('pending', 'enqueuing')
               )
             ORDER BY process_id ASC",
        )
        .map_err(process_sqlite_error)?;
    let rows = stmt
        .query_map(params![cutoff, max_change_seq], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(process_sqlite_error)?;
    let mut prunable = Vec::new();
    for row in rows {
        let (process_id, record_json) = row.map_err(process_sqlite_error)?;
        let record: ProcessRecord =
            serde_json::from_str(&record_json).map_err(process_decode_error)?;
        if filter
            .as_ref()
            .is_none_or(|filter| filter.matches_record(&record))
        {
            prunable.push(process_id);
        }
    }

    Ok(prunable)
}
