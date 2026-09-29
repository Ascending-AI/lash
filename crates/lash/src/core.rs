use crate::support::{
    Arc, EffectHost, EmbedError, InMemoryLiveReplayStore, LashRuntime, LashSession,
    LiveReplayStore, ParkedSession, PluginFactory, PluginHost, PluginOptions, PluginSpec,
    PluginStack, ProcessRegistry, PromptLayer, PromptLayerSink, ProviderHandle, Result,
    RuntimeEnvironment, RuntimeHandle, RuntimeHostConfig, SessionBuilder, SessionListFilter,
    SessionPolicy, SessionSpec, SessionStoreFactory, SessionSummary, SessionWorkEngine,
    StaticPluginFactory, TerminationPolicy, ToolProvider,
};
use lash_core::Backend;
use lash_core::facade_support;
use lash_core_worker::DurableProcessWorkerConfig;
use lash_sansio::SessionId;

mod advanced_builder;
mod drain;
pub(crate) mod held_drives;
mod recovery;
pub(crate) mod residents;
mod runtime_host_config;
pub(crate) mod session_driver;
mod session_policy;
mod tool_child_context;
mod work_drivers;

pub use advanced_builder::AdvancedLashCoreBuilder;
pub use drain::{DeploymentDrainStatus, GenerationDrainStatus};
use session_driver::{CoreSessionDriver, CoreSessionDriverConfig};
pub(crate) use work_drivers::ResolvedQueuedWork;
use work_drivers::{CoreWorkSetup, CoreWorkSlot, WakeDeliveryDriverSetup};
#[derive(Clone)]
/// Owns the configured runtime services used to create and resume Lash sessions.
pub struct LashCore {
    pub(crate) drive_owner: lash_core::LeaseOwnerIdentity,
    pub(crate) env: RuntimeEnvironment,
    pub(crate) tool_registry: Arc<lash_core::ToolRegistry>,
    pub(crate) policy: SessionPolicy,
    pub(crate) protocol_factory: Option<Arc<dyn PluginFactory>>,
    /// The one substrate every port and the effect host come from.
    pub(crate) backend: Backend,
    /// The backend's session catalog.
    pub(crate) store_factory: Arc<dyn SessionStoreFactory>,
    /// The backend's process registry, as the core sees it (watched, and
    /// on the core's clock).
    pub(crate) process_registry: Arc<dyn ProcessRegistry>,
    pub(crate) plugin_factories: Arc<Vec<Arc<dyn PluginFactory>>>,
    pub(crate) provider: Option<ProviderHandle>,
    pub(crate) live_replay_store: Arc<dyn LiveReplayStore>,
    pub(crate) process_observation_hub: Arc<crate::process_observation::ProcessObservationHub>,
    pub(crate) process_lifecycle_feed: Arc<crate::process_lifecycle::ProcessLifecycleFeed>,
    pub(crate) _process_lifecycle_registration:
        Option<Arc<facade_support::ProcessEventSinkRegistration>>,
    /// Whether process lifecycle is available; threaded into rebuilt session plugin hosts.
    pub(crate) process_lifecycle_available: bool,
    /// Base plugin-contributed engines available to host-level process APIs.
    /// Session runtimes still install onto their own clean registries so
    /// session-scoped plugin overlays can contribute additional engines.
    pub(crate) host_process_engines: lash_core::ProcessEngineRegistry,
    /// Shared across core clones so the work ports are resolved at most once.
    pub(crate) substrate_slot: Arc<CoreWorkSlot>,
    /// The session driver this core installed on its backend's session-work
    /// engine (FIG-3600), as the engine returned it. The engine may hold it
    /// weakly, so the core keeps it for its whole life, and no longer: a drive
    /// still running on this core's driver after the core is dropped does not
    /// keep the install live (FIG-4017).
    pub(crate) _session_driver: Arc<dyn lash_core::SessionDriver>,
    /// The sessions this core has open in this process: the driver runs a
    /// drive on the open session's runtime (FIG-3600 S5b).
    pub(crate) residents: Arc<residents::ResidentSessions>,
    /// This core's seat in the recovery leader election (ADR 0109 §1.6),
    /// shared with its session driver.
    pub(crate) recovery: Arc<recovery::RecoverySlot>,
    pub(crate) tool_intent_submission_gates:
        Arc<crate::tool_intent_ingress::RuntimeSubmissionGates>,
    /// The context a group tool child of this core's sessions runs under when
    /// its opener is not live where it runs (FIG-3712). The backend's
    /// tool-child host holds it weakly; the core and every session it opens
    /// hold it strongly, so a host that keeps its sessions and drops its core
    /// keeps rebuilding their children.
    pub(crate) tool_child_context_source:
        Arc<dyn lash_core::facade_support::ToolChildContextSource>,
}

pub use lash_core::session_delete::{
    SessionClosing, SessionDeleteFailure, SessionDeleteReport, SessionDeleteWait, SessionDeletion,
};

/// What a core builds its [`SessionAdministration`](lash_core::SessionAdministration)
/// from. Its session driver holds one, weakly bound to the core's substrate,
/// so the reconcile tick can deliver session deletes (ADR 0109 §4).
#[derive(Clone)]
pub(crate) struct AdministrationSource {
    slot: std::sync::Weak<CoreWorkSlot>,
    env: RuntimeEnvironment,
    store_factory: Arc<dyn SessionStoreFactory>,
    host_process_engines: lash_core::ProcessEngineRegistry,
}

impl AdministrationSource {
    /// The administration over the core's resolved ports; `None` once the
    /// core is gone.
    pub(crate) async fn administration(&self) -> Option<lash_core::SessionAdministration> {
        let slot = self.slot.upgrade()?;
        Some(self.administration_over(&slot).await)
    }

    async fn administration_over(&self, slot: &CoreWorkSlot) -> lash_core::SessionAdministration {
        let ports = slot.ports().await;
        let queued = ports.queued_port();
        let resolved_env = self
            .env
            .clone()
            .with_work_ports(Some(ports.process.clone()), Arc::clone(&queued));
        lash_core::SessionAdministration::new(
            Arc::clone(&self.store_factory),
            Arc::clone(&resolved_env.core.control.effect_host),
            Some(ports.process),
            Some(resolved_env.core.trigger_store()),
            Arc::clone(&resolved_env.core.durability.process_env_store),
            self.host_process_engines.clone(),
            lash_core::session_close::SessionCloseServices {
                work: queued,
                scopes: Arc::clone(&resolved_env.core.control.scope_close),
                scope_close_obligations: Arc::new(
                    lash_core::runtime::drive::ScopeCloseRelay::over_backend(
                        resolved_env.core.backend(),
                        Arc::clone(&self.store_factory),
                        Arc::clone(&resolved_env.core.control.scope_close),
                    ),
                ),
                intents: resolved_env
                    .core
                    .backend()
                    .obligation_ledger(lash_core::store::ObligationKind::ControlIntent),
                clock: Arc::clone(&resolved_env.core.clock),
                deletes: lash_core::session_delete::SessionDeleteStores::of(
                    resolved_env.core.backend(),
                ),
            },
        )
    }
}

