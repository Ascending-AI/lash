//! [`RestateBackend`]: the Restate engine host over one SQL store set
//! (ADR 0102, D2).
//!
//! Restate journals the effects and runs the background processes; the store
//! set keeps the sessions, the process registry and every other persistence
//! port. That pairing is one backend, not a mixture: the engine stores no
//! sessions, and the store set opens no effect journal of its own.

use std::sync::Arc;

use lash_core::facade_support::{ProcessEventSink, TurnWorkDriver};
use lash_core::{BackendQueuedWork, EffectHost as _, QueuedWorkSubstrate, StoreSet};

use crate::effect_host::RestateEffectHost;
use crate::ingress::{RestateAuthorityId, RestateConnection};
use crate::process::RestateProcessDeployment;
use crate::turn::RestateTurnAttach;

/// Who runs a [`RestateBackend`]'s queued session work: a required,
/// explicit choice with no default.
///
/// There is no in-process choice. The runtime's in-process driver would claim
/// and run queued turns outside any Restate handler, racing the handlers that
/// own them, and cannot legally execute their effects.
#[derive(Clone)]
pub enum RestateQueuedWork {
    /// The engine-backed driver: the port that hands each queued turn to the
    /// host's Restate workflow, so the turn runs inside a handler.
    Engine(Arc<dyn QueuedWorkSubstrate>),
    /// No driver: the host drains queued work from its own handlers, or runs
    /// no queued work at all.
    Disabled,
}

impl std::fmt::Debug for RestateQueuedWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Engine(_) => "Engine",
            Self::Disabled => "Disabled",
        })
    }
}

impl From<RestateQueuedWork> for BackendQueuedWork {
    fn from(queued_work: RestateQueuedWork) -> Self {
        match queued_work {
            RestateQueuedWork::Engine(driver) => Self::Engine(driver),
            RestateQueuedWork::Disabled => Self::Disabled,
        }
    }
}

/// The Restate engine host and Restate process work over one SQL
/// [`StoreSet`] (SQLite or PostgreSQL).
///
/// Its binding identity is the Restate authority's, the one the effect host's
/// turn-control binding and await-event keys derive from.
///
/// `S` is the store set's type. A backend over a store set that also keeps
/// Lashlang module artifacts (feature `lashlang`) is a
/// `LashlangArtifactBackend` and hands out that store set's artifact store, so
/// an RLM host reads its artifacts from the substrate it journals beside.
pub struct RestateBackend<S: ?Sized + StoreSet = dyn StoreSet> {
    stores: Arc<S>,
    effect_host: Arc<RestateEffectHost>,
    process: Arc<RestateProcessDeployment>,
    queued_work: BackendQueuedWork,
    identity: Arc<str>,
}

impl<S: ?Sized + StoreSet> Clone for RestateBackend<S> {
    fn clone(&self) -> Self {
        Self {
            stores: Arc::clone(&self.stores),
            effect_host: Arc::clone(&self.effect_host),
            process: Arc::clone(&self.process),
            queued_work: self.queued_work.clone(),
            identity: Arc::clone(&self.identity),
        }
    }
}

impl RestateBackend {
    /// Every service name this backend's wiring addresses on the endpoint:
    /// the durable-wait pair that carries turn terminal promises, await-event
    /// waits and cancellation gates, and the process workflow and attach
    /// services. Assert the set at wiring time with
    /// [`crate::assert_services_bound`] or
    /// [`assert_endpoint_bound`](Self::assert_endpoint_bound).
    pub fn required_service_names() -> Vec<&'static str> {
        RestateProcessDeployment::required_service_names()
    }
}

impl<S: ?Sized + StoreSet> RestateBackend<S> {
    /// The backend reaching Restate at `connection` under `authority_id`,
    /// over `stores`, with `queued_work` running its queued session work.
    pub fn new(
        connection: impl Into<RestateConnection>,
        authority_id: RestateAuthorityId,
        stores: Arc<S>,
        queued_work: RestateQueuedWork,
    ) -> Self {
        Self::with_process_event_sink(connection, authority_id, stores, queued_work, None)
    }

