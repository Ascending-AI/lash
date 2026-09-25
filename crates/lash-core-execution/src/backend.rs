//! The one value a runtime takes its persistence ports and effect host from
//! (ADR 0102, D2).

use std::sync::Arc;

use crate::{
    AttachmentStore, Clock, EffectHost, ModuleArtifactStore, ProcessContinuationStore,
    ProcessDefinitionRegistry, ProcessExecutionEnvStore, ProcessRegistry, ProcessWorkWiring,
    QueuedWorkSubstrate, SessionStoreFactory, TriggerStore,
};

/// One substrate: every persistence port a runtime needs and the effect host
/// that journals its effects, all over one store set.
///
/// A backend is the unit ADR 0102 rules on: it supplies the whole set, so
/// no API assembles ports from different substrates by hand. SQLite (a file
/// root or a named in-memory backend), PostgreSQL and Restate each answer
/// it once. Every accessor hands out a handle on the backend's one
/// instance of that port, so two calls reach the same state.
pub trait Backend: Send + Sync {
    /// The identity every binding this backend writes is keyed on: the
    /// turn-control binding of its effect host and the durable records that
    /// name it. Stable for the life of the substrate it names, and distinct
    /// between any two substrates.
    ///
    /// It is the single source of the effect host's binding: every
    /// implementation answers `effect_host().turn_control_binding_id() ==
    /// binding_identity()` (the conformance law `backend_tests!` holds
    /// each one to it).
    fn binding_identity(&self) -> &str;

    /// The clock this backend's stores and effect host stamp from and
    /// sleep on.
    fn clock(&self) -> Arc<dyn Clock>;

    /// The factory that creates and reopens this backend's session stores.
    fn session_store_factory(&self) -> Arc<dyn SessionStoreFactory>;

    /// The host that journals and replays this backend's effects.
    fn effect_host(&self) -> Arc<dyn EffectHost>;

    /// The durable registry of this backend's background processes.
    fn process_registry(&self) -> Arc<dyn ProcessRegistry>;

    /// The durable trigger subscriptions and occurrences.
    fn trigger_store(&self) -> Arc<dyn TriggerStore>;

    /// The named process-definition registry.
    fn process_definition_registry(&self) -> Arc<dyn ProcessDefinitionRegistry>;

    /// The store of process execution environments.
    fn process_env_store(&self) -> Arc<dyn ProcessExecutionEnvStore>;

    /// The attachment byte store sessions write through.
    fn attachment_store(&self) -> Arc<dyn AttachmentStore>;

    /// The Lashlang module-artifact store, beside the sessions that write
    /// its artifacts: an RLM host reads its artifacts from the substrate
    /// that reopens its sessions, and the artifact cleanup sweep reaches the
    /// store the sessions wrote.
    fn module_artifacts(&self) -> Arc<dyn ModuleArtifactStore>;

    /// The engine that executes this backend's background processes,
    /// wired over [`Self::process_registry`], when the substrate supplies one
    /// of its own (the Restate process workflow). `None` means the runtime's
    /// in-process worker drives the registry.
    ///
    /// Process work is part of the backend rather than a builder input
    /// because the wiring carries a registry: a runtime that took it
    /// separately could run one substrate's processes over another's.
    ///
    /// Required, with no default: a wrapper that forgot to forward it would
    /// silently hand an engine-driven registry to the in-process worker too.
    fn process_work(&self) -> Option<ProcessWorkWiring>;

    /// The driver that runs this backend's queued session work.
    ///
    /// The work driver belongs to the backend for the same reason process work
    /// does: the in-process driver on SQLite and PostgreSQL and the engine's on
    /// Restate are each part of one substrate, never a separate builder input.
    ///
    /// Required, with no default, for the same reason as
    /// [`Self::process_work`]: a forgotten forward would drive one
    /// substrate's queued work twice.
    fn queued_work(&self) -> BackendQueuedWork;
}

/// Which driver runs a backend's queued session work.
#[derive(Clone)]
pub enum BackendQueuedWork {
    /// The runtime's in-process driver claims and runs the work the
    /// backend's stores hold: SQLite and PostgreSQL.
    InProcess,
    /// The substrate's own engine-backed driver, such as a Restate workflow
    /// submitter.
    Engine(Arc<dyn QueuedWorkSubstrate>),
    /// No driver runs the backend's queued work: the host drains it itself,
    /// such as from its own engine handlers.
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

/// Every persistence port of one SQL substrate without an effect host: the
/// store set an engine-backed backend journals its effects beside (ADR
/// 0102, D2).
///
/// A Restate backend is the Restate engine host over one store set;
/// Restate journals the effects and stores no sessions, so the pairing is one
/// backend, not a mixture. A store set opens no effect journal of its own,
/// so nothing sweeps an idle one. Like [`Backend`], every accessor hands
/// out a handle on the store set's one instance of that port.
pub trait StoreSet: Send + Sync {
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
