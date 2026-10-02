//! Durable source projection, snapshot reads and retained deletion evidence.

use super::*;

pub(super) async fn changed_since(
    store: &SqliteTriggerStore,
    cursor: lash_core_execution::TriggerSubscriptionChangeCursor,
    limit: usize,
) -> Result<
    (
        Vec<lash_core_execution::TriggerSubscriptionChange>,
        lash_core_execution::TriggerSubscriptionChangeCursor,
    ),
    lash_core_execution::PluginError,
> {
    let sequence = plugin_sql_counter_value(
        "trigger_subscription_change_cursor",
        cursor.store_sequence(),
    )?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    store.conn
        .call(move |conn| {
            Ok((|| {
                let tx = conn.unchecked_transaction().map_err(process_sqlite_error)?;
                let sql = &trigger_sql().subscription_change;
                let horizon: i64 = tx
                    .query_row(
                        trigger_sql().subscription_change_clock.clock.sql(),
                        [],
                        |row| row.get(1),
                    )
                    .map_err(process_sqlite_error)?;
                if sequence < horizon {
                    return Err(lash_core_execution::PluginError::TriggerSubscriptionChangeCursorPruned {
                        requested_cursor: cursor,
                        tombstone_compaction_horizon: lash_core_execution::TriggerSubscriptionChangeCursor::from_store_sequence(horizon as u64),
                    });
                }
                let mut next = cursor;
                let mut changes = Vec::new();
                {
                    let mut stmt = tx.prepare(sql.page.sql()).map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map(params![sequence, limit], |row| {
                            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                        })
                        .map_err(process_sqlite_error)?;
                    for row in rows {
                        let (seq, json) = row.map_err(process_sqlite_error)?;
                        changes.push(serde_json::from_str(&json).map_err(process_decode_error)?);
                        next = lash_core_execution::TriggerSubscriptionChangeCursor::from_store_sequence(seq as u64);
                    }
                }
                tx.commit().map_err(process_sqlite_error)?;
                Ok((changes, next))
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn snapshot(
    store: &SqliteTriggerStore,
) -> Result<
    (
        Vec<lash_core_execution::TriggerSubscriptionRecord>,
        lash_core_execution::TriggerSubscriptionChangeCursor,
    ),
    lash_core_execution::PluginError,
> {
    store
        .conn
        .call(move |conn| {
            Ok((|| {
                let tx = conn.unchecked_transaction().map_err(process_sqlite_error)?;
                let seq: i64 = tx
                    .query_row(
                        trigger_sql().subscription_change_clock.clock.sql(),
                        [],
                        |row| row.get(0),
                    )
                    .map_err(process_sqlite_error)?;
                let mut records = Vec::new();
                {
                    let mut stmt = tx
                        .prepare(trigger_sql().subscription.live_snapshot.sql())
                        .map_err(process_sqlite_error)?;
                    let rows = stmt
                        .query_map([], |row| row.get::<_, String>(0))
                        .map_err(process_sqlite_error)?;
                    for row in rows {
                        records.push(
                            lash_core_execution::facade_support::decode_trigger_subscription_json(
                                &row.map_err(process_sqlite_error)?,
                            )?,
                        );
                    }
                }
                tx.commit().map_err(process_sqlite_error)?;
                Ok((
                    records,
                    lash_core_execution::TriggerSubscriptionChangeCursor::from_store_sequence(
                        seq as u64,
                    ),
                ))
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn compact(
    store: &SqliteTriggerStore,
    cutoff_epoch_ms: u64,
) -> Result<usize, lash_core_execution::PluginError> {
    let cutoff = i64::try_from(cutoff_epoch_ms).unwrap_or(i64::MAX);
    store
        .conn
        .write(move |tx| {
            let sql = &trigger_sql().subscription_change;
            let max: Option<i64> =
                tx.query_row(sql.compactable.sql(), params![cutoff], |row| row.get(0))?;
            let count = tx.execute(sql.compact.sql(), params![cutoff])?;
            if let Some(max) = max {
                tx.execute(
                    trigger_sql().subscription_change_clock.horizon.sql(),
                    params![max],
                )?;
            }
            Ok(count)
        })
        .await
        .map_err(process_sqlite_error)
}

pub(super) fn record_subscription_change(
    tx: &Connection,
    record: &lash_core_execution::TriggerSubscriptionRecord,
) -> Result<(), lash_core_execution::PluginError> {
    let sql = &trigger_sql().subscription_change;
    let change = lash_core_execution::TriggerSubscriptionChange::from(record);
    let json = serde_json::to_string(&change).map_err(process_decode_error)?;
    let previous: Option<String> = tx
        .query_row(sql.previous.sql(), params![record.subscription_id], |row| {
            row.get(0)
        })
        .optional()
        .map_err(process_sqlite_error)?;
    if previous.as_deref() == Some(json.as_str()) {
        return Ok(());
    }
    let seq: Option<i64> = tx
        .query_row(
            trigger_sql().subscription_change_clock.bump.sql(),
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(process_sqlite_error)?;
    let seq = seq.ok_or_else(
        || lash_core_execution::PluginError::MonotonicCounterOverflow {
            counter: "trigger_subscription_change_sequence".into(),
            current: i64::MAX as u64,
        },
    )?;
    tx.execute(
        sql.upsert.sql(),
        params![
            record.subscription_id,
            seq,
            record.lifecycle.deleted_at_ms().map(|ms| ms as i64),
            json
        ],
    )
    .map_err(process_sqlite_error)?;
    Ok(())
}
