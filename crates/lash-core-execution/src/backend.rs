//! The one value a runtime takes its persistence ports and effect host from:
//! one effect engine over one store set (ADR 0104, B2).

use std::sync::Arc;

use crate::{
    AttachmentStore, Clock, EffectHost, ModuleArtifactStore, ProcessContinuationStore,
    ProcessDefinitionRegistry, ProcessExecutionEnvStore, ProcessRegistry, ProcessWorkWiring,
    QueuedWorkSubstrate, SessionStoreFactory, TriggerStore,
};

/// The identity of one store set: the storage it names, such as a SQLite
/// location or a PostgreSQL catalog. Stable for the life of that storage
/// and distinct between any two.
///
/// It names storage only. An engine's effect authority is the engine's own
/// and is never compared with it: both are fixed when the engine is built
/// over its store set (ADR 0104, section 2).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StoreBindingId(Arc<str>);

impl StoreBindingId {
    pub fn new(identity: impl Into<Arc<str>>) -> Self {
        Self(identity.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for StoreBindingId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One effect engine: the effect host that journals a runtime's effects, the
/// work drivers that run its processes and queued work, and the store set it
/// was built over (ADR 0104, section 2).
///
/// No engine operation takes a store set: the engine already holds the one
/// it was built over, and every port a [`Backend`] hands out derives from it.
pub trait EffectEngine: Send + Sync {
    /// The store set this engine was built over.
    fn stores(&self) -> Arc<dyn StoreSet>;

    /// The host that journals and replays this engine's effects.
    fn effect_host(&self) -> Arc<dyn EffectHost>;

    /// The engine that executes the store set's background processes, when
    /// the engine runs them itself (the Restate process workflow). `None`
    /// means the runtime's in-process worker drives the store set's
    /// registry: the SQLite engine, until FIG-3668 deletes it.
    ///
    /// Required, with no default: a wrapper that forgot to forward it would
    /// silently hand an engine-driven registry to the in-process worker too.
    fn process_work(&self) -> Option<ProcessWorkWiring>;

    /// The driver that runs the store set's queued session work.
    ///
    /// Required, with no default, for the same reason as
    /// [`Self::process_work`]: a forgotten forward would drive one
    /// store set's queued work twice.
    fn queued_work(&self) -> BackendQueuedWork;
}

/// The one value a runtime takes every port from: one effect engine, and
/// through it the store set it was built over.
///
/// It is the unit ADR 0102 and ADR 0104 rule on: no API assembles ports from
/// different substrates by hand. Every accessor hands out a handle on the
/// engine's or its store set's one instance of that port, so two calls reach
/// the same state. Cloning shares the engine.
#[derive(Clone)]
pub struct Backend {
    engine: Arc<dyn EffectEngine>,
}

impl Backend {
    /// The backend over `engine`.
    pub fn new(engine: Arc<dyn EffectEngine>) -> Self {
        Self { engine }
    }

    /// The engine every port of this backend derives from.
    pub fn engine(&self) -> &Arc<dyn EffectEngine> {
        &self.engine
    }

    /// The store set the engine was built over.
    pub fn stores(&self) -> Arc<dyn StoreSet> {
        self.engine.stores()
    }

    /// The identity of the storage this backend's sessions, processes and
    /// artifacts live in.
    pub fn binding_identity(&self) -> StoreBindingId {
        self.stores().binding_identity().clone()
    }

    /// The clock the store set stamps from and the effect host sleeps on.
    pub fn clock(&self) -> Arc<dyn Clock> {
        self.stores().clock()
    }

    /// The factory that creates and reopens this backend's session stores.
    pub fn session_store_factory(&self) -> Arc<dyn SessionStoreFactory> {
        self.stores().session_store_factory()
    }

    /// The host that journals and replays this backend's effects.
    pub fn effect_host(&self) -> Arc<dyn EffectHost> {
        self.engine.effect_host()
    }

    /// The durable registry of this backend's background processes: the one
    /// the engine's process work is wired over when the engine runs them, so
    /// the runtime and the engine see one registry.
    pub fn process_registry(&self) -> Arc<dyn ProcessRegistry> {
        match self.engine.process_work() {
            Some(wiring) => Arc::clone(wiring.registry()),
            None => self.stores().process_registry(),
        }
    }

    /// The durable trigger subscriptions and occurrences.
    pub fn trigger_store(&self) -> Arc<dyn TriggerStore> {
        self.stores().trigger_store()
    }

    /// The named process-definition registry.
    pub fn process_definition_registry(&self) -> Arc<dyn ProcessDefinitionRegistry> {
        self.stores().process_definition_registry()
    }

    /// The store of process execution environments.
    pub fn process_env_store(&self) -> Arc<dyn ProcessExecutionEnvStore> {
        self.stores().process_env_store()
    }

    /// The attachment byte store sessions write through.
    pub fn attachment_store(&self) -> Arc<dyn AttachmentStore> {
        self.stores().attachment_store()
    }

    /// The Lashlang module-artifact store, beside the sessions that write
    /// its artifacts: an RLM host reads its artifacts from the storage that
    /// reopens its sessions, and the artifact cleanup sweep reaches the store
    /// the sessions wrote.
    pub fn module_artifacts(&self) -> Arc<dyn ModuleArtifactStore> {
        self.stores().module_artifacts()
    }

    /// See [`EffectEngine::process_work`].
    pub fn process_work(&self) -> Option<ProcessWorkWiring> {
        self.engine.process_work()
    }

    /// See [`EffectEngine::queued_work`].
    pub fn queued_work(&self) -> BackendQueuedWork {
        self.engine.queued_work()
    }
}

impl<E: EffectEngine + 'static> From<Arc<E>> for Backend {
    fn from(engine: Arc<E>) -> Self {
        Self::new(engine)
    }
}

impl<E: EffectEngine + 'static> From<E> for Backend {
    fn from(engine: E) -> Self {
        Self::new(Arc::new(engine))
    }
}

impl std::fmt::Debug for Backend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Backend")
            .field("stores", &self.binding_identity())
            .finish_non_exhaustive()
    }
}

