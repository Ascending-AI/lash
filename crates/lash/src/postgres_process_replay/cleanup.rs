//! Retention between publications: a periodic, jittered pass per replica
//! that deletes expired events, forgets idle processes and hands unused
//! byte reservations back, plus the per-process trim.
//!
//! Reads already cut the window by database time, so cleanup only reclaims
//! space, identities and budget; any replica's pass serves every replica.

use std::sync::Arc;
use std::time::Duration;

use lash_core::{ProcessId, ProcessReplayStoreError};
use sqlx::Row as _;

use super::Shared;
use super::codec::Doorbell;
use super::heads::{Attempt, Heads, attempt};
use super::schema::{Sentinel, column, db_error, micros, notify, position, purge, rotate};

pub(super) async fn run(shared: Arc<Shared>) {
    loop {
        tokio::time::sleep(shared.config.cleanup_interval + jitter(shared.config.cleanup_jitter))
            .await;
        if let Err(error) = clean(&shared).await {
            tracing::warn!(%error, "process replay cleanup failed; the next pass retries");
        }
    }
}

/// Processes one cleanup statement expires, forgets or shrinks, as its bind.
fn chunk(shared: &Shared) -> i64 {
    // `validate` bounds the batch at 65536.
    i64::try_from(shared.config.cleanup_batch).unwrap_or(i64::MAX)
}

/// A uniform draw in `0..=bound`.
fn jitter(bound: Duration) -> Duration {
    let micros = u64::try_from(bound.as_micros()).unwrap_or(u64::MAX);
    if micros == 0 {
        return Duration::ZERO;
    }
    let draw = uuid::Uuid::new_v4().as_u128() as u64;
    Duration::from_micros(draw % (micros + 1))
}

/// One cleanup pass.
async fn clean(shared: &Shared) -> Result<(), ProcessReplayStoreError> {
    loop {
        let processes = sqlx::query(&shared.sql.expired_heads)
            .bind(micros(shared.config.max_age))
            .bind(chunk(shared))
            .fetch_all(&shared.pool)
            .await
            .map_err(db_error("find expired processes"))?
            .into_iter()
            .map(|row| row.get::<String, _>("process_id"))
            .collect::<Vec<_>>();
        if processes.is_empty() {
            break;
        }
        expire(shared, &processes).await?;
        if processes.len() < shared.config.cleanup_batch {
            break;
        }
    }
    while reclaim(shared, Reclaim::Forget).await? >= shared.config.cleanup_batch {}
    while reclaim(shared, Reclaim::Shrink).await? >= shared.config.cleanup_batch {}
    Ok(())
}

/// Apply age retention to `processes` now (sorted).
async fn expire(shared: &Shared, processes: &[String]) -> Result<(), ProcessReplayStoreError> {
    let deadline = shared
        .prelude
        .deadline()
        .map(|deadline| tokio::time::Instant::now() + deadline);
    shared
        .retry
        .run(
            deadline,
            |failed| matches!(failed, Attempt::Retry(_)),
            || expire_once(shared, processes),
        )
        .await
        .map_err(|failed| match failed {
            Attempt::Retry(_) => ProcessReplayStoreError::Store(
                "postgres process replay trim kept racing; giving up".into(),
            ),
            Attempt::Failed(error) => error,
        })
}

async fn expire_once(shared: &Shared, processes: &[String]) -> Result<(), Attempt> {
    let mut tx = shared
        .prelude
        .begin(&shared.pool)
        .await
        .map_err(attempt("begin"))?;
    let mut heads = Heads::lock(&mut tx, &shared.sql, processes, shared.config.max_age).await?;
    heads.heads.retain(|_, head| head.expiring);
    if !heads.heads.is_empty() {
        heads
            .expire(&mut tx, &shared.sql, shared.config.max_age)
            .await?;
        heads.trim_dedupe(&mut tx, &shared.sql).await?;
        heads
            .write(&mut tx, &shared.sql, &Doorbell::default())
            .await?;
    }
    tx.commit().await.map_err(attempt("commit"))?;
    Ok(())
}

/// What a reclaim pass hands back to the aggregate budget.
#[derive(Clone, Copy)]
enum Reclaim {
    /// Processes idle past the window's age with nothing retained: their
    /// heads go, and the watermark rises past their tails so a process that
    /// returns starts above every position it ever had.
    Forget,
    /// The whole steps a head reserves beyond what it retains.
    Shrink,
}

/// One batch of `reclaim`, under the heads it takes without waiting and
/// then the sentinel. Answers the heads it changed.
async fn reclaim(shared: &Shared, reclaim: Reclaim) -> Result<usize, ProcessReplayStoreError> {
    let sql = &shared.sql;
    let mut tx = shared
        .prelude
        .begin(&shared.pool)
        .await
        .map_err(db_error("begin"))?;
    let mut doorbell = Doorbell::default();
    let (changed, released, forgotten) = match reclaim {
        Reclaim::Forget => {
            let rows = sqlx::query(&sql.forget_heads)
                .bind(micros(shared.config.max_age))
                .bind(chunk(shared))
                .fetch_all(&mut *tx)
                .await
                .map_err(db_error("forget processes"))?;
            let processes = rows
                .iter()
                .map(|row| row.get::<String, _>("process_id"))
                .collect::<Vec<_>>();
            purge(&mut tx, sql, &processes)
                .await
                .map_err(db_error("forget processes"))?;
            doorbell.processes.extend(processes);
            (rows.len(), 0, rows)
        }
        Reclaim::Shrink => {
            let row = sqlx::query(&sql.shrink_reservations)
                .bind(column(shared.config.reservation_bytes as u64))
                .bind(chunk(shared))
                .fetch_one(&mut *tx)
                .await
                .map_err(db_error("shrink reservations"))?;
            (
                usize::try_from(row.get::<i64, _>("heads")).unwrap_or(0),
                position(row.get("released")),
                Vec::new(),
            )
        }
    };
    if changed == 0 {
        tx.commit().await.map_err(db_error("commit"))?;
        return Ok(0);
    }
    let Some(mut sentinel) = Sentinel::lock(&mut tx, sql)
        .await
        .map_err(db_error("lock sentinel"))?
    else {
        // The history is gone, and its budget with it.
        drop(tx);
        rotate(&shared.pool, sql, false).await?;
        return Ok(0);
    };
    sentinel.released(&forgotten);
    sentinel.release_bytes(released);
    sentinel
        .write(&mut tx, sql)
        .await
        .map_err(db_error("write sentinel"))?;
    notify(&mut tx, sql, &doorbell).await?;
    tx.commit().await.map_err(db_error("commit"))?;
    shared.ring(&doorbell);
    Ok(changed)
}

/// `trim_process`: apply age retention to one process now.
pub(super) async fn trim(
    shared: &Shared,
    process_id: &ProcessId,
) -> Result<(), ProcessReplayStoreError> {
    expire(shared, &[process_id.to_string()]).await
}
