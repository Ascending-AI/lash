//! The faults a soak injects into a PostgreSQL database itself: a holder of
//! the writer fence that makes the engine's transactions hit their
//! `lock_timeout`, and a restart that terminates every connection.

use std::sync::Arc;
use std::time::Duration;

use crate::crash_matrix::world::World;

/// Hold the writer fence from another session for `length` of wall time,
/// released (rolled back) when the hold ends or the run does. PostgreSQL's
/// lock waits use wall time, and blocked store calls stop virtual time;
/// the fault's release must therefore run independently of virtual time.
/// Every engine write takes that fence first, so each one
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
    world.spawn(async move {
        tokio::time::sleep(length).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crash_matrix::deployment::{self, Dialect, Keep};
    use lash_durable_test::SimClock;

    /// A database lock fault must expire while blocked database calls keep
    /// virtual time stopped. It must still make a writer hit lock_timeout.
    #[tokio::test]
    #[ignore = "requires PostgreSQL; select inside a with-service.sh pg gate"]
    async fn writer_fence_storm_expires_while_virtual_time_is_stopped() {
        let dialect = Dialect::postgres_from_env().expect("a PostgreSQL server");
        let clock = SimClock::new();
        let mut keep = Keep::new();
        let (backend, _) = deployment::open(&dialect, Arc::clone(&clock), &mut keep)
            .await
            .expect("open the deployment");
        let world = Arc::new(World::default());
        world.set_parts(backend, Arc::clone(&clock));
        let url = deployment::isolated_url(&keep).expect("the isolated database");
        let pool = sqlx::PgPool::connect(&url).await.expect("a writer pool");
        hold_writer_fence(&world, &url, Duration::from_millis(100))
            .await
            .expect("hold the writer fence");
        let mut blocked = pool.begin().await.expect("begin a blocked writer");
        sqlx::query("SET LOCAL lock_timeout = '20ms'")
            .execute(&mut *blocked)
            .await
            .expect("bound the writer's lock wait");
        let error = sqlx::query("SELECT pg_advisory_xact_lock_shared(715425, 0)")
            .execute(&mut *blocked)
            .await
            .expect_err("the storm makes the writer time out");
        assert_eq!(
            error
                .as_database_error()
                .and_then(|error| error.code())
                .as_deref(),
            Some("55P03")
        );
        blocked
            .rollback()
            .await
            .expect("roll back the refused writer");
        tokio::time::timeout(Duration::from_secs(1), async {
            let mut writer = pool.begin().await.expect("begin the next writer");
            sqlx::query("SELECT pg_advisory_xact_lock_shared(715425, 0)")
                .execute(&mut *writer)
                .await
                .expect("the storm releases the fence");
            writer.rollback().await.expect("roll back the next writer");
        })
        .await
        .expect("the database fault expires without advancing virtual time");
        assert_eq!(clock.logical_ms(), 0);
        world.stop_tasks();
        pool.close().await;
    }
}
