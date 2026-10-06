//! A test backend that decorates the ports of one backend.
//!
//! A runtime takes every store port from one [`Backend`](crate::Backend)
//! (ADR 0104, B2). A test that records or faults a port does not hand the
//! runtime a second port beside the backend; it layers a decorator over the
//! backend's own port, and [`LayeredBackend`] builds the backend whose store
//! set answers with the decorated port and with the inner backend's for
//! every other. (Its engine half went with the effect engine in I0,
//! FIG-5194.)

use std::sync::Arc;

use crate::{
    AttachmentStore, Backend, Clock, DeploymentStore, ModuleArtifactStore,
    ProcessContinuationStore, ProcessExecutionEnvStore, ProcessRegistry, StoreBindingId, StoreSet,
    TriggerStore,
};

/// A decorator of one obligation kind's ledger.
type ObligationLedgerLayer = Arc<
    dyn Fn(
            crate::store::ObligationKind,
            Arc<dyn crate::store::ObligationLedger>,
        ) -> Arc<dyn crate::store::ObligationLedger>
        + Send
        + Sync,
>;

/// One backend with some of its ports decorated. See the module
/// documentation.
#[derive(Clone)]
pub struct LayeredBackend {
    inner: Backend,
    clock: Arc<dyn Clock>,
    session_store_factory: Arc<dyn DeploymentStore>,
    process_registry: Arc<dyn ProcessRegistry>,
    trigger_store: Arc<dyn TriggerStore>,
    process_env_store: Arc<dyn ProcessExecutionEnvStore>,
    attachment_store: Arc<dyn AttachmentStore>,
    module_artifacts: Arc<dyn ModuleArtifactStore>,
    obligation_ledgers: Option<ObligationLedgerLayer>,
    artifact_cleanup: Arc<dyn crate::store::ArtifactCleanupLedger>,
    worker_recovery: Arc<dyn crate::store::worker_recovery::WorkerRecoveryStore>,
}

