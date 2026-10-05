//! [`RestateEngine`]: the Restate effect engine over one SQL store set
//! (ADR 0104, B2).
//!
//! Restate journals the effects and runs the background processes; the store
//! set keeps the sessions, the process registry and every other persistence
//! port. A [`Backend`](lash_core::Backend) over the engine is one substrate,
//! not a mixture: the engine stores no sessions, and the store set opens no
//! effect journal of its own.

use std::sync::Arc;

use lash_core::engine::{BuildGeneration, EngineGeneration, GenerationUnbound};
use lash_core::facade_support::{ProcessEventSink, TurnWorkDriver};
use lash_core::{EffectHost as _, SessionWorkEngine, StoreSet};

use crate::effect_host::{RestateEffectHost, RestateJournalAuthority};
use crate::ingress::{RestateAuthorityId, RestateConnection, RestateIngressClient};
use crate::process::{RestateProcessDeployment, RestateProcessServing};
use crate::services::{LashServiceParts, RestateNamespace, bind_lash_services};
use crate::session_shifts::RestateSessionWork;
use crate::turn::RestateTurnAttach;

/// How a [`RestateEngine`] reaches Restate and under which authority and
/// build it journals.
#[derive(Clone)]
pub struct RestateConfig {
    connection: RestateConnection,
    authority: RestateAuthorityId,
    generation: EngineGeneration,
    process_event_sink: Option<Arc<dyn ProcessEventSink>>,
    admin_connection: RestateConnection,
    namespace: RestateNamespace,
    run_effect_budget: Option<u64>,
}

impl RestateConfig {
    /// Reach Restate's ingress at `connection` and its admin API at
    /// `admin_connection`, under `authority`. The engine's sessions' shifts
    /// run on the `LashSession` and `LashTurn` services its endpoint builder
    /// binds, and both record and stamp the build's drain generation.
    ///
    /// The admin API is required: releasing a cancelled or forked run's
    /// execution, resuming a redriven one, and reconciling executions Restate
    /// stopped retrying all go through it, and a deployment without it would
    /// refuse every such verb and hold its sessions behind them.
    ///
    /// No generation is handed in (FIG-4744): it folds in the core's plugins
    /// in hook order, so the core built over this engine's backend computes
    /// it after registration and binds it. Until then the engine has no
    /// generation: it builds no endpoint and stamps no work, so nothing can
    /// open on a lane no deployment serves.
    pub fn new(
        connection: impl Into<RestateConnection>,
        admin_connection: impl Into<RestateConnection>,
        authority: RestateAuthorityId,
    ) -> Self {
        Self {
            connection: connection.into(),
            authority,
            generation: EngineGeneration::unbound(),
            process_event_sink: None,
            admin_connection: admin_connection.into(),
            namespace: RestateNamespace::default(),
            run_effect_budget: None,
        }
    }

    /// End a run's `LashTurn` invocation at the execution's next quiet point once
    /// it has executed `effects` effects, instead of the default 10,000
    /// (FIG-4739): the run goes on in a new invocation with a journal of its
    /// own. A replay observes the same count and ends at the same point.
    pub fn with_run_effect_budget(mut self, effects: u64) -> Self {
        self.run_effect_budget = Some(effects.max(1));
        self
    }

    /// Stamp `build_generation` on the engine instead of the generation its
    /// core computes: a test double's stand-in for a build, which lets its
    /// endpoint exist before any core does and lets one process stand up
    /// several builds. A core built over the engine leaves the stamp as it
    /// is.
    #[doc(hidden)]
    pub fn stamped(mut self, build_generation: BuildGeneration) -> Self {
        self.generation = EngineGeneration::fixed(build_generation);
        self
    }