    /// Like [`new`](Self::new), but installs a host-facing
    /// [`ProcessEventSink`] on the process registry decorator the Restate
    /// process work wraps. Each appended event is pushed best-effort after
    /// its durable write.
    pub fn with_process_event_sink(
        connection: impl Into<RestateConnection>,
        authority_id: RestateAuthorityId,
        stores: Arc<S>,
        queued_work: RestateQueuedWork,
        sink: Option<Arc<dyn ProcessEventSink>>,
    ) -> Self {
        let connection = connection.into();
        let effect_host = Arc::new(RestateEffectHost::new(
            connection.clone(),
            authority_id.clone(),
        ));
        let process = Arc::new(RestateProcessDeployment::new_with_sink(
            connection,
            authority_id,
            stores.process_registry(),
            stores.process_continuations(),
            sink,
        ));
        let identity = Arc::from(effect_host.turn_control_binding_id());
        Self {
            stores,
            effect_host,
            process,
            queued_work: queued_work.into(),
            identity,
        }
    }

    /// Fail at wiring time when `endpoint` does not bind every service in
    /// [`required_service_names`](Self::required_service_names).
    pub async fn assert_endpoint_bound(
        &self,
        endpoint: &restate_sdk::endpoint::Endpoint,
    ) -> Result<(), crate::RestateBindingCheckError> {
        crate::assert_services_bound(
            endpoint,
            RestateBackend::required_service_names().as_slice(),
        )
        .await
    }

    /// The Restate effect host every runtime of this backend runs on.
    pub fn effect_host(&self) -> Arc<RestateEffectHost> {
        Arc::clone(&self.effect_host)
    }

    /// The Restate process work over this backend's registry: the
    /// workflow an endpoint binds and the port a host awaits work items
    /// through.
    pub fn process_deployment(&self) -> &RestateProcessDeployment {
        &self.process
    }

    /// The store set this backend journals its effects beside.
    pub fn stores(&self) -> &Arc<S> {
        &self.stores
    }

    /// Exact-turn control over this backend's sessions, usable from a
    /// process outside the turn's handler.
    pub fn turn_work_driver(&self) -> TurnWorkDriver {
        TurnWorkDriver::for_catalog(
            self.effect_host.clone(),
            self.stores.session_store_factory(),
        )
    }

    /// Attachment to a turn's reserved terminal promise.
    pub fn turn_attach(&self) -> Arc<RestateTurnAttach> {
        self.effect_host.turn_attach_handle()
    }
}

impl<S: ?Sized + StoreSet> lash_core::Backend for RestateBackend<S> {
    fn binding_identity(&self) -> &str {
        &self.identity
    }

    fn clock(&self) -> Arc<dyn lash_core::Clock> {
        self.stores.clock()
    }

    fn session_store_factory(&self) -> Arc<dyn lash_core::SessionStoreFactory> {
        self.stores.session_store_factory()
    }

    fn effect_host(&self) -> Arc<dyn lash_core::EffectHost> {
        self.effect_host()
    }

    fn process_registry(&self) -> Arc<dyn lash_core::ProcessRegistry> {
        Arc::clone(self.process.process_work().registry())
    }

    fn trigger_store(&self) -> Arc<dyn lash_core::TriggerStore> {
        self.stores.trigger_store()
    }

    fn process_definition_registry(&self) -> Arc<dyn lash_core::ProcessDefinitionRegistry> {
        self.stores.process_definition_registry()
    }

    fn process_env_store(&self) -> Arc<dyn lash_core::ProcessExecutionEnvStore> {
        self.stores.process_env_store()
    }

    fn attachment_store(&self) -> Arc<dyn lash_core::AttachmentStore> {
        self.stores.attachment_store()
    }

    fn process_work(&self) -> Option<lash_core::ProcessWorkWiring> {
        Some(self.process.process_work())
    }

    fn queued_work(&self) -> BackendQueuedWork {
        self.queued_work.clone()
    }
}

/// The store set this backend journals beside keeps its Lashlang module
/// artifacts.
#[cfg(feature = "lashlang")]
impl<S: ?Sized + lashlang::LashlangArtifactStoreSet> lashlang::LashlangArtifactBackend
    for RestateBackend<S>
{
    fn lashlang_artifact_store(&self) -> Arc<dyn lashlang::LashlangArtifactStore> {
        self.stores.lashlang_artifact_store()
    }
}

impl<S: ?Sized + StoreSet> std::fmt::Debug for RestateBackend<S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestateBackend")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}