impl LayeredBackend {
    /// `inner`, undecorated.
    pub fn over(inner: Backend) -> Self {
        Self {
            clock: inner.clock(),
            session_store_factory: inner.session_store_factory(),
            process_registry: inner.process_registry(),
            trigger_store: inner.trigger_store(),
            process_env_store: inner.process_env_store(),
            attachment_store: inner.attachment_store(),
            module_artifacts: inner.module_artifacts(),
            obligation_ledgers: None,
            artifact_cleanup: inner.artifact_cleanup(),
            worker_recovery: inner.worker_recovery(),
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
        layer: impl FnOnce(Arc<dyn DeploymentStore>) -> Arc<dyn DeploymentStore>,
    ) -> Self {
        self.session_store_factory = layer(self.session_store_factory);
        self
    }

    /// Replace the worker recovery store with a fixture's independent counters.
    pub fn map_worker_recovery(
        mut self,
        layer: impl FnOnce(
            Arc<dyn crate::store::worker_recovery::WorkerRecoveryStore>,
        ) -> Arc<dyn crate::store::worker_recovery::WorkerRecoveryStore>,
    ) -> Self {
        self.worker_recovery = layer(self.worker_recovery);
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

    /// Answer every obligation kind's ledger with `layer` over the inner
    /// store set's ledger of that kind. The layer runs on each ledger read,
    /// so a recorder it installs keeps its state outside the ledger.
    pub fn map_obligation_ledgers(
        mut self,
        layer: impl Fn(
            crate::store::ObligationKind,
            Arc<dyn crate::store::ObligationLedger>,
        ) -> Arc<dyn crate::store::ObligationLedger>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.obligation_ledgers = Some(Arc::new(layer));
        self
    }

    /// The decorated backend, as the handle a host config takes.
    pub fn into_backend(self) -> Backend {
        let inner_stores = self.inner.stores();
        let stores = Arc::new(LayeredStoreSet {
            binding: inner_stores.binding_identity().clone(),
            attachment_referrers: inner_stores.attachment_referrers(),
            generation_drain: inner_stores.generation_drain(),
            inner: inner_stores,
            clock: self.clock,
            session_store_factory: self.session_store_factory,
            process_registry: self.process_registry,
            trigger_store: self.trigger_store,
            process_env_store: self.process_env_store,
            attachment_store: self.attachment_store,
            module_artifacts: self.module_artifacts,
            obligation_ledgers: self.obligation_ledgers,
            artifact_cleanup: self.artifact_cleanup,
            worker_recovery: self.worker_recovery,
        });
        self.inner.over_stores(stores)
    }
}

/// One store set with some of its ports decorated, for a test that layers
/// the stores BEFORE an engine is built over them: an engine's own services
/// then run over the decorated ports too, where a [`LayeredBackend`] over a
/// built engine decorates only the ports read through the backend.
#[derive(Clone)]
pub struct LayeredStores(LayeredStoreSet);

impl LayeredStores {
    /// `inner`, undecorated.
    pub fn over(inner: Arc<dyn StoreSet>) -> Self {
        Self(LayeredStoreSet {
            binding: inner.binding_identity().clone(),
            attachment_referrers: inner.attachment_referrers(),
            clock: inner.clock(),
            session_store_factory: inner.session_store_factory(),
            process_registry: inner.process_registry(),
            trigger_store: inner.trigger_store(),
            process_env_store: inner.process_env_store(),
            attachment_store: inner.attachment_store(),
            module_artifacts: inner.module_artifacts(),
            obligation_ledgers: None,
            artifact_cleanup: inner.artifact_cleanup(),
            worker_recovery: inner.worker_recovery(),
            generation_drain: inner.generation_drain(),
            inner,
        })
    }

    /// Stamp and sleep on `clock` in place of the inner store set's clock.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.0.clock = clock;
        self
    }

    /// Replace the generation-drain store with `layer` over it.
    pub fn map_generation_drain(
        mut self,
        layer: impl FnOnce(
            Arc<dyn crate::store::generation_drain::GenerationDrainStore>,
        ) -> Arc<dyn crate::store::generation_drain::GenerationDrainStore>,
    ) -> Self {
        self.0.generation_drain = layer(self.0.generation_drain);
        self
    }

    /// Replace the session-store factory with `layer` over it.
    pub fn map_session_store_factory(
        mut self,
        layer: impl FnOnce(Arc<dyn DeploymentStore>) -> Arc<dyn DeploymentStore>,
    ) -> Self {
        self.0.session_store_factory = layer(self.0.session_store_factory);
        self
    }

    /// Replace the process registry with `layer` over it.
    pub fn map_process_registry(
        mut self,
        layer: impl FnOnce(Arc<dyn ProcessRegistry>) -> Arc<dyn ProcessRegistry>,
    ) -> Self {
        self.0.process_registry = layer(self.0.process_registry);
        self
    }

    /// Replace the trigger store with `layer` over it.
    pub fn map_trigger_store(
        mut self,
        layer: impl FnOnce(Arc<dyn TriggerStore>) -> Arc<dyn TriggerStore>,
    ) -> Self {
        self.0.trigger_store = layer(self.0.trigger_store);
        self
    }

    /// Replace the process-execution-environment store with `layer` over it.
    pub fn map_process_env_store(
        mut self,
        layer: impl FnOnce(Arc<dyn ProcessExecutionEnvStore>) -> Arc<dyn ProcessExecutionEnvStore>,
    ) -> Self {
        self.0.process_env_store = layer(self.0.process_env_store);
        self
    }

    /// Replace the attachment store with `layer` over it.
    pub fn map_attachment_store(
        mut self,
        layer: impl FnOnce(Arc<dyn AttachmentStore>) -> Arc<dyn AttachmentStore>,
    ) -> Self {
        self.0.attachment_store = layer(self.0.attachment_store);
        self
    }

    /// Replace the attachment referrers before the engine binds its services.
    pub fn map_attachment_referrers(
        mut self,
        layer: impl FnOnce(Arc<dyn crate::AttachmentReferrers>) -> Arc<dyn crate::AttachmentReferrers>,
    ) -> Self {
        self.0.attachment_referrers = layer(self.0.attachment_referrers);
        self
    }

    /// Replace the module-artifact store with `layer` over it.
    pub fn map_module_artifacts(
        mut self,
        layer: impl FnOnce(Arc<dyn ModuleArtifactStore>) -> Arc<dyn ModuleArtifactStore>,
    ) -> Self {
        self.0.module_artifacts = layer(self.0.module_artifacts);
        self
    }

    /// Answer every obligation kind's ledger with `layer` over the inner
    /// store set's ledger of that kind. The layer runs on each ledger read,
    /// so a recorder it installs keeps its state outside the ledger.
    pub fn map_obligation_ledgers(
        mut self,
        layer: impl Fn(
            crate::store::ObligationKind,
            Arc<dyn crate::store::ObligationLedger>,
        ) -> Arc<dyn crate::store::ObligationLedger>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.0.obligation_ledgers = Some(Arc::new(layer));
        self
    }

    /// The decorated store set.
    pub fn into_store_set(self) -> Arc<dyn StoreSet> {
        Arc::new(self.0)
    }
}

#[derive(Clone)]
struct LayeredStoreSet {
    inner: Arc<dyn StoreSet>,
    binding: StoreBindingId,
    clock: Arc<dyn Clock>,
    session_store_factory: Arc<dyn DeploymentStore>,
    process_registry: Arc<dyn ProcessRegistry>,
    trigger_store: Arc<dyn TriggerStore>,
    process_env_store: Arc<dyn ProcessExecutionEnvStore>,
    attachment_store: Arc<dyn AttachmentStore>,
    attachment_referrers: Arc<dyn crate::AttachmentReferrers>,
    module_artifacts: Arc<dyn ModuleArtifactStore>,
    obligation_ledgers: Option<ObligationLedgerLayer>,
    artifact_cleanup: Arc<dyn crate::store::ArtifactCleanupLedger>,
    worker_recovery: Arc<dyn crate::store::worker_recovery::WorkerRecoveryStore>,
    generation_drain: Arc<dyn crate::store::generation_drain::GenerationDrainStore>,
}

impl StoreSet for LayeredStoreSet {
    fn durable_store(&self) -> Arc<dyn lash_durable::DurableStore> {
        self.inner.durable_store()
    }

