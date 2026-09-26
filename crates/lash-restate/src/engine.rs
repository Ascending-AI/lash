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
use lash_core::{EffectHost as _, SessionWorkEngine, StoreSet};

use crate::effect_host::RestateEffectHost;
use crate::ingress::{RestateAuthorityId, RestateConnection, RestateIngressClient};
use crate::process::{RestateProcessDeployment, RestateProcessServing};
use crate::services::{LashServiceParts, bind_lash_services};
use crate::session_driver::RestateSessionWork;
use crate::turn::RestateTurnAttach;

/// How a [`RestateEngine`] reaches Restate and under which authority and
/// build it journals.
#[derive(Clone)]
pub struct RestateConfig {
    connection: RestateConnection,
    authority: RestateAuthorityId,
    build_generation: BuildGeneration,
    process_event_sink: Option<Arc<dyn ProcessEventSink>>,
    admin_connection: RestateConnection,
}

impl RestateConfig {
    /// Reach Restate's ingress at `connection` and its admin API at
    /// `admin_connection`, under `authority`, as a deployment of the build
    /// whose drain generation is `build_generation`. The engine's sessions'
    /// drives run on the `LashSession` and `LashTurn` services its endpoint
    /// builder binds, and both record and stamp that generation.
    ///
    /// The admin API is required: releasing a cancelled or forked root's
    /// execution, resuming a redriven one, and reconciling executions Restate
    /// stopped retrying all go through it, and a deployment without it would
    /// refuse every such verb and hold its sessions behind them.
    ///
    /// `build_generation` is the facade's `formats::build_generation()`
    /// answer: lash-restate cannot compute it (the format manifest lives in
    /// the facade), so the caller hands it in and the engine reports it as
    /// its own. A test double stamps a [`BuildGeneration::for_test`] value
    /// instead.
    pub fn new(
        connection: impl Into<RestateConnection>,
        admin_connection: impl Into<RestateConnection>,
        authority: RestateAuthorityId,
        build_generation: BuildGeneration,
    ) -> Self {
        Self {
            connection: connection.into(),
            authority,
            build_generation,
            process_event_sink: None,
            admin_connection: admin_connection.into(),
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
    session_work: Arc<RestateSessionWork>,
}

impl RestateEngine {
    /// The engine over `stores`, configured by `config`.
    pub fn new(stores: Arc<dyn StoreSet>, config: RestateConfig) -> Self {
        let RestateConfig {
            connection,
            authority,
            build_generation,
            process_event_sink,
            admin_connection,
        } = config;
        let effect_host = Arc::new(RestateEffectHost::new_for_build(
            connection.clone(),
            authority.clone(),
            Some(build_generation.clone()),
        ));
        let process = Arc::new(RestateProcessDeployment::new_with_sink(
            connection.clone(),
            authority,
            stores.process_registry(),
            stores.process_continuations(),
            process_event_sink,
        ));
        let session_work = Arc::new(RestateSessionWork::new(
            RestateIngressClient::new(connection.clone()),
            crate::RestateAdminClient::new(connection.clone()),
            crate::RestateSessionDriverSlot::new(),
            build_generation.clone(),
            Arc::new(crate::session_control::RestateSessionControl {
                admin: crate::RestateAdminClient::new(admin_connection),
                processes: stores.process_registry(),
                continuations: stores.process_continuations(),
            }),
        ));
        Self {
            stores,
            connection,
            build_generation,
            effect_host,
            process,
            session_work,
        }
    }

    /// The endpoint builder a deployment that serves this engine's work
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
    /// Every journal-bearing lash service is bound twice (FIG-3795): under
    /// its stable name, which Restate hands to the newest registered build,
    /// and under this build's generation name (`LashProcessWorkflow_g<G>`),
    /// which only builds of this engine's
    /// [`build_generation`](Self::build_generation) serve, so work pinned to
    /// this build — an effect group's children, a redrive, a successor a
    /// newer build refused — still reaches it after a newer build registers.
    /// Restate pins an invocation to the deployment URI it started on, so
    /// each build must register its endpoint under a URI of its own:
    /// [`deployment_path`] names one. Registering, retiring and ordering the
    /// deployments stay with the host (FIG-3794).
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
                process_workflow: self
                    .process
                    .workflow(processes.into(), self.build_generation.clone()),
                session_driver: self.session_work.driver_slot().clone(),
                build_generation: self.build_generation.clone(),
            },
        )
    }

    /// Another build of this engine's code in the same process: the same
    /// stores, effect host, process deployment and session driver, stamping
    /// `build_generation` on the endpoint its
    /// [`endpoint_builder`](Self::endpoint_builder) builds. What a test
    /// double registers as a second deployment to stand up two builds over
    /// one store; a real deployment of another build is its own process.
    #[doc(hidden)]
    pub fn sibling_build(&self, build_generation: BuildGeneration) -> Self {
        Self {
            stores: Arc::clone(&self.stores),
            connection: self.connection.clone(),
            build_generation,
            effect_host: Arc::clone(&self.effect_host),
            process: Arc::clone(&self.process),
            session_work: Arc::clone(&self.session_work),
        }
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

    fn session_work(&self) -> Option<Arc<dyn SessionWorkEngine>> {
        Some(Arc::clone(&self.session_work) as Arc<dyn SessionWorkEngine>)
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

/// The path, below a deployment's base URI, under which a build of drain
/// generation `generation` registers its endpoint with Restate:
/// `lash/<G>/<build_id>` (FIG-3795).
///
/// Restate pins every invocation to the deployment URI it started on and
/// routes by service name, so two builds must never share a URI: a build
/// swapped behind a pinned URI would replay the older build's journals (the
/// generation sentinel parks them, typed, but they then wait for that URI's
/// old code to come back). `build_id` distinguishes builds of one generation
/// (a rebuild that moved no drain format); `G` in the path lets an operator
/// read a deployment's generation from its URI. The endpoint serves any
/// path prefix, so a host mounts it here as it is. Registering, retiring and
/// ordering the deployments stay with the host (FIG-3794); FIG-3806
/// documents the operator flow.
///
/// `build_id` is a single path segment: every byte outside
/// `[A-Za-z0-9._~-]` is percent-encoded.
pub fn deployment_path(generation: &BuildGeneration, build_id: &str) -> String {
    format!(
        "lash/{generation}/{}",
        crate::ingress::restate_path_component(build_id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deployment_path_names_the_generation_and_one_build_segment() {
        let generation = BuildGeneration::from_digest([0x3f, 0xa9, 0x00, 0xbc, 0x12, 0xde]);
        assert_eq!(
            deployment_path(&generation, "2026-09-26.1"),
            "lash/3fa900bc12de/2026-09-26.1"
        );
        assert_eq!(
            deployment_path(&generation, "sha/abc 1"),
            "lash/3fa900bc12de/sha%2Fabc%201"
        );
        assert_ne!(
            deployment_path(&generation, "a"),
            deployment_path(&BuildGeneration::for_test("other"), "a"),
            "builds of two generations never share a path"
        );
    }
}
