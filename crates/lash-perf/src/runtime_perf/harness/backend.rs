//! The backends the runtime perf harness measures over.
//!
//! Every benchmark core runs on one [`lash::Backend`]. The in-process lane
//! is lash-restate's engine on the in-process Restate server double over a
//! SQLite memory store set; the durable lanes open a SQLite file or a
//! PostgreSQL backend. A lane that measures persistence puts the perf store
//! decorator in front of the backend's session catalog; every other port
//! stays the backend's.

use std::sync::Arc;

use lash_core::{Backend, SessionStoreFactory};

/// The seed of every in-process lane's server double. A perf run measures
/// cost, not a schedule, so one fixed seed serves every scenario.
const RESTATE_SEED: u64 = 0x5eed_9e4f;

/// `inner` with its session catalog decorated. It keeps
/// the inner backend's Lashlang artifacts, so an RLM factory built over it
/// keeps them there too.
pub(super) struct PerfBackend {
    layered: lash_core::testing::runtime_helpers::LayeredBackend,
}

impl PerfBackend {
    /// `inner`, undecorated.
    pub(super) fn over(inner: Backend) -> Self {
        Self {
            layered: lash_core::testing::runtime_helpers::LayeredBackend::over(inner),
        }
    }

    /// The Restate test backend, undecorated. Its Lashlang artifacts live in
    /// its store set, as a deployment's do.
    pub(super) fn over_restate(backend: &lash_restate_test::RestateTestBackend) -> Self {
        Self::over(backend.lash_backend())
    }

    /// Serve sessions from `catalog`, the perf store decorator.
    pub(super) fn with_catalog(self, catalog: Arc<dyn SessionStoreFactory>) -> Self {
        Self {
            layered: self.layered.map_session_store_factory(|_| catalog),
        }
    }
}

impl From<PerfBackend> for Backend {
    fn from(backend: PerfBackend) -> Self {
        backend.layered.into_backend()
    }
}

/// A fresh Restate test backend for the in-process lane: lash-restate's
/// engine on a new server double over a new SQLite memory store set.
pub(crate) async fn restate_backend() -> anyhow::Result<lash_restate_test::RestateTestBackend> {
    lash_restate_test::backend(RESTATE_SEED, lash_restate_test::ServerConfig::default())
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))
}

/// A fresh SQLite memory store set: storage only, for the store-level
/// scenarios that drive no engine.
pub(crate) async fn memory_stores() -> anyhow::Result<lash_sqlite_store::SqliteStoreSet> {
    lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))
}
