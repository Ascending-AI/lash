use crate::support::{
    Arc, EffectHost, EmbedError, InMemoryLiveReplayStore, LashRuntime, LashSession,
    LiveReplayStore, NativeQueuedWork, NativeSubstrateConfig, NoSessionWork, ParkedSession,
    PluginFactory, PluginHost, PluginOptions, PluginSpec, PluginStack, ProcessRegistry,
    PromptLayer, PromptLayerSink, ProviderHandle, Result, RuntimeEnvironment, RuntimeHandle,
    RuntimeHostConfig, SessionBuilder, SessionListFilter, SessionPolicy, SessionSpec,
    SessionStoreFactory, SessionSummary, SessionWorkEngine, StaticPluginFactory, TerminationPolicy,
    ToolProvider, WorkerSlotSupplier,
};
use lash_core::Backend;
use lash_core::facade_support;
use lash_core::runtime::{
    ProcessCommand, ProcessEffectOutcome, RuntimeEffectCommand, RuntimeEffectEnvelope,
    RuntimeEffectInvocation, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
};
use lash_core_worker::{DurableProcessWorkerConfig, WorkerProcessWork};
use lash_sansio::SessionId;

mod advanced_builder;
mod drain;
pub(crate) mod queued_work;
pub(crate) mod residents;
mod runtime_host_config;
mod session_policy;
mod tool_child_context;
mod work_drivers;
mod worker_capacity;

pub use advanced_builder::AdvancedLashCoreBuilder;
pub use drain::DeploymentDrainStatus;
use queued_work::{NativeQueuedWorkRunConfig, NativeQueuedWorkRunHandle};
pub(crate) use work_drivers::HeldWork;
use work_drivers::{
    DriveLifetime, NativeSubstrateSetup, NativeSubstrateSlot, ProcessPortSetup,
    ProcessWorkSelection, ProcessWorkSource, QueuedPortSetup, QueuedWorkSource,
    WakeDeliveryDriverSetup,
};
#[derive(Clone)]
/// Owns the configured runtime services used to create and resume Lash sessions.
pub struct LashCore {
    pub(crate) session_execution_owner: lash_core::LeaseOwnerIdentity,
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
    pub(crate) process_execution_concurrency: usize,
    /// Explicit host supplier; `None` preserves a fresh bound per process worker.
    pub(crate) worker_slot_supplier: Option<Arc<dyn WorkerSlotSupplier>>,
    /// Shared across core clones so native substrate ports are constructed at most once.
    pub(crate) substrate_slot: Arc<NativeSubstrateSlot>,
    /// Held by every core clone and every session, durable session and send
    /// handle: its drop stops the core's in-process drives.
    pub(crate) drive_lifetime: Arc<DriveLifetime>,
    /// The session driver this core installed on its backend's session-work
    /// engine (FIG-3600). The engine may hold it weakly, so the core keeps it
    /// for its whole life.
    pub(crate) _session_driver: Arc<dyn lash_core::SessionDriver>,
    /// The sessions this core has open in this process: the driver runs a
    /// drive on the open session's runtime (FIG-3600 S5b).
    pub(crate) residents: Arc<residents::ResidentSessions>,
    /// Host-facing process event sink, retained so a worker config built from
    /// this core reports its worker faults to the same sink the registry
    /// decorator emits events on.
    pub(crate) process_event_sink: Option<Arc<dyn facade_support::ProcessEventSink>>,
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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionDeleteReport {
    /// Identifier of the deleted session.
    pub session_id: SessionId,
    /// Storage reclaimed while deleting the session.
    pub storage: lash_core::SessionBlobReclaimReport,
    /// Process-state deletion report.
    pub process: Option<lash_core::ProcessSessionDeleteReport>,
}

impl Default for SessionDeleteReport {
    fn default() -> Self {
        Self {
            session_id: SessionId::from(String::default()),
            storage: lash_core::SessionBlobReclaimReport::default(),
            process: None,
        }
    }
}

impl LashCore {
    /// The core's queued work as a host-held handle carries it: holding it
    /// keeps the core's in-process drives running.
    pub(crate) async fn held_work(&self) -> HeldWork {
        HeldWork::new(
            Arc::clone(&self.substrate_slot.ports().await.queued),
            Arc::clone(&self.drive_lifetime),
        )
    }

