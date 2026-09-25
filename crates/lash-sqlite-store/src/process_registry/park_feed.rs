//! Parked processes: the park feed's in-transaction append and the parked
//! reads (FIG-3659 NOW-B).
//!
//! A process's park lives on its record and is projected onto the
//! `parked_since_ms`/`parked_reason_code` columns by the same save that
//! writes the record. Every event append that opens or closes a park runs the
//! same two steps inside its transaction: bump `process_park_clock` to
//! allocate the event's `seq` — the write transaction's lock orders writers,
//! so `seq` order is commit order — then insert the `process_park_events`
//! row. An append that moves no park appends nothing.

use lash_core_execution::store::{
    ParkEventKind, ParkFeedCursor, ParkFeedEvent, ParkFeedPage, ParkId, ParkReasonCode,
    ParkSummary, ProcessParkKey, ProcessParkQuery,
};

use super::*;

/// Allocate one process park feed sequence: the `bump` then the read-back
/// run back to back under the write lock.
fn allocate_process_park_seq_conn(
    conn: &Connection,
) -> Result<i64, lash_core_execution::PluginError> {
    let clock = &process_sql().park_clock_sqlite;
    let bumped = conn
        .execute(clock.bump.sql(), [])
        .map_err(process_sqlite_error)?;
    if bumped != 1 {
        return Err(lash_core_execution::PluginError::Session(format!(
            "process park clock bump touched {bumped} rows, expected its one seed row"
        )));
    }
    conn.query_row(clock.select_current.sql(), [], |row| row.get(0))
        .map_err(process_sqlite_error)
}

/// Append `transitions` — what one event append did to the process's park,
/// as [`process_park_transitions`](lash_core_execution::runtime::process_park_transitions)
/// computed it — to the process park feed, in the append's transaction.
pub(crate) fn log_process_park_transitions_conn(
    conn: &Connection,
    key: &ProcessParkKey,
    transitions: &[(ParkId, ParkEventKind)],
    at_ms: u64,
) -> Result<(), lash_core_execution::PluginError> {
    for (park_id, kind) in transitions {
        let seq = allocate_process_park_seq_conn(conn)?;
        let (cause, reason_json) = kind.encode_columns();
        conn.execute(
            process_sql().park_event.insert_event.sql(),
            params![
                seq,
                key.as_str(),
                crate::clamp_epoch_ms(park_id.feed_sequence()),
                kind.kind_code(),
                cause,
                reason_json,
                crate::clamp_epoch_ms(at_ms),
            ],
        )
        .map_err(process_sqlite_error)?;
    }
    Ok(())
}