    /// Name every Restate service the engine serves and calls under
    /// `namespace` (FIG-3898): `ns.LashSession`, `ns.LashProcessWorkflow`,
    /// and so on. Unset, the engine keeps the default namespace's bare names,
    /// which is all a deployment alone on its server needs. Deployments that
    /// share one server each take a distinct namespace;
    /// [`RestateEngine::register_deployment`] refuses one that would take
    /// over another's names.
    ///
    /// The namespace is part of every durable name the deployment records,
    /// so a changed namespace is a new deployment: work its predecessor
    /// journaled stays under the old names.
    pub fn with_namespace(mut self, namespace: RestateNamespace) -> Self {
        self.namespace = namespace;
        self
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
    admin: crate::RestateAdminClient,
    namespace: RestateNamespace,
    generation: EngineGeneration,
    effect_host: Arc<RestateEffectHost>,
    process: Arc<RestateProcessDeployment>,
    session_work: Arc<RestateSessionWork>,
    /// What the engine was built from, kept so the double can stand up
    /// another build over the same stores ([`Self::separate_build`]).
    config: RestateConfig,
}

impl RestateEngine {
    /// The engine over `stores`, configured by `config`.
    pub fn new(stores: Arc<dyn StoreSet>, config: RestateConfig) -> Self {
        let RestateConfig {
            connection,
            authority,
            generation,
            process_event_sink,
            admin_connection,
            namespace,
            run_effect_budget,
        } = config.clone();
        let effect_host = Arc::new(RestateEffectHost::in_deployment_namespace(
            connection.clone(),
            authority.clone(),
            namespace.clone(),
        ));
        let process = Arc::new(RestateProcessDeployment::in_namespace(
            connection.clone(),
            authority,
            stores.process_registry(),
            stores.process_continuations(),
            generation.clone(),
            process_event_sink,
            namespace.clone(),
        ));
        let admin = crate::RestateAdminClient::new(admin_connection);
        effect_host.bind_journal_authority(RestateJournalAuthority::new(
            admin.clone(),
            Arc::clone(&stores),
        ));
        effect_host.bind_wait_receipts(stores.session_store_factory(), stores.clock());
        let session_work = Arc::new(RestateSessionWork::new(
            RestateIngressClient::new(connection.clone()),
            crate::RestateSessionShiftsSlot::new()
                .with_run_effect_budget(run_effect_budget)
                .with_generation_drain(stores.generation_drain()),
            generation.clone(),
            namespace.clone(),
            Arc::new(crate::session_control::RestateSessionControl {
                lost_processes: Default::default(),
                lost_runs: Default::default(),
                admin: admin.clone(),
                ingress: RestateIngressClient::new(connection.clone()),
                namespace: namespace.clone(),
                processes: stores.process_registry(),
                continuations: stores.process_continuations(),
                generation: generation.clone(),
                sessions: stores.session_store_factory(),
            }),
        ));
        Self {
            stores,
            connection,
            admin,
            namespace,
            generation,
            effect_host,
            process,
            session_work,
            config,
        }
    }

