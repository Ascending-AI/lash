//! The backends the runtime perf harness measures over.
//!
//! Every benchmark core runs on one [`lash::Backend`]: lash's durable
//! engine, which I0 (FIG-5194) assembles and L3 (FIG-5172) makes serve. The
//! in-process lane runs it over a SQLite memory store set, the durable lane
//! over a SQLite store set on disk. A lane that measures persistence decorates
//! the session catalog and durable transaction port before building the backend,
//! so the served node uses both measurement ports.

use std::sync::Arc;

use lash_core::Backend;

/// The durable engine's backend over `stores`.
pub(crate) fn durable_backend(stores: Arc<dyn lash_core::StoreSet>) -> anyhow::Result<Backend> {
    lash::durable::DurableBackendBuilder::new(stores)
        .build()
        .map_err(|err| anyhow::anyhow!(err.to_string()))
}

/// A fresh SQLite memory store set: storage only, for the store-level
/// scenarios that execute no engine.
pub(crate) async fn sqlite_memory_stores() -> anyhow::Result<lash_sqlite_store::SqliteStoreSet> {
    lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))
}