pub(super) async fn list_parked_processes(
    registry: &SqliteProcessRegistry,
    query: &ProcessParkQuery,
) -> Result<Vec<ProcessRecord>, lash_core_execution::PluginError> {
    let limit = i64::try_from(query.limit.get()).unwrap_or(i64::MAX);
    let at_or_before = query.parked_at_or_before_ms.map(crate::clamp_epoch_ms);
    let (after_since, after_process) = match &query.after {
        Some((since_ms, process)) => (
            Some(crate::clamp_epoch_ms(*since_ms)),
            Some(process.as_str().to_string()),
        ),
        None => (None, None),
    };
    let reasons = query
        .reasons
        .as_ref()
        .filter(|reasons| !reasons.is_empty())
        .map(|reasons| {
            serde_json::to_string(&reasons.iter().map(|code| code.as_str()).collect::<Vec<_>>())
                .map_err(process_decode_error)
        })
        .transpose()?;
    registry
        .conn
        .call(move |conn| {
            Ok((|| {
                let mut statement = conn
                    .prepare(process_sql().process_sqlite.list_parked.sql())
                    .map_err(process_sqlite_error)?;
                let rows = statement
                    .query_map(
                        params![limit, at_or_before, after_since, after_process, reasons],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(process_sqlite_error)?;
                let mut records = Vec::new();
                for row in rows {
                    let json = row.map_err(process_sqlite_error)?;
                    records.push(serde_json::from_str(&json).map_err(process_decode_error)?);
                }
                Ok(records)
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn process_park_feed(
    registry: &SqliteProcessRegistry,
    after: ParkFeedCursor,
    limit: std::num::NonZeroUsize,
) -> Result<ParkFeedPage<ProcessParkKey>, lash_core_execution::PluginError> {
    let after_seq = crate::clamp_epoch_ms(after.store_sequence());
    let limit = i64::try_from(limit.get()).unwrap_or(i64::MAX);
    registry
        .conn
        .call(move |conn| {
            Ok((|| {
                let horizon: i64 = conn
                    .query_row(
                        process_sql()
                            .park_clock_sqlite
                            .select_compaction_horizon
                            .sql(),
                        [],
                        |row| row.get(0),
                    )
                    .map_err(process_sqlite_error)?;
                if after_seq < horizon {
                    return Err(
                        lash_core_execution::PluginError::ProcessParkFeedCursorCompacted {
                            horizon: ParkFeedCursor::from_store_sequence(plugin_u64_from_sql(
                                "ProcessParkClock",
                                "compaction_horizon",
                                horizon,
                            )?),
                        },
                    );
                }
                let mut statement = conn
                    .prepare(process_sql().park_event.select_events_after.sql())
                    .map_err(process_sqlite_error)?;
                let rows = statement
                    .query_map(params![after_seq, limit], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, i64>(6)?,
                        ))
                    })
                    .map_err(process_sqlite_error)?;
                let mut page = ParkFeedPage {
                    events: Vec::new(),
                    next: after,
                };
                for row in rows {
                    let (seq, process_id, park_id, kind, cause, reason_json, at_ms) =
                        row.map_err(process_sqlite_error)?;
                    let seq = plugin_u64_from_sql("ProcessParkEvent", "seq", seq)?;
                    page.events.push(ParkFeedEvent {
                        seq,
                        at_ms: plugin_u64_from_sql("ProcessParkEvent", "at_ms", at_ms)?,
                        target: ProcessParkKey::from(process_id),
                        park_id: ParkId::from_feed_sequence(plugin_u64_from_sql(
                            "ProcessParkEvent",
                            "park_id",
                            park_id,
                        )?),
                        kind: ParkEventKind::decode_columns(
                            &kind,
                            cause.as_deref(),
                            reason_json.as_deref(),
                        )
                        .map_err(|error| {
                            lash_core_execution::PluginError::StoredDataCorrupt {
                                record_kind: "ProcessParkEvent".to_string(),
                                message: error.to_string(),
                            }
                        })?,
                    });
                    page.next = ParkFeedCursor::from_store_sequence(seq);
                }
                Ok(page)
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn summarize_parked_processes(
    registry: &SqliteProcessRegistry,
) -> Result<ParkSummary, lash_core_execution::PluginError> {
    registry
        .conn
        .call(move |conn| {
            Ok((|| {
                let mut statement = conn
                    .prepare(process_sql().process.summarize_parked.sql())
                    .map_err(process_sqlite_error)?;
                let rows = statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    })
                    .map_err(process_sqlite_error)?;
                let mut summary = ParkSummary::default();
                for row in rows {
                    let (code, count, oldest) = row.map_err(process_sqlite_error)?;
                    let code = ParkReasonCode::from_code(&code).ok_or_else(|| {
                        lash_core_execution::PluginError::StoredDataCorrupt {
                            record_kind: "ProcessPark".to_string(),
                            message: format!("parked reason code `{code}` is unknown"),
                        }
                    })?;
                    let count = usize::try_from(count).unwrap_or(usize::MAX);
                    let oldest = plugin_u64_from_sql("ProcessPark", "parked_since_ms", oldest)?;
                    summary.by_reason.insert(code, count);
                    summary.oldest_since_ms = Some(
                        summary
                            .oldest_since_ms
                            .map_or(oldest, |current| current.min(oldest)),
                    );
                }
                Ok(summary)
            })())
        })
        .await
        .map_err(process_sqlite_error)?
}

pub(super) async fn compact_process_park_feed(
    registry: &SqliteProcessRegistry,
    through: ParkFeedCursor,
) -> Result<(), lash_core_execution::PluginError> {
    let through_seq = crate::clamp_epoch_ms(through.store_sequence());
    registry
        .conn
        .write_flow(move |tx| {
            Ok(tx_outcome((|| {
                let sql = process_sql();
                // The write lock is held, so this read is the clock's
                // committed sequence. `through` is clamped to it: raising the
                // horizon past `current_seq` would strand events the feed has
                // not yet appended.
                let current: i64 = tx
                    .query_row(sql.park_clock_sqlite.select_current.sql(), [], |row| {
                        row.get(0)
                    })
                    .map_err(process_sqlite_error)?;
                let through_seq = through_seq.min(current);
                tx.execute(
                    sql.park_event.delete_events_through.sql(),
                    params![through_seq],
                )
                .map_err(process_sqlite_error)?;
                tx.execute(
                    sql.park_clock_sqlite.raise_compaction_horizon.sql(),
                    params![through_seq],
                )
                .map_err(process_sqlite_error)?;
                Ok(())
            })()))
        })
        .await
        .map_err(process_sqlite_error)?
}
