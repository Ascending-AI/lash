//! The faults a soak injects into a PostgreSQL database itself: a holder of
//! the writer fence that makes the engine's transactions hit their
//! `lock_timeout`, and a restart that terminates every connection.

use std::sync::Arc;
use std::time::Duration;

use crate::crash_matrix::world::World;

/// Hold the writer fence's fleet-format row from another session for
/// `length` of virtual time, released (rolled back) when the hold ends or
/// the run does. Every engine write takes that fence first, so each one
/// meanwhile waits for its `lock_timeout` and fails as contended, and its
/// caller retries.
///
/// # Errors
///
/// The holder could not connect or take the row.
pub async fn hold_writer_fence(
    world: &Arc<World>,
    url: &str,
    length: Duration,
) -> Result<(), String> {
    let pool = sqlx::PgPool::connect(url)
        .await
        .map_err(|error| error.to_string())?;
    let held = lash_postgres_store::testing::HeldFinalize::begin(
        &pool,
        lash_core_execution::FleetFormat::current().version(),
    )
    .await
    .map_err(|error| error.to_string())?;
    let timer = Arc::clone(world);
    world.spawn(async move {
        timer.sleep(length).await;
        drop(held);
        pool.close().await;
    });
    Ok(())
}

/// Terminate every other connection to the database, as a server restart
/// does; answers how many were terminated.
///
/// # Errors
///
/// The terminating session could not connect or run.
pub async fn terminate_connections(url: &str) -> Result<i64, String> {
    let pool = sqlx::PgPool::connect(url)
        .await
        .map_err(|error| error.to_string())?;
    let terminated: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = current_database() AND pid <> pg_backend_pid()) AS terminated",
    )
    .fetch_one(&pool)
    .await
    .map_err(|error| error.to_string())?;
    pool.close().await;
    Ok(terminated)
}
