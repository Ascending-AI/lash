#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]
// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::sync::Arc;

use lash_execution_contracts::deployment::DeploymentStore;
use lash_execution_contracts::process::ProcessEnvStore;
use lash_sqlite_store::{SqliteStoreSet, SqliteStoreSetOptions};

#[path = "../../lash-core/tests/support/durable_read_fixture.rs"]
mod fixture;

/// The database file a SQLite deployment keeps its store in.
const DATABASE: &str = "lash.db";

/// What this build writes, it reads back with the same meaning: seed a fresh
/// store set, reopen every handle at the read instant, and assert the semantics
/// of what the seed returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_seed_round_trips_through_a_fresh_store() {
    let temp = tempfile::tempdir().expect("SQLite round-trip tempdir");
    let written_now = seed_fresh_store(temp.path()).await;
    let handles = open_handles(temp.path(), fixture::FIXTURE_READ_MS).await;
    Box::pin(fixture::assert_semantics(&handles, &written_now)).await;
}

async fn seed_fresh_store(root: &Path) -> fixture::ExpectedFixture {
    let handles = open_handles(root, fixture::FIXTURE_WRITE_MS).await;
    Box::pin(fixture::seed(&handles)).await
}

async fn open_handles(root: &Path, timestamp_ms: u64) -> fixture::FixtureHandles {
    std::fs::create_dir_all(root).expect("create SQLite fixture root");
    let clock = Arc::new(lash_core_ids::test_clock::TestClock::new(timestamp_ms));
    let options = SqliteStoreSetOptions {
        process_id_mint: lash_core_store::process_identity::ProcessIdMint::sequential_for_testing(),
        ..SqliteStoreSetOptions::standard(lash_sqlite_store::SqliteSynchronous::Normal)
    };
    let stores = SqliteStoreSet::open_with_options_and_clock(
        root.join(DATABASE),
        options,
        Arc::clone(&clock) as Arc<dyn lash_core_ids::clock::Clock>,
    )
    .await
    .expect("open the coherent SQLite fixture");
    let runtime = stores.session_store_factory();
    let processes = stores.process_registry();
    drop(stores);
    // The handles outlive the dropped assembly; take ownership only to pin
    // the fixture's otherwise random identities.
    let runtime = Arc::new(
        Arc::try_unwrap(runtime)
            .unwrap_or_else(|_| panic!("fixture owns the catalog handle"))
            .with_commit_count_seed_for_testing(0),
    );

    fixture::FixtureHandles {
        clock: Arc::clone(&clock) as Arc<dyn lash_core_ids::clock::Clock>,
        store: Arc::clone(&runtime) as Arc<dyn DeploymentStore>,
        processes: Arc::clone(&processes)
            as Arc<dyn lash_execution_contracts::process::ConformanceProcessRegistry>,
        process_envs: runtime as Arc<dyn ProcessEnvStore>,
    }
}
