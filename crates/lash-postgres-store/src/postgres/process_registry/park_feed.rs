//! Parked processes: the park feed's in-transaction append and the parked
//! reads, the PostgreSQL half (FIG-3659 NOW-B).
//!
//! A process's park lives on its record and is projected onto the
//! `parked_since_ms`/`parked_reason_code` columns by the same save that
//! writes the record. Every event append that opens or closes a park runs the
//! same two statements inside its transaction: `bump_returning` allocates the
//! event's `seq` — its row lock orders writers, so `seq` order is commit
//! order — and `insert_event` writes the row. An append that moves no park
//! appends nothing.

use lash_core_execution::store::{
    ParkEventKind, ParkFeedCursor, ParkFeedEvent, ParkFeedPage, ParkId, ParkReasonCode,
    ParkSummary, ProcessParkKey, ProcessParkQuery,
};

use super::*;

/// Append `transitions` — what one event append did to the process's park,
/// as [`process_park_transitions`](lash_core_execution::runtime::process_park_transitions)
/// computed it — to the process park feed, in the append's transaction.
pub(crate) async fn log_process_park_transitions_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    key: &ProcessParkKey,
    transitions: &[(ParkId, ParkEventKind)],
    at_ms: u64,
) -> Result<(), PluginError> {
    for (park_id, kind) in transitions {
        let seq: i64 = sqlx::query_scalar(process_sql().park_clock_postgres.bump_returning.sql())
            .fetch_one(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
        let (cause, reason_json) = kind.encode_columns();
        sqlx::query(process_sql().park_event.insert_event.sql())
            .bind(seq)
            .bind(key.as_str())
            .bind(clamp_epoch_ms(park_id.feed_sequence()))
            .bind(kind.kind_code())
            .bind(cause)
            .bind(reason_json)
            .bind(clamp_epoch_ms(at_ms))
            .execute(&mut **tx)
            .await
            .map_err(plugin_sqlx_error)?;
    }
    Ok(())
}

pub(super) async fn list_parked_processes(
    registry: &PostgresProcessRegistry,
    query: &ProcessParkQuery,
) -> Result<Vec<ProcessRecord>, PluginError> {
    let (after_since, after_process) = match &query.after {
        Some((since_ms, process)) => (
            Some(clamp_epoch_ms(*since_ms)),
            Some(process.as_str().to_string()),
        ),
        None => (None, None),
    };
    let reasons = query
        .reasons
        .as_ref()
        .filter(|reasons| !reasons.is_empty())
        .map(|reasons| {
            reasons
                .iter()
                .map(|code| code.as_str().to_string())
                .collect::<Vec<_>>()
        });
    let rows = sqlx::query_scalar::<_, String>(process_sql().process_postgres.list_parked.sql())
        .bind(i64::try_from(query.limit.get()).unwrap_or(i64::MAX))
        .bind(query.parked_at_or_before_ms.map(clamp_epoch_ms))
        .bind(after_since)
        .bind(after_process)
        .bind(reasons)
        .fetch_all(&registry.pool)
        .await
        .map_err(plugin_sqlx_error)?;
    rows.iter()
        .map(|json| serde_json::from_str(json).map_err(process_decode_error))
        .collect()
}

pub(super) async fn process_park_feed(
    registry: &PostgresProcessRegistry,
    after: ParkFeedCursor,
    limit: std::num::NonZeroUsize,
) -> Result<ParkFeedPage<ProcessParkKey>, PluginError> {
    let mut tx = registry.pool.begin().await.map_err(plugin_sqlx_error)?;
    let horizon: i64 = sqlx::query_scalar(
        process_sql()
            .park_clock_postgres
            .select_compaction_horizon_for_share
            .sql(),
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(plugin_sqlx_error)?;
    let horizon = plugin_u64_from_sql("ProcessParkClock", "compaction_horizon", horizon)?;
    if after.store_sequence() < horizon {
        return Err(PluginError::ProcessParkFeedCursorCompacted {
            horizon: ParkFeedCursor::from_store_sequence(horizon),
        });
    }
    let rows = sqlx::query(process_sql().park_event.select_events_after.sql())
        .bind(clamp_epoch_ms(after.store_sequence()))
        .bind(i64::try_from(limit.get()).unwrap_or(i64::MAX))
        .fetch_all(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
    tx.commit().await.map_err(plugin_sqlx_error)?;
    let mut page = ParkFeedPage {
        events: Vec::new(),
        next: after,
    };
    for row in rows {
        let seq = plugin_u64_from_sql("ProcessParkEvent", "seq", row.get::<i64, _>(0))?;
        let process_id: String = row.get(1);
        let park_id = plugin_u64_from_sql("ProcessParkEvent", "park_id", row.get::<i64, _>(2))?;
        let kind: String = row.get(3);
        let cause: Option<String> = row.get(4);
        let reason_json: Option<String> = row.get(5);
        let at_ms = plugin_u64_from_sql("ProcessParkEvent", "at_ms", row.get::<i64, _>(6))?;
        page.events.push(ParkFeedEvent {
            seq,
            at_ms,
            target: ProcessParkKey::from(process_id),
            park_id: ParkId::from_feed_sequence(park_id),
            kind: ParkEventKind::decode_columns(&kind, cause.as_deref(), reason_json.as_deref())
                .map_err(|error| PluginError::StoredDataCorrupt {
                    record_kind: "ProcessParkEvent".to_string(),
                    message: error.to_string(),
                })?,
        });
        page.next = ParkFeedCursor::from_store_sequence(seq);
    }
    Ok(page)
}

pub(super) async fn summarize_parked_processes(
    registry: &PostgresProcessRegistry,
) -> Result<ParkSummary, PluginError> {
    let rows = sqlx::query(process_sql().process.summarize_parked.sql())
        .fetch_all(&registry.pool)
        .await
        .map_err(plugin_sqlx_error)?;
    let mut summary = ParkSummary::default();
    for row in rows {
        let code: String = row.get(0);
        let count: i64 = row.get(1);
        let oldest: i64 = row.get(2);
        let code =
            ParkReasonCode::from_code(&code).ok_or_else(|| PluginError::StoredDataCorrupt {
                record_kind: "ProcessPark".to_string(),
                message: format!("parked reason code `{code}` is unknown"),
            })?;
        let oldest = plugin_u64_from_sql("ProcessPark", "parked_since_ms", oldest)?;
        summary
            .by_reason
            .insert(code, usize::try_from(count).unwrap_or(usize::MAX));
        summary.oldest_since_ms = Some(
            summary
                .oldest_since_ms
                .map_or(oldest, |current| current.min(oldest)),
        );
    }
    Ok(summary)
}

pub(super) async fn compact_process_park_feed(
    registry: &PostgresProcessRegistry,
    through: ParkFeedCursor,
) -> Result<(), PluginError> {
    let mut tx = registry.pool.begin().await.map_err(plugin_sqlx_error)?;
    // Locking the clock first serializes against concurrent bumps, and
    // clamping `through` to the allocated sequence keeps the horizon from
    // rising past events the feed has not yet committed.
    let current: i64 = sqlx::query_scalar(
        process_sql()
            .park_clock_postgres
            .select_current_for_update
            .sql(),
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(plugin_sqlx_error)?;
    let through_seq = clamp_epoch_ms(through.store_sequence()).min(current);
    sqlx::query(process_sql().park_event.delete_events_through.sql())
        .bind(through_seq)
        .execute(&mut *tx)
        .await
        .map_err(plugin_sqlx_error)?;
    sqlx::query(
        process_sql()
            .park_clock_postgres
            .raise_compaction_horizon
            .sql(),
    )
    .bind(through_seq)
    .execute(&mut *tx)
    .await
    .map_err(plugin_sqlx_error)?;
    tx.commit().await.map_err(plugin_sqlx_error)?;
    Ok(())
}
