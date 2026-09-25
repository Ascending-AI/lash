//! The turn park feed's in-transaction append (FIG-3659), the PostgreSQL
//! half.
//!
//! Every park write and every park clear that is not already folded into its
//! statement's CTE chain (the commit receipt's `settled_park_*` arms, the
//! prune cascade's `deleted_turn_park_*` arms) runs the same two statements
//! inside the transaction that changed `turn_parks`: `bump_returning`
//! allocates the event's `seq` — its row lock orders writers, so `seq` order
//! is commit order — and `insert_event` writes the row. A clear that deletes
//! no park row appends nothing, so the hot path pays nothing.

use super::*;

/// Allocate one turn park feed sequence.
pub(crate) async fn allocate_turn_park_seq_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<i64, StoreError> {
    sqlx::query_scalar::<_, i64>(
        crate::turn_ingress::turn_ingress_sql()
            .turn_park_clock
            .bump_returning
            .sql(),
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(store_sqlx_error)
}

/// Insert the feed row at sequence `seq`, naming the park `park_id`
/// transitioned in `session_id`'s turn `turn_id` at `at_ms`.
async fn insert_turn_park_event_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    seq: i64,
    session_id: &SessionId,
    turn_id: &str,
    park_id: i64,
    kind: &lash_core_execution::store::ParkEventKind,
    at_ms: u64,
) -> Result<(), StoreError> {
    let (cause, reason_json) = kind.encode_columns();
    sqlx::query(
        crate::turn_ingress::turn_ingress_sql()
            .turn_park_events
            .insert_event
            .sql(),
    )
    .bind(seq)
    .bind(session_id.as_str())
    .bind(turn_id)
    .bind(park_id)
    .bind(kind.kind_code())
    .bind(cause)
    .bind(reason_json)
    .bind(i64::try_from(at_ms).unwrap_or(i64::MAX))
    .execute(&mut **tx)
    .await
    .map_err(store_sqlx_error)?;
    Ok(())
}

/// Append the `Parked` event that opens a park, returning the feed sequence
/// the park record stores as its `park_id`.
pub(crate) async fn log_turn_parked_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &str,
    reason: &lash_core_execution::store::ParkReason,
    at_ms: u64,
) -> Result<i64, StoreError> {
    let seq = allocate_turn_park_seq_tx(tx).await?;
    insert_turn_park_event_tx(
        tx,
        seq,
        session_id,
        turn_id,
        seq,
        &lash_core_execution::store::ParkEventKind::Parked {
            reason: reason.clone(),
        },
        at_ms,
    )
    .await?;
    Ok(seq)
}

/// Append the event a park clear writes — `Unparked` or `Cancelled` — naming
/// the park `park_id` the delete returned.
pub(crate) async fn log_turn_park_closed_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: &SessionId,
    turn_id: &str,
    park_id: i64,
    kind: &lash_core_execution::store::ParkEventKind,
    at_ms: u64,
) -> Result<(), StoreError> {
    let seq = allocate_turn_park_seq_tx(tx).await?;
    insert_turn_park_event_tx(tx, seq, session_id, turn_id, park_id, kind, at_ms).await
}