impl LashCore {
    /// The core's session work as a host-held handle carries it.
    pub(crate) async fn held_work(&self) -> Arc<ResolvedQueuedWork> {
        Arc::clone(&self.substrate_slot.ports().await.queued)
    }

    /// The ingress relay an acceptance through this core delivers with
    /// (ADR 0109 §3): the backend's ingress ledger asking `work` for drives.
    pub(crate) fn ingress_relay(
        &self,
        work: &Arc<ResolvedQueuedWork>,
    ) -> lash_core::drive::IngressRelay {
        lash_core::drive::IngressRelay::over_backend(
            &self.backend,
            Arc::clone(work) as Arc<dyn SessionWorkEngine>,
            Arc::clone(&self.env.core.clock),
        )
    }

    /// A [`LashCoreBuilder`] over `backend`, the one substrate every
    /// persistence port and the effect host of this core come from (ADR 0102).
    ///
    /// The backend is the builder's only source of ports: there is no
    /// setter for a store, a registry or an effect host, so a core cannot mix
    /// substrates, and there is no in-memory default. The zero-infra
    /// backend for local tests is a Restate engine over a SQLite memory store set.
    pub fn builder(backend: Backend, turn_budget: lash_core::TurnBudget) -> LashCoreBuilder {
        LashCoreBuilder::new(backend, turn_budget)
    }

    /// Sugar entry point: a [`LashCoreBuilder`] over `backend` pre-seeded
    /// with the standard protocol plugin.
    pub fn standard_builder(
        backend: Backend,
        turn_budget: lash_core::TurnBudget,
    ) -> LashCoreBuilder {
        LashCore::builder(backend, turn_budget).protocol_plugin(Arc::new(
            lash_protocol_standard::StandardProtocolPluginFactory::new(),
        ))
    }

    /// The backend this core takes every port and its effect host from.
    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    /// The host owns admission and must pass its current admission state. Lash
    /// reads the process registry and the session store on demand; it does
    /// not maintain a counter or orchestrate routing, deadlines, worker
    /// shutdown, or retirement.
    ///
    /// Turns are counted as well as processes (FIG-3586): a parked turn, or a
    /// turn whose claims a crashed driver still holds, is unfinished work, so
    /// the deployment is not drained until none remains. A store that cannot
    /// count its turns refuses rather than report zero.
    pub async fn drain_status(&self, accepting_new_work: bool) -> Result<DeploymentDrainStatus> {
        let remaining_invocations = self.process_registry.count_non_terminal_processes().await?;
        let turns = self.store_factory.count_unsettled_turns().await?;
        let processes = self.process_registry.summarize_parked_processes().await?;
        let checked_at = self.env.core.clock.timestamp_ms();
        let parked = crate::parked_work::ParkedWorkSummary {
            turns: lash_core::store::ParkSummary {
                by_reason: turns.parked_by_reason.clone(),
                oldest_since_ms: turns.oldest_parked_since_ms,
                retired_by_executable_generation: turns.retired_by_executable_generation.clone(),
            },
            processes,
        };
        crate::parked_work::record_park_gauges(&parked, checked_at);
        let mut stalled_obligations = std::collections::BTreeMap::new();
        for kind in lash_core::store::ObligationKind::ALL {
            let count = self.backend.obligation_ledger(kind).count_stalled().await?;
            lash_core::operational_metrics::record_obligations_stalled(kind.label(), count);
            stalled_obligations.insert(kind, count);
        }
        Ok(DeploymentDrainStatus {
            accepting_new_work,
            remaining_invocations,
            in_flight_turns: turns.in_flight_turns,
            parked_turns: turns.parked_turns,
            parked_processes: parked.processes.total(),
            oldest_parked_since_ms: parked.oldest_since_ms(),
            retired_by_executable_generation: parked.retired_by_executable_generation(),
            stalled_obligations,
            checked_at,
        })
    }

    /// Mark `generation` draining (FIG-3799): from the next recovery tick the
    /// deployment that leads recovery wakes every live process whose current
    /// segment `generation` admitted, and each hands its open wait to a
    /// successor on the newest build, where it waits again. Poll
    /// [`generation_drain_status`](Self::generation_drain_status) until it
    /// reports drained, then retire the generation's deployment.
    ///
    /// Idempotent: `true` when this call marked the generation, `false` when
    /// it was already draining. A deployment cannot drain its own generation
    /// ([`EmbedError::DrainOwnGeneration`](crate::EmbedError::DrainOwnGeneration)):
    /// run it from a deployment of the replacing build. The generation's
    /// deployment keeps serving until the host retires it — the hand-over
    /// runs inside the segments it pinned.
    pub async fn drain_generation(
        &self,
        generation: &lash_core::engine::BuildGeneration,
    ) -> Result<bool> {
        if generation == self.backend.build_generation() {
            return Err(crate::EmbedError::DrainOwnGeneration {
                generation: generation.clone(),
            });
        }
        let now_ms = self.env.core.clock.timestamp_ms();
        Ok(self
            .backend
            .generation_drain()
            .mark_draining(generation, now_ms)
            .await?)
    }

    /// Stop draining `generation`: the recovery leader wakes none of its
    /// processes from the next tick (a rollback to the generation, or a drain
    /// abandoned). Segments already handed over stay where they run. `true`
    /// when a mark was removed.
    pub async fn end_generation_drain(
        &self,
        generation: &lash_core::engine::BuildGeneration,
    ) -> Result<bool> {
        Ok(self
            .backend
            .generation_drain()
            .clear_draining(generation)
            .await?)
    }

    /// What `generation` still holds (FIG-3799): whether it is marked
    /// draining, its live processes, the parked processes and turns its
    /// checkpoints hold, the turns its drives admitted that have not settled
    /// (FIG-3884), the closing sessions every drain waits on, and the stalled
    /// obligations, which it counts but does not wait on (FIG-4076).
    ///
    /// Reading the status is also the metrics refresh: the per-generation
    /// work gauges and each obligation kind's stalled gauge record inside
    /// [`GenerationDrainStatus::collect`].
    pub async fn generation_drain_status(
        &self,
        generation: &lash_core::engine::BuildGeneration,
    ) -> Result<GenerationDrainStatus> {
        let drain = self.backend.generation_drain();
        let session_delete = self.backend.session_delete_ledger();
        let backend = self.backend.clone();
        Ok(GenerationDrainStatus::collect(
            drain.as_ref(),
            session_delete.as_ref(),
            move |kind| backend.obligation_ledger(kind),
            generation,
            self.env.core.clock.timestamp_ms(),
        )
        .await?)
    }

