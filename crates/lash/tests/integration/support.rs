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

/// A fresh SQLite memory store set: storage ports only, no engine.
pub(crate) async fn sqlite_memory_store_set() -> Arc<lash_sqlite_store::SqliteStoreSet> {
    Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    )
}

/// A backend over a fresh SQLite memory store set whose effect host only
/// records: for a test that needs a backend value but runs no effect.
pub(crate) async fn sqlite_memory_store_backend() -> lash_core::Backend {
    let stores = sqlite_memory_store_set().await;
    lash_core::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    lash_conformance::backend_over(stores)
}