    /// A [`LashCoreBuilder`] over `backend`, the one substrate every
    /// persistence port and the effect host of this core come from (ADR 0102).
    ///
    /// The backend is the builder's only source of ports: there is no
    /// setter for a store, a registry or an effect host, so a core cannot mix
    /// substrates, and there is no in-memory default. The zero-infra
    /// backend is `lash::sqlite::SqliteBackend::memory()` (feature
    /// `sqlite`).
    pub fn builder(backend: Backend, turn_budget: lash_core::TurnBudget) -> LashCoreBuilder {
        LashCoreBuilder::new(backend, turn_budget)
    }

    /// Sugar entry point: a [`LashCoreBuilder`] over `backend` pre-seeded
    /// with the standard protocol plugin and the default runtime plugin
    /// stack.
    pub fn standard_builder(
        backend: Backend,
        turn_budget: lash_core::TurnBudget,
    ) -> LashCoreBuilder {
        LashCore::builder(backend, turn_budget)
            .protocol_plugin(Arc::new(
                lash_protocol_standard::StandardProtocolPluginFactory::new(),
            ))
            .plugins(default_runtime_stack())
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
        Ok(DeploymentDrainStatus {
            accepting_new_work,
            remaining_invocations,
            in_flight_turns: turns.in_flight_turns,
            parked_turns: turns.parked_turns,
            parked_processes: parked.processes.total(),
            oldest_parked_since_ms: parked.oldest_since_ms(),
            retired_by_executable_generation: parked.retired_by_executable_generation(),
            checked_at,
        })
    }

    /// The deployment's parked work — turns and processes whose redrive
    /// refuses to replay their journals — to list, summarize and follow
    /// (FIG-3659).
    pub fn parked_work(&self) -> crate::parked_work::ParkedWork {
        crate::parked_work::ParkedWork {
            work: self.env.queued_work(),
            scopes: Arc::clone(&self.env.core.control.scope_close),
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
        LashCore::builder(backend, turn_budget)
            .protocol_plugin(Arc::new(factory))
            .plugins(default_runtime_stack())
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
        let ports = self.substrate_slot.ports().await;
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
                clock: Arc::clone(&resolved_env.core.clock),
            },
        )
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
        let runtime =
            LashRuntime::resume(inner, &env, self.session_execution_owner.clone()).await?;
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
            process_phase_probe_slot: self.substrate_slot.phase_probe_slot(),
            turn_cancels: crate::turn::TurnCancelRegistry::default(),
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

