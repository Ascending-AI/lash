//! The PREP-F twins for the integration target (FIG-3600 D1 §3.4): the same
//! constructors `crates/lash/src/tests/harness.rs` gives the unit tests, which
//! this target cannot reach. Keep the two in step.
#![allow(
    dead_code,
    reason = "PREP-F twins: the S5c batches' fixture moves are their callers"
)]
#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and these fixtures are test code too"
)]

use std::sync::Arc;

/// The Restate double a facade test runs on: lash-restate's engine and
/// services over a fresh SQLite memory store set, connected to an in-process
/// server double.
///
/// `ServerConfig::default()` schedules concurrently, so no outside gates are
/// needed. Keep the returned double alive to the end of the test (FIG-3723):
/// a core built over `double.lash_backend()` does not hold it.
pub(crate) async fn restate_double(seed: u64) -> lash_restate_test::RestateTestBackend {
    lash_restate_test::backend(seed, lash_restate_test::ServerConfig::default())
        .await
        .expect("build the Restate double")
}

/// A fresh SQLite memory store set: storage ports only, no engine.
pub(crate) async fn memory_store_set() -> Arc<lash_sqlite_store::SqliteStoreSet> {
    Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    )
}

/// A backend over a fresh SQLite memory store set whose effect host only
/// records: for a test that needs a backend value but runs no effect.
pub(crate) async fn memory_store_backend() -> lash_core::Backend {
    let stores = memory_store_set().await;
    lash_conformance::recording_backend_over(stores)
}
