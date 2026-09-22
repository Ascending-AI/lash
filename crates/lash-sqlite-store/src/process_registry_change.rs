use super::*;
use crate::process_registry::sql::process_sql;
use lash_sansio::ProcessId;

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
    outstanding_trigger_delivery_process_ids: &[ProcessId],
) -> Result<usize, lash_core::PluginError> {
    let outstanding_trigger_delivery_process_ids =
        serde_json::to_string(outstanding_trigger_delivery_process_ids)
            .map_err(process_decode_error)?;
    let compacted_through: Option<i64> = conn
        .query_row(
            process_sql()
                .tombstone_sqlite
                .select_max_compactable_change_seq
                .sql(),
            params![
                cutoff_epoch_ms,
                max_change_seq,
                outstanding_trigger_delivery_process_ids
            ],
            |row| row.get(0),
        )
        .map_err(process_sqlite_error)?;
    let deleted = conn
        .execute(
            process_sql().tombstone_sqlite.delete_compactable.sql(),
            params![
                cutoff_epoch_ms,
                max_change_seq,
                outstanding_trigger_delivery_process_ids
            ],
        )
        .map_err(process_sqlite_error)?;
    if let Some(compacted_through) = compacted_through {
        conn.execute(
            process_sql().clock_sqlite.raise_compaction_horizon.sql(),
            params![compacted_through],
        )
        .map_err(process_sqlite_error)?;
    }
    Ok(deleted)
}

pub(crate) fn processes_changed_since_conn(
    conn: &Connection,
    cursor: ProcessChangeCursor,
    limit: usize,
) -> Result<(Vec<ProcessChange>, ProcessChangeCursor), lash_core::PluginError> {
    let tx = conn.unchecked_transaction().map_err(process_sqlite_error)?;
    let result = processes_changed_since_tx(&tx, cursor, limit);
    if result.is_ok() {
        tx.commit().map_err(process_sqlite_error)?;
    }
    result
}

fn processes_changed_since_tx(
    conn: &Connection,
    cursor: ProcessChangeCursor,
    limit: usize,
) -> Result<(Vec<ProcessChange>, ProcessChangeCursor), lash_core::PluginError> {
    let horizon = conn
        .query_row(
            process_sql().clock_sqlite.select_compaction_horizon.sql(),
            [],
            |row| row.get::<_, i64>(0),
        )
        .map_err(process_sqlite_error)?;
    let horizon = plugin_u64_from_sql(
        "ProcessChangeClock",
        "tombstone_compaction_horizon",
        horizon,
    )?;
    if cursor.store_sequence() < horizon {
        return Err(lash_core::PluginError::ProcessChangeCursorPruned {
            requested_cursor: cursor,
            tombstone_compaction_horizon: ProcessChangeCursor::from_store_sequence(horizon),
        });
    }
    if limit == 0 {
        return Ok((Vec::new(), cursor));
    }
    let mut stmt = conn
        .prepare(process_sql().process_sqlite.list_changes_after.sql())
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
    crate::process_registry::parent_end::reclaim_settled_plans_conn(conn, cutoff)?;
    if prunable.is_empty() {
        return Ok(ProcessPruneReport {
            pruned_processes: 0,
            pruned_events: 0,
            pruned_trigger_deliveries: 0,
            artifact_cleanup_acknowledgements: Vec::new(),
        });
    }

    prune_process_rows_conn(conn, &prunable, pruned_at_ms)
}

