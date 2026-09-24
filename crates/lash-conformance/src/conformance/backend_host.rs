//! Runtime hosts assembled from one backend (ADR 0102, D2).
//!
//! A law that runs a real runtime takes every port from the backend under
//! test: its effect host (or a testing layer over that host), its attachment
//! and process-exec-env stores, its session-store factory, trigger store and
//! process-definition registry, all on its clock. No law assembles a runtime
//! from ports of different substrates.

use std::sync::Arc;

/// The commit budget every conformance runtime runs under.
pub(crate) fn conformance_commit_budget() -> crate::CommitBudget {
    crate::CommitBudget::bounded(1024 * 1024, 512)
}

/// A runtime host config over every port of `backend`, with its own effect
/// host.
pub(crate) fn backend_host_config(
    backend: &dyn crate::Backend,
    batching: crate::QueuedWorkBatchingConfig,
) -> crate::RuntimeHostConfig {
    host_config_over(backend, backend.effect_host(), batching)
}

/// A runtime host config over `backend`'s ports, with `effect_host` in place
/// of the backend's own: a testing layer over that host
/// ([`crate::testing::LayeredEffectHost`]) or the backend's host itself.
pub(crate) fn host_config_over(
    backend: &dyn crate::Backend,
    effect_host: Arc<dyn crate::EffectHost>,
    batching: crate::QueuedWorkBatchingConfig,
) -> crate::RuntimeHostConfig {
    crate::RuntimeHostConfig::new(
        effect_host,
        backend.attachment_store(),
        backend.process_env_store(),
        conformance_commit_budget(),
        batching,
    )
    .with_clock(backend.clock())
}

/// A runtime host config over `stores`' attachment and process-exec-env
/// ports, with `effect_host` journaling beside them: the backend an engine
/// host and one store set make together.
pub(crate) fn store_set_host_config(
    stores: &dyn crate::StoreSet,
    effect_host: Arc<dyn crate::EffectHost>,
    batching: crate::QueuedWorkBatchingConfig,
) -> crate::RuntimeHostConfig {
    crate::RuntimeHostConfig::new(
        effect_host,
        stores.attachment_store(),
        stores.process_env_store(),
        conformance_commit_budget(),
        batching,
    )
    .with_clock(stores.clock())
}

/// An embedded runtime host over `core` whose session, trigger and
/// process-definition stores are `backend`'s.
pub(crate) fn backend_embedded_host(
    backend: &dyn crate::Backend,
    core: crate::RuntimeHostConfig,
) -> crate::EmbeddedRuntimeHost {
    crate::EmbeddedRuntimeHost::new(core)
        .with_session_store_factory(backend.session_store_factory())
        .with_trigger_store(backend.trigger_store())
        .with_process_definition_registry(backend.process_definition_registry())
}

/// The runtime host for a store law: a law over one session store that builds
/// a runtime to reach its facade (append, park, rematerialize) but runs no
/// effect, writes no attachment and publishes no execution environment.
///
/// The law's substrate is the store it was handed, so the runtime is given no
/// second one: its effect host is the recording double, whose controllers
/// journal nothing a store answers from, and its attachment and
/// process-exec-env ports refuse every write.
pub(crate) fn store_law_runtime_host() -> crate::EmbeddedRuntimeHost {
    crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::new(
        Arc::new(crate::RecordingEffectHost::default()),
        Arc::new(crate::attachments::UnavailableAttachmentStore),
        Arc::new(crate::testing::UnavailableProcessExecutionEnvStore),
        conformance_commit_budget(),
        crate::QueuedWorkBatchingConfig::new(1),
    ))
}
