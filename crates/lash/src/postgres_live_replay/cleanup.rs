//! Retention by age: a periodic, jittered pass per replica that deletes
//! expired events and forgets idle sessions, plus the per-session trim.
//!
//! Reads already cut the window by database time, so cleanup only reclaims
//! space and identities; any replica's pass serves every replica.
//!
//! A session is idle only while nobody follows it: each replica touches the
//! heads its subscribers follow when they subscribe and at the start of
//! every pass, so a quiet session keeps its window, and its followers
//! their cursors, for as long as one of them is connected.

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

/// Count `sessions` as accessed now: a head with a subscriber is not idle.
pub(super) async fn keep(shared: &Shared, sessions: &[String]) -> Result<(), LiveReplayStoreError> {
    if sessions.is_empty() {
        return Ok(());
    }
    sqlx::query(&shared.sql.touch_heads)
        .bind(sessions)
        .execute(&shared.pool)
        .await
        .map_err(db_error("keep followed sessions"))?;
    Ok(())
}

/// One cleanup pass.
async fn clean(shared: &Shared) -> Result<(), LiveReplayStoreError> {
    keep(shared, &shared.followed()).await?;
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
    let deadline = shared
        .prelude
        .deadline()
        .map(|deadline| tokio::time::Instant::now() + deadline);
    let doorbells = shared
        .retry
        .run(
            deadline,
            |failed| matches!(failed, Attempt::Retry(_)),
            || expire_once(shared, sessions),
        )
        .await
        .map_err(|failed| match failed {
            Attempt::Retry(_) => LiveReplayStoreError::Store(
                "postgres live replay trim kept racing; giving up".into(),
            ),
            Attempt::Failed(error) => error,
        })?;
    shared.ring_mirror(&doorbells);
    Ok(())
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

/// Forget sessions nobody touched for the window's age, with nothing
/// retained, and
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
