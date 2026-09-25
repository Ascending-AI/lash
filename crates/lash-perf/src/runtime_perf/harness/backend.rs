//! The backends the runtime perf harness measures over.
//!
//! Every benchmark core runs on one [`lash::Backend`]. The in-process lane
//! is lash-restate's engine on the in-process Restate server double over a
//! SQLite memory store set; the durable lanes open a SQLite file or a
//! PostgreSQL backend. A lane that measures persistence puts the perf store
//! decorator in front of the backend's session catalog; every other port
//! stays the backend's.

use std::sync::Arc;

use lash_core::{Backend, EffectHost, SessionStoreFactory};

/// The seed of every in-process lane's server double. A perf run measures
/// cost, not a schedule, so one fixed seed serves every scenario.
const RESTATE_SEED: u64 = 0x5eed_9e4f;

/// `inner` with its session catalog decorated. It keeps
/// the inner backend's Lashlang artifacts, so an RLM factory built over it
/// keeps them there too.
pub(super) struct PerfBackend {
    inner: Arc<dyn Backend>,
    catalog: Arc<dyn SessionStoreFactory>,
    effect_host: Arc<dyn EffectHost>,
}

impl PerfBackend {
    /// `inner`, undecorated.
    pub(super) fn over(inner: Arc<dyn lash_core::Backend>) -> Self {
        Self {
            catalog: inner.session_store_factory(),
            effect_host: inner.effect_host(),
            inner,
        }
    }

    /// The Restate test backend, undecorated. Its Lashlang artifacts live in
    /// its store set, as a deployment's do.
    pub(super) fn over_restate(backend: &lash_restate_test::RestateTestBackend) -> Self {
        Self::over(backend.lash_backend())
    }

    /// Serve sessions from `catalog`, the perf store decorator.
    pub(super) fn with_catalog(mut self, catalog: Arc<dyn SessionStoreFactory>) -> Self {
        self.catalog = catalog;
        self
    }
}

impl Backend for PerfBackend {
    fn binding_identity(&self) -> &str {
        self.inner.binding_identity()
    }

    fn clock(&self) -> Arc<dyn lash_core::Clock> {
        self.inner.clock()
    }

    fn session_store_factory(&self) -> Arc<dyn SessionStoreFactory> {
        Arc::clone(&self.catalog)
    }

    fn effect_host(&self) -> Arc<dyn EffectHost> {
        Arc::clone(&self.effect_host)
    }

    fn process_registry(&self) -> Arc<dyn lash_core::ProcessRegistry> {
        self.inner.process_registry()
    }

    fn trigger_store(&self) -> Arc<dyn lash_core::TriggerStore> {
        self.inner.trigger_store()
    }

    fn process_definition_registry(&self) -> Arc<dyn lash_core::ProcessDefinitionRegistry> {
        self.inner.process_definition_registry()
    }

    fn process_env_store(&self) -> Arc<dyn lash_core::ProcessExecutionEnvStore> {
        self.inner.process_env_store()
    }

    fn attachment_store(&self) -> Arc<dyn lash_core::AttachmentStore> {
        self.inner.attachment_store()
    }

    fn module_artifacts(&self) -> Arc<dyn lash_core::ModuleArtifactStore> {
        self.inner.module_artifacts()
    }

    fn process_work(&self) -> Option<lash_core::ProcessWorkWiring> {
        self.inner.process_work()
    }

    fn session_work(&self) -> Option<std::sync::Arc<dyn lash_core::runtime::SessionWorkEngine>> {
        self.inner.session_work()
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
