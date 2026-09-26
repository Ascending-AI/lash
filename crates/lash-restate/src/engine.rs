//! [`RestateEngine`]: the Restate effect engine over one SQL store set
//! (ADR 0104, B2).
//!
//! Restate journals the effects and runs the background processes; the store
//! set keeps the sessions, the process registry and every other persistence
//! port. A [`Backend`](lash_core::Backend) over the engine is one substrate,
//! not a mixture: the engine stores no sessions, and the store set opens no
//! effect journal of its own.

use std::sync::Arc;

use lash_core::engine::BuildGeneration;
use lash_core::facade_support::{ProcessEventSink, TurnWorkDriver};
use lash_core::{BackendQueuedWork, EffectHost as _, QueuedWorkSubstrate, StoreSet};

use crate::effect_host::RestateEffectHost;
use crate::ingress::{RestateAuthorityId, RestateConnection, RestateIngressClient};
use crate::process::{RestateProcessDeployment, RestateProcessServing};
use crate::services::{LashServiceParts, bind_lash_services};
use crate::turn::RestateTurnAttach;

/// Who runs a [`RestateEngine`]'s queued session work: a required,
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

/// How a [`RestateEngine`] reaches Restate and under which authority and
/// build it journals.
#[derive(Clone)]
pub struct RestateConfig {
    connection: RestateConnection,
    authority: RestateAuthorityId,
    build_generation: BuildGeneration,
    queued_work: RestateQueuedWork,
    process_event_sink: Option<Arc<dyn ProcessEventSink>>,
}

impl RestateConfig {
    /// Reach Restate at `connection` under `authority` as a deployment of the
    /// build whose drain generation is `build_generation`, with `queued_work`
    /// running the store set's queued session work.
    ///
    /// `build_generation` is the facade's `formats::build_generation()`
    /// answer: lash-restate cannot compute it (the format manifest lives in
    /// the facade), so the caller hands it in and the engine reports it as
    /// its own. A test double stamps a [`BuildGeneration::for_test`] value
    /// instead.
    pub fn new(
        connection: impl Into<RestateConnection>,
        authority: RestateAuthorityId,
        build_generation: BuildGeneration,
        queued_work: RestateQueuedWork,
    ) -> Self {
        Self {
            connection: connection.into(),
            authority,
            build_generation,
            queued_work,
            process_event_sink: None,
        }
    }

    /// Install a host-facing [`ProcessEventSink`] on the process registry
    /// decorator the Restate process work wraps. Each appended event is
    /// pushed best-effort after its durable write.
    pub fn with_process_event_sink(mut self, sink: Arc<dyn ProcessEventSink>) -> Self {
        self.process_event_sink = Some(sink);
        self
    }
}

/// The Restate effect host and Restate process work over one SQL
/// [`StoreSet`] (SQLite or PostgreSQL).
///
/// The store set names the storage ([`StoreSet::binding_identity`]); the
/// Restate authority names the effect state, and the effect host's
/// turn-control binding and await-event keys derive from it. Both are fixed
/// here, when the engine is built over its store set.
pub struct RestateEngine {
    stores: Arc<dyn StoreSet>,
    connection: RestateConnection,
    build_generation: BuildGeneration,
    effect_host: Arc<RestateEffectHost>,
    process: Arc<RestateProcessDeployment>,
    queued_work: BackendQueuedWork,
}

impl RestateEngine {
    /// The engine over `stores`, configured by `config`.
    pub fn new(stores: Arc<dyn StoreSet>, config: RestateConfig) -> Self {
        let RestateConfig {
            connection,
            authority,
            build_generation,
            queued_work,
            process_event_sink,
        } = config;
        let effect_host = Arc::new(RestateEffectHost::new(
            connection.clone(),
            authority.clone(),
        ));
        let process = Arc::new(RestateProcessDeployment::new_with_sink(
            connection.clone(),
            authority,
            stores.process_registry(),
            stores.process_continuations(),
            process_event_sink,
        ));
        Self {
            stores,
            connection,
            build_generation,
            effect_host,
            process,
            queued_work: queued_work.into(),
        }
    }

    /// The endpoint builder a deployment that serves this engine's work
    /// starts from, with every Restate service lash itself serves already
    /// bound: the durable-wait workflow and index, process attach, the
    /// process workflow over `processes` (a [`DurableProcessWorker`], or a
    /// [`RestateProcessServing`] that also sets the segment policy), and the
    /// effect-group index, payload and dispatcher. The host binds only its own services — its
    /// turn workflows, triggers and cron — on the builder, then builds it.
    ///
    /// There is no other way to bind lash's services, so an endpoint cannot
    /// serve a subset of them. A process that only submits work to Restate
    /// and serves no handlers does not call this.
    ///
    /// Effect-group children route through the resolver registered on this
    /// backend's effect host — the runtime's tool-child host once a core over
    /// this engine installs it — and a session-scope child checks its
    /// session's state generation in this engine's session catalog.
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
            },
        )
    }

    /// The Restate effect host every runtime of this engine runs on.
    ///
    /// Named for the concrete host it returns; the `EffectEngine` trait's
    /// `effect_host` is the same host as `dyn`.
    pub fn restate_effect_host(&self) -> Arc<RestateEffectHost> {
        Arc::clone(&self.effect_host)
    }

    /// The Restate process work over this engine's registry: the port a
    /// host admits pending processes and awaits work items through.
    pub fn process_deployment(&self) -> &RestateProcessDeployment {
        &self.process
    }

    /// The store set this engine journals its effects beside.
    ///
    /// Named `store_set` so a method call cannot be misread as the
    /// `EffectEngine` trait's `stores`, which returns the same set by value.
    pub fn store_set(&self) -> &Arc<dyn StoreSet> {
        &self.stores
    }

    /// The drain generation of the build this engine runs on: the caller's
    /// `formats::build_generation()` answer, reported through
    /// [`EffectEngine::build_generation`](lash_core::EffectEngine::build_generation).
    pub fn build_generation(&self) -> &BuildGeneration {
        &self.build_generation
    }

    /// Exact-turn control over this engine's sessions, usable from a
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

impl lash_core::EffectEngine for RestateEngine {
    fn stores(&self) -> Arc<dyn StoreSet> {
        Arc::clone(&self.stores)
    }

    fn effect_host(&self) -> Arc<dyn lash_core::EffectHost> {
        self.restate_effect_host()
    }

    fn build_generation(&self) -> &BuildGeneration {
        self.build_generation()
    }

    fn process_work(&self) -> Option<lash_core::ProcessWorkWiring> {
        Some(self.process.process_work())
    }

    fn queued_work(&self) -> BackendQueuedWork {
        self.queued_work.clone()
    }
}

impl std::fmt::Debug for RestateEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RestateEngine")
            .field("stores", self.stores.binding_identity())
            .field("authority", &self.effect_host.turn_control_binding_id())
            .finish_non_exhaustive()
    }
}
