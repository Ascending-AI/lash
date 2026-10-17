#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]
// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use lash_execution_contracts::deployment::DeploymentStore;
use lash_execution_contracts::process::ProcessEnvStore;
use lash_postgres_store::PostgresStorage;
use sqlx::postgres::PgPoolOptions;

mod support;

#[path = "../../lash-core/tests/support/durable_read_fixture.rs"]
mod fixture;

const FIXTURE_SCHEMA: &str = "lash_durable_read_fixture";

/// What this build writes, it reads back with the same meaning: seed a fresh
/// schema, reopen every handle at the read instant, and assert the semantics of
/// what the seed returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_seed_round_trips_through_a_fresh_store_when_configured() {
    let Some(database_url) = support::database_url() else {
        eprintln!("skipping Postgres durable round trip: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    let _database_lock = support::SharedDatabaseLock::acquire(&database_url).await;
    recreate_fixture_schema(&database_url).await;
    let fixture_database_url = fixture_database_url(&database_url);
    let storage = lash_postgres_store::testing::connect(&fixture_database_url)
        .await
        .expect("provision the Postgres round-trip schema");
    let written_now = {
        let handles = open_handles(&storage, fixture::FIXTURE_WRITE_MS);
        Box::pin(fixture::seed(&handles)).await
    };
    storage.pool().close().await;
    let storage = lash_postgres_store::testing::connect(&fixture_database_url)
        .await
        .expect("reopen the Postgres round-trip schema");
    let handles = open_handles(&storage, fixture::FIXTURE_READ_MS);
    Box::pin(fixture::assert_semantics(&handles, &written_now)).await;
    drop(handles);
    storage.pool().close().await;
    drop_fixture_schema(&database_url).await;
}

fn open_handles(storage: &PostgresStorage, timestamp_ms: u64) -> fixture::FixtureHandles {
    let clock = Arc::new(lash_core_ids::test_clock::TestClock::new(timestamp_ms));
    let processes = Arc::new(
        storage
            .process_registry()
            .with_clock(Arc::clone(&clock) as Arc<dyn lash_core_ids::clock::Clock>)
            .with_process_id_mint_for_testing(
                lash_core_store::process_identity::ProcessIdMint::sequential_for_testing(),
            ),
    );
    let process_envs = Arc::new(storage.process_env_store());

    let store =
        Arc::new(
            storage
                .session_store_factory()
                .with_lease_clock_for_testing(
                    Arc::clone(&clock) as Arc<dyn lash_core_ids::clock::Clock>
                )
                .with_clock(Arc::clone(&clock) as Arc<dyn lash_core_ids::clock::Clock>),
        );
    fixture::FixtureHandles {
        clock: Arc::clone(&clock) as Arc<dyn lash_core_ids::clock::Clock>,

        store: store as Arc<dyn DeploymentStore>,
        processes: Arc::clone(&processes)
            as Arc<dyn lash_execution_contracts::process::ConformanceProcessRegistry>,
        process_envs: process_envs as Arc<dyn ProcessEnvStore>,
    }
}

async fn recreate_fixture_schema(database_url: &str) {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(database_url)
        .await
        .expect("connect for Postgres durable-fixture reset");
    sqlx::raw_sql(&format!(
        "DROP SCHEMA IF EXISTS {FIXTURE_SCHEMA} CASCADE; CREATE SCHEMA {FIXTURE_SCHEMA};"
    ))
    .execute(&pool)
    .await
    .expect("recreate dedicated Postgres durable-fixture schema");
    // Worker open never provisions (FIG-3797): the fixture schema gets its
    // tables from the committed artifact, applied the way `lash migrate` does.
    sqlx::raw_sql(&format!("SET search_path TO {FIXTURE_SCHEMA};"))
        .execute(&pool)
        .await
        .expect("point the fixture pool at the recreated schema");
    sqlx::raw_sql(PostgresStorage::schema_ddl())
        .execute(&pool)
        .await
        .expect("provision the durable-fixture schema from schema.sql");
    pool.close().await;
}

async fn drop_fixture_schema(database_url: &str) {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(database_url)
        .await
        .expect("connect for Postgres durable-fixture teardown");
    sqlx::raw_sql(&format!("DROP SCHEMA IF EXISTS {FIXTURE_SCHEMA} CASCADE;"))
        .execute(&pool)
        .await
        .expect("drop dedicated Postgres durable-fixture schema");
    pool.close().await;
}

fn fixture_database_url(database_url: &str) -> String {
    let separator = if database_url.contains('?') { '&' } else { '?' };
    format!("{database_url}{separator}options=-csearch_path%3D{FIXTURE_SCHEMA}")
}
