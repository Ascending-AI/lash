//! The backends the runtime perf harness measures over.
//!
//! Every benchmark core runs on one [`lash::Backend`]. The in-process lane
//! is a SQLite memory backend; the durable lanes open a SQLite file or a
//! PostgreSQL backend. A lane that measures persistence puts the perf store
//! decorator in front of the backend's session catalog, and the start-gate
//! scenario layers its retry fixture over the backend's host; every other
//! port stays the backend's.

use std::sync::Arc;

use lash_core::{Backend, EffectHost, SessionStoreFactory};

/// `inner` with its session catalog or its effect host decorated.
pub(super) struct PerfBackend {
    inner: Arc<dyn Backend>,
    catalog: Arc<dyn SessionStoreFactory>,
    effect_host: Arc<dyn EffectHost>,
}

impl PerfBackend {
    /// `inner`, undecorated.
    pub(super) fn over(inner: Arc<dyn Backend>) -> Self {
        Self {
            catalog: inner.session_store_factory(),
            effect_host: inner.effect_host(),
            inner,
        }
    }

    /// Serve sessions from `catalog`, the perf store decorator.
    pub(super) fn with_catalog(mut self, catalog: Arc<dyn SessionStoreFactory>) -> Self {
        self.catalog = catalog;
        self
    }

    /// Run every effect through `layer` before the backend's host.
    pub(super) fn with_effect_layer(
        mut self,
        layer: Arc<dyn lash_core::testing::EffectLayer>,
    ) -> Self {
        self.effect_host = Arc::new(lash_core::testing::LayeredEffectHost::new(
            self.effect_host,
            layer,
        ));
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

    fn process_work(&self) -> Option<lash_core::ProcessWorkWiring> {
        self.inner.process_work()
    }

    fn queued_work(&self) -> lash_core::BackendQueuedWork {
        self.inner.queued_work()
    }
}

/// A fresh SQLite memory backend for the in-process lane.
pub(super) async fn memory_backend() -> anyhow::Result<Arc<lash_sqlite_store::SqliteBackend>> {
    Ok(Arc::new(
        lash_sqlite_store::SqliteBackend::memory()
            .await
            .map_err(|err| anyhow::anyhow!(err.to_string()))?,
    ))
}
