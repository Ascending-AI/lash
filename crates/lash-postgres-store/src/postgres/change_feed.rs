//! The change feeds' sequencing, after commit (FIG-5276).
//!
//! `turns_changed_since` and the process feed hand out cursors over a
//! sequence that must never skip a lower number that commits late. A writer
//! therefore never takes a feed sequence: it stages its change, which takes
//! the next value of the feed's staging sequence (a PostgreSQL `SEQUENCE`,
//! no row lock) and leaves its feed sequence empty. A writer holds nothing
//! another writer waits on, so commits across the fleet no longer queue on
//! the feed's clock.
//!
//! A sequencing transaction assigns the feed sequences. Under the clock row's
//! write lock it gives every committed, staged change the clock's next
//! numbers, in staging order, and moves the clock past them. Only committed
//! changes are visible to it, and its own commit publishes the whole batch at
//! once, so every change a reader can see has a sequence below every change
//! still to be sequenced: a cursor never passes a change that appears later,
//! and a change is read once. Two sequencing transactions are ordered by the
//! clock lock, and the second's statements read snapshots taken after the
//! first committed.
//!
//! The feed's order is staging order within a batch and batch order across
//! batches. Both agree with commit precedence: a change whose transaction
//! committed before another's began comes first.
//!
//! Readers sequence what is pending before they read, so a quiet feed costs a
//! reader one probe. Maintenance that judges changes by their sequence
//! (receipt retention, tombstone compaction) sequences first under the lock
//! it takes anyway.

use lash_core_execution::StoreError;
use sqlx::{PgConnection, PgPool};

use crate::guarded_tx::WriterFence;
use crate::process_sql::process_sql;
use crate::session_sql::session_sql;
use crate::store_sqlx_error;

/// A turn's commit receipt, staged on the turn feed.
#[derive(Debug)]
pub(crate) struct TurnReceipt {
    pub(crate) session_id: String,
    pub(crate) turn_id: String,
    pub(crate) turn_commit_hash: String,
    pub(crate) result_json: String,
    pub(crate) outcome_code: Option<String>,
    pub(crate) committed_at_ms: i64,
    pub(crate) request_identity_hash: Option<String>,
    pub(crate) requested_node_count: Option<i64>,
    pub(crate) identity_encoding_version: Option<i32>,
    pub(crate) failure_evidence: bool,
}

/// One change of the turn feed.
#[derive(Debug)]
pub(crate) enum TurnChange {
    /// A runtime commit's receipt.
    Receipt(TurnReceipt),
    /// A session's terminal record: its deletion, or its fault.
    SessionTerminal {
        session_id: String,
        fault_json: Option<String>,
        recorded_at_ms: i64,
    },
}

/// Write `change`, staged on the turn feed.
pub(crate) async fn stage_turn_change(
    tx: &mut PgConnection,
    change: TurnChange,
) -> Result<(), sqlx::Error> {
    let sql = &session_sql().turn_commits_postgres;
    match change {
        TurnChange::Receipt(receipt) => {
            sqlx::query(sql.insert_staged.sql())
                .bind(receipt.session_id)
                .bind(receipt.turn_id)
                .bind(receipt.turn_commit_hash)
                .bind(receipt.result_json)
                .bind(receipt.outcome_code)
                .bind(receipt.committed_at_ms)
                .bind(receipt.request_identity_hash)
                .bind(receipt.requested_node_count)
                .bind(receipt.identity_encoding_version)
                .bind(receipt.failure_evidence)
                .execute(crate::observed_sql::executor(&mut *tx))
                .await?;
        }
        TurnChange::SessionTerminal {
            session_id,
            fault_json,
            recorded_at_ms,
        } => {
            sqlx::query(sql.insert_session_terminal_staged.sql())
                .bind(session_id)
                .bind(fault_json)
                .bind(recorded_at_ms)
                .execute(crate::observed_sql::executor(&mut *tx))
                .await?;
        }
    }
    Ok(())
}

/// Lock the turn clock and sequence every committed, staged turn change, in
/// the caller's transaction. Returns how many it sequenced.
pub(crate) async fn sequence_turns(tx: &mut PgConnection) -> Result<i64, sqlx::Error> {
    let sql = &session_sql().turn_commits_postgres;
    sqlx::query(sql.lock_clock.sql())
        .execute(crate::observed_sql::executor(&mut *tx))
        .await?;
    sqlx::query_scalar(sql.sequence_committed.sql())
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
}

/// Lock the process clock and sequence every committed, staged save and
/// tombstone, in the caller's transaction. Returns how many it sequenced.
pub(crate) async fn sequence_processes(tx: &mut PgConnection) -> Result<i64, sqlx::Error> {
    let sql = &process_sql().clock_postgres;
    sqlx::query(sql.lock_clock.sql())
        .execute(crate::observed_sql::executor(&mut *tx))
        .await?;
    sqlx::query_scalar(sql.sequence_committed.sql())
        .fetch_one(crate::observed_sql::executor(&mut *tx))
        .await
}

/// Which feed a reader sequences.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Feed {
    Turns,
    Processes,
}

/// Sequence `feed`'s committed changes before a read, in a transaction of
/// its own, when the probe finds any staged.
pub(crate) async fn sequence_before_read(
    pool: &PgPool,
    fence: &WriterFence,
    feed: Feed,
) -> Result<(), StoreError> {
    let probe = match feed {
        Feed::Turns => session_sql().turn_commits_postgres.has_unsequenced.sql(),
        Feed::Processes => process_sql().clock_postgres.has_unsequenced.sql(),
    };
    let pending: bool = sqlx::query_scalar(probe)
        .fetch_one(pool)
        .await
        .map_err(store_sqlx_error)?;
    if !pending {
        return Ok(());
    }
    crate::guarded_tx::guarded(pool, fence, |tx| {
        Box::pin(async move {
            match feed {
                Feed::Turns => sequence_turns(tx.as_mut()).await,
                Feed::Processes => sequence_processes(tx.as_mut()).await,
            }
            .map(drop)
            .map_err(store_sqlx_error)
        })
    })
    .await
}
