//! The backends the runtime perf harness measures over.
//!
//! Every benchmark core runs on one [`lash::Backend`]: lash's durable
//! engine, which I0 (FIG-5194) assembles and L3 (FIG-5172) makes serve. The
//! in-process lane runs it over a SQLite memory store set, the durable lane
//! over a SQLite store set on disk. A lane that measures persistence puts the perf store decorator in
//! front of the backend's session catalog; every other port stays the
//! backend's.

use std::sync::Arc;

use lash_core::{Backend, DeploymentStore};

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

    /// Serve sessions from `catalog`, the perf store decorator.
    pub(super) fn with_catalog(self, catalog: Arc<dyn DeploymentStore>) -> Self {
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

/// The durable engine's backend over `stores`.
pub(crate) fn durable_backend(stores: Arc<dyn lash_core::StoreSet>) -> anyhow::Result<Backend> {
    lash::durable::DurableBackendBuilder::new(stores)
        .build()
        .map_err(|err| anyhow::anyhow!(err.to_string()))
}

/// A fresh SQLite memory store set: storage only, for the store-level
/// scenarios that execute no engine.
pub(crate) async fn sqlite_memory_stores() -> anyhow::Result<lash_sqlite_store::SqliteStoreSet> {
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    lash_core::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    Ok(stores)
}
