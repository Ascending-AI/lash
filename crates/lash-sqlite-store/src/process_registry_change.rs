use super::*;

pub(crate) fn max_change_sequence(watermark: lash_core::ProjectionWatermark) -> Option<i64> {
    match watermark {
        lash_core::ProjectionWatermark::UpTo(cursor) => Some(cursor.store_sequence() as i64),
        lash_core::ProjectionWatermark::NoProjector => None,
    }
}

pub(crate) fn compact_process_tombstones_conn(
    conn: &Connection,
    cutoff_epoch_ms: i64,
    max_change_seq: Option<i64>,
    outstanding_trigger_delivery_process_ids: &[String],
) -> Result<usize, lash_core::PluginError> {
    let outstanding_trigger_delivery_process_ids =
        serde_json::to_string(outstanding_trigger_delivery_process_ids)
            .map_err(process_decode_error)?;
    conn.execute(
        "DELETE FROM process_tombstones
         WHERE pruned_at_ms < ?1
           AND (?2 IS NULL OR pruned_change_seq <= ?2)
           AND process_id NOT IN (SELECT value FROM json_each(?3))",
        params![
            cutoff_epoch_ms,
            max_change_seq,
            outstanding_trigger_delivery_process_ids
        ],
    )
    .map_err(process_sqlite_error)
}

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
        next_cursor = ProcessChangeCursor::from_store_sequence(plugin_u64_from_sql(
            "ProcessChange",
            "change_seq",
            change_seq,
        )?);
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
    let prunable = prunable_terminal_process_ids_conn(conn, cutoff, filter, max_change_seq)?;
    if prunable.is_empty() {
        return Ok(ProcessPruneReport {
            pruned_processes: 0,
            pruned_events: 0,
            pruned_trigger_deliveries: 0,
        });
    }

    prune_process_rows_conn(conn, &prunable, pruned_at_ms)
}

fn prune_process_rows_conn(
    conn: &Connection,
    prunable: &[String],
    pruned_at_ms: i64,
) -> Result<ProcessPruneReport, lash_core::PluginError> {
    let process_ids_json = serde_json::to_string(&prunable).map_err(process_decode_error)?;
    let process_count = prunable.len() as i64;
    conn.execute(
        "UPDATE process_change_clock
         SET current_seq = current_seq + ?1
         WHERE singleton = 1",
        params![process_count],
    )
    .map_err(process_sqlite_error)?;
    let final_change_seq = conn
        .query_row(
            "SELECT current_seq FROM process_change_clock WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(process_sqlite_error)?;
    let first_change_seq = final_change_seq - process_count + 1;

    // json_each preserves the sorted candidate array's zero-based order, so
    // tombstone change sequences retain process-id ordering without one clock
    // update and insert per process.
    conn.execute(
        "INSERT INTO process_tombstones (
             process_id, terminal_label, pruned_at_ms, pruned_change_seq
         )
         SELECT process.process_id,
                process.status,
                ?2,
                ?3 + CAST(candidate.key AS INTEGER)
         FROM json_each(?1) AS candidate
         JOIN processes AS process ON process.process_id = candidate.value
         ORDER BY CAST(candidate.key AS INTEGER)",
        params![process_ids_json, pruned_at_ms, first_change_seq],
    )
    .map_err(process_sqlite_error)?;

    let pruned_events = conn
        .execute(
            "DELETE FROM process_events
             WHERE process_id IN (SELECT value FROM json_each(?1))",
            params![process_ids_json],
        )
        .map_err(process_sqlite_error)?;
    for table in [
        "process_observers",
        "process_leases",
        "process_segment_handovers",
    ] {
        conn.execute(
            &format!(
                "DELETE FROM {table}
                 WHERE process_id IN (SELECT value FROM json_each(?1))"
            ),
            params![process_ids_json],
        )
        .map_err(process_sqlite_error)?;
    }
    let pruned_processes = conn
        .execute(
            "DELETE FROM processes
             WHERE process_id IN (SELECT value FROM json_each(?1))",
            params![process_ids_json],
        )
        .map_err(process_sqlite_error)?;

    if pruned_processes != prunable.len() {
        return Err(lash_core::PluginError::Session(format!(
            "process prune candidate/tombstone divergence: expected {}, deleted {pruned_processes}",
            prunable.len()
        )));
    }

    Ok(ProcessPruneReport {
        pruned_processes,
        pruned_events,
        pruned_trigger_deliveries: 0,
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
               AND NOT EXISTS (
                   SELECT 1 FROM process_parent_end_plans AS plan
                   WHERE plan.process_id = processes.process_id
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_registry::tx_outcome;

    #[tokio::test]
    async fn candidate_tombstone_divergence_rolls_back_all_prune_mutations() {
        let registry = SqliteProcessRegistry::memory()
            .await
            .expect("open prune rollback registry");
        let process_id = format!("prune-rollback:{}", uuid::Uuid::new_v4());
        let ghost_id = format!("prune-rollback-ghost:{}", uuid::Uuid::new_v4());
        registry
            .register_process(ProcessRegistration::new(
                &process_id,
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
            ))
            .await
            .expect("register rollback process");
        registry
            .complete_process(
                &process_id,
                ProcessAwaitOutput::Success {
                    value: serde_json::Value::Null,
                    control: None,
                },
                lash_core::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete rollback process");
        let events_before = serde_json::to_value(
            registry
                .events_after(&process_id, 0)
                .await
                .expect("read events before divergent prune"),
        )
        .expect("encode events before divergent prune");
        let clock_before = registry
            .conn
            .call(|conn| {
                conn.query_row(
                    "SELECT current_seq FROM process_change_clock WHERE singleton = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
            })
            .await
            .expect("read process clock before divergent prune");

        let divergent_process_id = process_id.clone();
        let error = registry
            .conn
            .write_flow(move |tx| {
                Ok(tx_outcome(prune_process_rows_conn(
                    tx,
                    &[divergent_process_id, ghost_id],
                    123_456,
                )))
            })
            .await
            .expect("run divergent prune")
            .expect_err("candidate/tombstone divergence must abort the prune transaction");
        assert!(
            error.to_string().contains("candidate/tombstone divergence"),
            "unexpected divergence error: {error}"
        );

        assert!(
            registry
                .get_process(&process_id)
                .await
                .expect("read process after divergent prune")
                .is_some(),
            "divergent prune must retain the process row"
        );
        assert_eq!(
            serde_json::to_value(
                registry
                    .events_after(&process_id, 0)
                    .await
                    .expect("read events after divergent prune"),
            )
            .expect("encode events after divergent prune"),
            events_before,
            "divergent prune must retain the event journal"
        );
        let tombstone_process_id = process_id.clone();
        let tombstone_count = registry
            .conn
            .call(move |conn| {
                conn.query_row(
                    "SELECT count(*) FROM process_tombstones WHERE process_id = ?1",
                    params![tombstone_process_id],
                    |row| row.get::<_, i64>(0),
                )
            })
            .await
            .expect("count tombstones after divergent prune");
        assert_eq!(
            tombstone_count, 0,
            "divergent prune must not leave a tombstone"
        );
        let clock_after = registry
            .conn
            .call(|conn| {
                conn.query_row(
                    "SELECT current_seq FROM process_change_clock WHERE singleton = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
            })
            .await
            .expect("read process clock after divergent prune");
        assert_eq!(
            clock_after, clock_before,
            "divergent prune must roll back the clock"
        );
    }
}
