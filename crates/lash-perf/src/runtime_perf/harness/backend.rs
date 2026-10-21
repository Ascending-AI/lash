//! The backends the runtime perf harness measures over.
//!
//! Every benchmark core runs on one [`lash::Backend`]: lash's durable
//! engine, which I0 (FIG-5194) assembles and L3 (FIG-5172) makes serve. The
//! in-process lane runs it over a SQLite memory store set, the durable lane
//! over a SQLite store set on disk. A lane that measures persistence decorates
//! the session catalog and durable transaction port before building the backend,
//! so the served node uses both measurement ports.

use std::sync::Arc;

use lash_execution_contracts::backend::Backend;

/// The durable engine's backend over `stores`.
pub(crate) fn durable_backend(
    stores: Arc<dyn lash_execution_contracts::backend::StoreSet>,
) -> anyhow::Result<Backend> {
    lash::durable::DurableBackendBuilder::new(stores).config(lash::durable::DurableSettings { owner_retention: Some(lash::durable::OwnerRetentionSettings { idle_evict: std::time::Duration::from_secs(60),
 max_owner_bytes: const { std::num::NonZeroU64::new(64 * 1024 * 1024).unwrap() },
 max_node_bytes: const { std::num::NonZeroU64::new(512 * 1024 * 1024).unwrap() },
 max_owner_dirty_bytes: const { std::num::NonZeroU64::new(32 * 1024 * 1024).unwrap() },
 max_node_dirty_bytes: const { std::num::NonZeroU64::new(256 * 1024 * 1024).unwrap() }, }), ..lash::durable::DurableSettings::standard() })
        .build()
        .map_err(|err| anyhow::anyhow!(err.to_string()))
}

/// A fresh SQLite memory store set: storage only, for the store-level
/// scenarios that execute no engine.
pub(crate) async fn sqlite_memory_stores() -> anyhow::Result<lash_sqlite_store::SqliteStoreSet> {
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .map_err(|err| anyhow::anyhow!(err.to_string()))?;
    lash_execution_contracts::testing::process_execution_env_fixture(
        stores.process_env_store().as_ref(),
    )
    .await;
    Ok(stores)
}