    /// The stalled obligations of `kind` after `after`, in id order, at most
    /// `limit` (ADR 0109 §1.5): store→engine deliveries the relay stopped
    /// retrying, each with its reason, attempt count and last error.
    pub async fn stalled_obligations(
        &self,
        kind: lash_core::store::ObligationKind,
        after: Option<&lash_core::store::ObligationId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<lash_core::store::StalledObligation>> {
        Ok(self
            .backend
            .obligation_ledger(kind)
            .list_stalled(after, limit)
            .await?)
    }

    /// Put stalled obligation `id` of `kind` back in the relay's due index
    /// with its attempts reset, due now (ADR 0109 §1.5). Nothing re-arms a
    /// stalled obligation but this verb. `false` when `id` is not stalled.
    pub async fn rearm_obligation(
        &self,
        kind: lash_core::store::ObligationKind,
        id: &lash_core::store::ObligationId,
    ) -> Result<bool> {
        let now_ms = self.env.core.clock.timestamp_ms();
        Ok(self
            .backend
            .obligation_ledger(kind)
            .rearm(id, now_ms)
            .await?)
    }

    /// The deployment's parked work — turns and processes whose redrive
    /// refuses to replay their journals — to list, summarize and follow
    /// (FIG-3659).
    pub fn parked_work(&self) -> crate::parked_work::ParkedWork {
        crate::parked_work::ParkedWork {
            work: self.env.queued_work(),
            scopes: Arc::clone(&self.env.core.control.scope_close),
            scope_close_obligations: Arc::new(
                lash_core::runtime::drive::ScopeCloseRelay::over_backend(
                    &self.backend,
                    Arc::clone(&self.store_factory),
                    Arc::clone(&self.env.core.control.scope_close),
                ),
            ),
            intents: self
                .backend
                .obligation_ledger(lash_core::store::ObligationKind::ControlIntent),
            store_factory: Arc::clone(&self.store_factory),
            process_registry: Arc::clone(&self.process_registry),
            clock: Arc::clone(&self.env.core.clock),
        }
    }

    /// Sugar entry point: a [`LashCoreBuilder`] pre-seeded with a
    /// host-configured RLM protocol factory and the default runtime plugin
    /// stack.
    ///
    /// The host configures the factory (projection resolver, separate deferred
    /// tool and trigger-definition resolvers, execution sink/jsonl path) before
    /// passing it in. The factory is built over this same `backend`
    /// ([`RlmProtocolPluginFactory::new`](crate::rlm::RlmProtocolPluginFactory::new)),
    /// which supplies its Lashlang artifact store; a factory built over any
    /// other backend is refused when the core is built.
    #[cfg(feature = "rlm")]
    pub fn rlm_builder(
        backend: Backend,
        turn_budget: lash_core::TurnBudget,
        factory: crate::rlm::RlmProtocolPluginFactory,
    ) -> LashCoreBuilder {
        LashCore::builder(backend, turn_budget).protocol_plugin(Arc::new(factory))
    }

    pub fn session(&self, session_id: impl Into<SessionId>) -> SessionBuilder {
        SessionBuilder {
            core: self.clone(),
            session_id: session_id.into(),
            spec: SessionSpec::inherit(),
            parent_session_id: None,
            provider: None,
            plugin_factories: Vec::new(),
            plugin_options: PluginOptions::default(),
            tool_source_policy: None,
            tool_surface_open_mode: None,
        }
    }

    /// Shut down registered plugin factories after the host has stopped intake.
    ///
    /// This method releases plugin-factory resources; it does not stop intake,
    /// drain active turns, abort work, or orchestrate host shutdown. The host
    /// owns those steps and must call `shutdown` only after no new work can enter.
    /// The protocol factory is visited first, followed by common factories in
    /// configured order. These factories own disjoint resources, so the order
    /// carries no dependency semantics; it is fixed only for determinism and log
    /// auditability. A host that shares a resource across factories must not rely
    /// on this order.
    ///
    /// Every factory is visited even after failures. Each failure is warned and
    /// the first is returned after the walk. Implementations own their timeout
    /// policy and must make repeated shutdown calls idempotent. Factories added
    /// through [`AdvancedLashCoreBuilder::plugin_host`] are included. Extra
    /// factories supplied only to durable-process-worker configuration or to an
    /// individual session are host-owned and are not walked by this method.
    pub async fn shutdown(&self) -> Result<()> {
        // A stopping deployment hands recovery leadership over now rather
        // than after the lease's TTL (ADR 0109 §1.6).
        self.recovery.resign().await;
        let factories = self
            .protocol_factory
            .iter()
            .chain(self.plugin_factories.iter());
        let mut first_error = None;
        for factory in factories {
            let started = std::time::Instant::now();
            tracing::debug!(
                plugin_factory = factory.id(),
                "plugin factory shutdown started"
            );
            match factory.shutdown().await {
                Ok(()) => tracing::debug!(
                    plugin_factory = factory.id(),
                    elapsed_ms = started.elapsed().as_millis(),
                    "plugin factory shutdown completed"
                ),
                Err(error) => {
                    tracing::warn!(
                        plugin_factory = factory.id(),
                        elapsed_ms = started.elapsed().as_millis(),
                        error = %error,
                        "plugin factory shutdown failed"
                    );
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        match first_error {
            Some(error) => Err(EmbedError::Plugin(error)),
            None => Ok(()),
        }
    }

    /// Select the lifecycle owner used by administrative session operations.
    ///
    /// The returned handle keeps the catalog, effect host, process services,
    /// and trigger store chosen by this core together. Provider, plugin,
    /// prompt, tracing, and other live turn policy are deliberately excluded.
    pub async fn session_administration(&self) -> lash_core::SessionAdministration {
        self.administration_source()
            .administration_over(&self.substrate_slot)
            .await
    }

    /// What this core builds its session administration from.
    fn administration_source(&self) -> AdministrationSource {
        AdministrationSource {
            slot: Arc::downgrade(&self.substrate_slot),
            env: self.env.clone(),
            store_factory: Arc::clone(&self.store_factory),
            host_process_engines: self.host_process_engines.clone(),
        }
    }

    /// Rebuild a live session from a [`ParkedSession`](crate::ParkedSession)
    /// handle produced by [`LashSession::park`](crate::LashSession::park).
    ///
    /// Resume reloads the flushed state from the parked store (honoring this
    /// core's residency), reinstalls this core's plugin configuration and work
    /// drivers, and returns a ready [`LashSession`]. The parked store instance
    /// is reused directly, so the transcript the session flushed at park time is
    /// visible again after resume.
    ///
    /// This restores the core-level plugin stack. Session-specific plugins added
    /// per open via [`SessionBuilder::plugin`] are not re-applied here; parking
    /// is the round-trip for the core's own configuration.
    pub async fn resume(&self, parked: ParkedSession) -> Result<LashSession> {
        let ParkedSession { inner, binding } = parked;
        // Build the per-session env exactly like `SessionBuilder::open_resolved`
        // (minus builder-scoped plugins): a fresh plugin host with this core's
        // factories, the shared work drivers, and the core provider resolver
        // already carried on `self.env`.
        let plugin_host = build_plugin_host(
            self.protocol_factory.as_ref(),
            self.plugin_factories.as_ref(),
            Vec::new(),
        )?;
        let mut env = binding.apply_owner(self.env.clone());
        env.core = plugin_host.install_process_engine_contributions(
            env.core.clone(),
            self.process_lifecycle_available,
        )?;
        env.plugin_host = Some(Arc::new(plugin_host));
        let runtime = LashRuntime::resume(inner, &env, self.drive_owner.clone()).await?;
        let handle =
            RuntimeHandle::with_live_replay_store(runtime, Arc::clone(&self.live_replay_store));
        let process_lifecycle_route = self.process_lifecycle_feed.register(&handle);
        binding.register_resident(&handle);
        let parent_session_id =
            crate::session::recorded_parent_session_id(binding.store().as_ref()).await?;
        Ok(LashSession {
            runtime: handle,
            _process_lifecycle_route: process_lifecycle_route,
            binding,
            parent_session_id,
        })
    }

    /// Flush this core's configured trace sink, if any.
    ///
    /// Hosts that hand `lash` a trace sink via [`LashCoreBuilder::trace_sink`] already hold
    /// their own `Arc` and can flush it directly; this is the equivalent lever for hosts that
    /// did not retain the handle.
    /// It flushes the core's copy — for a [`JsonlTraceSink`](lash_trace::JsonlTraceSink) that
    /// fsyncs the file, and for an OTel sink it is a no-op (the host still owns provider
    /// flush; see the tracing docs).
    pub fn flush_trace_sink(&self) -> Result<()> {
        if let Some(sink) = self.env.core.tracing.trace_sink.as_ref() {
            sink.flush()?;
        }
        Ok(())
    }

    pub fn triggers(&self) -> crate::admin::CoreTriggerAdmin {
        crate::admin::CoreTriggerAdmin { core: self.clone() }
    }

    pub fn processes(&self) -> crate::process_admin::Processes {
        crate::process_admin::Processes { core: self.clone() }
    }

    /// The core's artifact stores, as a host publishes into them under a
    /// pin it minted (ADR 0113 §2.6).
    pub fn host_artifacts(&self) -> crate::artifacts::HostArtifacts {
        crate::artifacts::HostArtifacts::new(
            self.backend().module_artifacts(),
            Arc::clone(&self.env.core.durability.process_env_store),
            self.backend().artifact_cleanup(),
            Arc::clone(&self.env.core.clock),
        )
    }

    pub fn completions(&self) -> crate::admin::Completions {
        crate::admin::Completions { core: self.clone() }
    }

    pub fn effect_host(&self) -> Arc<dyn EffectHost> {
        Arc::clone(&self.env.core.control.effect_host)
    }

    /// Exact-turn cooperative control for this deployment's effect host.
    ///
    /// The returned driver is independently usable from any session handle.
    /// Session and turn ids are routing identity, not authorization; authorize
    /// requests in the host API before forwarding them to Lash.
    pub fn turn_work_driver(&self) -> facade_support::TurnWorkDriver {
        facade_support::TurnWorkDriver::for_catalog(
            self.effect_host(),
            Arc::clone(&self.store_factory),
        )
    }

    /// Retain the current continuation checkpoint for a turn-boundary node.
    ///
    /// A point must still be retained when this is called: ordinarily that
    /// means it is the leaf of a live session. Pin before advancing the head if
    /// a host wants to make a past turn forkable later.
    pub async fn pin(&self, node_id: impl AsRef<str>) -> Result<lash_core::ForkPoint> {
        self.store_factory
            .pin(node_id.as_ref())
            .await
            .map_err(Into::into)
    }

    /// Release an explicit continuation pin. A live tip at the same node
    /// remains forkable through its session-head checkpoint.
    pub async fn unpin(&self, node_id: impl AsRef<str>) -> Result<()> {
        self.store_factory
            .unpin(node_id.as_ref())
            .await
            .map_err(Into::into)
    }

    /// Enumerate pinned past turns and unpinned live tips that can be forked.
    pub async fn fork_points(&self) -> Result<Vec<lash_core::ForkPoint>> {
        self.store_factory.fork_points().await.map_err(Into::into)
    }

    /// Create `session_id` at a retained turn boundary without writing graph
    /// nodes.
    ///
    /// Unpinned past turns are ordinarily not retained. That normal outcome is
    /// returned as
    /// `EmbedError::Store(StoreError::ForkPointNotRetained { .. })`; Lash never
    /// silently substitutes a different checkpoint. An explicit pin remains
    /// forkable after its source session is deleted because the retained frame
    /// carries the provider and model needed to create the branch.
    pub async fn fork_at(&self, request: ForkRequest) -> Result<lash_core::ForkSessionReceipt> {
        let store_factory = &self.store_factory;
        let ForkRequest {
            session_id,
            node_id,
            relation,
            observed_processes,
        } = request;
        let point = store_factory
            .fork_points()
            .await?
            .into_iter()
            .find(|point| point.node_id == node_id)
            .ok_or_else(|| lash_core::StoreError::ForkPointNotRetained {
                node_id: node_id.clone(),
            })?;
        let mut fork_policy = self.policy.clone();
        fork_policy.provider_id = point.config.provider_id;
        fork_policy.model = point.config.model;
        let mut selected = std::collections::HashSet::new();
        let mut pending_observer_intents = Vec::new();
        for process_id in observed_processes {
            if !selected.insert(process_id.clone()) {
                continue;
            }
            pending_observer_intents.push(facade_support::SessionObserverIntent::host_requested(
                process_id,
            ));
        }
        let request = lash_core::ForkSessionRequest {
            session_id,
            node_id,
            relation,
            pending_observer_intents,
            policy: fork_policy,
        };
        let mut fork = store_factory.fork_at(&request).await?;
        let create_request = lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            session_id: request.session_id,
            relation: request.relation,
            pending_observer_intents: request.pending_observer_intents,
            policy: request.policy,
        };
        let branch_store = store_factory
            .open_existing_store(&create_request)
            .await
            .map_err(|error| {
                lash_core::StoreError::Backend(format!(
                    "failed to reopen fork store `{}`: {error}",
                    create_request.session_id
                ))
            })?
            .ok_or_else(|| {
                lash_core::StoreError::Backend(format!(
                    "fork store `{}` disappeared before observer publication completed",
                    create_request.session_id
                ))
            })?;
        fork.observed_processes = lash_core::runtime::reconcile_session_process_observer_intents(
            Some(self.process_registry.as_ref()),
            &fork.session_id,
            lash_core::runtime::SessionObserverIntentSource::Persisted(branch_store.as_ref()),
        )
        .await?;
        Ok(fork)
    }

    /// Delete a session in two phases (ADR 0109 §4).
    ///
    /// The close commits first, and every refusal of a deletion is asked
    /// before it: the session's `CloseSession` intent is recorded, the
    /// session is marked closing and refuses new sends with
    /// [`StoreError::SessionClosing`](lash_core::StoreError::SessionClosing),
    /// and the intent's engine half releases the session's roots and closes
    /// its scopes. Its acknowledgement arms the session's physical delete as
    /// an obligation, which this call attempts before it returns.
    ///
    /// The physical delete waits for the close's cleanup — each root's scope
    /// close, each owned scope's parent-end plan — to be delivered, and for
    /// the engine to finish the session's work (on Restate, a released root
    /// often still is, so the delete defers to the reconcile tick). What this
    /// call could not finish is [`SessionDeletion::Closing`]: the session
    /// stays closed and the recovery relay retries the delete with backoff,
    /// stalling it (surfaced in [`drain_status`](Self::drain_status) and
    /// [`stalled_obligations`](Self::stalled_obligations)) at the attempt
    /// ceiling. The caller does not retry to finish a deletion.
    pub async fn delete_session(
        context: lash_core::SessionDeleteContext<'_>,
    ) -> Result<SessionDeletion> {
        lash_core::session_delete::delete_session(&context)
            .await
            .map_err(|error| match error {
                lash_core::session_delete::SessionDeleteError::Close(
                    lash_core::session_close::SessionCloseError::Store(error),
                )
                | lash_core::session_delete::SessionDeleteError::Store(error) => {
                    EmbedError::from(error)
                }
                lash_core::session_delete::SessionDeleteError::Close(
                    lash_core::session_close::SessionCloseError::Runtime(error),
                ) => EmbedError::from(error),
                lash_core::session_delete::SessionDeleteError::Unrecorded {
                    session_id,
                    failure: lash_core::session_delete::SessionDeleteFailure::Storage(failure),
                } => EmbedError::SessionDeleteStorage {
                    session_id,
                    failure,
                },
                lash_core::session_delete::SessionDeleteError::Unrecorded {
                    session_id,
                    failure,
                } => EmbedError::SessionDeleteProcess {
                    session_id,
                    message: failure.to_string(),
                },
            })
    }

    /// The process registry of this core's backend.
    pub fn process_registry(&self) -> Arc<dyn ProcessRegistry> {
        Arc::clone(&self.process_registry)
    }

    /// Builds the durable process-worker configuration for this core.
    pub fn durable_process_worker_config(&self) -> Result<DurableProcessWorkerConfig> {
        self.durable_process_worker_config_with_plugins(std::iter::empty::<Arc<dyn PluginFactory>>())
    }

    /// Builds the durable process-worker configuration with additional plugins.
    pub fn durable_process_worker_config_with_plugins(
        &self,
        extra_plugin_factories: impl IntoIterator<Item = Arc<dyn PluginFactory>>,
    ) -> Result<DurableProcessWorkerConfig> {
        let extra_plugin_factories: Vec<_> = extra_plugin_factories.into_iter().collect();
        refuse_foreign_backend_factories(&self.backend, &extra_plugin_factories)?;
        let plugin_host = build_plugin_host(
            self.protocol_factory.as_ref(),
            self.plugin_factories.as_ref(),
            extra_plugin_factories,
        )?;
        let runtime_host = plugin_host.install_process_engine_contributions(
            self.env.core.clone(),
            self.process_lifecycle_available,
        )?;
        Ok(DurableProcessWorkerConfig::new(
            Arc::new(plugin_host),
            runtime_host,
            self.substrate_slot.setup.process.clone(),
            Arc::clone(&self.substrate_slot.setup.session_work),
            self.drive_owner.clone(),
        )
        .with_session_policy(self.policy.clone()))
    }
}

/// Builder for configuring lash core over one [`Backend`].
pub struct LashCoreBuilder {
    pub(crate) protocol_factory: Option<Arc<dyn PluginFactory>>,
    session_spec: SessionSpec,
    provider: Option<ProviderHandle>,
    /// The run definitions sent inputs' specs may name (FIG-3838).
    run_definitions: lash_core::RunDefinitions,
    /// The one substrate every persistence port and the effect host come from.
    backend: Backend,
    commit_budget: Option<facade_support::CommitBudget>,
    queued_work_batching: Option<facade_support::QueuedWorkBatchingConfig>,
    max_attachment_bytes: Option<Option<u64>>,
    process_wake_delivery_policy: Option<lash_core::DeliveryPolicy>,
    // Core fields applied over the config the backend's ports assemble.
    prompt: Option<PromptLayer>,
    trace_sink: Option<Arc<dyn lash_trace::TraceSink>>,
    trace_level: Option<lash_trace::TraceLevel>,
    trace_context: Option<lash_trace::TraceContext>,
    termination: Option<TerminationPolicy>,
    tool_source_policy: Option<lash_core::ToolSourcePolicy>,
    abort_drain_grace: Option<std::time::Duration>,
    tool_providers: Vec<Arc<dyn ToolProvider>>,
    plugin_stack: PluginStack,
    plugin_host: Option<PluginHost>,
    recovery_lease: Option<lash_core::engine::RecoveryLeaseConfig>,
    process_tool_visibility_filter: Option<Arc<dyn facade_support::ProcessToolVisibilityFilter>>,
    live_replay_store: Option<Arc<dyn LiveReplayStore>>,
    process_observation_config: crate::process_observation::ProcessObservationConfig,
}

impl LashCoreBuilder {
    fn new(backend: Backend, turn_budget: lash_core::TurnBudget) -> Self {
        Self {
            protocol_factory: None,
            session_spec: SessionSpec::new().turn_budget(turn_budget),
            provider: None,
            run_definitions: lash_core::RunDefinitions::default(),
            backend,
            commit_budget: None,
            queued_work_batching: None,
            max_attachment_bytes: None,
            process_wake_delivery_policy: None,
            prompt: None,
            trace_sink: None,
            trace_level: None,
            trace_context: None,
            termination: None,
            tool_source_policy: None,
            abort_drain_grace: None,
            tool_providers: Vec::new(),
            plugin_stack: PluginStack::default(),
            plugin_host: None,
            recovery_lease: None,
            process_tool_visibility_filter: None,
            live_replay_store: None,
            process_observation_config: Default::default(),
        }
    }

    pub fn protocol_plugin(mut self, plugin: Arc<dyn PluginFactory>) -> Self {
        self.protocol_factory = Some(plugin);
        self
    }

    /// Register a run definition a sent input's [`RunSpec`](crate::RunSpec)
    /// may name by its exact [`DefinitionRef`](crate::DefinitionRef). A root
    /// whose spec names a revision this deployment does not register retries
    /// and parks, and recovers once a deployment registers it; no other
    /// revision is ever used in its place.
    pub fn run_definition(mut self, definition: impl crate::RunDefinition + 'static) -> Self {
        self.run_definitions.register(Arc::new(definition));
        self
    }

    /// Configures the provider and returns the updated builder.
    pub fn provider(mut self, provider: ProviderHandle) -> Self {
        self.session_spec = self.session_spec.provider_id(provider.kind());
        self.provider = Some(provider);
        self
    }

    /// Configure the byte and graph-node limits for each atomic runtime
    /// commit. Hosts must choose bounded or unbounded behavior explicitly for
    /// both dimensions.
    pub fn commit_budget(mut self, commit_budget: facade_support::CommitBudget) -> Self {
        self.commit_budget = Some(commit_budget);
        self
    }

    /// The default `None` preserves unbounded attachment puts. `Some(max_bytes)`
    /// rejects larger puts before the configured attachment backend is called.
    /// This deployment limit is independent from [`Self::commit_budget`].
    pub fn max_attachment_bytes(mut self, max_attachment_bytes: Option<u64>) -> Self {
        self.max_attachment_bytes = Some(max_attachment_bytes);
        self
    }

    /// Configure queued-work batching with a required model-action reserve.
    /// Row-count and pending-age bounds default inside the supplied value and
    /// may be overridden by the host.
    pub fn queued_work_batching(
        mut self,
        policy: facade_support::QueuedWorkBatchingConfig,
    ) -> Self {
        self.queued_work_batching = Some(policy);
        self
    }

    pub fn process_wake_delivery_policy(mut self, policy: lash_core::DeliveryPolicy) -> Self {
        self.process_wake_delivery_policy = Some(policy);
        self
    }

    pub fn process_tool_visibility_filter(
        mut self,
        filter: Arc<dyn facade_support::ProcessToolVisibilityFilter>,
    ) -> Self {
        self.process_tool_visibility_filter = Some(filter);
        self
    }

    pub fn tools(mut self, tools: Arc<dyn ToolProvider>) -> Self {
        self.tool_providers.push(tools);
        self
    }

    pub fn plugin(mut self, plugin: Arc<dyn PluginFactory>) -> Self {
        self.plugin_stack.push(plugin);
        self
    }

    pub fn plugins(mut self, stack: PluginStack) -> Self {
        self.plugin_stack = stack;
        self
    }

    pub fn configure_plugins(mut self, configure: impl FnOnce(&mut PluginStack)) -> Self {
        configure(&mut self.plugin_stack);
        self
    }

    pub fn trace_sink(mut self, trace_sink: Arc<dyn lash_trace::TraceSink>) -> Self {
        self.trace_sink = Some(trace_sink);
        self
    }

    pub fn trace_jsonl_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.trace_sink = Some(Arc::new(lash_trace::JsonlTraceSink::new(path.into())));
        self
    }

    pub fn trace_level(mut self, trace_level: lash_trace::TraceLevel) -> Self {
        self.trace_level = Some(trace_level);
        self
    }

    pub fn trace_context(mut self, trace_context: lash_trace::TraceContext) -> Self {
        self.trace_context = Some(trace_context);
        self
    }

    pub fn termination(mut self, termination: TerminationPolicy) -> Self {
        self.termination = Some(termination);
        self
    }

    /// The default is [`ToolSourcePolicy::Tolerate`](lash_core::ToolSourcePolicy::Tolerate):
    /// the session opens and the host receives a typed
    /// [`ToolRestoreReport`](crate::support::ToolRestoreReport), because
    /// locking a user out of a conversation is worse than degrading it.
    /// Unattended and fixed-tool deployments set
    /// [`Require`](lash_core::ToolSourcePolicy::Require), which refuses an open
    /// whose report has lost members — a persisted Tool Catalog member no
    /// registered source resolves. Parked opt-outs and superseded identities
    /// never refuse.
    ///
    /// The choice is carried on the core's host config, so runtime-initiated
    /// constructions (process-spawned children, the queued-work driver, resume) honour
    /// it too. One open may override it with
    /// [`SessionBuilder::tool_source_policy`](crate::SessionBuilder::tool_source_policy).
    pub fn tool_source_policy(mut self, policy: lash_core::ToolSourcePolicy) -> Self {
        self.tool_source_policy = Some(policy);
        self
    }

    /// Bound how long a protocol-owned stream abort (an RLM cell boundary
    /// ending the model's turn) keeps draining the provider stream before the
    /// provider task is aborted. The drain lets a cooperative provider's
    /// trailing usage event land on the aborted attempt; past the grace the
    /// attempt is sealed with a typed unreported usage disposition and the
    /// turn's usage ledger records the hole. Defaults to 2 seconds.
    pub fn abort_drain_grace(mut self, grace: std::time::Duration) -> Self {
        self.abort_drain_grace = Some(grace);
        self
    }

    /// Configure how this deployment competes for the recovery leader lease
    /// (ADR 0109 §1.6): its build rank — a higher rank preempts a
    /// lower-ranked leader after the minimum tenure, so a rolling deploy that
    /// raises the rank per build hands recovery to the newest build — and the
    /// lease's cadence. Defaults to rank 0 on a 15 s TTL renewed every 5 s.
    pub fn recovery_lease(mut self, config: lash_core::engine::RecoveryLeaseConfig) -> Self {
        self.recovery_lease = Some(config);
        self
    }

    /// Configure the bounded live replay buffer used by session observation
    /// cursors. This is best-effort reconnect recovery only; durable state
    /// still comes from the session store and [`SessionReadView`].
    pub fn live_replay_store(mut self, live_replay_store: Arc<dyn LiveReplayStore>) -> Self {
        self.live_replay_store = Some(live_replay_store);
        self
    }

    /// Build a core under the host's stable worker identity.
    ///
    /// The owner id is stable for the worker or process and never scoped to a
    /// turn. The incarnation id changes once per process boot.
    pub fn build(mut self, drive_owner: lash_core::LeaseOwnerIdentity) -> Result<LashCore> {
        let protocol_factory = self.protocol_factory.clone();
        if protocol_factory.is_none() && self.plugin_host.is_none() {
            return Err(EmbedError::MissingProtocolPlugin);
        }
        let provider_id = self
            .session_spec
            .provider_id
            .clone()
            .or_else(|| {
                self.provider
                    .as_ref()
                    .map(|provider| provider.kind().to_string())
            })
            .unwrap_or_default();
        let model = self
            .session_spec
            .model
            .clone()
            .ok_or(EmbedError::MissingModelSpec)?;
        let turn_budget = self
            .session_spec
            .turn_budget
            .ok_or(EmbedError::MissingTurnBudget)?;
        let base_policy = SessionPolicy {
            provider_id,
            model,
            ..SessionPolicy::new(turn_budget)
        };
        let policy = self.session_spec.resolve_against(&base_policy);

        let backend = self.backend.clone();
        let store_factory = backend.session_store_factory();
        let core = self.resolve_runtime_host_config()?;
        let process_observation_hub = Arc::new(
            crate::process_observation::ProcessObservationHub::new(self.process_observation_config),
        );
        let observation_sink: Arc<dyn lash_trace::TraceSink> = process_observation_hub.clone();
        let core = core.with_process_observation_sink(observation_sink);
        let live_replay_store = self.live_replay_store.take().unwrap_or_else(|| {
            Arc::new(InMemoryLiveReplayStore::with_clock(
                facade_support::InMemoryLiveReplayStoreConfig::default(),
                Arc::clone(&core.clock),
            ))
        });
        let process_work = backend.process_work();
        let process_lifecycle_feed = Arc::new(crate::process_lifecycle::ProcessLifecycleFeed::new(
            Arc::clone(&live_replay_store),
            Arc::clone(&process_observation_hub),
        ));
        let process_event_sink: Arc<dyn facade_support::ProcessEventSink> =
            process_lifecycle_feed.clone();
        let process_lifecycle_registration = Some(Arc::new(
            process_work
                .watched()
                .add_event_sink(Arc::clone(&process_event_sink)),
        ));
        let plugin_factories = if let Some(plugin_host) = self.plugin_host {
            plugin_host.factories().to_vec()
        } else {
            let mut factories = Vec::new();
            if !self.tool_providers.is_empty() {
                let spec = self
                    .tool_providers
                    .into_iter()
                    .fold(PluginSpec::new(), PluginSpec::with_tool_provider);
                factories.push(Arc::new(StaticPluginFactory::new("embed_tools", spec))
                    as Arc<dyn PluginFactory>);
            }
            factories.extend(self.plugin_stack.into_factories());
            factories
        };
        refuse_foreign_backend_factories(
            &backend,
            protocol_factory.iter().chain(plugin_factories.iter()),
        )?;
        let default_plugin_host = Arc::new(build_plugin_host(
            protocol_factory.as_ref(),
            &plugin_factories,
            Vec::new(),
        )?);
        // Every backend supplies a process registry, so process lifecycle
        // is available on every core. Threaded to every plugin host so core
        // installs the same plugin-contributed process engines wherever it
        // rebuilds a runtime.
        let process_lifecycle_available = true;
        // Session construction still installs onto a clean clone so session-scoped plugin
        // overlays remain isolated.
        let host_process_engines = default_plugin_host
            .install_process_engine_contributions(core.clone(), process_lifecycle_available)?
            .process_engines;
        let tool_registry =
            lash_core::facade_support::build_core_tool_registry(&default_plugin_host)?;
        let process_registry = Arc::clone(process_work.registry());
        process_lifecycle_feed.bind_registry(Arc::clone(&process_registry));
        let env = RuntimeEnvironment::builder(core)
            .with_plugin_host(Arc::clone(&default_plugin_host))
            .with_process_work(process_work.clone())
            .build();
        // Registration owns the scope fence (ADR 0049): the registry lifts the
        // effect host's fence for a re-registered process id inside its own
        // registration write, on every registration path.
        process_registry.bind_effect_host(&env.core.control.effect_host);
        // The retained-evidence sweep owns deferred scope retirement (ADR
        // 0067): the catalog learns the host whose journal it sweeps.
        store_factory.bind_effect_host(&env.core.control.effect_host);
        let residents = Arc::new(residents::ResidentSessions::default());
        let session_work = backend.session_work();
        let (session_driver, installed_driver) = Self::build_session_driver(
            &session_work,
            Arc::clone(&residents),
            drive_owner.clone(),
            env.clone(),
            policy.clone(),
            protocol_factory.clone(),
            Arc::new(plugin_factories.clone()),
            &store_factory,
            Arc::clone(&live_replay_store),
            process_lifecycle_available,
            self.recovery_lease.unwrap_or_default(),
        );
        // The driver's reconcile tick runs every obligation kind's relay
        // (ADR 0109 §1.4): the backend's process wiring always supplies a
        // process port, and the driver administers through the slot bound
        // below.
        lash_core::drive::RelaySupply {
            process_work: true,
            session_administration: true,
        }
        .check()?;
        let substrate = CoreWorkSetup {
            process: process_work,
            session_work,
            store_binding: backend.binding_identity(),
            wake: WakeDeliveryDriverSetup {
                registry: Arc::clone(&process_registry),
                factory: Arc::clone(&store_factory),
                clock: Arc::clone(&env.core.clock),
                delivery_policy: env.core.control.process_wake_delivery_policy,
            },
        };

        let substrate_slot = Arc::new(CoreWorkSlot::new(substrate));
        // The driver is built before the slot it reconciles through, so the
        // binding lands here: its recovery pass asks the resolved work port.
        session_driver.bind_substrate_slot(Arc::downgrade(&substrate_slot));
        // The reconcile tick's session-delete relay administers through the
        // same source the core does (ADR 0109 §4).
        session_driver.bind_administration(AdministrationSource {
            slot: Arc::downgrade(&substrate_slot),
            env: env.clone(),
            store_factory: Arc::clone(&store_factory),
            host_process_engines: host_process_engines.clone(),
        });
        let plugin_factories = Arc::new(plugin_factories);
        let tool_child_context_source = tool_child_context::CoreToolChildContextSource::install(
            &env,
            protocol_factory.clone(),
            Arc::clone(&plugin_factories),
            self.provider.clone(),
            process_lifecycle_available,
            {
                let substrate_slot = Arc::clone(&substrate_slot);
                Arc::new(move || {
                    let substrate_slot = Arc::clone(&substrate_slot);
                    Box::pin(async move {
                        let ports = substrate_slot.ports().await;
                        (Some(ports.process.clone()), ports.queued_port())
                    }) as futures_util::future::BoxFuture<'static, _>
                })
            },
            drive_owner.clone(),
        );
        Ok(LashCore {
            drive_owner,
            env,
            tool_registry,
            policy,
            backend,
            store_factory,
            process_registry,
            plugin_factories,
            provider: self.provider,
            live_replay_store,
            process_observation_hub,
            process_lifecycle_feed,
            _process_lifecycle_registration: process_lifecycle_registration,
            protocol_factory,
            process_lifecycle_available,
            host_process_engines,
            substrate_slot,
            _session_driver: installed_driver,
            recovery: session_driver.recovery(),
            residents,
            tool_intent_submission_gates: Default::default(),
            tool_child_context_source,
        })
    }

    /// The core's session driver (FIG-3600), installed on the backend's
    /// session-work engine. Returns the driver and the one the engine kept.
    #[allow(clippy::too_many_arguments)]
    fn build_session_driver(
        session_work: &Arc<dyn SessionWorkEngine>,
        residents: Arc<residents::ResidentSessions>,
        drive_owner: lash_core::LeaseOwnerIdentity,
        env: RuntimeEnvironment,
        policy: SessionPolicy,
        protocol_factory: Option<Arc<dyn PluginFactory>>,
        plugin_factories: Arc<Vec<Arc<dyn PluginFactory>>>,
        store_factory: &Arc<dyn SessionStoreFactory>,
        live_replay_store: Arc<dyn LiveReplayStore>,
        process_lifecycle_available: bool,
        recovery_lease: lash_core::engine::RecoveryLeaseConfig,
    ) -> (Arc<CoreSessionDriver>, Arc<dyn lash_core::SessionDriver>) {
        let owner = drive_owner.clone();
        let recovery = Arc::new(recovery::RecoverySlot::new(&env, recovery_lease));
        let driver = Arc::new(CoreSessionDriver::new(Arc::new(CoreSessionDriverConfig {
            recovery,
            residents,
            drive_owner,
            env,
            policy,
            protocol_factory,
            plugin_factories,
            store_factory: Arc::clone(store_factory),
            live_replay_store,
            process_lifecycle_available,
        })));
        let installed = install_session_driver(session_work, driver.clone(), &owner);
        (driver, installed)
    }

    pub fn advanced(self) -> AdvancedLashCoreBuilder {
        AdvancedLashCoreBuilder { builder: self }
    }

    /// Bounds of the process observation hub: its per-process ring capacity
    /// and idle TTL, and the durable read budget of one snapshot.
    pub fn process_observation_config(
        mut self,
        config: crate::process_observation::ProcessObservationConfig,
    ) -> Self {
        self.process_observation_config = config;
        self
    }
}

/// Refuses a plugin factory bound to a backend other than `backend`
/// ([`PluginFactory::bound_backend`]): its state would live in a substrate
/// this core neither reopens nor sweeps (ADR 0102, D2).
pub(crate) fn refuse_foreign_backend_factories<'a>(
    backend: &Backend,
    factories: impl IntoIterator<Item = &'a Arc<dyn PluginFactory>>,
) -> Result<()> {
    for factory in factories {
        if let Some(bound) = factory.bound_backend()
            && bound != backend.binding_identity().as_str()
        {
            return Err(EmbedError::PluginBackendMismatch {
                plugin_id: factory.id().to_string(),
                plugin_backend: bound.to_string(),
                backend: backend.binding_identity().to_string(),
            });
        }
    }
    Ok(())
}

pub(crate) fn build_plugin_host(
    protocol_factory: Option<&Arc<dyn PluginFactory>>,
    common_factories: &[Arc<dyn PluginFactory>],
    extra_factories: Vec<Arc<dyn PluginFactory>>,
) -> Result<PluginHost> {
    let mut factories = Vec::with_capacity(
        usize::from(protocol_factory.is_some()) + common_factories.len() + extra_factories.len(),
    );
    if let Some(protocol_factory) = protocol_factory {
        factories.push(Arc::clone(protocol_factory));
    }
    factories.extend(common_factories.iter().cloned());
    factories.extend(extra_factories);
    Ok(PluginHost::new(factories))
}

impl PromptLayerSink for LashCoreBuilder {
    fn prompt_layer_mut(&mut self) -> &mut PromptLayer {
        self.prompt.get_or_insert_with(PromptLayer::new)
    }
}

impl LashCore {
    /// The live-replay cursor a reader should attach at after taking a durable
    /// snapshot of `session_id` at `revision`.
    ///
    /// A snapshot reader needs a cursor, and the only honest one names the
    /// replay incarnation that will serve it: a cursor naming any other
    /// incarnation is fenced at attach and answered with
    /// `replay_gap(unavailable)`, which sends the reader back for another
    /// snapshot, and round it goes (FIG-3162). This query reads the live-replay
    /// store only. Like [`Self::sessions`], it never opens a session or
    /// hydrates a checkpoint, so a host can pair it with a durable read.
    pub fn observation_cursor(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.live_replay_store.current_cursor(session_id, revision)
    }

