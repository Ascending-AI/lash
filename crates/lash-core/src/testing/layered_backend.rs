//! A test backend that decorates the ports of one backend.
//!
//! A runtime takes every store port and its effect host from one
//! [`Backend`](crate::Backend) (ADR 0104, B2). A test that records or faults
//! a port does not hand the runtime a second port beside the backend; it
//! layers a decorator over the backend's own port, and [`LayeredBackend`]
//! builds the backend whose engine and store set answer with the decorated
//! port and with the inner backend's for every other.

use std::sync::Arc;

use crate::engine::BuildGeneration;
use crate::{
    AttachmentStore, Backend, Clock, EffectEngine, EffectHost, ModuleArtifactStore,
    ProcessContinuationStore, ProcessDefinitionRegistry, ProcessExecutionEnvStore, ProcessRegistry,
    ProcessWorkWiring, SessionStoreFactory, StoreBindingId, StoreSet, TriggerStore,
};

/// One backend with some of its ports decorated. See the module
/// documentation.
#[derive(Clone)]
pub struct LayeredBackend {
    inner: Backend,
    clock: Arc<dyn Clock>,
    session_store_factory: Arc<dyn SessionStoreFactory>,
    effect_host: Arc<dyn EffectHost>,
    process_registry: Arc<dyn ProcessRegistry>,
    trigger_store: Arc<dyn TriggerStore>,
    process_definitions: Arc<dyn ProcessDefinitionRegistry>,
    process_env_store: Arc<dyn ProcessExecutionEnvStore>,
    attachment_store: Arc<dyn AttachmentStore>,
    module_artifacts: Arc<dyn ModuleArtifactStore>,
    process_work: Option<ProcessWorkWiring>,
    session_work: Option<Arc<dyn crate::SessionWorkEngine>>,
}

impl LayeredBackend {
    /// `inner`, undecorated.
    pub fn over(inner: Backend) -> Self {
        Self {
            clock: inner.clock(),
            session_store_factory: inner.session_store_factory(),
            effect_host: inner.effect_host(),
            process_registry: inner.process_registry(),
            trigger_store: inner.trigger_store(),
            process_definitions: inner.process_definition_registry(),
            process_env_store: inner.process_env_store(),
            attachment_store: inner.attachment_store(),
            module_artifacts: inner.module_artifacts(),
            process_work: inner.process_work(),
            session_work: inner.session_work(),
            inner,
        }
    }

    /// Stamp and sleep on `clock` in place of the inner backend's clock.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Replace the session-store factory with `layer` over it.
    pub fn map_session_store_factory(
        mut self,
        layer: impl FnOnce(Arc<dyn SessionStoreFactory>) -> Arc<dyn SessionStoreFactory>,
    ) -> Self {
        self.session_store_factory = layer(self.session_store_factory);
        self
    }

    /// Replace the effect host with `layer` over it.
    pub fn map_effect_host(
        mut self,
        layer: impl FnOnce(Arc<dyn EffectHost>) -> Arc<dyn EffectHost>,
    ) -> Self {
        self.effect_host = layer(self.effect_host);
        self
    }

    /// Replace the process registry with `layer` over it.
    ///
    /// Refuses when the engine runs its own processes: the backend then
    /// answers [`Backend::process_registry`] with the process-work wiring's
    /// registry rather than the store set's, and the layer would be dropped
    /// without a word. Decorate the store set before the engine is built, or
    /// build the wiring over the decorated registry with
    /// [`Self::wire_process_work`].
    pub fn map_process_registry(
        mut self,
        layer: impl FnOnce(Arc<dyn ProcessRegistry>) -> Arc<dyn ProcessRegistry>,
    ) -> Self {
        assert!(
            self.process_work.is_none(),
            "map_process_registry cannot decorate this backend's process \
             registry: its engine runs its own processes, so the backend \
             answers with the process-work wiring's registry and the layer \
             would be dropped — decorate the store set before the engine is \
             built, or use wire_process_work"
        );
        self.process_registry = layer(self.process_registry);
        self
    }

    /// Replace the trigger store with `layer` over it.
    pub fn map_trigger_store(
        mut self,
        layer: impl FnOnce(Arc<dyn TriggerStore>) -> Arc<dyn TriggerStore>,
    ) -> Self {
        self.trigger_store = layer(self.trigger_store);
        self
    }

    /// Replace the process-definition registry with `layer` over it.
    pub fn map_process_definition_registry(
        mut self,
        layer: impl FnOnce(Arc<dyn ProcessDefinitionRegistry>) -> Arc<dyn ProcessDefinitionRegistry>,
    ) -> Self {
        self.process_definitions = layer(self.process_definitions);
        self
    }

    /// Replace the process-execution-environment store with `layer` over it.
    pub fn map_process_env_store(
        mut self,
        layer: impl FnOnce(Arc<dyn ProcessExecutionEnvStore>) -> Arc<dyn ProcessExecutionEnvStore>,
    ) -> Self {
        self.process_env_store = layer(self.process_env_store);
        self
    }

    /// Replace the attachment store with `layer` over it.
    pub fn map_attachment_store(
        mut self,
        layer: impl FnOnce(Arc<dyn AttachmentStore>) -> Arc<dyn AttachmentStore>,
    ) -> Self {
        self.attachment_store = layer(self.attachment_store);
        self
    }