fn prune_process_rows_conn(
    conn: &Connection,
    prunable: &[ProcessId],
    pruned_at_ms: i64,
) -> Result<ProcessPruneReport, lash_core::PluginError> {
    let process_ids_json = serde_json::to_string(&prunable).map_err(process_decode_error)?;
    let process_count = prunable.len() as i64;
    conn.execute(
        process_sql().clock_sqlite.bump_by.sql(),
        params![process_count],
    )
    .map_err(process_sqlite_error)?;

    let final_change_seq = conn
        .query_row(process_sql().clock_sqlite.select_current.sql(), [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(process_sqlite_error)?;
    let first_change_seq = final_change_seq - process_count + 1;

    // json_each preserves the sorted candidate array's zero-based order, so
    // tombstone change sequences retain process-id ordering without one clock
    // update and insert per process.
    let inserted_tombstones = conn
        .execute(
            process_sql().tombstone_sqlite.insert_from_pruned.sql(),
            params![process_ids_json, pruned_at_ms, first_change_seq],
        )
        .map_err(process_sqlite_error)?;
    if inserted_tombstones != prunable.len() {
        return Err(lash_core::PluginError::Session(format!(
            "process prune candidate/tombstone divergence: expected {}, inserted {inserted_tombstones}",
            prunable.len()
        )));
    }

    for process_id in prunable {
        let record_json: String = conn
            .query_row(
                process_sql().process.select_record_json_by_id.sql(),
                params![process_id.as_str()],
                |row| row.get(0),
            )
            .map_err(process_sqlite_error)?;
        let record: lash_core::ProcessRecord =
            serde_json::from_str(&record_json).map_err(process_decode_error)?;
        let cleanup = lash_core::ProcessArtifactCleanup::from_record(&record);
        let cleanup_json = serde_json::to_string(&cleanup).map_err(process_decode_error)?;
        conn.execute(
            process_sql().cleanup_sqlite.insert.sql(),
            params![
                process_id.as_str(),
                cleanup.incarnation.registration_sequence() as i64,
                cleanup_json
            ],
        )
        .map_err(process_sqlite_error)?;
    }

    let sql = process_sql();
    let pruned_events = conn
        .execute(
            sql.event_sqlite.delete_by_process_ids.sql(),
            params![process_ids_json],
        )
        .map_err(process_sqlite_error)?;
    for dependent in [
        sql.observer_sqlite.delete_by_process_ids.sql(),
        sql.lease_sqlite.delete_by_process_ids.sql(),
        sql.handover_sqlite.delete_by_process_ids.sql(),
    ] {
        conn.execute(dependent, params![process_ids_json])
            .map_err(process_sqlite_error)?;
    }
    let pruned_processes = conn
        .execute(
            sql.process_sqlite.delete_by_ids.sql(),
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
        artifact_cleanup_acknowledgements: Vec::new(),
    })
}

/// The prune eligibility predicate: retired rows with no wake still owed and
/// no parent-end plan outstanding.
pub(crate) fn prunable_terminal_process_ids_conn(
    conn: &Connection,
    cutoff: i64,
    filter: Option<ProcessListFilter>,
    max_change_seq: Option<u64>,
) -> Result<Vec<ProcessId>, lash_core::PluginError> {
    let max_change_seq = max_change_seq.map(|seq| seq as i64);
    let mut stmt = conn
        .prepare(process_sql().process_sqlite.list_prunable_terminal.sql())
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
            prunable.push(ProcessId::from(process_id));
        }
    }

    Ok(prunable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process_registry::tx_outcome;
    use lash_core::{
        ProcessEventLogTestSupport as _, ProcessLifecycle as _, ProcessQuery as _,
        ProcessRegistrar as _,
    };

    #[tokio::test]
    async fn candidate_tombstone_divergence_rolls_back_all_prune_mutations() {
        let registry = SqliteProcessRegistry::memory()
            .await
            .expect("open prune rollback registry");
        let process_id = ProcessId::from(format!("prune-rollback:{}", uuid::Uuid::new_v4()));
        let ghost_id = format!("prune-rollback-ghost:{}", uuid::Uuid::new_v4());
        registry
            .register_process(ProcessRegistration::new(
                &process_id,
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            ))
            .await
            .expect("register rollback process");
        registry
            .complete_process(
                &process_id,
                ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                    serde_json::Value::Null,
                )),
                lash_core::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete rollback process");
        let events_before = serde_json::to_value(
            registry
                .full_event_window(&process_id, 0)
                .await
                .expect("read events before divergent prune"),
        )
        .expect("encode events before divergent prune");
        let clock_before = registry
            .conn
            .call(|conn| {
                conn.query_row(process_sql().clock_sqlite.select_current.sql(), [], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .await
            .expect("read process clock before divergent prune");

        let divergent_process_id = process_id.clone();
        let error = registry
            .conn
            .write_flow(move |tx| {
                Ok(tx_outcome(prune_process_rows_conn(
                    tx,
                    &[divergent_process_id, ProcessId::from(ghost_id)],
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
                    .full_event_window(&process_id, 0)
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
                    params![tombstone_process_id.as_str()],
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
                conn.query_row(process_sql().clock_sqlite.select_current.sql(), [], |row| {
                    row.get::<_, i64>(0)
                })
            })
            .await
            .expect("read process clock after divergent prune");
        assert_eq!(
            clock_after, clock_before,
            "divergent prune must roll back the clock"
        );
    }
}
