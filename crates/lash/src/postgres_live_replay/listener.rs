//! One LISTEN connection per replica.
//!
//! The listener confirms its LISTEN, then loads the mirror, then reports
//! itself listening; a subscribe waits for that, so every doorbell after a
//! subscriber registers reaches it (P7). When the connection drops it
//! reconnects with backoff and rings every local subscription, which then
//! re-reads from its cursor: notifications sent while it was away are
//! lost, the rows are not.

use std::sync::Arc;

use lash_core::LiveReplayStoreError;
use lash_sansio::SessionId;
use sqlx::Row as _;
use sqlx::postgres::PgListener;

use super::Shared;
use super::codec::unpack_doorbells;
use super::schema::{db_error, ensure_incarnation, position};

pub(super) async fn run(shared: Arc<Shared>) {
    let mut failures = 0_u32;
    let mut epoch = 0_u64;
    loop {
        match connect(&shared).await {
            Ok(mut listener) => {
                epoch += 1;
                shared.listening.send_replace(Some(epoch));
                failures = 0;
                shared.ring_all();
                loop {
                    match listener.try_recv().await {
                        Ok(Some(notification)) => {
                            let doorbells = unpack_doorbells(notification.payload());
                            shared.ring_mirror(&doorbells);
                            shared.ring_sessions(&doorbells);
                        }
                        Ok(None) => break,
                        Err(error) => {
                            tracing::warn!(%error, "the live replay listener failed");
                            break;
                        }
                    }
                }
                shared.listening.send_replace(None);
                tracing::warn!("the live replay listener lost its connection; reconnecting");
            }
            Err(error) => {
                tracing::warn!(%error, "the live replay listener could not connect");
            }
        }
        tokio::time::sleep(shared.reconnect.wait(failures)).await;
        failures = failures.saturating_add(1);
    }
}

/// Listen, confirmed, then load what the tables hold.
async fn connect(shared: &Shared) -> Result<PgListener, LiveReplayStoreError> {
    let mut listener = PgListener::connect_with(&shared.listener_pool)
        .await
        .map_err(db_error("listen"))?;
    listener
        .listen(&shared.sql.channel)
        .await
        .map_err(db_error("listen"))?;
    reload(shared).await?;
    Ok(listener)
}

/// Replace the mirror with the incarnation, heads and revision runs the
/// tables hold now. A doorbell after the LISTEN that this load already
/// reflects merges into it idempotently.
async fn reload(shared: &Shared) -> Result<(), LiveReplayStoreError> {
    let incarnation = ensure_incarnation(&shared.pool, &shared.sql).await?;
    let heads = sqlx::query(&shared.sql.load_heads)
        .fetch_all(&shared.pool)
        .await
        .map_err(db_error("load heads"))?
        .into_iter()
        .filter_map(|row| {
            let session = SessionId::parse(row.get::<String, _>("session_id")).ok()?;
            Some((
                session,
                position(row.get("tail_position")),
                position(row.get("floor_position")),
                position(row.get("first_retained")),
            ))
        })
        .collect();
    let runs = sqlx::query(&shared.sql.load_runs)
        .fetch_all(&shared.pool)
        .await
        .map_err(db_error("load runs"))?
        .into_iter()
        .filter_map(|row| {
            let session = SessionId::parse(row.get::<String, _>("session_id")).ok()?;
            Some((
                session,
                position(row.get("first")),
                position(row.get("last")),
                position(row.get("revision")),
            ))
        })
        .collect();
    shared.reload_mirror(incarnation, heads, runs);
    Ok(())
}