    fn binding_identity(&self) -> &StoreBindingId {
        &self.binding
    }

    fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.clock)
    }

    fn session_store_factory(&self) -> Arc<dyn DeploymentStore> {
        Arc::clone(&self.session_store_factory)
    }
    fn attachment_referrers(&self) -> Arc<dyn lash_core_execution::AttachmentReferrers> {
        Arc::clone(&self.attachment_referrers)
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

    fn tool_material_store(&self) -> Arc<dyn crate::store::ToolMaterialStore> {
        self.inner.tool_material_store()
    }

    fn process_env_store(&self) -> Arc<dyn ProcessExecutionEnvStore> {
        Arc::clone(&self.process_env_store)
    }

    fn turn_prelude_store(&self) -> Arc<dyn crate::TurnPreludeStore> {
        self.inner.turn_prelude_store()
    }

    fn worker_recovery(
        &self,
    ) -> Arc<dyn lash_core_execution::store::worker_recovery::WorkerRecoveryStore> {
        Arc::clone(&self.worker_recovery)
    }

    fn attachment_store(&self) -> Arc<dyn AttachmentStore> {
        Arc::clone(&self.attachment_store)
    }

    fn module_artifacts(&self) -> Arc<dyn ModuleArtifactStore> {
        Arc::clone(&self.module_artifacts)
    }

    fn definition_store(&self) -> Arc<dyn crate::ProcessDefinitionStore> {
        self.inner.definition_store()
    }

    fn recovery_leader(&self) -> Arc<dyn crate::store::RecoveryLeaderStore> {
        self.inner.recovery_leader()
    }

    fn generation_drain(&self) -> Arc<dyn crate::store::generation_drain::GenerationDrainStore> {
        Arc::clone(&self.generation_drain)
    }

    fn obligation_ledger(
        &self,
        kind: crate::store::ObligationKind,
    ) -> Arc<dyn crate::store::ObligationLedger> {
        let ledger = self.inner.obligation_ledger(kind);
        match &self.obligation_ledgers {
            Some(layer) => layer(kind, ledger),
            None => ledger,
        }
    }

    fn session_delete_ledger(&self) -> Arc<dyn crate::store::session_delete::SessionDeleteLedger> {
        self.inner.session_delete_ledger()
    }

    fn artifact_cleanup(&self) -> Arc<dyn crate::store::ArtifactCleanupLedger> {
        Arc::clone(&self.artifact_cleanup)
    }
}
