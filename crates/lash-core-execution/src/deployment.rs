//! The one value a runtime takes its persistence ports and effect host from
//! (ADR 0102, D2).

use std::sync::Arc;

use crate::{
    AttachmentStore, Clock, EffectHost, ProcessDefinitionRegistry, ProcessExecutionEnvStore,
    ProcessRegistry, SessionStoreFactory, TriggerStore,
};

/// One substrate: every persistence port a runtime needs and the effect host
/// that journals its effects, all over one store set.
///
/// A deployment is the unit ADR 0102 rules on: it supplies the whole set, so
/// no API assembles ports from different substrates by hand. SQLite (a file
/// root or a named in-memory deployment), PostgreSQL and Restate each answer
/// it once. Every accessor hands out a handle on the deployment's one
/// instance of that port, so two calls reach the same state.
pub trait Deployment: Send + Sync {
    /// The identity every binding this deployment writes is keyed on: the
    /// turn-control binding of its effect host and the durable records that
    /// name it. Stable for the life of the substrate it names, and distinct
    /// between any two substrates.
    ///
    /// It is the single source of the effect host's binding: every
    /// implementation answers `effect_host().turn_control_binding_id() ==
    /// binding_identity()` (the conformance law `deployment_tests!` holds
    /// each one to it).
    fn binding_identity(&self) -> &str;

    /// The clock this deployment's stores and effect host stamp from and
    /// sleep on.
    fn clock(&self) -> Arc<dyn Clock>;

    /// The factory that creates and reopens this deployment's session stores.
    fn session_store_factory(&self) -> Arc<dyn SessionStoreFactory>;

    /// The host that journals and replays this deployment's effects.
    fn effect_host(&self) -> Arc<dyn EffectHost>;

    /// The durable registry of this deployment's background processes.
    fn process_registry(&self) -> Arc<dyn ProcessRegistry>;

    /// The durable trigger subscriptions and occurrences.
    fn trigger_store(&self) -> Arc<dyn TriggerStore>;

    /// The named process-definition registry.
    fn process_definition_registry(&self) -> Arc<dyn ProcessDefinitionRegistry>;

    /// The store of process execution environments.
    fn process_env_store(&self) -> Arc<dyn ProcessExecutionEnvStore>;

    /// The attachment byte store sessions write through.
    fn attachment_store(&self) -> Arc<dyn AttachmentStore>;
}