    /// The endpoint builder a deployment that serves this engine's work
    /// starts from, with every Restate service lash itself serves already
    /// bound: the durable-wait workflow and index, the
    /// process workflow over `processes` (a [`DurableProcessWorker`], or a
    /// [`RestateProcessServing`] that also sets the segment policy), and the
    /// `SessionShifts`
    /// (`LashSession`, `LashTurn`) that runs every session's turns. The host
    /// binds only its own services — its triggers and cron — on the builder,
    /// then builds it and serves it with [`crate::serve_endpoint`].
    ///
    /// There is no other way to bind lash's services, so an endpoint cannot
    /// serve a subset of them. A process that only submits work to Restate
    /// and serves no handlers does not call this.
    ///
    /// It also names this engine's store as the process's fleet epoch for
    /// lash code the host's own handlers run: the calls they journal state
    /// the wire the store's recorded `F` selects, as a lash handler's do.
    ///
    /// Every journal-bearing lash service is bound twice (FIG-3795): under
    /// its stable name, which Restate hands to the newest registered build,
    /// and under this build's generation name (`LashProcessWorkflow_g<G>`),
    /// which only builds of this engine's
    /// [`build_generation`](Self::build_generation) serve, so work pinned to
    /// this build — a redrive, a successor a
    /// newer build refused — still reaches it after a newer build registers.
    /// Restate pins an invocation to the deployment URI it started on, so
    /// each build must register its endpoint under a URI of its own:
    /// [`deployment_path`] names one. Registering, retiring and ordering the
    /// deployments stay with the host (FIG-3794).
    ///
    /// # Errors
    /// [`GenerationUnbound`] before a core was built over this engine's
    /// backend: the generation lanes are named by the generation the core
    /// computes from its registered plugins (FIG-4744), so the endpoint is
    /// built after the core.
    ///
    /// [`DurableProcessWorker`]: lash_core_worker::DurableProcessWorker
    pub fn endpoint_builder(
        &self,
        processes: impl Into<RestateProcessServing>,
    ) -> Result<restate_sdk::endpoint::Builder, GenerationUnbound> {
        let build_generation = self.generation.get()?.clone();
        // The host's own handlers bound on this builder run lash code under
        // no lash handler: their journaled calls state the wire this
        // deployment's recorded `F` selects (FIG-3805).
        crate::compat::DeploymentWire::serve_host_fleet(crate::object_state::FleetView::of(
            self.stores.process_registry(),
        ));
        Ok(bind_lash_services(
            restate_sdk::endpoint::Endpoint::builder(),
            LashServiceParts {
                effect_host: &self.effect_host,

                admin: self.admin.clone(),

                materials: self.stores.tool_material_store(),
                attachments: self.stores.attachment_referrers(),
                process_workflow: self
                    .process
                    .workflow(
                        processes.into(),
                        build_generation.clone(),
                        self.stores.attachment_referrers(),
                    )
                    .with_generation_drain(self.stores.generation_drain()),
                session_shifts: self.session_work.shifts_slot().clone(),
                build_generation,
                namespace: self.namespace.clone(),
                fleet: crate::object_state::FleetView::of(self.stores.process_registry()),
            },
        ))
    }

