//! [`RestateBackend`]: the Restate engine host over one SQL store set
//! (ADR 0102, D2).
//!
//! Restate journals the effects and runs the background processes; the store
//! set keeps the sessions, the process registry and every other persistence
//! port. That pairing is one backend, not a mixture: the engine stores no
//! sessions, and the store set opens no effect journal of its own.

use std::sync::Arc;

use lash_core::facade_support::{ProcessEventSink, TurnWorkDriver};
use lash_core::{EffectHost as _, SessionWorkEngine, StoreSet};

use crate::effect_host::RestateEffectHost;
use crate::ingress::{RestateAuthorityId, RestateConnection, RestateIngressClient};
use crate::process::{RestateProcessDeployment, RestateProcessServing};
use crate::services::{LashServiceParts, bind_lash_services};
use crate::session_driver::RestateSessionWork;
use crate::turn::RestateTurnAttach;

/// The Restate engine host and Restate process work over one SQL
/// [`StoreSet`] (SQLite or PostgreSQL).
///
/// Its binding identity is the Restate authority's, the one the effect host's
/// turn-control binding and await-event keys derive from.
///
/// `S` is the store set's type. The backend hands out that store set's
/// Lashlang artifact store, so an RLM host reads its artifacts from the
/// substrate it journals beside.
pub struct RestateBackend<S: ?Sized + StoreSet = dyn StoreSet> {
    stores: Arc<S>,
    connection: RestateConnection,
    effect_host: Arc<RestateEffectHost>,
    process: Arc<RestateProcessDeployment>,
    session_work: Arc<RestateSessionWork>,
    identity: Arc<str>,
}

impl<S: ?Sized + StoreSet> Clone for RestateBackend<S> {
    fn clone(&self) -> Self {
        Self {
            stores: Arc::clone(&self.stores),
            connection: self.connection.clone(),
            effect_host: Arc::clone(&self.effect_host),
            process: Arc::clone(&self.process),
            session_work: Arc::clone(&self.session_work),
            identity: Arc::clone(&self.identity),
        }
    }
}

impl<S: ?Sized + StoreSet> RestateBackend<S> {
    /// The backend reaching Restate at `connection` under `authority_id`,
    /// over `stores`. Its sessions' drives run on the `LashSession` and
    /// `LashTurn` services its endpoint builder binds.
    pub fn new(
        connection: impl Into<RestateConnection>,
        authority_id: RestateAuthorityId,
        stores: Arc<S>,
    ) -> Self {
        Self::with_process_event_sink(connection, authority_id, stores, None)
    }

    /// Like [`new`](Self::new), but installs a host-facing
    /// [`ProcessEventSink`] on the process registry decorator the Restate
    /// process work wraps. Each appended event is pushed best-effort after
    /// its durable write.
    pub fn with_process_event_sink(
        connection: impl Into<RestateConnection>,
        authority_id: RestateAuthorityId,
        stores: Arc<S>,
        sink: Option<Arc<dyn ProcessEventSink>>,
    ) -> Self {
        let connection = connection.into();
        let effect_host = Arc::new(RestateEffectHost::new(
            connection.clone(),
            authority_id.clone(),
        ));
        let process = Arc::new(RestateProcessDeployment::new_with_sink(
            connection.clone(),
            authority_id,
            stores.process_registry(),
            stores.process_continuations(),
            sink,
        ));
        let identity = Arc::from(effect_host.turn_control_binding_id());
        let session_work = Arc::new(RestateSessionWork::new(
            RestateIngressClient::new(connection.clone()),
            crate::RestateSessionDriverSlot::new(),
        ));
        Self {
            stores,
            connection,
            effect_host,
            process,
            session_work,
            identity,
        }
    }

    /// The endpoint builder a deployment that serves this backend's work
    /// starts from, with every Restate service lash itself serves already
    /// bound: the durable-wait workflow and index, process attach, the
    /// process workflow over `processes` (a [`DurableProcessWorker`], or a
    /// [`RestateProcessServing`] that also sets the segment policy), and the
    /// effect-group index, payload and dispatcher, and the session driver
    /// (`LashSession`, `LashTurn`) that runs every session's turns. The host
    /// binds only its own services — its triggers and cron — on the builder,
    /// then builds it.
    ///
    /// There is no other way to bind lash's services, so an endpoint cannot
    /// serve a subset of them. A process that only submits work to Restate
    /// and serves no handlers does not call this.
    ///
    /// Effect-group children route through the resolver registered on this
    /// backend's effect host — the runtime's tool-child host once a core over
    /// this backend installs it — and a session-scope child checks its
    /// session's state generation in this backend's session catalog.
    ///
    /// [`DurableProcessWorker`]: lash_core_worker::DurableProcessWorker
    pub fn endpoint_builder(
        &self,
        processes: impl Into<RestateProcessServing>,
    ) -> restate_sdk::endpoint::Builder {
        bind_lash_services(
            restate_sdk::endpoint::Endpoint::builder(),
            LashServiceParts {
                effect_host: &self.effect_host,
                ingress: RestateIngressClient::new(self.connection.clone()),
                sessions: self.stores.session_store_factory(),
                process_workflow: self.process.workflow(processes.into()),
                session_driver: self.session_work.driver_slot().clone(),
            },
        )
    }

    /// The Restate effect host every runtime of this backend runs on.
    pub fn effect_host(&self) -> Arc<RestateEffectHost> {
        Arc::clone(&self.effect_host)
    }

    /// The Restate process work over this backend's registry: the port a
    /// host admits pending processes and awaits work items through.
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

    /// The engine that runs this backend's session drives: a drive is a send
    /// to the session's `LashSession` object.
    pub fn session_work_engine(&self) -> &Arc<RestateSessionWork> {
        &self.session_work
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

    /// The store set this backend journals beside keeps its Lashlang module
    /// artifacts.
    fn module_artifacts(&self) -> Arc<dyn lash_core::ModuleArtifactStore> {
        self.stores.module_artifacts()
    }

    fn process_work(&self) -> Option<lash_core::ProcessWorkWiring> {
        Some(self.process.process_work())
    }

    fn session_work(&self) -> Option<Arc<dyn SessionWorkEngine>> {
        Some(Arc::clone(&self.session_work) as Arc<dyn SessionWorkEngine>)
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