    pub async fn delete_session(
        context: lash_core::SessionDeleteContext<'_>,
    ) -> Result<SessionDeleteReport> {
        let session_id = context.session_id().clone();
        let administration = context.administration();
        // Every refusal of a deletion is asked inside the close, before its
        // recorded step, the point of no return (FIG-3600 S7, FIG-3607 item
        // 7): the close ends the session's roots and stops it accepting and
        // admitting, and its engine half releases the roots and closes the
        // session's scopes, or is retained for reconciliation. From here the
        // deletion only retries: nothing below refuses.
        lash_core::session_close::close_session(&context)
            .await
            .map_err(|error| match error {
                lash_core::session_close::SessionCloseError::Store(error) => {
                    EmbedError::from(error)
                }
                lash_core::session_close::SessionCloseError::Runtime(error) => {
                    EmbedError::from(error)
                }
            })?;
        let process = if let Some(process) = administration.process() {
            #[expect(
                clippy::expect_used,
                reason = "the scope comes from the session's own live controller, which \
                          is admitted by construction"
            )]
            let invocation = RuntimeEffectInvocation::new(
                lash_core::EffectAddress::new(
                    lash_core::facade_support::ScopedEffectControllerFacadeOps::execution_scope(
                        context.controller(),
                    )
                    .clone(),
                    format!("{session_id}:delete-session"),
                )
                .expect(
                    "the scope comes from the session's own live controller, which is \
                     admitted by construction",
                ),
                lash_core::RuntimeAttribution::for_session(session_id.clone()),
                format!("process:delete-session:{session_id}"),
            );
            let outcome = context
                .controller()
                .controller()
                .execute_effect(
                    RuntimeEffectEnvelope::new(
                        invocation,
                        RuntimeEffectCommand::process(ProcessCommand::DeleteSession {
                            session_id: session_id.clone(),
                        }),
                    ),
                    RuntimeEffectLocalExecutor::processes(
                        Arc::clone(process.registry()),
                        Arc::clone(process.port()),
                    ),
                )
                .await
                .map_err(|err| EmbedError::SessionDeleteProcess {
                    session_id: session_id.clone(),
                    message: err.to_string(),
                })?;
            match outcome {
                RuntimeEffectOutcome::Process {
                    result: ProcessEffectOutcome::DeleteSession { report },
                } => Some(report),
                other => {
                    return Err(EmbedError::SessionDeleteProcess {
                        session_id,
                        message: format!(
                            "process delete returned the wrong outcome: {}",
                            other.kind().as_str()
                        ),
                    });
                }
            }
        } else {
            None
        };
        if let Some(trigger_store) = administration.trigger_store() {
            trigger_store
                .delete_session_subscriptions(&session_id)
                .await
                .map_err(|err| EmbedError::SessionDeleteProcess {
                    session_id: session_id.clone(),
                    message: err.to_string(),
                })?;
        }
        administration
            .effect_host()
            .revoke_await_events_for_session(&session_id)
            .await
            .map_err(|err| EmbedError::SessionDeleteProcess {
                session_id: session_id.clone(),
                message: err.to_string(),
            })?;
        let storage = administration
            .store_factory()
            .delete_session(&session_id)
            .await
            .map_err(|failure| EmbedError::SessionDeleteStorage {
                session_id: session_id.clone(),
                failure: Box::new(failure),
            })?;
        administration
            .effect_host()
            .retire_effect_journal(lash_core::EffectJournalRetirement::session(&session_id))
            .await
            .map_err(|err| EmbedError::SessionDeleteProcess {
                session_id: session_id.clone(),
                message: err.to_string(),
            })?;
        for scope in administration
            .effect_host()
            .pending_artifact_owner_retirements()
            .await
            .map_err(|err| EmbedError::SessionDeleteProcess {
                session_id: session_id.clone(),
                message: err.to_string(),
            })?
        {
            let owner = lash_core::ArtifactOwner::execution(scope.clone());
            administration
                .process_env_store()
                .retire_process_execution_env_owner(&owner)
                .await
                .map_err(|err| EmbedError::SessionDeleteProcess {
                    session_id: session_id.clone(),
                    message: err.to_string(),
                })?;
            administration
                .process_engines()
                .retire_artifact_owner(&owner)
                .await
                .map_err(|err| EmbedError::SessionDeleteProcess {
                    session_id: session_id.clone(),
                    message: err.to_string(),
                })?;
            administration
                .effect_host()
                .complete_artifact_owner_retirement(&scope)
                .await
                .map_err(|err| EmbedError::SessionDeleteProcess {
                    session_id: session_id.clone(),
                    message: err.to_string(),
                })?;
        }
        Ok(SessionDeleteReport {
            session_id,
            storage,
            process,
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
        let process_work = self.substrate_slot.configured_worker_process_work();
        let queued_work: Arc<dyn SessionWorkEngine> = match &self.substrate_slot.setup.queued {
            QueuedPortSetup::External { port } => Arc::clone(port),
            // The outer dispatcher owns the native queued-work lane; nested
            // process runtimes must not start a competing dispatcher.
            QueuedPortSetup::Disabled | QueuedPortSetup::Native { .. } => {
                Arc::new(NoSessionWork::new())
            }
        };
        worker_config(
            &plugin_host,
            &self.env,
            self.process_lifecycle_available,
            self.policy.clone(),
            self.process_execution_concurrency,
            self.worker_slot_supplier.clone(),
            self.session_execution_owner.clone(),
            process_work,
            queued_work,
            self.process_event_sink.clone(),
            lash_core::runtime::RuntimeTurnPhaseProbeSlot::default(),
            self.substrate_slot.setup.config.clone(),
        )
    }
}

#[derive(Clone)]
struct NativeProcessWorkerSetup {
    worker_plugin_host: PluginHost,
    env: RuntimeEnvironment,
    process_lifecycle_available: bool,
    policy: SessionPolicy,
    process_execution_concurrency: usize,
    worker_slot_supplier: Option<Arc<dyn WorkerSlotSupplier>>,
    session_execution_owner: lash_core::LeaseOwnerIdentity,
    process_work: WorkerProcessWork,
    process_event_sink: Option<Arc<dyn facade_support::ProcessEventSink>>,
    turn_phase_probe_slot: lash_core::runtime::RuntimeTurnPhaseProbeSlot,
    native_substrate: NativeSubstrateConfig,
}

impl NativeProcessWorkerSetup {
    fn build(&self, queued_work: Arc<dyn SessionWorkEngine>) -> Result<DurableProcessWorkerConfig> {
        worker_config(
            &self.worker_plugin_host,
            &self.env,
            self.process_lifecycle_available,
            self.policy.clone(),
            self.process_execution_concurrency,
            self.worker_slot_supplier.clone(),
            self.session_execution_owner.clone(),
            self.process_work.clone(),
            queued_work,
            self.process_event_sink.clone(),
            self.turn_phase_probe_slot.clone(),
            self.native_substrate.clone(),
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn worker_config(
    worker_plugin_host: &PluginHost,
    env: &RuntimeEnvironment,
    process_lifecycle_available: bool,
    policy: SessionPolicy,
    process_execution_concurrency: usize,
    worker_slot_supplier: Option<Arc<dyn WorkerSlotSupplier>>,
    session_execution_owner: lash_core::LeaseOwnerIdentity,
    process_work: WorkerProcessWork,
    queued_work: Arc<dyn SessionWorkEngine>,
    process_event_sink: Option<Arc<dyn facade_support::ProcessEventSink>>,
    turn_phase_probe_slot: lash_core::runtime::RuntimeTurnPhaseProbeSlot,
    native_substrate: NativeSubstrateConfig,
) -> Result<DurableProcessWorkerConfig> {
    let runtime_host = worker_plugin_host
        .install_process_engine_contributions(env.core.clone(), process_lifecycle_available)?;
    let mut config = DurableProcessWorkerConfig::new(
        Arc::new(worker_plugin_host.clone()),
        runtime_host,
        process_work,
        queued_work,
        session_execution_owner,
    )
    .with_session_policy(policy)
    .with_turn_phase_probe_slot(turn_phase_probe_slot)
    .with_process_execution_concurrency(process_execution_concurrency)?;
    config.native_substrate = native_substrate;
    if let Some(worker_slot_supplier) = worker_slot_supplier {
        config = config.with_worker_slot_supplier(worker_slot_supplier);
    }
    if let Some(sink) = process_event_sink {
        config = config.with_process_event_sink(sink);
    }
    Ok(config)
}

fn default_runtime_stack() -> PluginStack {
    lash_plugin_tool_output_budget::tool_output_budget_stack()
}

/// Builder for configuring lash core over one [`Backend`].
pub struct LashCoreBuilder {
    pub(crate) protocol_factory: Option<Arc<dyn PluginFactory>>,
    session_spec: SessionSpec,
    provider: Option<ProviderHandle>,
    /// The one substrate every persistence port and the effect host come from.
    backend: Backend,
    commit_budget: Option<facade_support::CommitBudget>,
    queued_work_batching: Option<facade_support::QueuedWorkBatchingConfig>,
    max_attachment_bytes: Option<Option<u64>>,
    process_wake_delivery_policy: Option<lash_core::DeliveryPolicy>,
    native_substrate: NativeSubstrateConfig,
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
    lease_timings: Option<facade_support::LeaseTimings>,
    // Per-worker bound for the default native process executor.
    process_execution_concurrency: Option<usize>,
    // Per-driver bound for the default native queued-work executor.
    queued_work_execution_concurrency: Option<usize>,
    // Optional host admission controller replacing both fixed worker lanes.
    worker_slot_supplier: Option<Arc<dyn WorkerSlotSupplier>>,
    // Optional host-facing best-effort feed of appended process events,
    // installed on the native process-registry decorator at build time.
    process_event_sink: Option<Arc<dyn facade_support::ProcessEventSink>>,
    process_tool_visibility_filter: Option<Arc<dyn facade_support::ProcessToolVisibilityFilter>>,
    queued_work_source: QueuedWorkSource,
    live_replay_store: Option<Arc<dyn LiveReplayStore>>,
    process_observation_config: crate::process_observation::ProcessObservationConfig,
}

impl LashCoreBuilder {
    fn new(backend: Backend, turn_budget: lash_core::TurnBudget) -> Self {
        Self {
            protocol_factory: None,
            session_spec: SessionSpec::new().turn_budget(turn_budget),
            provider: None,
            backend,
            commit_budget: None,
            queued_work_batching: None,
            max_attachment_bytes: None,
            process_wake_delivery_policy: None,
            native_substrate: NativeSubstrateConfig::default(),
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
            lease_timings: None,
            process_execution_concurrency: None,
            queued_work_execution_concurrency: None,
            worker_slot_supplier: None,
            process_event_sink: None,
            process_tool_visibility_filter: None,
            queued_work_source: QueuedWorkSource::Backend,
            live_replay_store: None,
            process_observation_config: Default::default(),
        }
    }

    pub fn protocol_plugin(mut self, plugin: Arc<dyn PluginFactory>) -> Self {
        self.protocol_factory = Some(plugin);
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

    pub fn native_substrate_config(mut self, config: NativeSubstrateConfig) -> Self {
        self.native_substrate = config;
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

    /// Configure the lease timing capability for every durable single-writer
    /// lease lane this deployment renews: session execution leases, process
    /// leases, and durable effect-replay leases. Queued-work and turn-input
    /// claims are not leases and carry no TTL.
    ///
    /// This is the failover-latency vs false-takeover-risk knob.
    /// Like [`process_execution_concurrency`](Self::process_execution_concurrency) it is an
    /// operational deployment decision, so it lives on the main builder tier rather than
    /// behind [`advanced`](Self::advanced).
    /// Effect hosts accept the same type at construction (e.g.
    /// SQLite/Postgres effect-replay options), so a host can share one timing decision across
    /// both boundaries.
    pub fn lease_timings(mut self, lease_timings: facade_support::LeaseTimings) -> Self {
        self.lease_timings = Some(lease_timings);
        self
    }

    /// Configure the bounded live replay buffer used by session observation
    /// cursors. This is best-effort reconnect recovery only; durable state
    /// still comes from the session store and [`SessionReadView`].
    pub fn live_replay_store(mut self, live_replay_store: Arc<dyn LiveReplayStore>) -> Self {
        self.live_replay_store = Some(live_replay_store);
        self
    }

    /// Build a core under the host's stable worker/process lease identity.
    ///
    /// The owner id is stable for the worker or process and never scoped to a
    /// turn. The incarnation id changes once per process boot.
    pub fn build(
        mut self,
        session_execution_owner: lash_core::LeaseOwnerIdentity,
    ) -> Result<LashCore> {
        let process_execution_concurrency = self
            .process_execution_concurrency
            .unwrap_or(lash_core_worker::DEFAULT_PROCESS_EXECUTION_CONCURRENCY);
        DurableProcessWorkerConfig::validate_process_execution_concurrency(
            process_execution_concurrency,
        )?;
        let queued_work_execution_concurrency = self
            .queued_work_execution_concurrency
            .unwrap_or(facade_support::DEFAULT_QUEUED_WORK_EXECUTION_CONCURRENCY);
        NativeQueuedWork::validate_execution_concurrency(queued_work_execution_concurrency)?;
        self.native_substrate.validate()?;
        let worker_slot_supplier = self.worker_slot_supplier.clone();
        let native_substrate = self.native_substrate.clone();
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
        let process_work_selection = match backend.process_work() {
            Some(wiring) => ProcessWorkSelection::External(wiring),
            None => ProcessWorkSelection::Native(backend.process_registry()),
        };
        let external_process_work =
            matches!(&process_work_selection, ProcessWorkSelection::External(_));
        let process_lifecycle_feed = Arc::new(crate::process_lifecycle::ProcessLifecycleFeed::new(
            Arc::clone(&live_replay_store),
            Arc::clone(&process_observation_hub),
            self.process_event_sink.clone(),
            !external_process_work,
        ));
        let process_event_sink: Option<Arc<dyn facade_support::ProcessEventSink>> =
            Some(process_lifecycle_feed.clone());
        let process_work_source =
            process_work_selection.resolve(Arc::clone(&core.clock), process_event_sink.clone());
        let process_lifecycle_registration =
            if let ProcessWorkSource::External(wiring) = &process_work_source {
                process_event_sink
                    .clone()
                    .map(|sink| Arc::new(wiring.watched().add_event_sink(sink)))
            } else {
                None
            };
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
        let process_registry = process_work_source.process_registry();
        process_lifecycle_feed.bind_registry(Arc::clone(&process_registry));
        let mut env_builder =
            RuntimeEnvironment::builder(core).with_plugin_host(Arc::clone(&default_plugin_host));
        env_builder = match &process_work_source {
            ProcessWorkSource::Native(_) => {
                env_builder.with_process_registry(Arc::clone(&process_registry))
            }
            ProcessWorkSource::External(wiring) => env_builder.with_process_work(wiring.clone()),
        };
        let env = env_builder.build();
        // Registration owns the scope fence (ADR 0049): the registry lifts the
        // effect host's fence for a re-registered process id inside its own
        // registration write, on every registration path.
        process_registry.bind_effect_host(&env.core.control.effect_host);
        // The retained-evidence sweep owns deferred scope retirement (ADR
        // 0067): the catalog learns the host whose journal it sweeps.
        store_factory.bind_effect_host(&env.core.control.effect_host);
        store_factory.bind_artifact_stores(
            Arc::clone(&env.core.durability.process_env_store),
            host_process_engines.clone(),
        );
        let process_port = Self::resolve_process_work(
            &process_work_source,
            default_plugin_host.as_ref(),
            &env,
            process_lifecycle_available,
            &policy,
            process_execution_concurrency,
            worker_slot_supplier.clone(),
            session_execution_owner.clone(),
            process_event_sink.clone(),
            native_substrate.clone(),
        )?;
        let residents = Arc::new(residents::ResidentSessions::default());
        let (queued_port, session_driver) = Self::resolve_queued_work(
            Arc::clone(&residents),
            self.queued_work_source,
            backend.session_work(),
            session_execution_owner.clone(),
            env.clone(),
            policy.clone(),
            protocol_factory.clone(),
            Arc::new(plugin_factories.clone()),
            &store_factory,
            Arc::clone(&live_replay_store),
            process_lifecycle_available,
            worker_slot_supplier.clone(),
            queued_work_execution_concurrency,
        );
        let substrate = NativeSubstrateSetup {
            config: native_substrate,
            process: process_port,
            queued: queued_port,
            wake: WakeDeliveryDriverSetup {
                registry: Arc::clone(&process_registry),
                factory: Arc::clone(&store_factory),
                clock: Arc::clone(&env.core.clock),
                delivery_policy: env.core.control.process_wake_delivery_policy,
            },
        };

        let drive_lifetime = DriveLifetime::new();
        let substrate_slot = Arc::new(NativeSubstrateSlot::new(substrate, &drive_lifetime));
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
            session_execution_owner.clone(),
        );
        Ok(LashCore {
            session_execution_owner,
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
            process_execution_concurrency,
            worker_slot_supplier,
            substrate_slot,
            drive_lifetime,
            _session_driver: session_driver,
            residents,
            process_event_sink,
            tool_intent_submission_gates: Default::default(),
            tool_child_context_source,
        })
    }

    /// - the backend supplies its own process work => use it
    ///   ([`ProcessPortSetup::External`]);
    /// - otherwise the in-process worker drives the backend's registry,
    ///   constructed lazily on first open ([`ProcessPortSetup::NativeDefault`]).
    ///   Its [`DurableProcessWorkerConfig`] is built eagerly so a bad config
    ///   fails the build.
    // Mirrors `resolve_queued_work`; inputs are the required driver state.
    #[allow(clippy::too_many_arguments)]
    fn resolve_process_work(
        process_work_source: &ProcessWorkSource,
        worker_plugin_host: &PluginHost,
        env: &RuntimeEnvironment,
        process_lifecycle_available: bool,
        policy: &SessionPolicy,
        process_execution_concurrency: usize,
        worker_slot_supplier: Option<Arc<dyn WorkerSlotSupplier>>,
        session_execution_owner: lash_core::LeaseOwnerIdentity,
        process_event_sink: Option<Arc<dyn facade_support::ProcessEventSink>>,
        native_substrate: NativeSubstrateConfig,
    ) -> Result<ProcessPortSetup> {
        let watched = match process_work_source {
            ProcessWorkSource::External(wiring) => {
                return Ok(ProcessPortSetup::External {
                    wiring: wiring.clone(),
                });
            }
            ProcessWorkSource::Native(watched) => watched.clone(),
        };
        let config = Box::new(NativeProcessWorkerSetup {
            worker_plugin_host: worker_plugin_host.clone(),
            env: env.clone(),
            process_lifecycle_available,
            policy: policy.clone(),
            process_execution_concurrency,
            worker_slot_supplier,
            session_execution_owner,
            process_work: WorkerProcessWork::SelfNative(watched.clone()),
            // Admission-only drive faults otherwise have no path to the host.
            process_event_sink,
            turn_phase_probe_slot: lash_core::runtime::RuntimeTurnPhaseProbeSlot::default(),
            native_substrate,
        });
        // The live native worker is constructed lazily once the outer queued-work dispatcher
        // exists.
        config.build(Arc::new(NoSessionWork::new()))?;
        Ok(ProcessPortSetup::NativeDefault { config, watched })
    }

    /// The core's session driver and where it runs (FIG-3600): installed on
    /// the backend's own session-work engine when it has one, else run by the
    /// in-process engine of the interim SQL backends. Returns the port setup
    /// and the driver the core keeps.
    #[allow(clippy::too_many_arguments)]
    fn resolve_queued_work(
        residents: Arc<residents::ResidentSessions>,
        queued_work_source: QueuedWorkSource,
        backend_engine: Option<Arc<dyn lash_core::SessionWorkEngine>>,
        session_execution_owner: lash_core::LeaseOwnerIdentity,
        env: RuntimeEnvironment,
        policy: SessionPolicy,
        protocol_factory: Option<Arc<dyn PluginFactory>>,
        plugin_factories: Arc<Vec<Arc<dyn PluginFactory>>>,
        store_factory: &Arc<dyn SessionStoreFactory>,
        live_replay_store: Arc<dyn LiveReplayStore>,
        process_lifecycle_available: bool,
        worker_slot_supplier: Option<Arc<dyn WorkerSlotSupplier>>,
        queued_work_execution_concurrency: usize,
    ) -> (QueuedPortSetup, Arc<dyn lash_core::SessionDriver>) {
        let owner = session_execution_owner.clone();
        let build_generation = env.core.backend().build_generation().clone();
        let driver = Arc::new(NativeQueuedWorkRunHandle::new(Arc::new(
            NativeQueuedWorkRunConfig {
                residents,
                session_execution_owner,
                env,
                policy,
                protocol_factory,
                plugin_factories,
                store_factory: Arc::clone(store_factory),
                live_replay_store,
                process_lifecycle_available,
            },
        )));
        match (queued_work_source, backend_engine) {
            // The host turned the backend's own engine off: a send is
            // refused, since nothing would drive what it accepted.
            (QueuedWorkSource::Disabled, Some(_)) => (QueuedPortSetup::Disabled, driver),
            // No engine on the backend and the host drains queued work itself:
            // a waiting send drives its session in the caller's task (D1 §2.4;
            // S5d deletes `without_queued_work` and this arm together).
            (QueuedWorkSource::Disabled, None) => {
                let port: Arc<dyn lash_core::SessionWorkEngine> =
                    Arc::new(lash_core::runtime::InlineSessionWork::new(build_generation));
                let installed = install_session_driver(&port, driver, &owner);
                (QueuedPortSetup::External { port }, installed)
            }
            (QueuedWorkSource::Backend, Some(port)) => {
                let installed = install_session_driver(&port, driver, &owner);
                (QueuedPortSetup::External { port }, installed)
            }
            (QueuedWorkSource::Backend, None) => (
                QueuedPortSetup::Native {
                    driver: Arc::clone(&driver),
                    slot_supplier: worker_slot_supplier,
                    execution_concurrency: queued_work_execution_concurrency,
                },
                driver,
            ),
        }
    }

    pub fn advanced(self) -> AdvancedLashCoreBuilder {
        AdvancedLashCoreBuilder { builder: self }
    }

    /// Each appended process event is pushed to the sink after its durable
    /// write, in per-process append order. This is freshness, not truth: it
    /// never buffers or retries, and consumers reconcile from the durable event
    /// log. Observe completion via the await seam even though the terminal
    /// append is also emitted. See [`ProcessEventSink`] for the full contract.
    ///
    /// Event emission applies to the in-process registry path; a backend
    /// whose engine runs its own processes installs the sink through its
    /// own constructor.
    ///
    /// Worker faults are not registry events and do not follow that split: the
    /// durable process worker this core configures reports every
    /// [`ProcessWorkerFault`](facade_support::ProcessWorkerFault) to the sink
    /// installed here, whichever process work the backend supplies. A host
    /// that drives pending processes wants this installed, because the drive
    /// is an admission call and a fault after admission has no other way home.
    ///
    /// [`ProcessWorkerFault`]: facade_support::ProcessWorkerFault
    ///
    /// [`ProcessEventSink`]: facade_support::ProcessEventSink
    pub fn process_event_sink(mut self, sink: Arc<dyn facade_support::ProcessEventSink>) -> Self {
        self.process_event_sink = Some(sink);
        self
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

    /// Run no queued-work driver: the host runs every queued turn itself.
    ///
    /// By default the core runs the backend's driver — the in-process driver
    /// on SQLite and PostgreSQL, the engine's where the backend supplies one
    /// ([`Backend::queued_work`](lash_core::Backend::queued_work)).
    pub fn without_queued_work(mut self) -> Self {
        self.queued_work_source = QueuedWorkSource::Disabled;
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
    /// store only. Like [`Self::sessions`], it never opens a session, claims
    /// the execution lease, or hydrates a checkpoint, so a host can pair it
    /// with a lease-free durable read.
    pub fn observation_cursor(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.live_replay_store.current_cursor(session_id, revision)
    }

    /// Enumerate every durable session catalog entry.
    ///
    /// This is a read-only catalog query. It does not open sessions, acquire
    /// execution leases, hydrate checkpoints, or mutate catalog generations.
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
    if !Arc::ptr_eq(&installed, &driver) {
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
