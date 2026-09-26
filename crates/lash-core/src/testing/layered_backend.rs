//! A test backend that decorates the ports of one backend.
//!
//! A runtime takes every store port and its effect host from one
//! [`Backend`](crate::Backend) (ADR 0102, D2). A test that records or faults a
//! port does not hand the runtime a second port beside the backend; it layers
//! a decorator over the backend's own port, and [`LayeredBackend`] is the
//! backend that answers with the decorated port and with the inner backend's
//! for every other.

use std::sync::Arc;

use crate::{
    AttachmentStore, Backend, Clock, EffectHost, ModuleArtifactStore, ProcessDefinitionRegistry,
    ProcessExecutionEnvStore, ProcessRegistry, ProcessWorkWiring, SessionStoreFactory,
    TriggerStore,
};

/// One backend with some of its ports decorated. See the module
/// documentation.
pub struct LayeredBackend {
    inner: Arc<dyn Backend>,
    session_store_factory: Arc<dyn SessionStoreFactory>,
    effect_host: Arc<dyn EffectHost>,
    process_registry: Arc<dyn ProcessRegistry>,
    trigger_store: Arc<dyn TriggerStore>,
    process_definitions: Arc<dyn ProcessDefinitionRegistry>,
    process_work: Option<ProcessWorkWiring>,
}

impl LayeredBackend {
    /// `inner`, undecorated.
    pub fn over(inner: Arc<dyn Backend>) -> Self {
        Self {
            session_store_factory: inner.session_store_factory(),
            effect_host: inner.effect_host(),
            process_registry: inner.process_registry(),
            trigger_store: inner.trigger_store(),
            process_definitions: inner.process_definition_registry(),
            process_work: inner.process_work(),
            inner,
        }
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
    pub fn map_process_registry(
        mut self,
        layer: impl FnOnce(Arc<dyn ProcessRegistry>) -> Arc<dyn ProcessRegistry>,
    ) -> Self {
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

    /// Replace the process-work port with `layer` over it, keeping its
    /// watched registry. A backend without process work stays without.
    pub fn map_process_work_port(
        mut self,
        layer: impl FnOnce(Arc<dyn crate::ProcessWorkSubstrate>) -> Arc<dyn crate::ProcessWorkSubstrate>,
    ) -> Self {
        self.process_work = self.process_work.map(|wiring| {
            ProcessWorkWiring::new(wiring.watched().clone(), layer(Arc::clone(wiring.port())))
        });
        self
    }

    /// The decorated backend, as the handle a host config takes.
    pub fn into_backend(self) -> Arc<dyn Backend> {
        Arc::new(self)
    }
}

impl Backend for LayeredBackend {
    fn binding_identity(&self) -> &str {
        self.inner.binding_identity()
    }

    fn clock(&self) -> Arc<dyn Clock> {
        self.inner.clock()
    }

    fn session_store_factory(&self) -> Arc<dyn SessionStoreFactory> {
        Arc::clone(&self.session_store_factory)
    }

    fn effect_host(&self) -> Arc<dyn EffectHost> {
        Arc::clone(&self.effect_host)
    }

    fn process_registry(&self) -> Arc<dyn ProcessRegistry> {
        Arc::clone(&self.process_registry)
    }

    fn trigger_store(&self) -> Arc<dyn TriggerStore> {
        Arc::clone(&self.trigger_store)
    }

    fn process_definition_registry(&self) -> Arc<dyn ProcessDefinitionRegistry> {
        Arc::clone(&self.process_definitions)
    }

    fn process_env_store(&self) -> Arc<dyn ProcessExecutionEnvStore> {
        self.inner.process_env_store()
    }

    fn attachment_store(&self) -> Arc<dyn AttachmentStore> {
        self.inner.attachment_store()
    }

    fn module_artifacts(&self) -> Arc<dyn ModuleArtifactStore> {
        self.inner.module_artifacts()
    }

    fn process_work(&self) -> Option<ProcessWorkWiring> {
        self.process_work.clone()
    }

    fn session_work(&self) -> Option<Arc<dyn crate::SessionWorkEngine>> {
        self.inner.session_work()
    }
}