/// Which driver runs a backend's queued session work.
#[derive(Clone)]
pub enum BackendQueuedWork {
    /// The runtime's in-process driver claims and runs the work the
    /// store set holds: the SQLite engine.
    InProcess,
    /// The engine's own driver, such as a Restate workflow submitter.
    Engine(Arc<dyn QueuedWorkSubstrate>),
    /// No driver runs the queued work: the host drains it itself, such as
    /// from its own engine handlers.
    Disabled,
}

impl std::fmt::Debug for BackendQueuedWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InProcess => "InProcess",
            Self::Engine(_) => "Engine",
            Self::Disabled => "Disabled",
        })
    }
}

/// Every persistence port of one SQL substrate: the storage an effect
/// engine is built over (ADR 0104, section 1).
///
/// A store set opens no effect journal of its own, so nothing sweeps an idle
/// one. Every accessor hands out a handle on the store set's one instance of
/// that port.
pub trait StoreSet: Send + Sync {
    /// The identity of this store set's storage.
    fn binding_identity(&self) -> &StoreBindingId;

    /// The clock this store set stamps from.
    fn clock(&self) -> Arc<dyn Clock>;

    /// The factory that creates and reopens this store set's session stores.
    fn session_store_factory(&self) -> Arc<dyn SessionStoreFactory>;

    /// The durable registry of background processes.
    fn process_registry(&self) -> Arc<dyn ProcessRegistry>;

    /// The continuation records of [`Self::process_registry`]'s processes,
    /// which an engine that runs them resumes from.
    fn process_continuations(&self) -> Arc<dyn ProcessContinuationStore>;

    /// The durable trigger subscriptions and occurrences.
    fn trigger_store(&self) -> Arc<dyn TriggerStore>;

    /// The named process-definition registry.
    fn process_definition_registry(&self) -> Arc<dyn ProcessDefinitionRegistry>;

    /// The store of process execution environments.
    fn process_env_store(&self) -> Arc<dyn ProcessExecutionEnvStore>;

    /// The attachment byte store sessions write through.
    fn attachment_store(&self) -> Arc<dyn AttachmentStore>;

    /// The Lashlang module-artifact store.
    fn module_artifacts(&self) -> Arc<dyn ModuleArtifactStore>;
}