    /// Enumerate every durable session catalog entry.
    ///
    /// This is a read-only catalog query. It does not open sessions, hydrate
    /// checkpoints, or mutate catalog generations.
    /// Results are ordered by creation time and then session id, and include
    /// permanent deletion tombstones.
    pub async fn sessions(&self) -> Result<Vec<SessionSummary>> {
        self.sessions_filtered(SessionListFilter::default()).await
    }

    /// Enumerate durable session catalog entries matching `filter`.
    ///
    /// Like [`Self::sessions`], this query never opens a session or acquires
    /// execution authority.
    pub async fn sessions_filtered(
        &self,
        filter: SessionListFilter,
    ) -> Result<Vec<SessionSummary>> {
        self.store_factory
            .list_sessions(&filter)
            .await
            .map_err(Into::into)
    }
}

/// Explicit host selection for a retained-history fork.
///
/// Lineage is independent of the history node's writer. Observers are the exact
/// runs the host selected; an empty list creates a history-only fork.
#[derive(Clone, Debug)]
pub struct ForkRequest {
    pub session_id: SessionId,
    pub node_id: lash_core::NodeId,
    pub relation: lash_core::SessionRelation,
    pub observed_processes: Vec<lash_core::ProcessId>,
}

/// Install `driver` on `port` for the core that `owner` names, and return the
/// driver the engine serves.
///
/// One engine serves one driver (get-or-init), so a second core over the same
/// backend does not drive its own sessions: its plugins, protocol and policy
/// are not the ones that run them. That is reported, naming the core whose
/// driver is ignored (#2290 review, LOW-14).
fn install_session_driver(
    port: &Arc<dyn lash_core::SessionWorkEngine>,
    driver: Arc<dyn lash_core::SessionDriver>,
    owner: &lash_core::LeaseOwnerIdentity,
) -> Arc<dyn lash_core::SessionDriver> {
    let installed = port.install_session_driver(Arc::clone(&driver));
    if !installed.runs_on(driver.as_ref()) {
        tracing::warn!(
            event = "session_driver.install_ignored",
            owner_id = %owner.owner_id,
            incarnation_id = %owner.incarnation_id,
            "the backend's session-work engine already serves another core's session driver; \
             this core's sessions are driven by that core's plugins, protocol and policy"
        );
    }
    installed
}