    /// Replace the module-artifact store with `layer` over it.
    pub fn map_module_artifacts(
        mut self,
        layer: impl FnOnce(Arc<dyn ModuleArtifactStore>) -> Arc<dyn ModuleArtifactStore>,
    ) -> Self {
        self.module_artifacts = layer(self.module_artifacts);
        self
    }

    /// Drive the backend's processes through `wire`, which receives the
    /// (possibly decorated) registry the wiring must be built over.
    pub fn wire_process_work(
        mut self,
        wire: impl FnOnce(Arc<dyn ProcessRegistry>) -> ProcessWorkWiring,
    ) -> Self {
        let wiring = wire(Arc::clone(&self.process_registry));
        self.process_registry = Arc::clone(wiring.registry());
        self.process_work = Some(wiring);
        self
    }

    /// Drive the backend's sessions on `session_work` (`None`: in process).
    pub fn with_session_work(
        mut self,
        session_work: Option<Arc<dyn crate::SessionWorkEngine>>,
    ) -> Self {
        self.session_work = session_work;
        self
    }

    /// The decorated backend, as the handle a host config takes.
    pub fn into_backend(self) -> Backend {
        let inner_stores = self.inner.stores();
        let stores = Arc::new(LayeredStoreSet {
            binding: inner_stores.binding_identity().clone(),
            inner: inner_stores,
            clock: self.clock,
            session_store_factory: self.session_store_factory,
            process_registry: self.process_registry,
            trigger_store: self.trigger_store,
            process_definitions: self.process_definitions,
            process_env_store: self.process_env_store,
            attachment_store: self.attachment_store,
            module_artifacts: self.module_artifacts,
        });
        Backend::new(Arc::new(LayeredEngine {
            stores,
            effect_host: self.effect_host,
            build_generation: self.inner.build_generation().clone(),
            process_work: self.process_work,
            session_work: self.session_work,
        }))
    }
}

struct LayeredEngine {
    stores: Arc<LayeredStoreSet>,
    effect_host: Arc<dyn EffectHost>,
    build_generation: BuildGeneration,
    process_work: Option<ProcessWorkWiring>,
    session_work: Option<Arc<dyn crate::SessionWorkEngine>>,
}

impl EffectEngine for LayeredEngine {
    fn stores(&self) -> Arc<dyn StoreSet> {
        Arc::clone(&self.stores) as Arc<dyn StoreSet>
    }

    fn effect_host(&self) -> Arc<dyn EffectHost> {
        Arc::clone(&self.effect_host)
    }

    fn build_generation(&self) -> &BuildGeneration {
        // The layered backend is the inner backend's substrate with decorated
        // ports: it reports the inner build's generation, not one of its own.
        &self.build_generation
    }

    fn process_work(&self) -> Option<ProcessWorkWiring> {
        self.process_work.clone()
    }

    fn session_work(&self) -> Option<Arc<dyn crate::SessionWorkEngine>> {
        self.session_work.clone()
    }
}

struct LayeredStoreSet {
    inner: Arc<dyn StoreSet>,
    binding: StoreBindingId,
    clock: Arc<dyn Clock>,
    session_store_factory: Arc<dyn SessionStoreFactory>,
    process_registry: Arc<dyn ProcessRegistry>,
    trigger_store: Arc<dyn TriggerStore>,
    process_definitions: Arc<dyn ProcessDefinitionRegistry>,
    process_env_store: Arc<dyn ProcessExecutionEnvStore>,
    attachment_store: Arc<dyn AttachmentStore>,
    module_artifacts: Arc<dyn ModuleArtifactStore>,
}

impl StoreSet for LayeredStoreSet {
    fn binding_identity(&self) -> &StoreBindingId {
        &self.binding
    }

    fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    fn session_store_factory(&self) -> Arc<dyn SessionStoreFactory> {
        Arc::clone(&self.session_store_factory)
    }

    fn process_registry(&self) -> Arc<dyn ProcessRegistry> {
        Arc::clone(&self.process_registry)
    }

    fn process_continuations(&self) -> Arc<dyn ProcessContinuationStore> {
        self.inner.process_continuations()
    }

    fn trigger_store(&self) -> Arc<dyn TriggerStore> {
        Arc::clone(&self.trigger_store)
    }

    fn process_definition_registry(&self) -> Arc<dyn ProcessDefinitionRegistry> {
        Arc::clone(&self.process_definitions)
    }

    fn process_env_store(&self) -> Arc<dyn ProcessExecutionEnvStore> {
        Arc::clone(&self.process_env_store)
    }

    fn attachment_store(&self) -> Arc<dyn AttachmentStore> {
        Arc::clone(&self.attachment_store)
    }

    fn module_artifacts(&self) -> Arc<dyn ModuleArtifactStore> {
        Arc::clone(&self.module_artifacts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backend whose engine runs its own processes answers
    /// [`Backend::process_registry`] with the process-work wiring's registry,
    /// not the store set's, so a layer over the store-set registry would be
    /// silently dropped. `map_process_registry` refuses that combination.
    #[tokio::test]
    #[should_panic(expected = "map_process_registry cannot decorate")]
    async fn map_process_registry_refuses_an_engine_with_its_own_process_work() {
        let engine_driven = LayeredBackend::over(crate::testing::memory_backend().await)
            .wire_process_work(crate::testing::process_work_wiring_for_registry)
            .into_backend();
        assert!(
            engine_driven.process_work().is_some(),
            "the fixture's engine runs its own processes"
        );
        let _ = LayeredBackend::over(engine_driven).map_process_registry(|registry| registry);
    }
}
