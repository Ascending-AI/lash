//! Retention by age: a periodic, jittered pass per replica that deletes
//! expired events and forgets idle sessions, plus the per-session trim.
//!
//! Reads already cut the window by database time, so cleanup only reclaims
//! space and identities; any replica's pass serves every replica.

use std::sync::Arc;
use std::time::Duration;

use lash_core::LiveReplayStoreError;
use lash_sansio::SessionId;
use sqlx::Row as _;

use super::Shared;
use super::codec::Doorbell;
use super::heads::{Attempt, Heads, attempt};
use super::schema::{column, db_error, micros, notify, position};

pub(super) async fn run(shared: Arc<Shared>) {
    loop {
        tokio::time::sleep(shared.config.cleanup_interval + jitter(shared.config.cleanup_jitter))
            .await;
        if let Err(error) = clean(&shared).await {
            tracing::warn!(%error, "live replay cleanup failed; the next pass retries");
        }
    }
}

/// Sessions one cleanup statement expires or forgets, as its bind.
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
async fn clean(shared: &Shared) -> Result<(), LiveReplayStoreError> {
    loop {
        let sessions = sqlx::query(&shared.sql.expired_heads)
            .bind(micros(shared.config.max_age))
            .bind(chunk(shared))
            .fetch_all(&shared.pool)
            .await
            .map_err(db_error("find expired sessions"))?
            .into_iter()
            .map(|row| row.get::<String, _>("session_id"))
            .collect::<Vec<_>>();
        if sessions.is_empty() {
            break;
        }
        expire(shared, &sessions).await?;
        if sessions.len() < shared.config.cleanup_batch {
            break;
        }
    }
    forget(shared).await
}

/// Apply age retention to `sessions` now (sorted).
pub(super) async fn expire(
    shared: &Shared,
    sessions: &[String],
) -> Result<(), LiveReplayStoreError> {
    for retry in 0..shared.retry.attempts {
        if retry > 0 {
            tokio::time::sleep(shared.retry.pause(retry - 1)).await;
        }
        match expire_once(shared, sessions).await {
            Ok(doorbells) => {
                shared.ring_mirror(&doorbells);
                return Ok(());
            }
            Err(Attempt::Retry(_)) => {}
            Err(Attempt::Failed(error)) => return Err(error),
        }
    }
    Err(LiveReplayStoreError::Store(
        "postgres live replay trim kept racing; giving up".into(),
    ))
}

async fn expire_once(shared: &Shared, sessions: &[String]) -> Result<Vec<Doorbell>, Attempt> {
    let mut tx = shared
        .prelude
        .begin(&shared.pool)
        .await
        .map_err(attempt("begin"))?;
    let mut heads =
        Heads::lock(&mut tx, &shared.sql, sessions, shared.config.max_age, false).await?;
    heads.heads.retain(|_, head| head.expiring);
    let mut doorbells = Vec::new();
    if !heads.heads.is_empty() {
        heads
            .expire(&mut tx, &shared.sql, shared.config.max_age)
            .await?;
        heads.trimmed(&mut doorbells);
        heads.write(&mut tx, &shared.sql, &doorbells).await?;
    }
    tx.commit().await.map_err(attempt("commit"))?;
    Ok(doorbells)
}

/// Forget sessions idle past the window's age with nothing retained, and
/// raise the watermark past their tails so a session that returns starts
/// above every position it ever had (P6).
async fn forget(shared: &Shared) -> Result<(), LiveReplayStoreError> {
    loop {
        let mut tx = shared
            .prelude
            .begin(&shared.pool)
            .await
            .map_err(db_error("begin"))?;
        let forgotten = sqlx::query(&shared.sql.forget_heads)
            .bind(micros(shared.config.max_age))
            .bind(chunk(shared))
            .fetch_all(&mut *tx)
            .await
            .map_err(db_error("forget sessions"))?;
        if forgotten.is_empty() {
            return tx.commit().await.map_err(db_error("commit"));
        }
        let above = forgotten
            .iter()
            .map(|row| position(row.get("tail_position")) + 1)
            .max()
            .unwrap_or_default();
        let watermark = position(
            sqlx::query(&shared.sql.raise_watermark)
                .bind(column(above))
                .fetch_one(&mut *tx)
                .await
                .map_err(db_error("raise watermark"))?
                .get("watermark"),
        );
        let doorbells = vec![Doorbell::Forgotten {
            sessions: forgotten
                .iter()
                .map(|row| row.get::<String, _>("session_id"))
                .collect(),
            watermark,
        }];
        notify(&mut tx, &shared.sql, &doorbells).await?;
        tx.commit().await.map_err(db_error("commit"))?;
        shared.ring_mirror(&doorbells);
        shared.ring_sessions(&doorbells);
        if forgotten.len() < shared.config.cleanup_batch {
            return Ok(());
        }
    }
}

/// `trim_session`: apply age retention to one session now.
pub(super) async fn trim(
    shared: &Shared,
    session_id: &SessionId,
) -> Result<(), LiveReplayStoreError> {
    expire(shared, &[session_id.to_string()]).await
}
