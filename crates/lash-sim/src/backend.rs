//! The backends the simulator drives its runtimes over.
//!
//! Every simulated core runs on one [`Backend`]. The in-process lane is a
//! SQLite memory backend on the simulator clock; the durable lanes open a
//! SQLite file or PostgreSQL backend. When the simulator records the
//! checkpoint writes a run commits, it wraps the backend's own session
//! factory in an observer, and every other port stays the backend's.

use std::sync::Arc;

use lash_core::{Backend, SessionStoreFactory};

use crate::clock::SimClock;
use crate::runner::FixedScriptRunnerError;
use crate::store::{CheckpointWriteCollector, ObservedSessionStoreFactory};

/// A fresh SQLite memory backend on the system clock.
pub async fn memory_backend()
-> Result<Arc<lash_sqlite_store::SqliteBackend>, FixedScriptRunnerError> {
    lash_sqlite_store::SqliteBackend::memory()
        .await
        .map(Arc::new)
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))
}

/// `base` with the effect host's leases held for the simulator's runtime
/// lease span: the simulator advances its clock across whole schedules, and
/// an effect lease is an operational liveness guard, not a generated
/// scenario event, exactly like the runtime leases [`crate::lease`] sets.
pub(crate) fn sim_sqlite_options(
    base: lash_sqlite_store::SqliteBackendOptions,
) -> lash_sqlite_store::SqliteBackendOptions {
    lash_sqlite_store::SqliteBackendOptions {
        effect_replay: lash_sqlite_store::SqliteEffectReplayOptions {
            lease_timings: crate::lease::sim_runtime_lease_timings(),
            ..base.effect_replay
        },
        ..base
    }
}

/// The PostgreSQL backend options of [`sim_sqlite_options`].
pub(crate) fn sim_postgres_options() -> lash_postgres_store::PostgresBackendOptions {
    let base = lash_postgres_store::PostgresBackendOptions::default();
    lash_postgres_store::PostgresBackendOptions {
        effect_replay: lash_postgres_store::PostgresEffectReplayOptions {
            lease_timings: crate::lease::sim_runtime_lease_timings(),
            ..base.effect_replay
        },
        ..base
    }
}

/// A fresh SQLite memory backend whose runtime and store stamps follow
/// `clock`.
pub(crate) async fn sim_memory_backend(
    clock: Arc<SimClock>,
) -> Result<Arc<lash_sqlite_store::SqliteBackend>, FixedScriptRunnerError> {
    lash_sqlite_store::SqliteBackend::memory_with_options_and_clock(
        sim_sqlite_options(lash_sqlite_store::SqliteBackendOptions::memory()),
        clock,
    )
    .await
    .map(Arc::new)
    .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))
}

/// `inner` with its session factory or its effect host decorated.
///
/// A checkpoint-write observer over the session factory forwards every
/// binding to the factory it wraps, and an effect layer over the host keeps
/// the host's binding identity, so the backend's ports still meet each
/// other exactly as the undecorated backend's do. Over a backend that keeps
/// Lashlang artifacts, the decorated backend keeps the same ones.
pub struct DecoratedBackend<B: ?Sized + Backend = dyn Backend> {
    inner: Arc<B>,
    factory: Arc<dyn SessionStoreFactory>,
    effect_host: Arc<dyn lash_core::EffectHost>,
}

impl<B: ?Sized + Backend> DecoratedBackend<B> {
    /// `inner`, undecorated.
    pub fn over(inner: Arc<B>) -> Self {
        Self {
            factory: inner.session_store_factory(),
            effect_host: inner.effect_host(),
            inner,
        }
    }

    /// Observe the commits made through the session factory into
    /// `collector`.
    pub fn observing(mut self, collector: CheckpointWriteCollector) -> Self {
        self.factory = Arc::new(ObservedSessionStoreFactory::new(self.factory, collector));
        self
    }

    /// Run every effect through `layer` before the host.
    pub fn with_effect_layer(mut self, layer: Arc<dyn lash_core::testing::EffectLayer>) -> Self {
        self.effect_host = Arc::new(lash_core::testing::LayeredEffectHost::new(
            self.effect_host,
            layer,
        ));
        self
    }
}

impl<B: ?Sized + Backend> Backend for DecoratedBackend<B> {
    fn binding_identity(&self) -> &str {
        self.inner.binding_identity()
    }

    fn clock(&self) -> Arc<dyn lash_core::Clock> {
        self.inner.clock()
    }

    fn session_store_factory(&self) -> Arc<dyn SessionStoreFactory> {
        Arc::clone(&self.factory)
    }

    fn effect_host(&self) -> Arc<dyn lash_core::EffectHost> {
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

impl<B: ?Sized + lash_lashlang_runtime::LashlangArtifactBackend>
    lash_lashlang_runtime::LashlangArtifactBackend for DecoratedBackend<B>
{
    fn lashlang_artifact_store(&self) -> Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> {
        self.inner.lashlang_artifact_store()
    }
}