    /// Register the endpoint at `uri` with the server as a deployment of
    /// this engine, unless another lash deployment already serves one of its
    /// names (FIG-3898), or the URI already serves another build (ADR 0115
    /// §3.5).
    ///
    /// Restate hands every new call to a service name to the deployment that
    /// registered the name last, so registering over another deployment's
    /// names would take over its work in silence. Every lash service a
    /// deployment binds declares the Restate authority it journals under, and
    /// this reads that claim for each of the engine's names first: a name
    /// claimed by another authority refuses the registration with
    /// [`RestateRegistrationError::NameTaken`], before anything is
    /// registered. Deployments of one authority share their names — a newer
    /// build, or the same deployment coming back — and so does the
    /// deployment already registered at `uri` itself. A name registered
    /// without a claim is taken over.
    ///
    /// Endpoints are immutable: Restate pins a started invocation to the
    /// deployment URI it started on, so a build registered over another
    /// build's URI would hand that build's pinned journals to new code. The
    /// registration reads the deployment at `uri` first. None: it registers
    /// without force. One that serves this build's generation names
    /// (`…_g<G>`): it registers with force, a redeploy of the same build.
    /// Any other: [`RestateRegistrationError::EndpointServesAnotherGeneration`],
    /// with nothing registered. A rollback registers the older build at a
    /// fresh URI ([`deployment_path`]), which leaves the newer build's
    /// deployment serving its own pinned work.
    ///
    /// The checks and the registration are separate admin calls, so two
    /// deployments that register the same names at once can both pass them.
    ///
    /// # Errors
    /// [`RestateRegistrationError::NameTaken`] for a name another deployment
    /// holds; [`RestateRegistrationError::EndpointServesAnotherGeneration`]
    /// for a URI another build holds; [`RestateRegistrationError::Admin`]
    /// when the admin API fails or refuses the registration;
    /// [`RestateRegistrationError::GenerationUnbound`] before a core was
    /// built over this engine's backend.
    #[allow(
        clippy::result_large_err,
        reason = "RestateHttpError travels unboxed across the crate's admin and ingress API"
    )]
    pub async fn register_deployment(&self, uri: &str) -> Result<(), RestateRegistrationError> {
        let build_generation = self.generation.get()?;
        let force = self.redeploys_endpoint(uri, build_generation).await?;
        let authority = self.effect_host.authority_id().binding_id();
        let routes = crate::services::LASH_SERVICES.iter().flat_map(|&service| {
            crate::services::lanes(&self.namespace, service, build_generation)
        });
        for route in routes {
            let name = route.name();
            let Some(registration) = self.admin.service_registration(&name).await? else {
                continue;
            };
            let Some(claim) = registration
                .metadata
                .get(crate::services::CLAIM_AUTHORITY_METADATA)
            else {
                continue;
            };
            if claim == authority {
                continue;
            }
            let registered_at = self
                .admin
                .deployment_uri(&registration.deployment_id)
                .await?;
            if registered_at
                .as_deref()
                .is_some_and(|registered| same_uri(registered, uri))
            {
                continue;
            }
            return Err(RestateRegistrationError::NameTaken {
                service: name.into_owned(),
                deployment_id: registration.deployment_id,
                claimed_by: claim.clone(),
                uri: uri.to_owned(),
            });
        }
        self.admin.register_deployment(uri, force).await?;
        Ok(())
    }

    /// Whether registering at `uri` redeploys this build over itself: `false`
    /// for a URI no deployment holds, `true` for one whose deployment serves
    /// this build's generation names, and the typed refusal for any other.
    #[allow(
        clippy::result_large_err,
        reason = "RestateHttpError travels unboxed across the crate's admin and ingress API"
    )]
    async fn redeploys_endpoint(
        &self,
        uri: &str,
        build_generation: &BuildGeneration,
    ) -> Result<bool, RestateRegistrationError> {
        let Some(held) = self
            .admin
            .deployments()
            .await?
            .into_iter()
            .find(|deployment| {
                deployment
                    .uri
                    .as_deref()
                    .is_some_and(|registered| same_uri(registered, uri))
            })
        else {
            return Ok(false);
        };
        let generations = held
            .services
            .iter()
            .filter_map(|name| self.namespace.parse(name))
            .filter_map(|route| match route.lane() {
                crate::services::Lane::Generation(generation) => Some(generation.clone()),
                crate::services::Lane::Stable => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        if generations.contains(build_generation) {
            return Ok(true);
        }
        Err(RestateRegistrationError::EndpointServesAnotherGeneration {
            uri: uri.to_owned(),
            held: generations.into_iter().next(),
            local: build_generation.clone(),
        })
    }

    /// Another build of this engine's code in the same process: the same
    /// stores, effect host, process deployment and `SessionShifts`, stamping
    /// `build_generation` on the endpoint its
    /// [`endpoint_builder`](Self::endpoint_builder) builds. What a test
    /// double registers as a second deployment to stand up two builds over
    /// one store; a real deployment of another build is its own process.
    #[doc(hidden)]
    pub fn sibling_build(&self, build_generation: BuildGeneration) -> Self {
        Self {
            stores: Arc::clone(&self.stores),
            connection: self.connection.clone(),
            admin: self.admin.clone(),
            namespace: self.namespace.clone(),
            generation: EngineGeneration::fixed(build_generation.clone()),
            effect_host: Arc::clone(&self.effect_host),
            process: Arc::clone(&self.process),
            session_work: Arc::clone(&self.session_work),
            config: self.config.clone().stamped(build_generation),
        }
    }

    /// Another build over the same stores that shares nothing else with
    /// this one: its own effect host, process deployment and session
    /// driver, as a real deployment of another build has in its own
    /// process. A core built over it installs its own plugins, so a test
    /// double can run two builds whose plugin compositions differ
    /// (FIG-4744): work in flight on this build keeps running under this
    /// build's plugins while the other serves new work under its own.
    #[doc(hidden)]
    pub fn separate_build(&self, build_generation: BuildGeneration) -> Self {
        Self::new(
            Arc::clone(&self.stores),
            self.config.clone().stamped(build_generation),
        )
    }

    /// The namespace every Restate service of this engine is named in
    /// (FIG-3898).
    pub fn namespace(&self) -> &RestateNamespace {
        &self.namespace
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

    /// The drain generation of the build this engine runs on: the one the
    /// core built over its backend computed from its formats and its
    /// registered plugins and bound
    /// ([`EffectEngine::generation`](lash_core::EffectEngine::generation)).
    ///
    /// # Errors
    /// [`GenerationUnbound`] before that core is built.
    pub fn build_generation(&self) -> Result<&BuildGeneration, GenerationUnbound> {
        self.generation.get()
    }

    /// Exact-turn control over this engine's sessions, usable from a
    /// process outside the turn's handler.
    pub fn turn_work_driver(&self) -> TurnWorkDriver {
        TurnWorkDriver::for_catalog(
            self.effect_host.clone(),
            self.stores.session_store_factory(),
        )
    }

    /// The engine that runs this backend's session shifts: a shift is a send
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

    fn generation(&self) -> &EngineGeneration {
        &self.generation
    }

    fn process_work(&self) -> lash_core::ProcessWorkWiring {
        self.process.process_work()
    }

    fn session_work(&self) -> Arc<dyn SessionWorkEngine> {
        Arc::clone(&self.session_work) as Arc<dyn SessionWorkEngine>
    }

    /// The Restate server's deployments and the groups whose committed
    /// children owe a drain on a lane, read through the admin API (FIG-4454).
    fn deployment_registry(&self) -> Arc<dyn lash_core::store::fleet_finalize::DeploymentRegistry> {
        Arc::new(crate::RestateDeploymentRegistry::new(self.admin.clone()))
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

/// Why [`RestateEngine::register_deployment`] did not register a deployment.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RestateRegistrationError {
    /// Another lash deployment serves `service`: registering would take
    /// over its calls. The deployments need distinct namespaces
    /// ([`RestateConfig::with_namespace`]).
    #[error(
        "Restate service `{service}` is served by deployment `{deployment_id}` of authority \
         `{claimed_by}`; registering `{uri}` would take over its calls, so it was not \
         registered: give each deployment on the server its own namespace"
    )]
    NameTaken {
        /// The contested Restate service name.
        service: String,
        /// The deployment serving it now.
        deployment_id: String,
        /// The authority that deployment journals under.
        claimed_by: String,
        /// The URI this registration would have registered.
        uri: String,
    },
    /// The deployment registered at `uri` serves another build: `held` is
    /// the drain generation whose names it serves, `None` when it serves no
    /// lash generation lane of this namespace. Each build registers at a URI
    /// of its own ([`deployment_path`]), because Restate pins a started
    /// invocation to the URI it started on (ADR 0115 §3.5).
    #[error(
        "Restate endpoint `{uri}` already serves {}; a build of generation `{local}` registers \
         at a URI of its own (`deployment_path` names one), so it was not registered",
        .held.as_ref().map_or_else(
            || "a deployment of no lash generation".to_owned(),
            |held| format!("the deployment of generation `{held}`"),
        )
    )]
    EndpointServesAnotherGeneration {
        /// The URI this registration would have registered.
        uri: String,
        /// The generation the deployment at `uri` serves.
        held: Option<BuildGeneration>,
        /// This build's generation.
        local: BuildGeneration,
    },
    /// No core has been built over the engine's backend yet, so the engine
    /// has no generation to name its lanes by (FIG-4744).
    #[error(transparent)]
    GenerationUnbound(#[from] GenerationUnbound),
    /// The admin API failed, or refused the registration itself.
    #[error(transparent)]
    Admin(#[from] crate::RestateHttpError),
}

/// Whether two deployment URIs name one endpoint: Restate reports a
/// registered URI with the trailing `/` it normalizes to.
fn same_uri(left: &str, right: &str) -> bool {
    left.trim_end_matches('/') == right.trim_end_matches('/')
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
